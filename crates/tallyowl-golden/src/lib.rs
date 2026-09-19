//! The golden CBOR vectors.
//!
//! `docs/PLAN.md` Phase 2 says the contract is not trustworthy until every
//! maintained language encodes each golden vector to identical bytes. This crate
//! builds the vectors in Rust and writes them to `golden/vectors.json`. The Go
//! and TypeScript suites read that file, build the same values, and compare.
//!
//! # What a vector is for
//!
//! Each one holds a construct that a generator can get wrong: an optional field
//! that is absent, an optional field that is present, an array, an enum, raw
//! bytes, an exact decimal, a nested record, and each of the three shapes that
//! carry an explicit discriminant. A vector is not a sample of realistic
//! traffic. It is the smallest value that would change bytes if a generator
//! changed its mind.
//!
//! # Regenerating
//!
//! ```text
//! TALLYOWL_UPDATE_GOLDEN=1 cargo test -p tallyowl-golden
//! ```
//!
//! Regenerating is a deliberate act. A change to `golden/vectors.json` in a
//! review means the wire changed, and a reviewer should be able to see that
//! from the file alone.

use tallyowl_wire::{collector as cwire, collector_items_bridge as citems, control, query, Value};

/// One vector: a named value, the type it is, the package that encoded it, and
/// the bytes every language must produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vector {
    pub name: &'static str,
    /// The CSIL type name, so a reader of the file can find the rule.
    pub type_name: &'static str,
    /// The generated package, because each package carries its own copy of the
    /// shared types and a copy could drift.
    pub package: &'static str,
    pub bytes: Vec<u8>,
}

const COLLECTOR: &str = "tallyowl-collector-api";
const CONTROL: &str = "tallyowl-control-api";
const INGEST: &str = "tallyowl-ingest-api";

/// A fixed identifier. A vector never uses a random one, because a result
/// nobody can reproduce is not a result.
fn id(fill: u8) -> Vec<u8> {
    vec![fill; 16]
}

/// The one time every vector uses, so a reader sees the same digits in each.
const AT: i64 = 1_785_628_800_000;

fn minimal_envelope() -> tallyowl_collector_api::types::Envelope {
    use tallyowl_collector_api::types::{Envelope, TelemetryKind};
    Envelope {
        event_id: id(1),
        kind: TelemetryKind::Event,
        schema_version: 1,
        occurred_at: AT,
        observed_at: None,
        received_at: None,
        workspace_id: None,
        project_id: None,
        source_id: None,
        sequence: None,
        release: None,
        service_name: None,
        request_id: None,
        session_id: None,
        end_user_id: None,
        anonymous_id: None,
        trace_id: None,
        span_id: None,
        consent: None,
        sdk_name: "tallyowl-driver-rust".into(),
        sdk_version: "0.0.0".into(),
        properties: Vec::new(),
        measurements: None,
    }
}

fn full_envelope() -> tallyowl_collector_api::types::Envelope {
    use tallyowl_collector_api::types::{Consent, ConsentState, PropertyOrigin};
    tallyowl_collector_api::types::Envelope {
        observed_at: Some(AT + 1),
        received_at: Some(AT + 2),
        workspace_id: Some(id(8)),
        project_id: Some(id(9)),
        source_id: Some(id(7)),
        sequence: Some(42),
        release: Some("2026.8.1".into()),
        service_name: Some("checkout".into()),
        request_id: Some("r-1".into()),
        session_id: Some("s-1".into()),
        end_user_id: Some("u-1".into()),
        anonymous_id: Some("a-1".into()),
        trace_id: Some(id(3)),
        span_id: Some(vec![4; 8]),
        consent: Some(Consent {
            marketing: ConsentState::Denied,
            analytics: ConsentState::Granted,
            policy_version: Some("2026-01".into()),
        }),
        properties: vec![
            cwire::property(
                "region",
                Value::Text("us-west2".into()),
                PropertyOrigin::Collector,
            ),
            cwire::property("attempts", Value::Unsigned(3), PropertyOrigin::Driver),
            cwire::property("ratio", Value::Float(0.5), PropertyOrigin::Client),
        ],
        measurements: Some(vec![cwire::measurement(
            "render",
            Value::Float(12.5),
            Some("ms"),
        )
        .expect("a number")]),
        ..minimal_envelope()
    }
}

