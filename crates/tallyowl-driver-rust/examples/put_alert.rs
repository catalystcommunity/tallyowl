//! Write an alert rule over the real control socket, and read its state back.
//!
//! It exists for the same reason `put_policy` does: a rule that only ever runs
//! inside a test proves the rules and nothing about the wiring, and L124
//! records three defects a running loop found that the whole test suite missed.
//!
//! ```sh
//! ./tools.sh dev up
//! cargo run -p tallyowl-driver-rust --example put_alert
//! cargo run -p tallyowl-driver-rust --example send_one_event
//! # wait one interval
//! cargo run -p tallyowl-driver-rust --example put_alert -- show
//! ```
//!
//! The first call writes a rule that fires as soon as one event exists. The
//! second call reads the instance back, so a person sees the state the
//! scheduler and the worker actually produced.

use tallyowl_control_api::codec::{
    decode_alert_instance_list, decode_alert_rule, decode_project_list, decode_workflow_list,
    encode_alert_list_request, encode_alert_rule, encode_list_request,
};
use tallyowl_control_api::types::{
    AlertListRequest, AlertRule, CompareOp, ListRequest, NotificationTarget,
    NotificationTarget_kind as TargetKind, ThresholdCondition, TimeBasis, TimeRange,
};

const RULE: &str = "any-events";

fn main() {
    let argument = std::env::args().nth(1);
    let showing = argument.as_deref() == Some("show");
    // `many <n>` writes n rules on a one-minute interval, which is the L144
    // measurement case: nothing had run a thousand rules to find out whether
    // one permit and one worker are enough. The head and the session come from
    // the environment so the measurement can point at the soak cluster, and
    // the defaults are the development loop's.
    let many: Option<usize> = if argument.as_deref() == Some("many") {
        Some(
            std::env::args()
                .nth(2)
                .and_then(|n| n.parse().ok())
                .unwrap_or(1000),
        )
    } else {
        None
    };
    let session_file = std::env::var("TALLYOWL_SESSION_FILE")
        .unwrap_or_else(|_| "data/operator.session".to_string());
    let head = std::env::var("TALLYOWL_HEAD").unwrap_or_else(|_| "127.0.0.1:5110".to_string());
    let session = std::fs::read_to_string(&session_file)
        .expect("`./tools.sh dev up` writes data/operator.session")
        .trim()
        .to_string();
    let client = tallyowl_rpc::Client::new(head, 16 * 1024 * 1024).with_credential(session);

    let projects = client
        .call(
            "TallyOwlControl",
            "list-projects",
            encode_list_request(&ListRequest {
                cursor: None,
                limit: None,
            }),
        )
        .expect("the head answers");
    let projects = decode_project_list(&projects.payload).expect("a project list");
    let project = projects.projects.first().expect("one project exists");
    let project_id = project.project_id.clone();
    println!("project {}", project.name);

    if showing {
        show(&client, &project_id);
        return;
    }

    if let Some(count) = many {
        write_many(&client, &project_id, count);
        return;
    }

    let now = tallyowl_obs::time::now_ms();
    let rule = AlertRule {
        rule_id: RULE.to_string(),
        name: "Any events at all".to_string(),
        project_id: project_id.clone(),
        query: tallyowl_wire::query::trend(
            1,
            tallyowl_wire::query::events(
                &to_id(&project_id),
                TimeRange {
                    // A day behind and an hour ahead, so a clock that is a
                    // little out does not make this look like no data.
                    range_start: now - 86_400_000,
                    range_end: now + 3_600_000,
                    basis: TimeBasis::OccurredAt,
                    timezone: None,
                },
            ),
            86_400_000,
            "events",
        ),
        interval_ms: 5_000,
        threshold: Some(ThresholdCondition {
            alias: "events".to_string(),
            compare: CompareOp::Gt,
            value: 0.0,
            sustained_ms: None,
        }),
        absence: None,
        // An address nothing is listening on, on purpose: the delivery fails,
        // the attempt is recorded, and the alert state does not move. That is
        // the rule `docs/ALERTS.md` section 6 states and it is worth seeing.
        notify: vec![NotificationTarget {
            kind: TargetKind::Webhook,
            url: Some("http://127.0.0.1:5199/alerts".to_string()),
            secret_ref: None,
        }],
        enabled: true,
        escalate_after_ms: None,
        silenced_until: None,
        silence_reason: None,
        disabled_reason: None,
        updated_at: None,
        updated_by: None,
    };

    let stored = client
        .call(
            "TallyOwlControl",
            "put-alert-rule",
            encode_alert_rule(&rule),
        )
        .expect("the head answers");
    let stored = decode_alert_rule(&stored.payload).expect("a rule");
    println!(
        "wrote `{}`, evaluating every {} ms",
        stored.rule_id, stored.interval_ms
    );
    println!("send an event, wait one interval, then run this again with `show`.");
}

