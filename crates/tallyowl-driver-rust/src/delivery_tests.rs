//! The delivery state, on its failure paths.
//!
//! The fake here is a collector, not a mock of the driver's own seam. It decodes
//! a real `SubmitBatchRequest` off a real CSIL-RPC frame and answers with a real
//! reply. Time is injected: a test moves the clock, and nothing here sleeps to
//! let a retry wait pass.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tallyowl_collector_api::codec::{
    decode_submit_batch_request, encode_service_error, encode_submit_batch_response,
};
use tallyowl_collector_api::types::{
    ErrorCode as WireCode, ServiceError, SubmitBatchRequest, SubmitBatchResponse,
};
use tallyowl_obs::time::now_ms;
use tallyowl_rpc::{error_outcome, reply, Dispatcher, Outcome, Request, TransportStatus};

use crate::driver::{backoff_delay, Clock, Random};
use crate::{retry_after, Capture, Driver, ErrorCode, Settings, TallyOwlError};

/// What the fake collector saw: each batch ID, and how many items it held.
type Seen = Arc<Mutex<Vec<(Vec<u8>, usize)>>>;

/// A collector whose answer to call number `n` is up to the test.
fn collector(
    max_frame: usize,
    answer: impl Fn(usize, &SubmitBatchRequest) -> Outcome + Send + Sync + 'static,
) -> (tallyowl_rpc::Server, Seen) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let calls = AtomicUsize::new(0);
    let server = tallyowl_rpc::serve(
        "127.0.0.1:0",
        Arc::new(move |request: &Request| {
            let batch = decode_submit_batch_request(&request.payload).expect("a batch");
            record
                .lock()
                .unwrap()
                .push((batch.batch.batch_id.clone(), batch.batch.items.len()));
            answer(calls.fetch_add(1, Ordering::SeqCst), &batch)
        }) as Arc<dyn Dispatcher>,
        max_frame,
    )
    .expect("serve");
    (server, seen)
}

fn accept(batch: &SubmitBatchRequest) -> Outcome {
    reply(
        "SubmitBatchResponse",
        encode_submit_batch_response(&SubmitBatchResponse {
            batch_id: batch.batch.batch_id.clone(),
            accepted: batch.batch.items.len() as u64,
            durable_copies: 1,
            queued_at: now_ms(),
            rejected: None,
            policy_version: None,
        }),
    )
}

fn refuse(code: WireCode, retryable: bool) -> Outcome {
    error_outcome(encode_service_error(&ServiceError {
        code,
        message: "The collector said no.".to_string(),
        retryable,
        detail: None,
    }))
}

/// A clock a test moves by hand, and jitter that is always zero, so the wait
/// after failure `n` is exactly half its ceiling.
fn sources() -> (Clock, Random, Arc<Mutex<Duration>>) {
    let base = Instant::now();
    let offset = Arc::new(Mutex::new(Duration::ZERO));
    let read = Arc::clone(&offset);
    (
        Arc::new(move || base + *read.lock().unwrap()),
        Arc::new(|| 0),
        offset,
    )
}

fn settings_for(address: impl Into<String>) -> Settings {
    let mut settings = Settings::new(address, "key-a");
    settings.linger = Duration::from_secs(3600);
    // A shutdown in these tests never has a reason to wait.
    settings.shutdown_flush_deadline = Duration::from_millis(200);
    settings
}

fn capture_events(driver: &Driver, count: usize) {
    for _ in 0..count {
        driver.capture(Capture::event("checkout-started")).unwrap();
    }
}

fn errors() -> (
    Arc<Mutex<Vec<TallyOwlError>>>,
    impl Fn(&TallyOwlError) + Send + Sync,
) {
    let reported = Arc::new(Mutex::new(Vec::new()));
    let write = Arc::clone(&reported);
    (reported, move |error: &TallyOwlError| {
        write.lock().unwrap().push(error.clone())
    })
}

#[test]
fn a_shutdown_with_no_collector_reports_every_item_it_could_not_send() {
    // The defect this replaces: the first flush sealed the buffer and failed,
    // the batch was dropped, the second flush found an empty buffer, and the
    // shutdown returned 0 with five events gone.
    let mut settings = settings_for("127.0.0.1:1");
    // The first wait is longer than the deadline, so the shutdown returns at
    // once rather than resting.
    settings.retry_backoff_min = Duration::from_secs(5);
    let driver = Driver::new(settings);
    capture_events(&driver, 5);

    assert_eq!(driver.shutdown(), 5);
    let stats = driver.stats();
    assert_eq!(
        stats.lost, 0,
        "an unreachable collector loses nothing by itself"
    );
    assert_eq!(stats.buffered + stats.unacknowledged, 5);
    assert!(stats.last_error.is_some(), "and the reason is on record");
}

