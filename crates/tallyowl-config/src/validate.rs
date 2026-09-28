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

/// Whether a name can be a DNS name on a certificate: dot-separated parts of 1
/// to 63 letters, digits, and hyphens, with no hyphen at either end of a part,
/// and at most 253 characters in all.
fn is_dns_name(name: &str) -> bool {
    name.len() <= 253
        && name.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

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

    // The intake frame limit is what stops an unauthenticated peer from making
    // a collector decode a frame many times the size of a batch. A limit below
    // the batch limit would refuse every full batch at the socket, where the
    // app driver reads it as a lost connection and not as a setting.
    let max_frame = resolved.integer("collector.maxFrameBytes");
    let max_batch = resolved.integer("collector.maxBatchBytes");
    if max_frame < max_batch {
        errors.push(refuse(
            "collector.maxFrameBytes",
            format!(
                "`collector.maxFrameBytes` is {max_frame} bytes and `collector.maxBatchBytes` is {max_batch} bytes, so intake would close the connection on every full batch. Set `collector.maxFrameBytes` to at least the batch limit. A valid example is `4MiB`."
            ),
        ));
    }
    let connections = resolved.integer("corndogs.connections");
    if connections < 1 {
        errors.push(refuse(
            "corndogs.connections",
            format!("A process needs at least one connection to the durable store, and `corndogs.connections` is {connections}. A valid example is `8`."),
        ));
    }
    for key in [
        "corndogs.callTimeout",
        "collector.headCallTimeout",
        "corndogs.depthInterval",
    ] {
        if resolved.integer(key) <= 0 {
            errors.push(refuse(
                key,
                format!("`{key}` must be longer than zero. A call with no time limit is what lets one stalled dependency hold every thread that reaches it. A valid example is `30s`."),
            ));
        }
    }
    if resolved.integer("collector.maxConnections") < 0 {
        errors.push(refuse(
            "collector.maxConnections",
            "`collector.maxConnections` cannot be negative. Use `0` for no limit. A valid example is `1024`.".to_string(),
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

    // L073: a scrape target is refused here and not on every scrape. A target
    // the scraper cannot use otherwise costs one warning each interval while
    // the collector starts normally, and an operator who wrote `https` believes
    // the scrape is encrypted.
    for target in resolved.list("compatibility.prometheus.targets") {
        let problem = match target.split_once("://") {
            Some(("http", _)) | None => target
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
                .then_some("it holds a space or a control character"),
            Some(("https", _)) => Some(
                "this build scrapes over HTTP only. Put the target inside a network you control and write `http://`, or leave it out",
            ),
            Some(_) => Some("its scheme is not one the scraper reads"),
        };
        if let Some(problem) = problem {
            errors.push(refuse(
                "compatibility.prometheus.targets",
                format!(
                    "The scrape target `{target}` is not usable: {problem}. Write each target as `http://host:port/metrics`."
                ),
            ));
        }
    }
    if resolved.integer("compatibility.prometheus.workers") < 1 {
        errors.push(refuse(
            "compatibility.prometheus.workers",
            format!(
                "`compatibility.prometheus.workers` is {}. At least 1 worker must read the scrape targets. The default is 8.",
                resolved.integer("compatibility.prometheus.workers")
            ),
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

    // The advertise address is the one peers dial, so it has to name a host.
    // A pod listens on every interface, and that address copied here would
    // have every peer dial itself.
    let advertise = resolved.text("replication.advertise");
    let advertised_host = advertise
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(advertise)
        .trim_start_matches('[')
        .trim_end_matches(']');
    if !advertise.is_empty() && matches!(advertised_host, "" | "0.0.0.0" | "::" | "*") {
        errors.push(refuse(
            "replication.advertise",
            format!(
                "`replication.advertise` is `{advertise}`, which is not an address another node can dial. Set it to this node's own reachable address. A valid example is `tallyowl-0.tallyowl:5200`."
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

    check_single_values(resolved, &mut errors);
    check_addresses(resolved, &mut errors);
    check_transport(resolved, &mut errors);
    check_not_built(resolved, &mut errors);

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Values that parse and then fail later, where the failure names no setting.
fn check_single_values(resolved: &Resolved, errors: &mut Vec<ConfigError>) {
    // `retention.detailed: 0s` keeps detailed telemetry without limit, and
    // stored files are removed whole at the longer of the two retentions. With
    // a rollup retention, "keep everything" therefore ended at the rollup's
    // age. An operator who wrote zero to keep less kept everything; one who
    // wrote it to keep everything lost it at `retention.rollup`.
    let detailed = resolved.integer("retention.detailed");
    let rollup = resolved.integer("retention.rollup");
    if detailed == 0 && rollup > 0 {
        errors.push(refuse(
            "retention.detailed",
            format!(
                "`retention.detailed` is 0s, which keeps detailed telemetry without limit, and `retention.rollup` is {}, which removes whole stored files after that time. The two cannot both be true. To keep everything, set `retention.rollup` to 0s as well. To keep less, set `retention.detailed` to the time you want, for example `30d`. Zero does not mean \"keep none\" here; it does for `retention.raw`.",
                format_duration(rollup)
            ),
        ));
    }

    // A session that ends the moment it is issued makes every sign-in fail with
    // "That sign-in did not complete", which names nothing.
    let lifetime = resolved.integer("linkkeys.sessionLifetime");
    if lifetime < 60_000 {
        errors.push(refuse(
            "linkkeys.sessionLifetime",
            format!(
                "`linkkeys.sessionLifetime` is {}, and a session that short ends before the person who signed in can use it. Use at least `1m`. A valid example is `24h`.",
                format_duration(lifetime)
            ),
        ));
    }

    // D62. A node certificate carries the node's name as a DNS name, because a
    // TLS client checks that name and never the common name. A name that is
    // not a DNS name would pass here and stop the head's own certificate from
    // being issued at start, with a message about certificates rather than
    // about this setting.
    let node_name = resolved.text("node.name");
    if !node_name.is_empty() && !is_dns_name(node_name) {
        errors.push(refuse(
            "node.name",
            format!(
                "`node.name` is `{node_name}`, and a node's name goes on its certificate as a DNS name. Use letters, digits, and hyphens, in parts of at most 63 characters separated by dots, with no hyphen at the start or end of a part. A valid example is `head-0`."
            ),
        ));
    }

    // D62. A lifetime of zero issues certificates that are expired on arrival,
    // and a lifetime of years turns "a node that cannot renew stops within this
    // time" into no revocation at all.
    let hours = resolved.integer("enrollment.certificateLifetimeHours");
    if !(1..=8760).contains(&hours) {
        errors.push(refuse(
            "enrollment.certificateLifetimeHours",
            format!("`enrollment.certificateLifetimeHours` is {hours}, and it must be from 1 to 8760 (one year). The short lifetime is what stops a revoked node, so keep it short. A valid example is `24`."),
        ));
    }
    let signing_certificate = !resolved.text("installation.signingCertificate").is_empty();
    let signing_key = !resolved.text("installation.signingKey").is_empty();
    if signing_certificate != signing_key {
        let (set, missing) = if signing_certificate {
            ("installation.signingCertificate", "installation.signingKey")
        } else {
            ("installation.signingKey", "installation.signingCertificate")
        };
        errors.push(refuse(
            missing,
            format!("`{set}` is set and `{missing}` is not. A head signs with both or with neither. `tallyowl-head ca create` makes a certificate and its key."),
        ));
    }
    if signing_certificate && resolved.list("installation.authorities").is_empty() {
        errors.push(refuse(
            "installation.authorities",
            "`installation.signingCertificate` is set and `installation.authorities` is empty. A head checks its signer against the authorities it trusts, and every node verifies the head against them. Add the root that signed the intermediate, for example `/etc/tallyowl/root.crt`.".to_string(),
        ));
    }

    let keep = resolved.integer("sampling.tail.keepPercent");
    if !(0..=100).contains(&keep) {
        errors.push(refuse(
            "sampling.tail.keepPercent",
            format!("`sampling.tail.keepPercent` is {keep}, and a percentage is from 0 to 100. A valid example is `10`."),
        ));
    }

    // The dashboard serves the path, and LinkKeys is told the whole address.
    // When they differ, the browser returns to a path the dashboard does not
    // serve and the sign-in ends on a 404.
    if resolved.boolean("linkkeys.enabled") {
        let path = resolved.text("dashboard.callbackPath");
        let url = resolved.text("linkkeys.callbackUrl");
        let url_path = url
            .split_once("://")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.find('/').map(|at| &rest[at..]))
            .map(|path| path.split(['?', '#']).next().unwrap_or(path));
        if let Some(url_path) = url_path {
            if url_path != path {
                errors.push(refuse(
                    "dashboard.callbackPath",
                    format!(
                        "`dashboard.callbackPath` is `{path}` and the path of `linkkeys.callbackUrl` is `{url_path}`. A sign-in returns to the second and the dashboard serves the first, so every sign-in would end on a page that is not there. Make them the same. A valid example is `/sign-in/callback`."
                    ),
                ));
            }
        }
    }
}

/// An address this process binds is an IP address and a port.
///
/// `head.listen: localhost` used to pass every check and then stop the head
/// with "invalid socket address", which names no setting. An address the
/// process dials is a different thing, and takes a DNS name.
fn check_addresses(resolved: &Resolved, errors: &mut Vec<ConfigError>) {
    for entry in resolved.entries() {
        let path = entry.setting.path;
        let last = path.rsplit('.').next().unwrap_or(path);
        if last != "listen" && !last.ends_with("Listen") {
            continue;
        }
        let value = resolved.text(path);
        // Empty is "do not listen", and each listener has its own rule for
        // when that is legal. A `unix:` socket is a place to listen as well.
        if value.is_empty()
            || value.parse::<std::net::SocketAddr>().is_ok()
            || value
                .strip_prefix("unix:")
                .is_some_and(|path| !path.is_empty())
        {
            continue;
        }
        errors.push(refuse(
            path,
            format!(
                "`{path}` is `{value}`, and an address to listen on is an IP address and a port. A host name is not accepted here, because it can resolve to an address this host does not have. A valid example is `{}`.",
                entry.setting.example
            ),
        ));
    }

    let endpoint = resolved.text("metrics.selfObservation.endpoint");
    if !endpoint.is_empty() {
        let usable = endpoint.rsplit_once(':').is_some_and(|(host, port)| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            !host.is_empty()
                && !host.chars().any(|c| c.is_whitespace() || c.is_control() || c == '/')
                && port.parse::<u16>().is_ok_and(|port| port != 0)
                // `0.0.0.0` is where something listens. Nothing can dial it.
                && !host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_unspecified())
        });
        if !usable {
            errors.push(refuse(
                "metrics.selfObservation.endpoint",
                format!(
                    "`metrics.selfObservation.endpoint` is `{endpoint}`, and the head dials this address, so it must be a host and a port that another process can reach. `0.0.0.0` is an address to listen on and cannot be dialed. A valid example is `tallyowl-collector:5100`."
                ),
            ));
        }
    }
}

/// Where an address is, for the D62 plaintext rule.
#[derive(Debug, PartialEq, Eq)]
enum Reach {
    /// Nothing crosses a network: a loopback address or a `unix:` socket.
    Local,
    /// Anything else, including `0.0.0.0`, which listens on every network.
    Network,
}

/// Classify a listen address or a dial address. A host name other than
/// `localhost` is a network address, because it can resolve to anything.
fn reach(address: &str) -> Reach {
    if address
        .strip_prefix("unix:")
        .is_some_and(|path| !path.is_empty())
    {
        return Reach::Local;
    }
    let host = match address.rsplit_once(':') {
        Some((host, _)) => host,
        None => address,
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        Reach::Local
    } else {
        Reach::Network
    }
}

/// D62: a CSIL connection that crosses a network uses TLS.
///
/// Each rule names the setting whose value crosses a network, and says what
/// makes it legal. `transport.allowPlaintext` lifts the CSIL rules and nothing
/// else; Corndogs and the dashboard each have their own, because each is an
/// exception with its own reason.
fn check_transport(resolved: &Resolved, errors: &mut Vec<ConfigError>) {
    let plaintext = resolved.boolean("transport.allowPlaintext");
    let certificates = !resolved.list("tls.certificateDirectories").is_empty();
    let authorities = !resolved.list("installation.authorities").is_empty();
    let signer = !resolved.text("installation.signingCertificate").is_empty()
        && !resolved.text("installation.signingKey").is_empty();
    let token = !resolved.text("enrollment.roleToken").is_empty();

    let mut app_facing = vec!["collector.listen"];
    if resolved.boolean("compatibility.openTelemetry.enabled") {
        app_facing.push("compatibility.openTelemetry.listen");
    }
    for path in app_facing {
        let value = resolved.text(path);
        if value.is_empty() || reach(value) == Reach::Local || plaintext || certificates {
            continue;
        }
        errors.push(refuse(
            path,
            format!(
                "`{path}` is `{value}`, which applications reach over a network, and `tls.certificateDirectories` is empty. Applications reach this listener with TLS. Set `tls.certificateDirectories` to a directory that holds `tls.crt` and `tls.key`, listen on a loopback or `unix:` address, or set `transport.allowPlaintext: true` if something else protects this network."
            ),
        ));
    }

    // Node to node: mutual TLS, so a node needs the authorities it trusts and
    // an identity to show. A head that signs has one; any other node enrolls.
    for path in ["head.listen", "replication.listen"] {
        let value = resolved.text(path);
        if value.is_empty() || reach(value) == Reach::Local || plaintext {
            continue;
        }
        if authorities && (signer || token) {
            continue;
        }
        errors.push(refuse(
            path,
            format!(
                "`{path}` is `{value}`, which other nodes reach over a network, so it uses mutual TLS. Set `installation.authorities` to the certificate file of the installation authority, and either `installation.signingCertificate` and `installation.signingKey` (a head that signs) or `enrollment.roleToken` (a node that enrolls). Or listen on a loopback or `unix:` address, or set `transport.allowPlaintext: true` if something else protects this network."
            ),
        ));
    }
    let head = resolved.text("head.endpoint");
    if !head.is_empty()
        && reach(head) == Reach::Network
        && !plaintext
        && !(authorities && (signer || token))
    {
        errors.push(refuse(
            "head.endpoint",
            format!(
                "`head.endpoint` is `{head}`, which is reached over a network, so the connection uses mutual TLS. Set `installation.authorities` and `enrollment.roleToken`, use a loopback or `unix:` address, or set `transport.allowPlaintext: true` if something else protects this network."
            ),
        ));
    }

    let corndogs = resolved.text("corndogs.endpoint");
    if corndogs.starts_with("unix:") {
        errors.push(refuse(
            "corndogs.endpoint",
            format!(
                "`corndogs.endpoint` is `{corndogs}`. The Corndogs client reaches Corndogs over TCP only. A valid example is `127.0.0.1:5080`."
            ),
        ));
    }
    // A Corndogs endpoint on a network is reached over TLS (D62), with the
    // operating system's authorities unless `corndogs.tls.caFile` names others.
    // That is valid material, so there is nothing to refuse here. A name that
    // is set must still be a name a certificate can carry.
    let corndogs_name = resolved.text("corndogs.tls.serverName");
    if !corndogs_name.is_empty() && !is_dns_name(corndogs_name) {
        errors.push(refuse(
            "corndogs.tls.serverName",
            format!(
                "`corndogs.tls.serverName` is `{corndogs_name}`, and a certificate carries a DNS name. Use letters, digits, hyphens, and dots. A valid example is `corndogs.tallyowl.svc`."
            ),
        ));
    }

    let dashboard = resolved.text("dashboard.listen");
    if resolved.boolean("dashboard.enabled")
        && !dashboard.is_empty()
        && reach(dashboard) == Reach::Network
        && !resolved.boolean("dashboard.allowPlaintext")
    {
        errors.push(refuse(
            "dashboard.listen",
            format!(
                "`dashboard.listen` is `{dashboard}`, which is reached over a network, and the dashboard serves plaintext and carries session tokens. Put it behind a gateway that ends TLS and set `dashboard.allowPlaintext: true`, or listen on a loopback address."
            ),
        ));
    }
}

/// Settings this release parses and nothing reads.
///
/// **A setting that does nothing must not look as if it does something.** Each
/// of these is documented, was accepted with any value, and changed nothing:
/// an operator who turned catalog snapshots on had none. The default stays
/// accepted, so a configuration that names the default still starts.
const NOT_BUILT: &[(&str, &str)] = &[
    (
        "integrity.scrub.period",
        "No background scrub runs. `integrity.mode: verify-on-read` checks every page when a query reads it",
    ),
    (
        "integrity.scrub.rateLimit",
        "No background scrub runs. `integrity.mode: verify-on-read` checks every page when a query reads it",
    ),
    (
        "catalog.snapshots.enabled",
        "No periodic catalog snapshot is taken. `tallyowl-head snapshot <directory>` takes one when you run it, and `tallyowl-head rebuild` rebuilds a lost catalog from the stored files",
    ),
    (
        "catalog.snapshots.period",
        "No periodic catalog snapshot is taken. `tallyowl-head snapshot <directory>` takes one when you run it",
    ),
    (
        "catalog.snapshots.keep",
        "No periodic catalog snapshot is taken. `tallyowl-head snapshot <directory>` takes one when you run it",
    ),
    (
        "storage.coldTier.enabled",
        "No data moves to object storage. Every stored file stays on the local volume until its retention ends",
    ),
    (
        "placement.slowNode.factor",
        "No node is measured against its peers, so no node is reported or demoted as slow",
    ),
    (
        "placement.slowNode.duration",
        "No node is measured against its peers, so no node is reported or demoted as slow",
    ),
    (
        "retention.audit",
        "Control-plane, deletion, and export records are kept without limit",
    ),
];

fn check_not_built(resolved: &Resolved, errors: &mut Vec<ConfigError>) {
    for (path, instead) in NOT_BUILT {
        let Some(entry) = resolved.get(path) else {
            continue;
        };
        let default = crate::value::parse(&entry.setting.kind, entry.setting.default).ok();
        if default.as_ref() == Some(&entry.value) {
            continue;
        }
        errors.push(refuse(
            path,
            format!(
                "`{path}` is `{}`, and this release does not build what it controls. {instead}. Remove the setting, or set it to the default, `{}`.",
                entry.value.to_display(),
                entry.setting.default
            ),
        ));
    }
    if resolved.text("integrity.mode") == "scrub" {
        errors.push(refuse(
            "integrity.mode",
            "`integrity.mode` is `scrub`, and this release does not build the background scrub. Nothing would read the stored files on a schedule. Use `verify-on-read`, which checks every page when a query reads it.".to_string(),
        ));
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
            "storage:\n  receiptPolicy: local-quorum\n  tabletVoters: 3\nreplication:\n  listen: 0.0.0.0:5200\ntransport:\n  allowPlaintext: true\n"
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

        let replicated = "installation:\n  profile: replicated\ntransport:\n  allowPlaintext: true\nreplication:\n  listen: 0.0.0.0:5200\nstorage:\n  receiptPolicy: local-quorum\n  tabletVoters: 3\ncell:\n  controllers: ";
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
        // D59's rule stays for the release that builds periodic snapshots. This
        // one does not, so turning them on is refused whatever the count is:
        // see `a_setting_nothing_reads_is_refused_unless_it_holds_its_default`.
        let errors = refusal("catalog:\n  snapshots:\n    enabled: true\n    keep: 2\n");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].setting, "catalog.snapshots.enabled");
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
    fn an_advertise_address_nobody_can_dial_is_refused() {
        // A pod listens on every interface. That address copied into the
        // advertise setting would have every peer dial itself.
        for unspecified in ["0.0.0.0:5200", "[::]:5200", ":5200"] {
            let errors = refusal(&format!("replication:\n  advertise: \"{unspecified}\"\n"));
            assert!(
                errors.iter().any(|e| e.setting == "replication.advertise"),
                "`{unspecified}` was accepted"
            );
        }
        assert!(with("replication:\n  advertise: \"tallyowl-0.tallyowl:5200\"\n").is_ok());
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

    #[test]
    fn a_scrape_target_the_scraper_cannot_use_is_refused_before_the_collector_starts() {
        // L073. Found on every scrape instead, this is one warning an interval
        // from a collector that started normally.
        let errors = refusal(
            "compatibility:\n  prometheus:\n    targets:\n      - https://node.example:9100/metrics\n",
        );
        assert_eq!(errors[0].setting, "compatibility.prometheus.targets");
        assert!(
            errors[0]
                .message
                .contains("https://node.example:9100/metrics"),
            "{}",
            errors[0].message
        );
        assert!(errors[0].message.contains("http://host:port/metrics"));

        assert!(
            with("compatibility:\n  prometheus:\n    targets:\n      - ftp://node/metrics\n")
                .is_err()
        );
        assert!(with(
            "compatibility:\n  prometheus:\n    targets:\n      - http://[::1]:9100/metrics\n      - node.example:9100\n"
        )
        .is_ok());
    }

    #[test]
    fn a_scrape_with_no_worker_is_refused() {
        let errors = refusal("compatibility:\n  prometheus:\n    workers: 0\n");
        assert_eq!(errors[0].setting, "compatibility.prometheus.workers");
    }

    fn refused_for(yaml: &str, setting: &str) -> String {
        refusal(yaml)
            .into_iter()
            .find(|error| error.setting == setting)
            .unwrap_or_else(|| panic!("`{setting}` was not refused"))
            .message
    }

    #[test]
    fn a_certificate_lifetime_is_at_least_an_hour_and_at_most_a_year() {
        for hours in ["0", "-1", "8761"] {
            let message = refused_for(
                &format!("enrollment:\n  certificateLifetimeHours: {hours}\n"),
                "enrollment.certificateLifetimeHours",
            );
            assert!(message.contains("from 1 to 8760"), "{message}");
        }
        assert!(with("enrollment:\n  certificateLifetimeHours: 1\n").is_ok());
    }

    #[test]
    fn a_signer_needs_its_key_and_an_authority_it_chains_to() {
        let message = refused_for(
            "installation:\n  signingCertificate: /tls/intermediate.crt\n  authorities:\n    - /tls/root.crt\n",
            "installation.signingKey",
        );
        assert!(
            message.contains("installation.signingCertificate"),
            "{message}"
        );
        let message = refused_for(
            "installation:\n  signingKey: file:/tls/intermediate.key\n",
            "installation.signingCertificate",
        );
        assert!(message.contains("ca create"), "{message}");
        let message = refused_for(
            "installation:\n  signingCertificate: /tls/intermediate.crt\n  signingKey: file:/tls/intermediate.key\n",
            "installation.authorities",
        );
        assert!(message.contains("root"), "{message}");
        assert!(with(
            "installation:\n  signingCertificate: /tls/intermediate.crt\n  signingKey: file:/tls/intermediate.key\n  authorities:\n    - /tls/root.crt\n"
        )
        .is_ok());
    }

    #[test]
    fn keep_everything_and_a_rollup_retention_cannot_both_be_true() {
        let message = refused_for("retention:\n  detailed: 0s\n", "retention.detailed");
        assert!(message.contains("retention.rollup"), "{message}");
        assert!(message.contains("retention.raw"), "{message}");
        // Both zero is one consistent meaning, and so is the ordinary case.
        assert!(with("retention:\n  detailed: 0s\n  rollup: 0s\n").is_ok());
        assert!(with("retention:\n  detailed: 30d\n").is_ok());
    }

    #[test]
    fn a_negative_length_of_time_does_not_parse() {
        let inputs = Inputs {
            file: flatten_yaml("retention:\n  detailed: -5m\n").expect("valid YAML"),
            ..Inputs::default()
        };
        let errors = resolve(&inputs).expect_err("refused");
        assert!(errors[0].message.contains("retention.detailed"));
        assert!(
            errors[0].message.contains("cannot be negative"),
            "{}",
            errors[0].message
        );
    }

    #[test]
    fn a_node_name_that_cannot_go_on_a_certificate_is_refused() {
        for name in [
            "head_0",
            "-head",
            "head-",
            "head..0",
            "héad",
            &"a".repeat(64),
        ] {
            refused_for(&format!("node:\n  name: \"{name}\"\n"), "node.name");
        }
        for name in [
            "head-0",
            "node-10-0-0-7-5200",
            "head-0.tallyowl-nodes.prod.svc",
        ] {
            assert!(with(&format!("node:\n  name: {name}\n")).is_ok(), "{name}");
        }
    }

    #[test]
    fn a_session_that_ends_at_once_and_a_percentage_over_a_hundred_are_refused() {
        refused_for(
            "linkkeys:\n  sessionLifetime: 0s\n",
            "linkkeys.sessionLifetime",
        );
        assert!(with("linkkeys:\n  sessionLifetime: 1m\n").is_ok());
        refused_for(
            "sampling:\n  tail:\n    keepPercent: 500\n",
            "sampling.tail.keepPercent",
        );
        refused_for(
            "sampling:\n  tail:\n    keepPercent: -1\n",
            "sampling.tail.keepPercent",
        );
        assert!(with("sampling:\n  tail:\n    keepPercent: 0\n").is_ok());
    }

    #[test]
    fn an_address_to_listen_on_is_refused_by_name_when_it_is_a_host_name() {
        let message = refused_for("head:\n  listen: localhost\n", "head.listen");
        assert!(message.contains("`localhost`"), "{message}");
        refused_for(
            "dashboard:\n  listen: dashboard.internal:5120\n",
            "dashboard.listen",
        );
        refused_for(
            "collector:\n  operationalListen: \"5101\"\n",
            "collector.operationalListen",
        );
        let open = "transport:\n  allowPlaintext: true\n";
        assert!(with(&format!("{open}head:\n  listen: \"[::]:5110\"\n")).is_ok());
        assert!(with(&format!("{open}head:\n  listen: 0.0.0.0:5110\n")).is_ok());
        assert!(with("head:\n  listen: unix:/run/tallyowl/head.sock\n").is_ok());
        refused_for("head:\n  listen: \"unix:\"\n", "head.listen");
    }

    // ---- D62: a CSIL connection that crosses a network uses TLS --------------

    #[test]
    fn an_intake_on_a_network_address_with_no_certificate_is_refused_by_name() {
        let message = refused_for("collector:\n  listen: 0.0.0.0:5100\n", "collector.listen");
        assert!(message.contains("`0.0.0.0:5100`"), "{message}");
        assert!(message.contains("tls.certificateDirectories"), "{message}");
        assert!(message.contains("transport.allowPlaintext"), "{message}");

        assert!(with(
            "collector:\n  listen: 0.0.0.0:5100\ntls:\n  certificateDirectories: /etc/tallyowl/tls\n"
        )
        .is_ok());
        assert!(
            with("collector:\n  listen: 0.0.0.0:5100\ntransport:\n  allowPlaintext: true\n")
                .is_ok()
        );
        assert!(with("collector:\n  listen: unix:/run/tallyowl/intake.sock\n").is_ok());
        assert!(with("collector:\n  listen: \"[::1]:5100\"\n").is_ok());
    }

    #[test]
    fn an_opentelemetry_receiver_follows_the_intake_rule_only_when_it_is_on() {
        let off = "compatibility:\n  openTelemetry:\n    listen: 0.0.0.0:4318\n";
        assert!(with(off).is_ok());
        let on = "compatibility:\n  openTelemetry:\n    enabled: true\n    listen: 0.0.0.0:4318\n";
        refused_for(on, "compatibility.openTelemetry.listen");
    }

    #[test]
    fn a_node_listener_on_a_network_needs_authorities_and_an_identity() {
        let message = refused_for("head:\n  listen: 0.0.0.0:5110\n", "head.listen");
        assert!(message.contains("installation.authorities"), "{message}");
        assert!(message.contains("enrollment.roleToken"), "{message}");

        // Authorities alone are not an identity to show.
        refused_for(
            "head:\n  listen: 0.0.0.0:5110\ninstallation:\n  authorities: /etc/tallyowl/ca.crt\n",
            "head.listen",
        );
        refused_for(
            "replication:\n  listen: 0.0.0.0:5200\n",
            "replication.listen",
        );
    }

    #[test]
    fn a_network_head_endpoint_needs_an_identity_for_the_collector() {
        let message = refused_for("head:\n  endpoint: tallyowl:5110\n", "head.endpoint");
        assert!(message.contains("mutual TLS"), "{message}");
        assert!(with("head:\n  endpoint: unix:/run/tallyowl/head.sock\n").is_ok());
    }

    #[test]
    fn corndogs_over_a_network_is_reached_over_tls_and_needs_no_exception() {
        // Before Corndogs had TLS, a network endpoint needed
        // `corndogs.allowPlaintext`. That setting is gone (D62): the hop uses
        // TLS, with the system's authorities or `corndogs.tls.caFile`.
        assert!(with("corndogs:\n  endpoint: corndogs:5080\n").is_ok());
        assert!(with(
            "corndogs:\n  endpoint: corndogs:5080\n  tls:\n    caFile: /etc/tallyowl/root.crt\n    serverName: corndogs.tallyowl.svc\n"
        )
        .is_ok());
        refused_for(
            "corndogs:\n  endpoint: corndogs:5080\n  tls:\n    serverName: \"not a name\"\n",
            "corndogs.tls.serverName",
        );
        let message = refused_for(
            "corndogs:\n  endpoint: unix:/run/corndogs.sock\n",
            "corndogs.endpoint",
        );
        assert!(message.contains("TCP only"), "{message}");
    }

    #[test]
    fn a_dashboard_on_a_network_needs_the_gateway_setting() {
        let message = refused_for("dashboard:\n  listen: 0.0.0.0:5120\n", "dashboard.listen");
        assert!(message.contains("dashboard.allowPlaintext"), "{message}");
        assert!(with("dashboard:\n  listen: 0.0.0.0:5120\n  allowPlaintext: true\n").is_ok());
        assert!(with("dashboard:\n  enabled: false\n  listen: 0.0.0.0:5120\n").is_ok());
    }

    #[test]
    fn the_home_profile_needs_no_certificate_at_all() {
        assert!(with("").is_ok());
        assert!(with("installation:\n  profile: home\n").is_ok());
    }

    #[test]
    fn the_address_the_head_dials_takes_a_name_and_refuses_what_nothing_can_dial() {
        let key = "metrics.selfObservation.endpoint";
        let yaml =
            |value: &str| format!("metrics:\n  selfObservation:\n    endpoint: \"{value}\"\n");
        assert!(with(&yaml("tallyowl-collector:5100")).is_ok());
        assert!(with(&yaml("10.0.0.7:5100")).is_ok());
        assert!(with(&yaml("[fd00::7]:5100")).is_ok());
        for unusable in [
            "0.0.0.0:5100",
            "[::]:5100",
            "tallyowl-collector",
            ":5100",
            "a b:5100",
            "host:0",
        ] {
            let message = refused_for(&yaml(unusable), key);
            assert!(message.contains(unusable), "{message}");
        }
    }

    #[test]
    fn the_callback_path_must_be_the_path_of_the_callback_address() {
        let yaml = |path: &str| {
            format!(
                "linkkeys:\n  enabled: true\n  trustedDomains: id.example\n  callbackUrl: https://owl.example/auth/return?x=1\ndashboard:\n  callbackPath: {path}\n"
            )
        };
        let message = refused_for(&yaml("/sign-in/callback"), "dashboard.callbackPath");
        assert!(message.contains("/auth/return"), "{message}");
        assert!(with(&yaml("/auth/return")).is_ok());
    }

    #[test]
    fn a_setting_nothing_reads_is_refused_unless_it_holds_its_default() {
        for (yaml, setting) in [
            (
                "catalog:\n  snapshots:\n    enabled: true\n",
                "catalog.snapshots.enabled",
            ),
            (
                "catalog:\n  snapshots:\n    period: 5m\n",
                "catalog.snapshots.period",
            ),
            (
                "catalog:\n  snapshots:\n    keep: 5\n",
                "catalog.snapshots.keep",
            ),
            (
                "storage:\n  coldTier:\n    enabled: true\n",
                "storage.coldTier.enabled",
            ),
            (
                "integrity:\n  scrub:\n    period: 1d\n",
                "integrity.scrub.period",
            ),
            (
                "integrity:\n  scrub:\n    rateLimit: 1MiB\n",
                "integrity.scrub.rateLimit",
            ),
            ("integrity:\n  mode: scrub\n", "integrity.mode"),
            (
                "placement:\n  slowNode:\n    factor: 9\n",
                "placement.slowNode.factor",
            ),
            (
                "placement:\n  slowNode:\n    duration: 1m\n",
                "placement.slowNode.duration",
            ),
            ("retention:\n  audit: 30d\n", "retention.audit"),
        ] {
            let message = refused_for(yaml, setting);
            assert!(message.contains("does not build"), "{message}");
        }
        // The default, written out, still starts. An existing configuration
        // that names it is not broken by this.
        assert!(
            with("catalog:\n  snapshots:\n    enabled: false\n    keep: 2\n    period: 1h\n")
                .is_ok()
        );
        assert!(with("integrity:\n  mode: none\n").is_ok());
    }
}
