//! Rules that involve more than one setting.
//!
//! A single setting validates against its own kind in `value.rs`. These rules
//! are the ones that need two settings to see the fault, and they are exactly
//! the ones a person gets wrong: a receipt policy that a voter count cannot
//! satisfy, a durable-copy count that a backend cannot reach, a payload limit
//! smaller than the batch that has to fit inside it.
//!
//! Every one of them refuses at startup. `docs/DELIVERY.md` section 1 states the
//! principle: TallyOwl does not accept the configuration and then acknowledge a
//! weaker guarantee.

use crate::loader::{ConfigError, Resolved};
use crate::value::{format_bytes, format_duration};

const COLLECTOR_ROLES: &[&str] = &["intake", "forwarder", "compatibility-receiver"];

fn refuse(setting: &str, message: String) -> ConfigError {
    ConfigError {
        setting: setting.to_string(),
        message,
    }
}

/// Check every cross-setting rule. Reports all of them, rather than the first.
pub fn check_rules(resolved: &Resolved) -> Result<(), Vec<ConfigError>> {
    let mut errors = Vec::new();

    // D27: TallyOwl does not acknowledge an uncommitted entry in a multi-voter
    // group, so `local-one` is legal only for a tablet with one voter. The
    // control plane refuses the configuration; it never silently upgrades or
    // downgrades the policy.
    let policy = resolved.text("storage.receiptPolicy");
    let voters = resolved.integer("storage.tabletVoters");
    if policy == "local-one" && voters > 1 {
        errors.push(refuse(
            "storage.receiptPolicy",
            format!(
                "The receipt policy `local-one` needs a tablet with one voter, and `storage.tabletVoters` is {voters}. Use `local-quorum` for a tablet with more than one voter, or set `storage.tabletVoters` to 1."
            ),
        ));
    }
    if voters < 1 {
        errors.push(refuse(
            "storage.tabletVoters",
            format!("A tablet needs at least one voter, and `storage.tabletVoters` is {voters}. A valid example is `1`."),
        ));
    }

    // POLICY.md section 4: "`rollup` must be at least as long as `detailed`,
    // because a rollup that expires first leaves a gap that no query can fill."
    // A chart over a long range would then show a hole in the middle rather
    // than at the far end, which reads as an outage.
    let detailed = resolved.integer("retention.detailed");
    let rollup = resolved.integer("retention.rollup");
    if rollup < detailed {
        errors.push(refuse(
            "retention.rollup",
            format!(
                "`retention.rollup` is {} and `retention.detailed` is {}. A rollup that expires before the detailed data leaves a gap in the middle of a chart that no query can fill. Set `retention.rollup` to at least {}.",
                format_duration(rollup),
                format_duration(detailed),
                format_duration(detailed)
            ),
        ));
    }

    // D4 and DELIVERY.md section 1: a `durable_copies` value the backend cannot
    // satisfy fails at startup. The clustered file backend is a Corndogs design
    // and is not implemented, so every shipped backend supports exactly one.
    let backend = resolved.text("corndogs.backend");
    let copies = resolved.integer("corndogs.durableCopies");
    if copies < 1 {
        errors.push(refuse(
            "corndogs.durableCopies",
            format!("A batch needs at least one durable copy, and `corndogs.durableCopies` is {copies}. A valid example is `1`."),
        ));
    } else if copies > 1 {
        errors.push(refuse(
            "corndogs.durableCopies",
            format!(
                "The durable store cannot hold {copies} copies. The `{backend}` backend supports 1. Set `corndogs.durableCopies` to 1, or run a backend that holds more."
            ),
        ));
    }

    // D36: `dedup_window >= max_outage_buffer + max_replay_window + safety`. A
    // receipt is what makes a repeated batch ID one logical commit, so a head
    // that forgets a batch ID while the collector may still retry it commits a
    // second logical batch, and no query can remove it afterwards.
    //
    // Until receipt expiry existed the window was unbounded and any retry age
    // satisfied this trivially. It is a real pairing now, so it is checked. See
    // L044 and L056.
    let dedup_window = resolved.integer("storage.deduplicationWindow");
    let retry_age = resolved.integer("corndogs.maxDeliveryAge");
    if dedup_window <= retry_age {
        errors.push(refuse(
            "storage.deduplicationWindow",
            format!(
                "The head must remember a batch ID for longer than the collector may keep retrying it, or a retry commits the batch a second time. `storage.deduplicationWindow` is {} and `corndogs.maxDeliveryAge` is {}. Raise the deduplication window above the retry age, or lower the retry age.",
                format_duration(dedup_window),
                format_duration(retry_age)
            ),
        ));
    }

    // DELIVERY.md section 1: the `interval` and `never` flush modes acknowledge
    // writes that a power loss can destroy. A receipt that rests on one is a lie.
    let fsync = resolved.text("corndogs.fsyncMode");
    if backend == "file" && (fsync == "interval" || fsync == "never") {
        errors.push(refuse(
            "corndogs.fsyncMode",
            format!(
                "The flush mode `{fsync}` acknowledges a write that a power loss can destroy, so no receipt written against it is true. Use `group` or `always`."
            ),
        ));
    }

    // A batch payload travels inside the Corndogs task, so the payload limit has
    // to hold a whole sealed batch. See DEPLOYMENT.md section 4.
    let payload_limit = resolved.integer("corndogs.maxPayloadBytes");
    let batch_limit = resolved.integer("collector.maxBatchBytes");
    if payload_limit <= batch_limit {
        errors.push(refuse(
            "corndogs.maxPayloadBytes",
            format!(
                "A whole batch travels inside one durable task, so `corndogs.maxPayloadBytes` ({}) must be larger than `collector.maxBatchBytes` ({}). Raise the payload limit or seal a smaller batch.",
                format_bytes(payload_limit),
                format_bytes(batch_limit)
            ),
        ));
    }

    let event_limit = resolved.integer("collector.maxEventBytes");
    if event_limit > batch_limit {
        errors.push(refuse(
            "collector.maxEventBytes",
            format!(
                "One item cannot be larger than the batch that carries it. `collector.maxEventBytes` is {} and `collector.maxBatchBytes` is {}.",
                format_bytes(event_limit),
                format_bytes(batch_limit)
            ),
        ));
    }

    // FAILURE_MODES.md section 8.1: a generation that compaction deletes while a
    // query still reads it produces a wrong answer, so the grace period must
    // outlast the longest query the budget permits.
    let grace = resolved.integer("compaction.gcGrace");
    let max_runtime = resolved.integer("query.maxRuntime");
    if grace <= max_runtime {
        errors.push(refuse(
            "compaction.gcGrace",
            format!(
                "`compaction.gcGrace` ({}) must be longer than `query.maxRuntime` ({}), or compaction can remove data that a running query still reads.",
                format_duration(grace),
                format_duration(max_runtime)
            ),
        ));
    }

    // D15: a cell has three controllers by default and an operator can select
    // five. A home installation has one and no quorum at all.
    let profile = resolved.text("installation.profile");
    let controllers = resolved.integer("cell.controllers");
    let permitted: &[i64] = if profile == "home" { &[1] } else { &[3, 5] };
    if !permitted.contains(&controllers) {
        let allowed = permitted
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" or ");
        errors.push(refuse(
            "cell.controllers",
            format!(
                "The `{profile}` profile uses {allowed} controllers, and `cell.controllers` is {controllers}. A quorum needs an odd number, and a home installation has no quorum."
            ),
        ));
    }

    // A collector that runs no role does nothing and reports itself healthy,
    // which is the worst way to discover a typo in a deployment.
    let roles = resolved.list("collector.roles");
    if roles.is_empty() {
        errors.push(refuse(
            "collector.roles",
            format!(
                "A collector must run at least one role. Use one or more of {}.",
                COLLECTOR_ROLES.join(", ")
            ),
        ));
    }
    for role in &roles {
        if !COLLECTOR_ROLES.contains(&role.as_str()) {
            errors.push(refuse(
                "collector.roles",
                format!(
                    "`{role}` is not a collector role. Use one or more of {}.",
                    COLLECTOR_ROLES.join(", ")
                ),
            ));
        }
    }

    // D59: two snapshots survive a snapshot that is itself damaged, and one does
    // not. Keeping zero while snapshots are on is a setting that does nothing.
    if resolved.boolean("catalog.snapshots.enabled") {
        let keep = resolved.integer("catalog.snapshots.keep");
        if keep < 2 {
            errors.push(refuse(
                "catalog.snapshots.keep",
                format!(
                    "Catalog snapshots are on and `catalog.snapshots.keep` is {keep}. Keep at least 2, because two survive a snapshot that is itself damaged and one does not."
                ),
            ));
        }
    }

    // D12: a compatibility receiver never listens by default. An operator turns
    // it on, and the address then has to exist.
    if resolved.boolean("compatibility.openTelemetry.enabled")
        && resolved
            .text("compatibility.openTelemetry.listen")
            .is_empty()
    {
        errors.push(refuse(
            "compatibility.openTelemetry.listen",
            "The OpenTelemetry receiver is on and has no address. Give it one, or turn the receiver off.".to_string(),
        ));
    }

    // A tablet with more than one voter has peers, and peers reach it here.
    // A node configured for replication with no address to be reached at would
    // start, elect nothing, and refuse every write, which reads as a storage
    // fault rather than as a missing setting.
    if voters > 1 && resolved.text("replication.listen").is_empty() {
        errors.push(refuse(
            "replication.listen",
            format!(
                "`storage.tabletVoters` is {voters}, so this node has peers, and `replication.listen` is empty so no peer can reach it. A valid example is `0.0.0.0:5200`."
            ),
        ));
    }

    // CELLS.md section 6 calls for hysteresis. A merge threshold at or above
    // half the split threshold lets two merged tablets be immediately over the
    // split threshold, so a cell rewrites the same data for ever.
    let split_above = resolved.integer("placement.splitAbove");
    let merge_below = resolved.integer("placement.mergeBelow");
    if merge_below * 2 >= split_above {
        errors.push(refuse(
            "placement.mergeBelow",
            format!(
                "`placement.mergeBelow` is {} and `placement.splitAbove` is {}. Two merged tablets would be over the split threshold at once, so a cell would split and merge the same data for ever. Set `placement.mergeBelow` below {}.",
                format_bytes(merge_below),
                format_bytes(split_above),
                format_bytes(split_above / 2)
            ),
        ));
    }

    // A fan-out of nothing answers nothing. QUERY.md section 10 makes the limit
    // configurable and a limit of zero is not a configuration, it is an outage.
    let fan_out = resolved.integer("query.maxFanOut");
    if fan_out < 1 {
        errors.push(refuse(
            "query.maxFanOut",
            format!("`query.maxFanOut` is {fan_out}, so no query could reach a tablet. A valid example is `256`."),
        ));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{flatten_yaml, resolve, Inputs};

    fn with(yaml: &str) -> Result<(), Vec<ConfigError>> {
        let inputs = Inputs {
            file: flatten_yaml(yaml).expect("valid YAML"),
            ..Inputs::default()
        };
        let resolved = resolve(&inputs).expect("every value parses");
        check_rules(&resolved)
    }

    fn refusal(yaml: &str) -> Vec<ConfigError> {
        with(yaml).expect_err("this configuration must be refused")
    }

    #[test]
    fn the_home_defaults_satisfy_every_rule() {
        assert!(with("").is_ok());
    }

    #[test]
    fn local_one_is_refused_on_a_tablet_with_more_than_one_voter() {
        let errors = refusal("storage:\n  receiptPolicy: local-one\n  tabletVoters: 3\n");
        assert_eq!(errors[0].setting, "storage.receiptPolicy");
        assert!(errors[0].message.contains("local-quorum"));
    }

    #[test]
    fn local_quorum_is_accepted_on_a_tablet_with_more_than_one_voter() {
        assert!(with(
            "storage:\n  receiptPolicy: local-quorum\n  tabletVoters: 3\nreplication:\n  listen: 0.0.0.0:5200\n"
        )
        .is_ok());
    }

    #[test]
    fn a_durable_copy_count_the_backend_cannot_reach_is_refused() {
        let errors = refusal("corndogs:\n  durableCopies: 3\n");
        assert_eq!(errors[0].setting, "corndogs.durableCopies");
        assert!(errors[0].message.contains("supports 1"));
    }

    #[test]
    fn a_flush_mode_that_can_lose_an_acknowledged_write_is_refused() {
        for mode in ["interval", "never"] {
            let errors = refusal(&format!("corndogs:\n  fsyncMode: {mode}\n"));
            assert_eq!(errors[0].setting, "corndogs.fsyncMode");
            assert!(errors[0].message.contains("power loss"));
        }
        assert!(with("corndogs:\n  fsyncMode: always\n").is_ok());
    }

    #[test]
    fn a_payload_limit_smaller_than_a_batch_is_refused() {
        let errors = refusal("corndogs:\n  maxPayloadBytes: 256KiB\n");
        assert_eq!(errors[0].setting, "corndogs.maxPayloadBytes");
        assert!(errors[0].message.contains("512KiB"));
        // The boundary case matters: equal is not larger.
        let errors = refusal("corndogs:\n  maxPayloadBytes: 512KiB\n");
        assert_eq!(errors[0].setting, "corndogs.maxPayloadBytes");
    }

    #[test]
    fn an_item_larger_than_its_batch_is_refused() {
        let errors = refusal("collector:\n  maxEventBytes: 1MiB\n");
        assert_eq!(errors[0].setting, "collector.maxEventBytes");
    }

    #[test]
    fn a_grace_period_shorter_than_a_query_is_refused() {
        let errors = refusal("compaction:\n  gcGrace: 10s\n");
        assert_eq!(errors[0].setting, "compaction.gcGrace");
        assert!(errors[0].message.contains("30s"));
        assert!(with("compaction:\n  gcGrace: 1h\nquery:\n  maxRuntime: 59m\n").is_ok());
    }

    #[test]
    fn a_home_installation_has_one_controller_and_a_cell_has_three_or_five() {
        assert!(with("installation:\n  profile: home\ncell:\n  controllers: 1\n").is_ok());
        let errors = refusal("installation:\n  profile: home\ncell:\n  controllers: 3\n");
        assert_eq!(errors[0].setting, "cell.controllers");

        let replicated = "installation:\n  profile: replicated\nreplication:\n  listen: 0.0.0.0:5200\nstorage:\n  receiptPolicy: local-quorum\n  tabletVoters: 3\ncell:\n  controllers: ";
        assert!(with(&format!("{replicated}3\n")).is_ok());
        assert!(with(&format!("{replicated}5\n")).is_ok());
        let errors = with(&format!("{replicated}4\n")).unwrap_err();
        assert!(errors[0].message.contains("odd number"));
    }

    #[test]
    fn a_collector_with_no_role_is_refused() {
        let errors = refusal("collector:\n  roles: []\n");
        assert_eq!(errors[0].setting, "collector.roles");
    }

    #[test]
    fn a_role_that_does_not_exist_is_refused_and_the_message_lists_the_real_ones() {
        let errors = refusal("collector:\n  roles:\n    - intake\n    - forwarders\n");
        assert!(errors[0].message.contains("forwarders"));
        assert!(errors[0].message.contains("compatibility-receiver"));
    }

    #[test]
    fn catalog_snapshots_need_at_least_two_when_they_are_on() {
        let errors = refusal("catalog:\n  snapshots:\n    enabled: true\n    keep: 1\n");
        assert_eq!(errors[0].setting, "catalog.snapshots.keep");
        assert!(with("catalog:\n  snapshots:\n    enabled: true\n    keep: 2\n").is_ok());
        // Off by default, and the count then does not matter.
        assert!(with("catalog:\n  snapshots:\n    keep: 1\n").is_ok());
    }

    #[test]
    fn every_broken_rule_is_reported_rather_than_only_the_first() {
        let errors = refusal(
            "storage:\n  receiptPolicy: local-one\n  tabletVoters: 3\ncorndogs:\n  durableCopies: 5\n  fsyncMode: never\n",
        );
        // Four now: the policy, the durable-copy count, the flush mode, and the
        // missing replication address that three voters imply.
        assert_eq!(errors.len(), 4, "{errors:?}");
    }

    #[test]
    fn a_multi_voter_node_with_no_replication_address_is_refused() {
        // A node that started, elected nothing, and refused every write would
        // look like a storage fault rather than a missing setting.
        let errors = refusal("storage:\n  tabletVoters: 3\n  receiptPolicy: local-quorum\n");
        assert!(errors.iter().any(|e| e.setting == "replication.listen"));
        assert!(errors
            .iter()
            .any(|e| e.message.contains("no peer can reach it")));
    }

    #[test]
    fn a_merge_threshold_that_would_make_a_cell_oscillate_is_refused() {
        let errors = refusal("placement:\n  splitAbove: 64GiB\n  mergeBelow: 32GiB\n");
        assert!(errors.iter().any(|e| e.setting == "placement.mergeBelow"));
        assert!(errors.iter().any(|e| e.message.contains("for ever")));
    }

    #[test]
    fn a_fan_out_of_nothing_is_refused() {
        let errors = refusal("query:\n  maxFanOut: 0\n");
        assert!(errors.iter().any(|e| e.setting == "query.maxFanOut"));
    }

    #[test]
    fn a_rollup_that_expires_before_the_detailed_data_is_refused() {
        // POLICY.md section 4: a rollup that expires first leaves a gap in the
        // middle of a chart that no query can fill, which reads as an outage.
        let errors = refusal("retention:\n  detailed: 30d\n  rollup: 7d\n");
        assert!(errors.iter().any(|e| e.setting == "retention.rollup"));
        assert!(errors[0].message.contains("gap"));
    }

    #[test]
    fn a_rollup_as_long_as_the_detailed_data_is_accepted() {
        assert!(with("retention:\n  detailed: 30d\n  rollup: 30d\n").is_ok());
    }

    #[test]
    fn a_retry_that_can_outlive_the_deduplication_window_is_refused() {
        // D36: `dedup_window >= max_outage_buffer + max_replay_window + safety`.
        // A retry that arrives after the head has forgotten the batch ID commits
        // a second logical batch, and no query can remove it afterwards. Until
        // receipt expiry existed the window was unbounded and this was trivially
        // satisfied. See L044 and L056.
        let errors =
            refusal("storage:\n  deduplicationWindow: 12h\ncorndogs:\n  maxDeliveryAge: 24h\n");
        assert_eq!(errors[0].setting, "storage.deduplicationWindow");
        // 24h is a whole day, so `format_duration` writes it as `1d`.
        assert!(errors[0].message.contains("12h"), "{}", errors[0].message);
        assert!(errors[0].message.contains("1d"), "{}", errors[0].message);

        // Equal is refused too: the two must not meet, or a retry that lands on
        // the boundary is a coin toss.
        assert!(
            with("storage:\n  deduplicationWindow: 24h\ncorndogs:\n  maxDeliveryAge: 24h\n")
                .is_err()
        );
        assert!(
            with("storage:\n  deduplicationWindow: 72h\ncorndogs:\n  maxDeliveryAge: 24h\n")
                .is_ok()
        );
        // And the defaults satisfy it, which is what a home installation gets.
        assert!(with("").is_ok());
    }
}
