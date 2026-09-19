//! The node that is alive but slow, and the voter whose disk is full.
//!
//! `docs/FAILURE_MODES.md` section 6.1 states the problem in one sentence: "A
//! dead node is easy. A node that answers every request slowly holds up writes
//! while looking healthy, and a health check that only asks 'are you there'
//! says yes."
//!
//! # Against the group, never against a constant
//!
//! A node is slow when it is much slower than the group it is in, for long
//! enough. An absolute threshold would call every node slow during a busy hour
//! and no node slow on fast hardware. `placement.slowNode.factor` is the
//! multiple of the group median and `placement.slowNode.duration` is how long
//! it must hold, and both defaults are deliberately forgiving because a brief
//! spike is not a sick node.
//!
//! # The alert must say why
//!
//! A node that reports only "slow" sends an operator to look at the wrong
//! thing. [`diagnose`] is the cause table from section 6.1, in order of how
//! specific the evidence is, and it returns [`SlowCause::Unknown`] when nothing
//! matches.
//!
//! **`unknown` is a valid answer and is reported as one.** A guessed cause
//! sends an operator down a wrong path, which is worse than sending them
//! nowhere. When the cause is unknown the condition still carries every raw
//! reading beside the group median for the same measure, so the operator has
//! what the controller had.
//!
//! # `alert` rather than `demote`, by default
//!
//! `docs/DECISIONS.md` D60 and section 6.1 give the reason: automatic demotion
//! during a network-wide slowdown cascades, because every node looks slow
//! against a moving median and membership churns while the real fault is
//! somewhere else.

use std::collections::BTreeMap;

use crate::topology::{ControllerCommand, NodeName, NodeState};

/// What a node measures about itself and sends to its controller.
///
/// Every field is evidence. None of it is a verdict: the node never decides
/// that it is slow, because it has no view of the group.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HealthReport {
    pub node: NodeName,
    pub reported_at: i64,
    pub append_latency_us: u64,
    pub fsync_latency_us: u64,
    pub queue_depth: u64,
    pub accepted_bytes_each_second: u64,
    pub device_errors: u64,
    pub device_service_time_us: u64,
    pub compaction_backlog_bytes: u64,
    pub memory_reclaim_events: u64,
    pub peer_round_trip_us: u64,
    /// False when the node cannot make a write durable. A voter that reports
    /// this stops accepting appends and the quorum continues without it.
    pub writable: bool,
    pub free_bytes: u64,
}

/// The most specific cause the evidence supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowCause {
    StorageErrors,
    StorageSaturated,
    StorageSlow,
    WriteVolume,
    CompactionPressure,
    MemoryPressure,
    NetworkLatency,
    Unknown,
}

impl SlowCause {
    pub fn as_str(self) -> &'static str {
        match self {
            SlowCause::StorageErrors => "storage-errors",
            SlowCause::StorageSaturated => "storage-saturated",
            SlowCause::StorageSlow => "storage-slow",
            SlowCause::WriteVolume => "write-volume",
            SlowCause::CompactionPressure => "compaction-pressure",
            SlowCause::MemoryPressure => "memory-pressure",
            SlowCause::NetworkLatency => "network-latency",
            SlowCause::Unknown => "unknown",
        }
    }
}

/// What to do about a slow node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlowAction {
    /// Raise the state and alert. An operator decides. The default.
    Alert,
    /// Demote a slow voter, promote a learner, and step down a slow leader.
    Demote,
}

impl SlowAction {
    pub fn parse(text: &str) -> Option<SlowAction> {
        match text {
            "alert" => Some(SlowAction::Alert),
            "demote" => Some(SlowAction::Demote),
            _ => None,
        }
    }
}

/// How the controller decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowPolicy {
    /// The multiple of the group median that counts as slow.
    pub factor: u64,
    /// How long a node must stay slow before the state changes.
    pub duration_ms: i64,
    pub action: SlowAction,
}