#[test]
fn a_failed_flush_keeps_its_batch_and_the_same_batch_goes_again() {
    // D5: "The app retains the stable batch until that acknowledgement."
    let (server, seen) = collector(1024 * 1024, |call, batch| match call {
        0 => refuse(WireCode::Unavailable, true),
        _ => accept(batch),
    });
    let (clock, random, offset) = sources();
    let driver = Driver::with_sources(
        settings_for(server.local_address().to_string()),
        clock,
        random,
    );
    capture_events(&driver, 3);

    let failure = driver.flush().unwrap_err();
    assert_eq!(failure.code, ErrorCode::Unavailable);
    assert!(failure.retryable);
    assert_eq!(driver.buffered(), 0);
    assert_eq!(driver.outstanding_items(), 3, "the batch is retained");

    // The wait is open, so the next call returns at once and says how long.
    let waiting = driver.flush().unwrap_err();
    assert_eq!(retry_after(&waiting), Some(Duration::from_millis(50)));
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "nothing was sent while waiting"
    );

    *offset.lock().unwrap() = Duration::from_millis(50);
    let receipt = driver.flush().unwrap().expect("a receipt");
    assert_eq!(receipt.accepted, 3);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].0, seen[1].0,
        "the batch kept its ID across the retry"
    );
    assert_eq!(receipt.batch_id.to_vec(), seen[1].0);
    let stats = driver.stats();
    assert_eq!(
        (stats.accepted, stats.lost, stats.unacknowledged),
        (3, 0, 0)
    );
}

#[test]
fn a_retryable_refusal_never_uses_up_the_attempts_of_a_batch() {
    // A collector whose durable store is down says "try again" for as long as
    // the outage lasts. The batch must outlive that, however long it is.
    let (server, _) = collector(1024 * 1024, |call, batch| {
        if call < 20 {
            refuse(WireCode::Unavailable, true)
        } else {
            accept(batch)
        }
    });
    let (clock, random, offset) = sources();
    let mut settings = settings_for(server.local_address().to_string());
    settings.max_batch_attempts = 2;
    let driver = Driver::with_sources(settings, clock, random);
    capture_events(&driver, 2);

    let mut receipt = None;
    for _ in 0..100 {
        match driver.flush() {
            Ok(done) => {
                receipt = done;
                break;
            }
            Err(error) => {
                if let Some(wait) = retry_after(&error) {
                    *offset.lock().unwrap() += wait;
                }
            }
        }
    }
    assert_eq!(receipt.expect("the batch went in the end").accepted, 2);
    assert_eq!(driver.stats().lost, 0);
}

#[test]
fn a_buffer_larger_than_one_frame_goes_as_several_batches_and_loses_nothing() {
    // The defect this replaces: a seal took the whole buffer, the encoded batch
    // passed the frame limit, and every item in it was dropped with advice the
    // caller could not follow.
    let (server, seen) = collector(4096, |_, batch| accept(batch));
    let mut settings = settings_for(server.local_address().to_string());
    settings.max_frame_bytes = 4096;
    settings.max_batch_bytes = 2048;
    settings.max_items = 100_000;
    let driver = Driver::new(settings);
    capture_events(&driver, 200);

    let receipt = driver.flush().unwrap().expect("a receipt");
    assert_eq!(receipt.accepted, 200, "one receipt reports every batch");

    let seen = seen.lock().unwrap();
    assert!(seen.len() > 1, "the buffer needed more than one frame");
    assert_eq!(seen.iter().map(|(_, items)| items).sum::<usize>(), 200);
    assert_eq!(driver.buffered(), 0);
    assert_eq!(driver.stats().lost, 0);
}

