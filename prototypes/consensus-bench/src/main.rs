//! D15 prototype: real consensus, not just the substrate.
//!
//! An earlier version measured only the per-group task and timer cost. This one
//! runs actual openraft groups and exercises what D15 asks about: election,
//! partition, membership change, snapshot transfer, large batches, and the cost
//! of many small groups on one process.
//!
//! STORAGE.md section 7 places hundreds of three-voter tablet groups on one
//! storage node. A general Raft library targets a few large groups, so the
//! multi-group case is the TallyOwl-specific risk.
//!
//! SCOPE. Storage is in memory and the network is in process. This measures the
//! library and the algorithm, not TallyOwl's durable storage or its transport.
//! A real deployment adds an fsync to every append, and BENCHMARKS.md sections
//! 3 and 13 measure that separately.
//!
//! This is decision-support code. It is not product code.

use openraft::error::{InstallSnapshotError, RPCError, RaftError};
use openraft::network::{RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use openraft::storage::{LogFlushed, LogState, RaftLogStorage, RaftStateMachine, Snapshot};
use openraft::{RaftLogId,
    BasicNode, Entry, EntryPayload, LogId, OptionalSend, RaftLogReader, RaftSnapshotBuilder,
    SnapshotMeta, StorageError, StoredMembership, Vote,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub type NodeId = u64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub applied: u64,
}

openraft::declare_raft_types!(
    pub TypeConfig:
        D = Request,
        R = Response,
        NodeId = NodeId,
        Node = BasicNode,
        Entry = Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
        AsyncRuntime = openraft::TokioRuntime,
);

// ---------------------------------------------------------------------------
// In-memory storage
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct StateMachineData {
    last_applied: Option<LogId<NodeId>>,
    membership: StoredMembership<NodeId, BasicNode>,
    applied_count: u64,
}

#[derive(Debug, Default)]
struct StoreInner {
    log: BTreeMap<u64, Entry<TypeConfig>>,
    committed: Option<LogId<NodeId>>,
    vote: Option<Vote<NodeId>>,
    sm: StateMachineData,
    snapshot: Option<(SnapshotMeta<NodeId, BasicNode>, Vec<u8>)>,
    snapshot_index: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Store {
    inner: Arc<Mutex<StoreInner>>,
}

impl RaftLogReader<TypeConfig> for Store {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<TypeConfig>>, StorageError<NodeId>> {
        let g = self.inner.lock().unwrap();
        Ok(g.log.range(range).map(|(_, e)| e.clone()).collect())
    }
}

impl RaftSnapshotBuilder<TypeConfig> for Store {
    async fn build_snapshot(&mut self) -> Result<Snapshot<TypeConfig>, StorageError<NodeId>> {
        let mut g = self.inner.lock().unwrap();
        let last_applied = g.sm.last_applied;
        let membership = g.sm.membership.clone();
        g.snapshot_index += 1;
        let id = format!(
            "{}-{}",
            last_applied.map(|l| l.index).unwrap_or(0),
            g.snapshot_index
        );
        // The payload models a state machine snapshot. Its size is what a
        // lagging follower must transfer to catch up.
        let data = vec![0u8; 256 * 1024];
        let meta = SnapshotMeta {
            last_log_id: last_applied,
            last_membership: membership,
            snapshot_id: id,
        };
        g.snapshot = Some((meta.clone(), data.clone()));
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }
}

impl RaftLogStorage<TypeConfig> for Store {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        let g = self.inner.lock().unwrap();
        let last = g.log.iter().next_back().map(|(_, e)| *e.get_log_id());
        let last_purged = g.snapshot.as_ref().and_then(|(m, _)| m.last_log_id);
        Ok(LogState {
            last_purged_log_id: last_purged,
            last_log_id: last.or(last_purged),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.inner.lock().unwrap().vote = Some(*vote);
        Ok(())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.lock().unwrap().vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<NodeId>>,
    ) -> Result<(), StorageError<NodeId>> {
        self.inner.lock().unwrap().committed = committed;
        Ok(())
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        Ok(self.inner.lock().unwrap().committed)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<TypeConfig>,
    ) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        {
            let mut g = self.inner.lock().unwrap();
            for e in entries {
                g.log.insert(e.get_log_id().index, e);
            }
        }
        // A durable implementation calls fsync here. BENCHMARKS.md section 3
        // measures what that costs and section 13 measures group commit.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut g = self.inner.lock().unwrap();
        let keys: Vec<u64> = g.log.range(log_id.index..).map(|(k, _)| *k).collect();
        for k in keys {
            g.log.remove(&k);
        }
        Ok(())
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        let mut g = self.inner.lock().unwrap();
        let keys: Vec<u64> = g.log.range(..=log_id.index).map(|(k, _)| *k).collect();
        for k in keys {
            g.log.remove(&k);
        }
        Ok(())
    }
}

impl RaftStateMachine<TypeConfig> for Store {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<NodeId>>, StoredMembership<NodeId, BasicNode>), StorageError<NodeId>>
    {
        let g = self.inner.lock().unwrap();
        Ok((g.sm.last_applied, g.sm.membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<Response>, StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry<TypeConfig>> + OptionalSend,
    {
        let mut g = self.inner.lock().unwrap();
        let mut out = Vec::new();
        for e in entries {
            let log_id = *e.get_log_id();
            g.sm.last_applied = Some(log_id);
            match e.payload {
                EntryPayload::Normal(_) => {
                    g.sm.applied_count += 1;
                }
                EntryPayload::Membership(m) => {
                    g.sm.membership = StoredMembership::new(Some(log_id), m);
                }
                EntryPayload::Blank => {}
            }
            out.push(Response {
                applied: g.sm.applied_count,
            });
        }
        Ok(out)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<NodeId>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<NodeId, BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<NodeId>> {
        let mut g = self.inner.lock().unwrap();
        g.sm.last_applied = meta.last_log_id;
        g.sm.membership = meta.last_membership.clone();
        g.snapshot = Some((meta.clone(), snapshot.into_inner()));
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<TypeConfig>>, StorageError<NodeId>> {
        let g = self.inner.lock().unwrap();
        Ok(g.snapshot.as_ref().map(|(m, d)| Snapshot {
            meta: m.clone(),
            snapshot: Box::new(Cursor::new(d.clone())),
        }))
    }
}

// ---------------------------------------------------------------------------
// In-process network with a partition switch for each node
// ---------------------------------------------------------------------------

type RaftHandle = openraft::Raft<TypeConfig>;

#[derive(Clone, Default)]
struct Cluster {
    nodes: Arc<Mutex<BTreeMap<NodeId, RaftHandle>>>,
    /// An isolated node drops every message in both directions, which is what a
    /// network partition looks like to Raft.
    isolated: Arc<Mutex<BTreeMap<NodeId, Arc<AtomicBool>>>>,
}

impl Cluster {
    fn isolate(&self, id: NodeId, on: bool) {
        if let Some(f) = self.isolated.lock().unwrap().get(&id) {
            f.store(on, Ordering::SeqCst);
        }
    }
    fn is_isolated(&self, id: NodeId) -> bool {
        self.isolated
            .lock()
            .unwrap()
            .get(&id)
            .map(|f| f.load(Ordering::SeqCst))
            .unwrap_or(false)
    }
    fn get(&self, id: NodeId) -> Option<RaftHandle> {
        self.nodes.lock().unwrap().get(&id).cloned()
    }
}

#[derive(Clone)]
struct Net {
    cluster: Cluster,
    from: NodeId,
}

#[derive(Clone)]
struct Conn {
    cluster: Cluster,
    from: NodeId,
    to: NodeId,
}

impl RaftNetworkFactory<TypeConfig> for Net {
    type Network = Conn;
    async fn new_client(&mut self, target: NodeId, _node: &BasicNode) -> Self::Network {
        Conn {
            cluster: self.cluster.clone(),
            from: self.from,
            to: target,
        }
    }
}

fn unreachable<E: std::error::Error + 'static>(to: NodeId) -> RPCError<NodeId, BasicNode, E> {
    RPCError::Unreachable(openraft::error::Unreachable::new(&std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        format!("node {to}"),
    )))
}

impl RaftNetwork<TypeConfig> for Conn {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _o: openraft::network::RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        if self.cluster.is_isolated(self.from) || self.cluster.is_isolated(self.to) {
            return Err(unreachable(self.to));
        }
        let target = self.cluster.get(self.to).ok_or_else(|| unreachable(self.to))?;
        target.append_entries(rpc).await.map_err(|e| {
            RPCError::RemoteError(openraft::error::RemoteError::new(self.to, e))
        })
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _o: openraft::network::RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        if self.cluster.is_isolated(self.from) || self.cluster.is_isolated(self.to) {
            return Err(unreachable(self.to));
        }
        let target = self.cluster.get(self.to).ok_or_else(|| unreachable(self.to))?;
        target.install_snapshot(rpc).await.map_err(|e| {
            RPCError::RemoteError(openraft::error::RemoteError::new(self.to, e))
        })
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _o: openraft::network::RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        if self.cluster.is_isolated(self.from) || self.cluster.is_isolated(self.to) {
            return Err(unreachable(self.to));
        }
        let target = self.cluster.get(self.to).ok_or_else(|| unreachable(self.to))?;
        target.vote(rpc).await.map_err(|e| {
            RPCError::RemoteError(openraft::error::RemoteError::new(self.to, e))
        })
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn config() -> Arc<openraft::Config> {
    Arc::new(
        openraft::Config {
            heartbeat_interval: 150,
            election_timeout_min: 300,
            election_timeout_max: 600,
            ..Default::default()
        }
        .validate()
        .unwrap(),
    )
}

async fn build_group(ids: &[NodeId], cluster: &Cluster) {
    for id in ids {
        let store = Store::default();
        let net = Net {
            cluster: cluster.clone(),
            from: *id,
        };
        let raft = openraft::Raft::new(*id, config(), net, store.clone(), store.clone())
            .await
            .unwrap();
        cluster.nodes.lock().unwrap().insert(*id, raft);
        cluster
            .isolated
            .lock()
            .unwrap()
            .insert(*id, Arc::new(AtomicBool::new(false)));
    }
}

async fn leader_of(cluster: &Cluster, ids: &[NodeId]) -> Option<NodeId> {
    for id in ids {
        if let Some(r) = cluster.get(*id) {
            if r.current_leader().await == Some(*id) {
                return Some(*id);
            }
        }
    }
    None
}

async fn wait_for_leader(
    cluster: &Cluster,
    ids: &[NodeId],
    limit: Duration,
) -> Option<(NodeId, Duration)> {
    let t = Instant::now();
    while t.elapsed() < limit {
        if let Some(id) = leader_of(cluster, ids).await {
            return Some((id, t.elapsed()));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    None
}

fn short(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(88).collect()
}

fn rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<f64>().ok())
            })
        })
        .map(|k| k / 1024.0)
        .unwrap_or(0.0)
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    println!("# D15 consensus benchmark, openraft 0.9.24");
    println!("SCOPE: in-memory storage, in-process network.");
    println!("It measures the library and the algorithm, not durable storage.");
    println!("Add the fsync cost from BENCHMARKS.md section 3 for a real deployment.");
    println!();

    // -- Formation ----------------------------------------------------------
    println!("## Formation");
    let cluster = Cluster::default();
    let ids = [1u64, 2, 3];
    build_group(&ids, &cluster).await;

    let mut members = BTreeMap::new();
    for id in ids {
        members.insert(id, BasicNode::default());
    }
    cluster
        .get(1)
        .unwrap()
        .initialize(members.clone())
        .await
        .unwrap();
    let (leader, elected) = wait_for_leader(&cluster, &ids, Duration::from_secs(10))
        .await
        .expect("no leader formed");
    println!(
        "initialize to leader   {:>8.0} ms   (node {leader})",
        elected.as_secs_f64() * 1000.0
    );

    // -- Commit throughput --------------------------------------------------
    println!();
    println!("## Commit throughput, three voters");
    println!(
        "{:<16} {:>12} {:>14} {:>12} {:>12}",
        "entry size", "entries/s", "MiB/s", "p50 ms", "p99 ms"
    );
    for size in [256usize, 4096, 64 * 1024] {
        let raft = cluster.get(leader).unwrap();
        let n = if size >= 64 * 1024 { 400 } else { 3000 };
        let payload = vec![7u8; size];
        let mut lat = Vec::with_capacity(n);
        let start = Instant::now();
        for _ in 0..n {
            let t0 = Instant::now();
            raft.client_write(Request {
                payload: payload.clone(),
            })
            .await
            .unwrap();
            lat.push(t0.elapsed().as_micros() as u64);
        }
        let el = start.elapsed().as_secs_f64();
        lat.sort_unstable();
        println!(
            "{:<16} {:>12.0} {:>14.1} {:>12.2} {:>12.2}",
            format!("{} B", size),
            n as f64 / el,
            (n * size) as f64 / (1024.0 * 1024.0) / el,
            lat[lat.len() / 2] as f64 / 1000.0,
            lat[lat.len() * 99 / 100] as f64 / 1000.0
        );
    }

    // -- Leader loss --------------------------------------------------------
    println!();
    println!("## Leader loss and partition");
    let old = leader_of(&cluster, &ids).await.unwrap();
    cluster.isolate(old, true);
    let survivors: Vec<NodeId> = ids.iter().copied().filter(|i| *i != old).collect();
    let new_leader = match wait_for_leader(&cluster, &survivors, Duration::from_secs(10)).await {
        Some((n, took)) => {
            println!(
                "isolate node {old}, new leader {n} in {:>6.0} ms",
                took.as_secs_f64() * 1000.0
            );
            n
        }
        None => {
            println!("isolate node {old}: NO new leader within 10 s");
            return;
        }
    };

    let r = cluster.get(new_leader).unwrap();
    let t0 = Instant::now();
    r.client_write(Request {
        payload: vec![1u8; 128],
    })
    .await
    .unwrap();
    println!(
        "majority still commits {:>8.2} ms",
        t0.elapsed().as_secs_f64() * 1000.0
    );

    // The isolated node must not commit. A commit here would be split brain.
    let minority = cluster.get(old).unwrap();
    let t0 = Instant::now();
    let res = tokio::time::timeout(
        Duration::from_secs(3),
        minority.client_write(Request {
            payload: vec![2u8; 128],
        }),
    )
    .await;
    match res {
        Err(_) => println!(
            "minority write         blocked after {:>4.1} s, no commit",
            t0.elapsed().as_secs_f64()
        ),
        Ok(Err(e)) => println!("minority write         refused: {}", short(&format!("{e}"))),
        Ok(Ok(_)) => println!("minority write         COMMITTED. That would be split brain."),
    }

    // -- Rejoin -------------------------------------------------------------
    println!();
    println!("## Rejoin");
    cluster.isolate(old, false);
    let t0 = Instant::now();
    let mut converged = false;
    while t0.elapsed() < Duration::from_secs(20) {
        let a = cluster.get(new_leader).unwrap().metrics().borrow().last_applied;
        let b = cluster.get(old).unwrap().metrics().borrow().last_applied;
        if a.is_some() && a == b {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if converged {
        println!(
            "rejoined node caught up {:>7.0} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
    } else {
        println!("rejoined node did NOT catch up within 20 s");
    }

    // -- Membership ---------------------------------------------------------
    println!();
    println!("## Membership change");
    {
        let store = Store::default();
        let net = Net {
            cluster: cluster.clone(),
            from: 4,
        };
        let raft = openraft::Raft::new(4, config(), net, store.clone(), store.clone())
            .await
            .unwrap();
        cluster.nodes.lock().unwrap().insert(4, raft);
        cluster
            .isolated
            .lock()
            .unwrap()
            .insert(4, Arc::new(AtomicBool::new(false)));
    }
    let l = leader_of(&cluster, &ids).await.unwrap_or(new_leader);
    let raft = cluster.get(l).unwrap();
    let t0 = Instant::now();
    match raft.add_learner(4, BasicNode::default(), true).await {
        Ok(_) => println!(
            "add learner            {:>8.0} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        ),
        Err(e) => println!("add learner failed: {}", short(&format!("{e}"))),
    }
    let t0 = Instant::now();
    match raft.change_membership([1u64, 2, 3, 4], false).await {
        Ok(_) => println!(
            "promote to voter       {:>8.0} ms   (now four voters)",
            t0.elapsed().as_secs_f64() * 1000.0
        ),
        Err(e) => println!("promote failed: {}", short(&format!("{e}"))),
    }

    // -- Snapshot -----------------------------------------------------------
    println!();
    println!("## Snapshot");
    let raft = cluster.get(l).unwrap();
    let t0 = Instant::now();
    match raft.trigger().snapshot().await {
        Ok(_) => {
            let mut built = false;
            while t0.elapsed() < Duration::from_secs(10) {
                if cluster.get(l).unwrap().metrics().borrow().snapshot.is_some() {
                    built = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            if built {
                println!(
                    "build 256 KiB snapshot {:>8.0} ms",
                    t0.elapsed().as_secs_f64() * 1000.0
                );
            } else {
                println!("snapshot not built within 10 s");
            }
        }
        Err(e) => println!("snapshot trigger failed: {}", short(&format!("{e}"))),
    }

    for id in [1u64, 2, 3, 4] {
        if let Some(r) = cluster.get(id) {
            let _ = r.shutdown().await;
        }
    }

    // -- Many groups --------------------------------------------------------
    println!();
    println!("## Many three-voter groups on one process");
    println!(
        "{:<10} {:>12} {:>14} {:>16}",
        "groups", "build ms", "RSS MiB", "with a leader"
    );
    for groups in [10usize, 50, 200] {
        let base_rss = rss_mib();
        let t0 = Instant::now();
        let mut built = Vec::with_capacity(groups);
        for g in 0..groups {
            let c = Cluster::default();
            let gid = (g as u64 + 1) * 100;
            let m: Vec<NodeId> = vec![gid + 1, gid + 2, gid + 3];
            build_group(&m, &c).await;
            let mut mm = BTreeMap::new();
            for id in &m {
                mm.insert(*id, BasicNode::default());
            }
            c.get(m[0]).unwrap().initialize(mm).await.unwrap();
            built.push((c, m));
        }
        let build_ms = t0.elapsed().as_secs_f64() * 1000.0;
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let mut with_leader = 0usize;
        for (c, m) in &built {
            if leader_of(c, m).await.is_some() {
                with_leader += 1;
            }
        }
        println!(
            "{:<10} {:>12.0} {:>14.1} {:>12}/{:<4}",
            groups,
            build_ms,
            rss_mib() - base_rss,
            with_leader,
            groups
        );
        for (c, m) in built {
            for id in m {
                if let Some(r) = c.get(id) {
                    let _ = r.shutdown().await;
                }
            }
        }
    }

    println!();
    println!("A group without a leader cannot accept a write.");
}