/// Write `count` rules on a one-minute interval, for the L144 measurement.
///
/// Each rule is the same cheap trend query with its own identifier, and none
/// notifies anywhere real. What the measurement reads afterwards is
/// `tallyowl_alert_evaluations_total` over time: the rate is the throughput
/// one permit and one worker give, and `count` divided by it is the delay the
/// last rule sees.
fn write_many(client: &tallyowl_rpc::Client, project_id: &[u8], count: usize) {
    let now = tallyowl_obs::time::now_ms();
    for index in 0..count {
        let rule = AlertRule {
            rule_id: format!("scale-{index}"),
            name: format!("Scale rule {index}"),
            project_id: project_id.to_vec(),
            query: tallyowl_wire::query::trend(
                1,
                tallyowl_wire::query::events(
                    &to_id(project_id),
                    TimeRange {
                        range_start: now - 3_600_000,
                        range_end: now + 3_600_000,
                        basis: TimeBasis::OccurredAt,
                        timezone: None,
                    },
                ),
                3_600_000,
                "events",
            ),
            interval_ms: 60_000,
            threshold: Some(ThresholdCondition {
                alias: "events".to_string(),
                compare: CompareOp::Gt,
                value: 1e18,
                sustained_ms: None,
            }),
            absence: None,
            notify: Vec::new(),
            enabled: true,
            escalate_after_ms: None,
            silenced_until: None,
            silence_reason: None,
            disabled_reason: None,
            updated_at: None,
            updated_by: None,
        };
        client
            .call(
                "TallyOwlControl",
                "put-alert-rule",
                encode_alert_rule(&rule),
            )
            .expect("the head answers");
        if (index + 1) % 100 == 0 {
            println!("wrote {} rules", index + 1);
        }
    }
    println!("wrote {count} rules on a one-minute interval.");
    println!("watch: curl -s http://<head-operational>/metrics | grep alert_evaluations");
}

fn show(client: &tallyowl_rpc::Client, project_id: &[u8]) {
    let instances = client
        .call(
            "TallyOwlControl",
            "list-alert-instances",
            encode_alert_list_request(&AlertListRequest {
                project_id: project_id.to_vec(),
                cursor: None,
                limit: None,
            }),
        )
        .expect("the head answers");
    let instances = decode_alert_instance_list(&instances.payload).expect("instances");
    if instances.instances.is_empty() {
        println!("no alert has been evaluated yet.");
    }
    for instance in &instances.instances {
        println!(
            "{}: {:?} since {}, value {:?}, notifications {:?}",
            instance.rule_id,
            instance.state,
            instance.since,
            instance.observed_value,
            instance.notifications_sent
        );
    }

    let workflows = client
        .call(
            "TallyOwlControl",
            "list-workflows",
            encode_list_request(&ListRequest {
                cursor: None,
                limit: None,
            }),
        )
        .expect("the head answers");
    let workflows = decode_workflow_list(&workflows.payload).expect("workflows");
    for workflow in &workflows.workflows {
        println!(
            "{:?}: waiting {}, running {}, lag {} ms, failures {}, quarantined {}",
            workflow.kind,
            workflow.pending,
            workflow.in_flight,
            workflow.oldest_pending_age_ms,
            workflow.failures,
            workflow.quarantined
        );
    }
}

fn to_id(bytes: &[u8]) -> [u8; 16] {
    <[u8; 16]>::try_from(bytes).expect("a project identifier is sixteen bytes")
}