#[test]
fn a_seal_stops_at_the_item_limit() {
    let (server, seen) = collector(1024 * 1024, |_, batch| accept(batch));
    let mut settings = settings_for(server.local_address().to_string());
    settings.max_items = 10;
    let driver = Driver::new(settings);
    capture_events(&driver, 35);

    driver.submit().unwrap();
    driver.drain().unwrap();
    let sizes: Vec<usize> = seen.lock().unwrap().iter().map(|(_, n)| *n).collect();
    assert_eq!(sizes.iter().sum::<usize>(), 35);
    assert!(sizes.iter().all(|n| *n <= 10), "{sizes:?}");
}

#[test]
fn an_item_that_cannot_fit_one_frame_is_refused_at_capture() {
    let mut settings = settings_for("127.0.0.1:1");
    settings.max_frame_bytes = 2048;
    let driver = Driver::new(settings);
    let failure = driver
        .capture(
            Capture::event("large").with_property("blob", crate::Value::Text("x".repeat(4096))),
        )
        .unwrap_err();
    assert!(
        failure.message.contains("was not recorded"),
        "{}",
        failure.message
    );
    assert_eq!(driver.buffered(), 0);
    assert_eq!(driver.stats().refused, 1);
}

#[test]
fn a_refusal_that_cannot_change_loses_the_batch_counts_it_and_says_so() {
    let (server, _) = collector(1024 * 1024, |call, batch| match call {
        0 => refuse(WireCode::PermissionDenied, false),
        _ => accept(batch),
    });
    let (reported, hook) = errors();
    let driver = Driver::new(settings_for(server.local_address().to_string()).with_on_error(hook));
    capture_events(&driver, 4);

    let failure = driver.flush().unwrap_err();
    assert_eq!(failure.code, ErrorCode::PermissionDenied);
    assert!(!failure.retryable);

    let stats = driver.stats();
    assert_eq!(stats.lost, 4);
    assert_eq!(stats.unacknowledged, 0, "a lost batch is not held for ever");
    let reported = reported.lock().unwrap();
    assert_eq!(reported.len(), 1);
    assert!(
        reported[0].message.contains("4 events were not recorded"),
        "{}",
        reported[0].message
    );

    // The lost batch does not stand in front of the next one.
    capture_events(&driver, 1);
    assert_eq!(driver.flush().unwrap().expect("a receipt").accepted, 1);
}

#[test]
fn a_wrong_key_names_the_key() {
    let (server, _) = collector(1024 * 1024, |_, _| refuse(WireCode::Unauthenticated, false));
    let driver = Driver::new(settings_for(server.local_address().to_string()));
    capture_events(&driver, 1);
    let failure = driver.flush().unwrap_err();
    assert_eq!(failure.code, ErrorCode::Unauthenticated);
    assert!(failure.message.contains("key"), "{}", failure.message);
    assert_eq!(driver.shutdown(), 0, "nothing is left to send");
    assert_eq!(driver.stats().lost, 1, "and the loss is counted");
}

#[test]
fn a_batch_that_leaves_and_gets_no_answer_is_given_up_after_its_attempts() {
    // A batch that makes the collector fail every time must not go again
    // without limit, and it must not stand in front of every later batch.
    let (server, seen) = collector(1024 * 1024, |_, _| {
        Outcome::Transport(TransportStatus::Internal, "the handler failed".to_string())
    });
    let (clock, random, offset) = sources();
    let (reported, hook) = errors();
    let mut settings = settings_for(server.local_address().to_string()).with_on_error(hook);
    settings.max_batch_attempts = 3;
    let driver = Driver::with_sources(settings, clock, random);
    capture_events(&driver, 2);

    for _ in 0..3 {
        let failure = driver.flush().unwrap_err();
        assert_eq!(retry_after(&failure), None, "a real attempt, not a wait");
        let wait = retry_after(&driver.flush().unwrap_err()).expect("the wait is open");
        *offset.lock().unwrap() += wait;
    }

    assert_eq!(seen.lock().unwrap().len(), 3, "three attempts and no more");
    let stats = driver.stats();
    assert_eq!((stats.lost, stats.unacknowledged), (2, 0));
    assert_eq!(reported.lock().unwrap().len(), 1);
}