impl Default for SlowPolicy {
    fn default() -> SlowPolicy {
        // The schema defaults: 4x the median for 5 minutes, and alert.
        SlowPolicy {
            factor: 4,
            duration_ms: 5 * 60 * 1000,
            action: SlowAction::Alert,
        }
    }
}

/// What the controller decided about one node, and the evidence for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub node: NodeName,
    pub state: NodeState,
    pub cause: Option<SlowCause>,
    pub append_latency_us: u64,
    pub group_median_append_latency_us: u64,
    pub fsync_latency_us: u64,
    pub group_median_fsync_latency_us: u64,
    pub queue_depth: u64,
    pub accepted_bytes_each_second: u64,
    pub since: i64,
    pub action_taken: Option<String>,
    pub writable: bool,
    pub free_bytes: u64,
}

/// The controller's view of one group's health over time.
///
/// It keeps the last report from each node and the moment each node first
/// looked slow, which is the only state the duration rule needs.
#[derive(Debug, Default)]
pub struct HealthWatch {
    policy: SlowPolicy,
    latest: BTreeMap<NodeName, HealthReport>,
    slow_since: BTreeMap<NodeName, i64>,
}

impl HealthWatch {
    pub fn new(policy: SlowPolicy) -> HealthWatch {
        HealthWatch {
            policy,
            latest: BTreeMap::new(),
            slow_since: BTreeMap::new(),
        }
    }

    pub fn policy(&self) -> SlowPolicy {
        self.policy
    }

    /// Take one node's report.
    pub fn observe(&mut self, report: HealthReport) {
        self.latest.insert(report.node.clone(), report);
    }

    pub fn latest(&self, node: &str) -> Option<&HealthReport> {
        self.latest.get(node)
    }

    /// The group median for one measure. An empty group has no median, and a
    /// node with no group to compare against is never called slow.
    fn median(&self, of: impl Fn(&HealthReport) -> u64) -> Option<u64> {
        let mut values: Vec<u64> = self.latest.values().map(&of).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        Some(values[values.len() / 2])
    }

    /// Decide the state of every node in the group, as of `now`.
    pub fn assess(&mut self, now: i64) -> Vec<Condition> {
        let median_append = self.median(|r| r.append_latency_us).unwrap_or(0);
        let median_fsync = self.median(|r| r.fsync_latency_us).unwrap_or(0);
        let median_bytes = self.median(|r| r.accepted_bytes_each_second).unwrap_or(0);
        let median_round_trip = self.median(|r| r.peer_round_trip_us).unwrap_or(0);

        let reports: Vec<HealthReport> = self.latest.values().cloned().collect();
        let mut conditions = Vec::with_capacity(reports.len());

        for report in reports {
            let over = exceeds(report.append_latency_us, median_append, self.policy.factor)
                || exceeds(report.fsync_latency_us, median_fsync, self.policy.factor);

            // A node that cannot make a write durable is read-only whatever its
            // latency says. It is a different failure and it needs a different
            // answer: the quorum continues without it rather than waiting.
            let state = if !report.writable {
                self.slow_since.remove(&report.node);
                NodeState::ReadOnly
            } else if over {
                let started = *self
                    .slow_since
                    .entry(report.node.clone())
                    .or_insert(report.reported_at);
                if now - started >= self.policy.duration_ms {
                    NodeState::Slow
                } else {
                    // Over the threshold and not for long enough. A brief spike
                    // is not a sick node.
                    NodeState::Healthy
                }
            } else {
                self.slow_since.remove(&report.node);
                NodeState::Healthy
            };

            let cause = match state {
                NodeState::Slow => Some(diagnose(
                    &report,
                    median_bytes,
                    median_round_trip,
                    median_append,
                )),
                NodeState::ReadOnly => None,
                _ => None,
            };

            let action_taken = match (state, self.policy.action) {
                (NodeState::Slow, SlowAction::Alert) => Some(
                    "The state was raised and an alert was sent. An operator decides what happens next."
                        .to_string(),
                ),
                (NodeState::Slow, SlowAction::Demote) => Some(
                    "The controller demoted this voter and promoted a learner in its place."
                        .to_string(),
                ),
                (NodeState::ReadOnly, _) => Some(
                    "This node stopped accepting appends. The quorum continues without it."
                        .to_string(),
                ),
                _ => None,
            };

            conditions.push(Condition {
                since: self
                    .slow_since
                    .get(&report.node)
                    .copied()
                    .unwrap_or(report.reported_at),
                node: report.node.clone(),
                state,
                cause,
                append_latency_us: report.append_latency_us,
                group_median_append_latency_us: median_append,
                fsync_latency_us: report.fsync_latency_us,
                group_median_fsync_latency_us: median_fsync,
                queue_depth: report.queue_depth,
                accepted_bytes_each_second: report.accepted_bytes_each_second,
                action_taken,
                writable: report.writable,
                free_bytes: report.free_bytes,
            });
        }
        conditions
    }

