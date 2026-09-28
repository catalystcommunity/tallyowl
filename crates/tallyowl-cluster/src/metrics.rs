//! What an operator sees of this node's consensus groups.
//!
//! Before this there was nothing: no gauge said a tablet had no leader, that a
//! group had stopped on a storage failure, that a replica was falling behind
//! the log its leader still keeps, or that snapshots were failing and the log
//! was therefore not being purged. Each of those was found by its consequence.
//!
//! # Why these are sums and not one series for each group
//!
//! A node holds as many groups as placement gave it, and D27 measured 600. A
//! `group` label would put that many series for each gauge into the operator's
//! own monitoring, from every node. The counts here say *whether* something is
//! wrong. `describe-topology` and the node's log say *which*: a stopped group
//! is logged by name, once, when it is first seen stopped.

use std::collections::BTreeSet;
use std::sync::Arc;

use tallyowl_obs::metrics::{labels, Registry};
use tallyowl_obs::MetricKind;

use crate::groups::GroupRegistry;

const GROUPS: &str = "tallyowl_consensus_groups_count";
const LED: &str = "tallyowl_consensus_groups_led_count";
const LEADERLESS: &str = "tallyowl_consensus_groups_leaderless_count";
const STOPPED: &str = "tallyowl_consensus_groups_stopped_count";
const LAGGING: &str = "tallyowl_consensus_groups_lagging_count";
const REPLICATION_LAG: &str = "tallyowl_consensus_replication_lag_count";
const APPLY_LAG: &str = "tallyowl_consensus_apply_lag_count";
const SNAPSHOT_FAILURES: &str = "tallyowl_consensus_snapshot_failures_total";

/// Declare every instrument this module moves.
pub fn declare(metrics: &Registry) {
    // A name that breaks a rule is a defect at this call site, so it stops the
    // tests rather than being found as a missing dashboard panel.
    let gauge = |name: &str, help: &str| {
        metrics
            .declare(name, MetricKind::Gauge, help, &[])
            .expect("a consensus metric name follows the rules");
    };
    gauge(GROUPS, "Consensus groups this node runs.");
    gauge(LED, "Consensus groups this node leads.");
    gauge(
        LEADERLESS,
        "Consensus groups on this node that have no leader it knows of. A write to such a tablet is refused.",
    );
    gauge(
        STOPPED,
        "Consensus groups that stopped on this node after a storage failure. A stopped group takes no write and no message until the node restarts.",
    );
    gauge(
        LAGGING,
        "Groups this node leads in which a replica is further behind than the log the leader keeps, so it catches up by snapshot and segment copy.",
    );
    gauge(
        REPLICATION_LAG,
        "Log entries the slowest replica is behind, over every group this node leads.",
    );
    gauge(
        APPLY_LAG,
        "Log entries this node holds and has not applied, over every group on it.",
    );
    metrics
        .declare(
            SNAPSHOT_FAILURES,
            MetricKind::Counter,
            "Consensus snapshots that could not be prepared. The log behind a snapshot is not purged while these continue, so the consensus log grows.",
            &[],
        )
        .expect("a consensus metric name follows the rules");
}

/// What a sampler remembers between samples.
#[derive(Default)]
pub struct Sampler {
    snapshot_failures_seen: u64,
    stopped_seen: BTreeSet<String>,
}

impl Sampler {
    /// Read every group once and move the instruments. It answers the names of
    /// the groups that were seen stopped for the first time, for the caller to
    /// log.
    pub fn sample(&mut self, registry: &GroupRegistry, metrics: &Arc<Registry>) -> Vec<String> {
        let none = labels(&[]);
        let health = registry.health();
        metrics.set_gauge(GROUPS, &none, health.groups as i64);
        metrics.set_gauge(LED, &none, health.led_here as i64);
        metrics.set_gauge(LEADERLESS, &none, health.leaderless as i64);
        metrics.set_gauge(STOPPED, &none, health.stopped as i64);
        metrics.set_gauge(LAGGING, &none, health.lagging as i64);
        metrics.set_gauge(REPLICATION_LAG, &none, health.worst_replication_lag as i64);
        metrics.set_gauge(APPLY_LAG, &none, health.worst_apply_lag as i64);
        if health.snapshot_failures > self.snapshot_failures_seen {
            metrics.add(
                SNAPSHOT_FAILURES,
                &none,
                health.snapshot_failures - self.snapshot_failures_seen,
            );
            self.snapshot_failures_seen = health.snapshot_failures;
        }
        let newly: Vec<String> = health
            .stopped_names
            .iter()
            .filter(|name| !self.stopped_seen.contains(*name))
            .cloned()
            .collect();
        self.stopped_seen = health.stopped_names.into_iter().collect();
        newly
    }
}
