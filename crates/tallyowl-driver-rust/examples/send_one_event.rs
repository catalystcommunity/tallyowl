//! Send one event to a running collector, then ask the head for it.
//!
//! This is the round trip a person runs by hand after `./tools.sh dev up`, and
//! it is the smallest complete demonstration of the delivery contract:
//!
//! ```text
//! cargo run -p tallyowl-driver-rust --example send_one_event
//! ```
//!
//! It reads the same configuration file the services read, so it reaches the
//! same addresses without anybody repeating them.

use std::time::Duration;

use tallyowl_config::Config;
use tallyowl_control_api::codec::{decode_query_response, encode_query_request};
use tallyowl_control_api::types::{TimeBasis, TimeRange};
use tallyowl_driver_rust::{Capture, Driver, Session, Settings};
use tallyowl_wire::{control as wire, query, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load_from_host("tallyowl.local.yaml")
        .map_err(|errors| errors[0].message.clone())?;

    let collector = config.text("collector.listen").to_string();
    let head = config.text("head.listen").to_string();
    println!("Collector: {collector}");
    println!("Head:      {head}");

    // The application holds a key and knows the collector address. It never
    // learns where the head is, and it never learns its own project: the
    // collector resolves that from the key and stamps it.
    let credential = config
        .secret("collector.apiKey")
        .map_err(|e| e.to_string())?
        .expose()
        .to_string();
    if credential.is_empty() {
        return Err(
            "There is no key in `collector.apiKey`. Run `./tools.sh dev up`, which makes one, \
             or make one yourself with `tallyowl-head provision <project>`."
                .into(),
        );
    }
    let driver =
        Driver::new(Settings::new(&collector, &credential).with_property("service", "example"));
    let session = Session::start();
    let at = tallyowl_obs::time::now_ms();

    driver.capture(
        Capture::event("checkout-started")
            .with_session(session.id())
            .with_service("example")
            .at(at),
    )?;
    driver.capture(
        Capture::event("checkout-completed")
            .with_session(session.id())
            .with_service("example")
            .at(at + 1)
            .critical(),
    )?;

    let receipt = driver.flush()?.expect("two events went");
    println!(
        "\nAccepted {} events. The durable store holds {} copy.",
        receipt.accepted, receipt.durable_copies
    );
    println!("That acknowledgement means the durable store has them, and nothing more.");

    // The forwarder moves them to the head on its own. Give it a moment.
    print!("\nWaiting for the forwarder");
    // A query is a control operation, and every control operation checks
    // authorization. The session is a person's credential; the key above is an
    // application's. Neither is a substitute for the other.
    let session = std::fs::read_to_string("data/operator.session")
        .map_err(|_| {
            "There is no session in data/operator.session. Run `./tools.sh dev up`, which makes \
             one, or make one yourself with `tallyowl-head session create <name>`."
        })?
        .trim()
        .to_string();
    let query_client = tallyowl_rpc::Client::new(&head, 16 * 1024 * 1024).with_credential(session);

    // Which project did that key reach?
    //
    // The *application* above never asked and never needs to. This part of the
    // example is standing in for an operator with the key in hand, so that the
    // demonstration can run one query without a dashboard sign-in. A dashboard
    // reads the project list from the control plane after somebody signs in.
    let resolved = query_client.call(
        "TallyOwlCollector",
        "resolve-key",
        tallyowl_collector_api::codec::encode_resolve_key_request(
            &tallyowl_collector_api::types::ResolveKeyRequest {
                credential: credential.clone(),
            },
        ),
    )?;
    if resolved.variant.as_deref() == Some(tallyowl_rpc::SERVICE_ERROR_VARIANT) {
        return Err(
            "The head does not know that key. Run `./tools.sh dev reset` and try again.".into(),
        );
    }
    let project =
        tallyowl_collector_api::codec::decode_resolve_key_response(&resolved.payload)?.project_id;
    let project: [u8; 16] = project
        .try_into()
        .map_err(|_| "the head answered with a project that is not 16 bytes")?;
    let request = encode_query_request(&query::trend(
        1,
        query::events(
            // The collector resolved this from the credential. An application
            // never sends it and never needs to know it.
            &project,
            TimeRange {
                range_start: at - 3_600_000,
                range_end: at + 3_600_000,
                basis: TimeBasis::OccurredAt,
                timezone: None,
            },
        ),
        60_000,
        "events",
    ));

    for attempt in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();

        let response = query_client.call("TallyOwlControl", "run-query", request.clone())?;
        let decoded = decode_query_response(&response.payload)?;
        let total: u64 = decoded
            .rows
            .iter()
            .filter_map(|row| match row.values.get(1).map(wire::read) {
                Some(Ok(Value::Unsigned(count))) => Some(count),
                _ => None,
            })
            .sum();
        if total >= receipt.accepted {
            println!("\n\nThe query returned {total} events.");
            println!(
                "Watermark {}, complete: {}, exact: {}.",
                decoded.metadata.commit_watermark,
                decoded.metadata.complete,
                decoded.metadata.exactness[0].exact
            );
            println!("\nThe round trip works.");
            return Ok(());
        }
        let _ = attempt;
    }

    Err("The events did not reach the head within ten seconds. Check `./tools.sh dev logs`.".into())
}