#[test]
fn the_wait_doubles_is_capped_and_is_never_less_than_half_its_ceiling() {
    let min = Duration::from_millis(100);
    let max = Duration::from_secs(30);
    for (failures, ceiling_ms) in [(1, 100), (2, 200), (3, 400), (4, 800), (9, 25_600)] {
        let ceiling = Duration::from_millis(ceiling_ms);
        assert_eq!(
            backoff_delay(failures, min, max, 0),
            ceiling / 2,
            "{failures}"
        );
        assert!(backoff_delay(failures, min, max, u64::MAX) <= ceiling);
        for random in [1, 7, 1_000_003, u64::MAX - 1] {
            let wait = backoff_delay(failures, min, max, random);
            assert!(
                wait >= ceiling / 2 && wait <= ceiling,
                "{failures}: {wait:?}"
            );
        }
    }
    // Past the cap, and at a count that would overflow a shift.
    for failures in [10, 11, 64, u32::MAX] {
        assert_eq!(backoff_delay(failures, min, max, 0), max / 2, "{failures}");
    }
}

#[test]
fn an_unreachable_collector_is_tried_on_a_growing_wait_and_not_in_a_tight_loop() {
    let (clock, random, offset) = sources();
    let driver = Driver::with_sources(settings_for("127.0.0.1:1"), clock, random);
    capture_events(&driver, 1);

    let mut waits = Vec::new();
    for _ in 0..4 {
        let attempt = driver.flush().unwrap_err();
        assert_eq!(retry_after(&attempt), None, "this call tried the collector");
        let wait = retry_after(&driver.submit().unwrap_err()).expect("the wait is open");
        assert!(retry_after(&driver.drain().unwrap_err()).is_some());
        waits.push(wait.as_millis());
        *offset.lock().unwrap() += wait;
    }
    assert_eq!(waits, vec![50, 100, 200, 400]);
    assert_eq!(
        driver.stats().lost,
        0,
        "a batch that never left used no attempt"
    );
    assert_eq!(driver.outstanding_items(), 1);
}

#[test]
fn sealed_batches_count_against_the_bound_so_an_outage_cannot_grow_memory() {
    let mut settings = settings_for("127.0.0.1:1");
    settings.max_unacknowledged_bytes = 2_000;
    settings.retry_backoff_min = Duration::from_secs(3600);
    let driver = Driver::new(settings);

    let mut refused = None;
    for _ in 0..1_000 {
        if let Err(error) = driver.capture(Capture::event("checkout-started")) {
            refused = Some(error);
            break;
        }
        // The first failed flush moves the buffer into a retained batch. Before
        // the change that emptied the account, and capture never refused.
        let _ = driver.flush();
    }
    let refused = refused.expect("the bound is reached");
    assert_eq!(refused.code, ErrorCode::ResourceExhausted);
    assert!(
        driver.outstanding_items() > 0,
        "the failed batch is still held"
    );
    assert_eq!(driver.stats().lost, 0);
}

/// A collector that accepts a connection and never answers.
fn silent_collector() -> (String, Arc<Mutex<Vec<TcpStream>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
    let address = listener.local_addr().unwrap().to_string();
    let held = Arc::new(Mutex::new(Vec::new()));
    let keep = Arc::clone(&held);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            keep.lock().unwrap().push(stream);
        }
    });
    (address, held)
}

#[test]
fn a_collector_that_accepts_and_never_answers_holds_a_call_for_the_call_timeout_only() {
    let (address, _held) = silent_collector();
    let mut settings = settings_for(address).with_call_timeout(Duration::from_millis(40));
    settings.shutdown_flush_deadline = Duration::from_millis(120);
    let driver = Driver::new(settings);
    capture_events(&driver, 3);

    let started = Instant::now();
    let failure = driver.flush().unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the call waited {:?}",
        started.elapsed()
    );
    assert_eq!(failure.code, ErrorCode::Unavailable);
    assert_eq!(driver.outstanding_items(), 3, "the batch is retained");

    let started = Instant::now();
    assert_eq!(
        driver.shutdown(),
        3,
        "and the shutdown says what did not go"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the shutdown waited {:?}",
        started.elapsed()
    );
}

#[test]
fn a_shutdown_returns_at_its_deadline_when_the_call_timeout_is_longer() {
    // The default call timeout is 10 s and the default shutdown deadline is
    // 2 s. The deadline wins: the work runs beside the caller, not in front.
    let (address, _held) = silent_collector();
    let mut settings = settings_for(address).with_call_timeout(Duration::from_secs(30));
    settings.shutdown_flush_deadline = Duration::from_millis(60);
    let driver = Driver::new(settings);
    capture_events(&driver, 2);

    let started = Instant::now();
    assert_eq!(driver.shutdown(), 2);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the shutdown waited {:?}",
        started.elapsed()
    );
}