    /// The commands that carry these conditions into the replicated topology.
    pub fn commands(conditions: &[Condition]) -> Vec<ControllerCommand> {
        conditions
            .iter()
            .map(|c| ControllerCommand::SetNodeCondition {
                node: c.node.clone(),
                state: c.state,
                cause: c.cause.map(|cause| cause.as_str().to_string()),
                at: c.since,
                writable: c.writable,
                free_bytes: c.free_bytes,
            })
            .collect()
    }
}

fn exceeds(value: u64, median: u64, factor: u64) -> bool {
    // With no group and no history there is nothing to be slower than. Calling
    // the only node in a group slow against its own median would make every
    // single-node installation permanently sick.
    median > 0 && value > median.saturating_mul(factor)
}

/// The cause table from `docs/FAILURE_MODES.md` section 6.1, most specific
/// first.
///
/// Order matters. A device that is returning errors is also saturated and also
/// slow, and reporting the least specific of the three would be true and
/// useless.
pub fn diagnose(
    report: &HealthReport,
    median_accepted_bytes: u64,
    median_round_trip: u64,
    median_append: u64,
) -> SlowCause {
    if report.device_errors > 0 {
        return SlowCause::StorageErrors;
    }
    // Saturation is a queue that is deep *and* a device that is taking a long
    // time to answer. Either alone is not saturation: a deep queue with fast
    // service is a burst, and slow service with an empty queue is a sick
    // device, which is the row below.
    if report.queue_depth >= SATURATED_QUEUE_DEPTH
        && report.device_service_time_us >= SATURATED_SERVICE_US
    {
        return SlowCause::StorageSaturated;
    }
    if report.compaction_backlog_bytes >= COMPACTION_BACKLOG_BYTES {
        return SlowCause::CompactionPressure;
    }
    if report.memory_reclaim_events >= MEMORY_RECLAIM_EVENTS {
        return SlowCause::MemoryPressure;
    }
    // The load changed rather than the node. This is checked before the two
    // "the device is sick" rows, because a node taking much more work than its
    // peers explains its own latency and a device replacement would not fix it.
    if median_accepted_bytes > 0
        && report.accepted_bytes_each_second > median_accepted_bytes.saturating_mul(2)
    {
        return SlowCause::WriteVolume;
    }
    // Peer round trips grew while local input and output stayed flat.
    if median_round_trip > 0
        && report.peer_round_trip_us > median_round_trip.saturating_mul(4)
        && (median_append == 0 || report.append_latency_us <= median_append.saturating_mul(2))
    {
        return SlowCause::NetworkLatency;
    }
    if report.device_service_time_us > 0 && median_append > 0 {
        return SlowCause::StorageSlow;
    }
    SlowCause::Unknown
}

/// Queue depth at which a device is holding work rather than doing it.
const SATURATED_QUEUE_DEPTH: u64 = 32;
/// Service time at which a device is at its limit rather than merely busy.
const SATURATED_SERVICE_US: u64 = 20_000;
/// Compaction backlog that is large enough to be the reason for the latency.
const COMPACTION_BACKLOG_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Reclaim events in one reporting period that mean the process is short of
/// memory rather than merely using it.
const MEMORY_RECLAIM_EVENTS: u64 = 1000;