/// Every vector, in the order the file holds them.
pub fn vectors() -> Vec<Vector> {
    use tallyowl_collector_api::codec as cc;
    use tallyowl_collector_api::types as ct;

    let mut out: Vec<Vector> = Vec::new();
    let mut typed = |name: &'static str, value: Value| {
        out.push(Vector {
            name,
            type_name: "TypedValue",
            package: COLLECTOR,
            bytes: cc::encode_typed_value(&cwire::write(&value)),
        });
    };

    // Every arm of the discriminated value. These are the eight the earlier
    // bare choice could not carry out of a dynamically typed language.
    typed("typed-value-null", Value::Null);
    typed("typed-value-bool", Value::Boolean(true));
    typed("typed-value-int", Value::Integer(-3));
    typed("typed-value-uint", Value::Unsigned(3));
    typed("typed-value-float", Value::Float(0.5));
    typed(
        "typed-value-decimal",
        Value::decimal_from_text("19.99").expect("a number"),
    );
    typed("typed-value-text", Value::Text("us-west2".into()));
    typed(
        "typed-value-bytes",
        Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
    );

    out.push(Vector {
        name: "property-collector-origin",
        type_name: "Property",
        package: COLLECTOR,
        bytes: cc::encode_property(&cwire::property(
            "region",
            Value::Text("us-west2".into()),
            ct::PropertyOrigin::Collector,
        )),
    });

    out.push(Vector {
        name: "measurement-decimal-with-unit",
        type_name: "Measurement",
        package: COLLECTOR,
        bytes: cc::encode_measurement(
            &cwire::measurement(
                "amount",
                Value::decimal_from_text("-0.01").expect("a number"),
                Some("USD"),
            )
            .expect("a number"),
        ),
    });

    out.push(Vector {
        name: "envelope-minimal",
        type_name: "Envelope",
        package: COLLECTOR,
        bytes: cc::encode_envelope(&minimal_envelope()),
    });
    out.push(Vector {
        name: "envelope-full",
        type_name: "Envelope",
        package: COLLECTOR,
        bytes: cc::encode_envelope(&full_envelope()),
    });

    out.push(Vector {
        name: "item-event",
        type_name: "TelemetryItem",
        package: COLLECTOR,
        bytes: cc::encode_telemetry_item(&citems::event(
            minimal_envelope(),
            ct::EventPayload {
                name: "checkout-started".into(),
                route: Some("/checkout".into()),
                page_title: None,
            },
        )),
    });

    out.push(Vector {
        name: "item-page-view-with-campaign",
        type_name: "TelemetryItem",
        package: COLLECTOR,
        bytes: cc::encode_telemetry_item(&citems::page_view(
            minimal_envelope(),
            ct::PageViewPayload {
                route: "/pricing".into(),
                page_title: Some("Pricing".into()),
                referrer: None,
                campaign: Some(ct::CampaignParameters {
                    source: Some("newsletter".into()),
                    medium: Some("email".into()),
                    campaign: Some("spring".into()),
                    term: None,
                    content: None,
                    click_id: None,
                }),
            },
        )),
    });

    out.push(Vector {
        name: "item-conversion-exact-money",
        type_name: "TelemetryItem",
        package: COLLECTOR,
        bytes: cc::encode_telemetry_item(&citems::conversion(
            minimal_envelope(),
            ct::ConversionPayload {
                goal: "purchase".into(),
                value: Some(ct::CsilDecimal {
                    exponent: -2,
                    mantissa: 1999,
                }),
                currency: Some("USD".into()),
                order_id: None,
                campaign: None,
                touch_event_id: None,
            },
        )),
    });

    out.push(Vector {
        name: "item-error-with-frames",
        type_name: "TelemetryItem",
        package: COLLECTOR,
        bytes: cc::encode_telemetry_item(&citems::error(
            minimal_envelope(),
            ct::ErrorPayload {
                error_type: "TypeError".into(),
                message: "x is not a function".into(),
                handled: false,
                severity: ct::ErrorPayload_severity::Fatal,
                mechanism: None,
                runtime: Some("node".into()),
                frames: Some(vec![ct::StackFrame {
                    module: Some("checkout".into()),
                    function: Some("submit".into()),
                    file: None,
                    line: Some(42),
                    in_app: true,
                }]),
                breadcrumbs: None,
            },
        )),
    });

    out.push(Vector {
        name: "item-session-heartbeat-no-payload",
        type_name: "TelemetryItem",
        package: COLLECTOR,
        bytes: {
            let mut item = citems::empty_item(minimal_envelope());
            item.envelope.kind = ct::TelemetryKind::SessionHeartbeat;
            cc::encode_telemetry_item(&item)
        },
    });

    out.push(Vector {
        name: "batch-two-items",
        type_name: "Batch",
        package: COLLECTOR,
        bytes: cc::encode_batch(&ct::Batch {
            batch_id: id(2),
            items: vec![
                citems::event(
                    minimal_envelope(),
                    ct::EventPayload {
                        name: "a".into(),
                        route: None,
                        page_title: None,
                    },
                ),
                citems::event(
                    minimal_envelope(),
                    ct::EventPayload {
                        name: "b".into(),
                        route: None,
                        page_title: None,
                    },
                ),
            ],
            common_properties: None,
            sealed_at: AT,
            compression: Some(ct::Compression::Zstd),
        }),
    });

    out.push(Vector {
        name: "commit-batch-response-with-rejection",
        type_name: "CommitBatchResponse",
        package: COLLECTOR,
        bytes: cc::encode_commit_batch_response(&ct::CommitBatchResponse {
            batch_id: id(2),
            accepted: 1,
            committed_at: AT,
            satisfied_policy: ct::ReceiptPolicy::LocalOne,
            commit_watermark: 7,
            protocol_version: 1,
            projector_version: 1,
            rejected: Some(vec![ct::RejectedItem {
                event_id: id(5),
                code: ct::ErrorCode::InvalidArgument,
                message: "This item carries no project.".into(),
            }]),
            deduplicated: Some(false),
        }),
    });

    out.push(Vector {
        name: "service-error-retryable",
        type_name: "ServiceError",
        package: COLLECTOR,
        bytes: cc::encode_service_error(&ct::ServiceError {
            code: ct::ErrorCode::Unavailable,
            message: "We could not reach the durable store.".into(),
            retryable: true,
            detail: None,
        }),
    });

    // The ingest package carries the same shared types. A vector from each
    // package is what proves the copies still agree.
    {
        use tallyowl_ingest_api::codec as ic;
        use tallyowl_ingest_api::types as it;
        use tallyowl_wire::ingest_items_bridge as iitems;

        let envelope = it::Envelope {
            event_id: id(1),
            kind: it::TelemetryKind::Event,
            schema_version: 1,
            occurred_at: AT,
            observed_at: None,
            received_at: None,
            workspace_id: None,
            project_id: None,
            source_id: None,
            sequence: None,
            release: None,
            service_name: None,
            request_id: None,
            session_id: None,
            end_user_id: None,
            anonymous_id: None,
            trace_id: None,
            span_id: None,
            consent: None,
            sdk_name: "tallyowl-browser".into(),
            sdk_version: "0.0.0".into(),
            properties: Vec::new(),
            measurements: None,
        };
        out.push(Vector {
            name: "capture-request-one-event",
            type_name: "CaptureRequest",
            package: INGEST,
            bytes: ic::encode_capture_request(&it::CaptureRequest {
                items: vec![iitems::event(
                    envelope,
                    it::EventPayload {
                        name: "checkout-started".into(),
                        route: Some("/checkout".into()),
                        page_title: None,
                    },
                )],
            }),
        });
    }

    // The query algebra. Both discriminated shapes and the encoded child
    // reference that every nesting point carries.
    {
        use tallyowl_control_api::codec as qc;
        use tallyowl_control_api::types as qt;

        let range = qt::TimeRange {
            range_start: AT,
            range_end: AT + 3_600_000,
            basis: qt::TimeBasis::OccurredAt,
            timezone: None,
        };

        out.push(Vector {
            name: "query-node-scan",
            type_name: "QueryNodeBox",
            package: CONTROL,
            bytes: qc::encode_query_node_box(&query::node::scan(query::events(
                &id(9),
                range.clone(),
            ))),
        });

        out.push(Vector {
            name: "expression-compare",
            type_name: "ExpressionNode",
            package: CONTROL,
            bytes: qc::encode_expression_node(&query::expression::compare(qt::CompareExpr {
                compare: qt::CompareOp::Eq,
                left: query::expression_ref(&query::expression::field(qt::FieldRef {
                    name: "route".into(),
                    value_type: Some("text".into()),
                    origin: None,
                })),
                right: query::expression_ref(&query::expression::literal(control::write(
                    &Value::Text("/pricing".into()),
                ))),
            })),
        });

        out.push(Vector {
            name: "query-request-trend",
            type_name: "QueryRequest",
            package: CONTROL,
            bytes: qc::encode_query_request(&query::trend(
                1,
                query::events(&id(9), range),
                60_000,
                "events",
            )),
        });
    }

    out
}

/// The file every language reads.
pub fn to_json(vectors: &[Vector]) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"note\": \"Generated by `TALLYOWL_UPDATE_GOLDEN=1 cargo test -p tallyowl-golden`. A change here means the wire changed.\",\n");
    out.push_str("  \"vectors\": [\n");
    for (index, vector) in vectors.iter().enumerate() {
        out.push_str("    {\n");
        out.push_str(&format!("      \"name\": \"{}\",\n", vector.name));
        out.push_str(&format!("      \"type\": \"{}\",\n", vector.type_name));
        out.push_str(&format!("      \"package\": \"{}\",\n", vector.package));
        out.push_str(&format!("      \"bytes\": \"{}\"\n", hex(&vector.bytes)));
        out.push_str(if index + 1 == vectors.len() {
            "    }\n"
        } else {
            "    },\n"
        });
    }
    out.push_str("  ]\n}\n");
    out
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