/// A relay in front of a collector, which a test can cut. A cut connection and
/// a relay that still listens is what a collector restart looks like.
struct Relay {
    address: String,
    live: Arc<Mutex<Vec<TcpStream>>>,
}

impl Relay {
    fn to(target: std::net::SocketAddr) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listen");
        let address = listener.local_addr().unwrap().to_string();
        let live = Arc::new(Mutex::new(Vec::new()));
        let keep = Arc::clone(&live);
        std::thread::spawn(move || {
            for inbound in listener.incoming().flatten() {
                let Ok(outbound) = TcpStream::connect(target) else {
                    continue;
                };
                keep.lock().unwrap().push(inbound.try_clone().unwrap());
                keep.lock().unwrap().push(outbound.try_clone().unwrap());
                pump(inbound.try_clone().unwrap(), outbound.try_clone().unwrap());
                pump(outbound, inbound);
            }
        });
        Relay { address, live }
    }

    fn cut(&self) {
        for stream in self.live.lock().unwrap().drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        while let Ok(read) = from.read(&mut buffer) {
            if read == 0 || to.write_all(&buffer[..read]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(Shutdown::Both);
    });
}

#[test]
fn a_connection_that_died_while_idle_costs_one_reconnect_and_no_wait() {
    // A collector that restarted is the ordinary case. The first call after it
    // must not fail for it, and must not deliver the batch twice as two IDs.
    let (server, seen) = collector(1024 * 1024, |_, batch| accept(batch));
    let relay = Relay::to(server.local_address());
    let driver = Driver::new(settings_for(relay.address.clone()));

    capture_events(&driver, 1);
    driver.flush().unwrap().expect("the first batch");

    relay.cut();
    capture_events(&driver, 2);
    let receipt = driver
        .flush()
        .unwrap()
        .expect("the batch after the restart");
    assert_eq!(receipt.accepted, 2);
    assert_eq!(driver.stats().lost, 0);

    let seen = seen.lock().unwrap();
    let last = seen.last().unwrap();
    assert_eq!(last.0, receipt.batch_id.to_vec());
    assert!(
        seen.iter().all(|(id, _)| *id == seen[0].0 || *id == last.0),
        "a resend keeps its batch ID"
    );
}

/// A writer a test reads back.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_dry_run_writes_each_item_and_opens_no_socket() {
    let lines = Lines::default();
    // The address is a closed port. A dry run that opened a socket would fail.
    let driver = Driver::new(
        settings_for("127.0.0.1:1")
            .with_property("service", "checkout")
            .with_dry_run(lines.clone()),
    );
    driver
        .capture(Capture::event("checkout-started").with_session("s-1"))
        .unwrap();
    driver
        .capture(Capture::error("Timeout", "took too long", true))
        .unwrap();

    let receipt = driver.flush().unwrap().expect("a receipt");
    assert_eq!(receipt.accepted, 2);
    assert_eq!(
        receipt.durable_copies, 0,
        "nothing is durable, and it says so"
    );

    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let written: Vec<&str> = text.lines().collect();
    assert_eq!(written.len(), 2, "{text}");
    assert!(written[0].contains("kind=event"), "{text}");
    assert!(written[0].contains("name=\"checkout-started\""), "{text}");
    assert!(written[0].contains("session_id=\"s-1\""), "{text}");
    assert!(written[0].contains("driver.service=\"checkout\""), "{text}");
    assert!(written[1].contains("kind=error"), "{text}");
    assert_eq!(driver.shutdown(), 0);
}

#[test]
fn a_flusher_sends_without_the_host_writing_a_loop_and_stops_clean() {
    let (server, seen) = collector(1024 * 1024, |_, batch| accept(batch));
    let mut settings = settings_for(server.local_address().to_string());
    settings.linger = Duration::ZERO;
    settings.shutdown_flush_deadline = Duration::from_secs(5);
    let driver = Driver::new(settings);
    let flusher = driver.spawn_flusher().expect("a thread");

    capture_events(&driver, 25);
    // Stopping is what this test waits on, not the clock: the flusher sends
    // what is left before it returns.
    assert_eq!(flusher.stop(), 0, "nothing was left behind");
    let total: usize = seen.lock().unwrap().iter().map(|(_, n)| n).sum();
    assert_eq!(total, 25);
    assert_eq!(driver.stats().accepted, 25);
}

#[test]
fn a_flusher_with_no_collector_reports_the_failure_and_the_unsent_count() {
    let (reported, hook) = errors();
    let mut settings = settings_for("127.0.0.1:1").with_on_error(hook);
    settings.linger = Duration::ZERO;
    settings.retry_backoff_min = Duration::from_secs(3600);
    let driver = Driver::new(settings);

    capture_events(&driver, 6);
    // One turn by hand, so the test does not depend on when the thread runs.
    driver.tick();
    driver.tick();
    assert_eq!(
        reported.lock().unwrap().len(),
        1,
        "the failed attempt is reported once, and the open wait is not"
    );

    let flusher = driver.spawn_flusher().expect("a thread");
    assert_eq!(flusher.stop(), 6);
}

#[test]
fn an_address_that_does_not_resolve_keeps_the_batch_and_names_the_address() {
    // A name service that fails for a minute must not cost the telemetry of
    // that minute, and a developer who typed the address wrong has to be told.
    let (clock, random, _) = sources();
    let driver = Driver::with_sources(settings_for("not an address"), clock, random);
    capture_events(&driver, 2);

    let failure = driver.flush().unwrap_err();
    assert!(
        failure.message.contains("not an address"),
        "{}",
        failure.message
    );
    assert_eq!(driver.stats().lost, 0);
    assert_eq!(driver.outstanding_items(), 2);
    assert!(retry_after(&driver.flush().unwrap_err()).is_some());
}

#[test]
fn a_flusher_reports_a_batch_it_gave_up_once_and_not_twice() {
    let (server, _) = collector(1024 * 1024, |_, _| {
        refuse(WireCode::PermissionDenied, false)
    });
    let (reported, hook) = errors();
    let mut settings = settings_for(server.local_address().to_string()).with_on_error(hook);
    settings.linger = Duration::ZERO;
    let driver = Driver::new(settings);
    capture_events(&driver, 3);

    // The first turn sends, and the second collects the refusal.
    driver.tick();
    driver.tick();
    let reported = reported.lock().unwrap();
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(reported[0].message.contains("3 events were not recorded"));
}

// D62: how the driver reaches the collector. Real sockets, a real authority,
// and a real collector certificate. The only real clock is the one rustls
// checks the certificate against, so every certificate is valid now.

mod transport_security {
    use super::*;
    use crate::Transport;
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    use std::path::PathBuf;
    use tallyowl_rpc::material::CertificateSet;
    use tallyowl_rpc::{tls::serve_server_auth, ServerOptions};

    struct Authority {
        key: KeyPair,
        certificate: rcgen::Certificate,
    }

    fn authority(name: &str) -> Authority {
        let mut params = CertificateParams::default();
        let mut subject = DistinguishedName::new();
        subject.push(DnType::CommonName, name);
        params.distinguished_name = subject;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let key = KeyPair::generate().expect("a key");
        let certificate = params.self_signed(&key).expect("an authority");
        Authority { key, certificate }
    }

    /// A directory with `tls.crt` and `tls.key` for `localhost`, as an operator
    /// mounts them.
    fn collector_certificates(signer: &Authority) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let directory = std::env::temp_dir().join(format!(
            "tallyowl-driver-tls-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&directory).expect("a directory");
        let key = KeyPair::generate().expect("a key");
        let params = CertificateParams::new(vec!["localhost".to_string()]).expect("params");
        let certificate = params
            .signed_by(&key, &signer.certificate, &signer.key)
            .expect("signed");
        std::fs::write(
            directory.join("tls.crt"),
            format!("{}{}", certificate.pem(), signer.certificate.pem()),
        )
        .expect("write");
        std::fs::write(directory.join("tls.key"), key.serialize_pem()).expect("write");
        directory
    }

    /// A collector on server-authenticated TLS, which accepts every batch.
    fn tls_collector(signer: &Authority) -> (tallyowl_rpc::Server, Seen) {
        let directory = collector_certificates(signer);
        let set = CertificateSet::load(std::slice::from_ref(&directory), now_ms())
            .expect("the certificates load");
        // The set holds what it read, and nothing here reloads it, so the
        // throwaway key goes now rather than staying in the temp directory.
        let _ = std::fs::remove_dir_all(&directory);
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let server = serve_server_auth(
            "127.0.0.1:0",
            Arc::new(move |request: &Request| {
                let batch = decode_submit_batch_request(&request.payload).expect("a batch");
                record
                    .lock()
                    .unwrap()
                    .push((batch.batch.batch_id.clone(), batch.batch.items.len()));
                accept(&batch)
            }) as Arc<dyn Dispatcher>,
            ServerOptions::new(1024 * 1024),
            Arc::new(set),
        )
        .expect("serve");
        (server, seen)
    }

    fn tls_settings(server: &tallyowl_rpc::Server, transport: Transport) -> Settings {
        let address = format!("127.0.0.1:{}", server.local_address().port());
        settings_for(address).with_transport(transport)
    }

    #[test]
    fn a_batch_crosses_tls_to_a_collector_that_proves_itself() {
        let signer = authority("test authority");
        let (server, seen) = tls_collector(&signer);
        let transport = Transport::default()
            .with_roots_pem(signer.certificate.pem().as_bytes())
            .expect("the authority reads")
            .with_server_name("localhost");
        let driver = Driver::new(tls_settings(&server, transport));

        capture_events(&driver, 2);
        let receipt = driver.flush().expect("flush over TLS").expect("a receipt");
        assert_eq!(receipt.accepted, 2);

        // The pipelined path shares the connection, and is proved on its own.
        capture_events(&driver, 3);
        driver.submit().expect("submit over TLS");
        let receipts = driver.drain().expect("drain over TLS");
        assert_eq!(receipts.iter().map(|r| r.accepted).sum::<u64>(), 3);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_collector_no_trusted_authority_signed_keeps_the_batch_and_uses_no_attempt() {
        let signer = authority("test authority");
        let stranger = authority("somebody else");
        let (server, seen) = tls_collector(&signer);
        let transport = Transport::default()
            .with_roots_pem(stranger.certificate.pem().as_bytes())
            .expect("the authority reads")
            .with_server_name("localhost");
        let (clock, random, offset) = sources();
        let mut settings = tls_settings(&server, transport);
        settings.max_batch_attempts = 1;
        let (reported, hook) = errors();
        let driver = Driver::with_sources(settings.with_on_error(hook), clock, random);
        capture_events(&driver, 3);

        let mut last = None;
        for _ in 0..5 {
            let error = driver
                .flush()
                .expect_err("the collector did not prove itself");
            if let Some(wait) = retry_after(&error) {
                *offset.lock().unwrap() += wait;
            }
            last = Some(error);
        }
        let last = last.expect("an error");
        assert!(
            last.message.contains("no trusted authority")
                || reported_mentions(&reported, "no trusted authority"),
            "the reason is named: {}",
            last.message
        );
        assert!(
            reported_mentions(&reported, "with_roots_pem")
                || last.message.contains("with_roots_pem"),
            "the developer is told what to change"
        );
        let stats = driver.stats();
        assert_eq!(
            stats.lost, 0,
            "a handshake sends nothing, so nothing is lost"
        );
        assert_eq!(stats.unacknowledged + stats.buffered, 3);
        assert!(seen.lock().unwrap().is_empty(), "no batch reached it");
    }

    fn reported_mentions(reported: &Arc<Mutex<Vec<TallyOwlError>>>, text: &str) -> bool {
        reported
            .lock()
            .unwrap()
            .iter()
            .any(|error| error.message.contains(text))
    }

    #[cfg(unix)]
    #[test]
    fn a_batch_reaches_a_collector_on_a_unix_socket() {
        let path = std::env::temp_dir().join(format!(
            "tallyowl-driver-{}-{}.sock",
            std::process::id(),
            now_ms()
        ));
        let address = format!("unix:{}", path.display());
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let _server = tallyowl_rpc::serve_with(
            &address,
            Arc::new(move |request: &Request| {
                let batch = decode_submit_batch_request(&request.payload).expect("a batch");
                record
                    .lock()
                    .unwrap()
                    .push((batch.batch.batch_id.clone(), batch.batch.items.len()));
                accept(&batch)
            }) as Arc<dyn Dispatcher>,
            ServerOptions::new(1024 * 1024),
        )
        .expect("serve on a unix socket");

        let driver = Driver::new(settings_for(address));
        capture_events(&driver, 2);
        let receipt = driver.flush().expect("flush").expect("a receipt");
        assert_eq!(receipt.accepted, 2);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
