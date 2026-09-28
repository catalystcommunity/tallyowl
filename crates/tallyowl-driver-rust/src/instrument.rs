//! Helpers for the common cases: an error a host already holds, a panic, and a
//! span that measures its own duration.
//!
//! Each one is opt-in. None records a request body, a query string, or a value
//! the host did not hand it. The host decides what an error message says, so a
//! host that puts personal data in its error messages sends that data; keep
//! identifiers and values out of error text.

use std::time::Instant;

use tallyowl_collector_api::types::SpanKind;
use tallyowl_obs::time::now_ms;

use crate::{Capture, Driver, Frame, SpanContext};

/// The longest error type the contract takes, in bytes.
const MAX_ERROR_TYPE: usize = 256;
/// The longest error message the contract takes, in bytes.
const MAX_ERROR_MESSAGE: usize = 2048;

impl Capture {
    /// An error occurrence, from an error the host already holds.
    ///
    /// The error type is the Rust type name, because that is what stays the
    /// same between two occurrences of one defect. The message is the error's
    /// own text followed by the text of each cause, so the root cause is on the
    /// occurrence and nobody has to reproduce the failure to read it.
    pub fn from_error<E: std::error::Error + ?Sized>(error: &E, handled: bool) -> Capture {
        let mut message = error.to_string();
        let mut cause = error.source();
        while let Some(inner) = cause {
            message.push_str(": ");
            message.push_str(&inner.to_string());
            cause = inner.source();
        }
        let mut capture = Capture::error(
            truncated(std::any::type_name::<E>(), MAX_ERROR_TYPE),
            truncated(&message, MAX_ERROR_MESSAGE),
            handled,
        );
        if let Some(payload) = capture.item.error.as_mut() {
            payload.mechanism = Some("error".to_string());
        }
        capture
    }
}

/// The text, cut at a character boundary so that it fits the contract. An
/// occurrence with a short message is worth more than a refused one.
fn truncated(text: &str, limit: usize) -> &str {
    if text.is_empty() {
        return "unknown";
    }
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Record every panic in this process as an unhandled error.
///
/// The hook that was installed before this one still runs, so the panic message
/// still reaches the standard error stream. The occurrence carries the panic
/// message and the source location, and it seals its batch, so a flusher sends
/// it at once.
///
/// The hook never waits and never panics: when another thread holds the buffer
/// at that moment, the occurrence is given up rather than risk a panic that
/// never finishes. Call this once, after the driver is built.
pub fn capture_panics(driver: &Driver) {
    let driver = driver.clone();
    let earlier = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        driver.capture_without_waiting(panic_capture(
            info.payload(),
            info.location().map(|at| (at.file(), at.line())),
        ));
        earlier(info);
    }));
}

/// The occurrence for one panic. It is separate from the hook so that a test
/// reads it without replacing the hook of the test process.
pub(crate) fn panic_capture(
    payload: &(dyn std::any::Any + Send),
    location: Option<(&str, u32)>,
) -> Capture {
    let message = payload
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "The panic carried no message.".to_string());
    let mut capture = Capture::error("panic", truncated(&message, MAX_ERROR_MESSAGE), false);
    if let Some(payload) = capture.item.error.as_mut() {
        payload.mechanism = Some("panic".to_string());
    }
    match location {
        Some((file, line)) => capture.with_frames(vec![Frame {
            module: None,
            function: None,
            file: Some(file.to_string()),
            line: Some(u64::from(line)),
            in_app: true,
        }]),
        None => capture,
    }
}

/// A span that is running. It records itself when it ends, with the duration it
/// measured, so a caller does not compute two timestamps by hand.
///
/// Ending is explicit with [`ActiveSpan::end`] or [`ActiveSpan::fail`], and
/// dropping the guard ends it too, so an early return still records the span.
pub struct ActiveSpan {
    driver: Driver,
    context: SpanContext,
    operation: String,
    kind: SpanKind,
    start_at: i64,
    started: Instant,
    failed: Option<Option<[u8; 16]>>,
    done: bool,
}

impl Driver {
    /// Start a span. The span is recorded when the returned guard ends.
    ///
    /// Name the operation with the route template, such as `GET /orders/{id}`,
    /// and never with the address a person asked for: an address carries
    /// identifiers and a query string.
    pub fn start_span(&self, context: &SpanContext, operation: &str, kind: SpanKind) -> ActiveSpan {
        ActiveSpan {
            driver: self.clone(),
            context: *context,
            operation: operation.to_string(),
            kind,
            start_at: now_ms(),
            started: self.now(),
            failed: None,
            done: false,
        }
    }
}

impl ActiveSpan {
    /// Where this span sits in its trace. Give `context().child()` to the work
    /// inside it, and `context().traceparent()` to a service it calls.
    pub fn context(&self) -> &SpanContext {
        &self.context
    }

    /// End the span as a success.
    pub fn end(mut self) {
        self.record();
    }

    /// End the span as a failure, and link it to the error that explains it.
    pub fn fail(mut self, error_event_id: Option<[u8; 16]>) {
        self.failed = Some(error_event_id);
        self.record();
    }

    fn record(&mut self) {
        if self.done {
            return;
        }
        self.done = true;
        let elapsed = self.driver.now().saturating_duration_since(self.started);
        let duration_ms = i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX);
        let mut capture = Capture::span(
            &self.context,
            &self.operation,
            self.kind.clone(),
            self.start_at,
            duration_ms,
        );
        if let Some(error_event_id) = self.failed {
            capture = capture.failed(error_event_id);
        }
        // A refused span is counted by the driver. A span guard has nobody to
        // return the refusal to.
        let _ = self.driver.capture(capture);
    }
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        self.record();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Clock, Random};
    use crate::Settings;
    use std::fmt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Debug)]
    struct Outer(Inner);
    #[derive(Debug)]
    struct Inner;

    impl fmt::Display for Outer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("the order was not saved")
        }
    }
    impl fmt::Display for Inner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("the database refused the connection")
        }
    }
    impl std::error::Error for Inner {}
    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn an_error_a_host_holds_carries_its_type_and_every_cause() {
        let capture = Capture::from_error(&Outer(Inner), true);
        let payload = capture.item().error.as_ref().expect("an error payload");
        assert!(
            payload.error_type.ends_with("Outer"),
            "{}",
            payload.error_type
        );
        assert_eq!(
            payload.message,
            "the order was not saved: the database refused the connection"
        );
        assert!(payload.handled);
        assert_eq!(payload.mechanism.as_deref(), Some("error"));
    }

    #[test]
    fn a_message_longer_than_the_contract_is_cut_at_a_character_and_not_refused() {
        let long = "é".repeat(MAX_ERROR_MESSAGE);
        let cut = truncated(&long, MAX_ERROR_MESSAGE);
        assert!(cut.len() <= MAX_ERROR_MESSAGE);
        assert!(cut.chars().all(|c| c == 'é'), "no character was split");
        assert_eq!(
            truncated("", 10),
            "unknown",
            "the contract refuses an empty text"
        );
    }

    #[test]
    fn a_panic_becomes_an_unhandled_error_with_its_location() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("the index was out of range");
        let capture = panic_capture(payload.as_ref(), Some(("src/orders.rs", 40)));
        let error = capture.item().error.as_ref().expect("an error payload");
        assert_eq!(error.error_type, "panic");
        assert_eq!(error.message, "the index was out of range");
        assert!(!error.handled);
        assert_eq!(error.mechanism.as_deref(), Some("panic"));
        let frames = error.frames.as_ref().expect("the location");
        assert_eq!(frames[0].file.as_deref(), Some("src/orders.rs"));
        assert_eq!(frames[0].line, Some(40));

        // A panic payload that is not text still makes an occurrence.
        let odd: Box<dyn std::any::Any + Send> = Box::new(7u8);
        let capture = panic_capture(odd.as_ref(), None);
        assert!(capture.item().error.is_some());
    }

    #[test]
    fn a_panic_hook_that_cannot_take_the_buffer_gives_up_and_does_not_wait() {
        let driver = Driver::new(Settings::new("127.0.0.1:1", "key-a"));
        let held = driver.inner.buffer.lock().unwrap();
        // If this waited for the lock, the test would never return.
        driver.capture_without_waiting(Capture::error("panic", "boom", false));
        drop(held);
        assert_eq!(driver.buffered(), 0);

        driver.capture_without_waiting(Capture::error("panic", "boom", false));
        assert_eq!(driver.buffered(), 1);
    }

    fn driver_with_clock() -> (Driver, Arc<Mutex<Duration>>) {
        let base = Instant::now();
        let offset = Arc::new(Mutex::new(Duration::ZERO));
        let read = Arc::clone(&offset);
        let clock: Clock = Arc::new(move || base + *read.lock().unwrap());
        let random: Random = Arc::new(|| 0);
        let mut settings = Settings::new("127.0.0.1:1", "key-a");
        settings.linger = Duration::from_secs(3600);
        (Driver::with_sources(settings, clock, random), offset)
    }

    #[test]
    fn a_span_guard_measures_its_own_duration() {
        let (driver, clock) = driver_with_clock();
        let context = SpanContext::root();
        let span = driver.start_span(&context, "GET /orders/{id}", SpanKind::Server);
        *clock.lock().unwrap() = Duration::from_millis(12);
        span.end();

        let buffer = driver.inner.buffer.lock().unwrap();
        let payload = buffer.items[0].span.as_ref().expect("a span payload");
        assert_eq!(payload.duration_ms, 12);
        assert_eq!(payload.operation, "GET /orders/{id}");
        assert_eq!(buffer.items.len(), 1, "ending a span records it once");
    }

    #[test]
    fn a_span_guard_that_is_dropped_still_records_and_a_failed_one_says_so() {
        use tallyowl_collector_api::types::SpanPayload_status;
        let (driver, clock) = driver_with_clock();
        let context = SpanContext::root();
        {
            let _span = driver.start_span(&context, "charge", SpanKind::Client);
            *clock.lock().unwrap() = Duration::from_millis(5);
            // An early return drops the guard.
        }
        driver
            .start_span(&context.child(), "save", SpanKind::Internal)
            .fail(Some([7; 16]));

        let buffer = driver.inner.buffer.lock().unwrap();
        assert_eq!(buffer.items.len(), 2);
        assert_eq!(buffer.items[0].span.as_ref().unwrap().duration_ms, 5);
        let failed = buffer.items[1].span.as_ref().unwrap();
        assert_eq!(failed.status, SpanPayload_status::Error);
        assert_eq!(failed.error_event_id.as_deref(), Some([7u8; 16].as_slice()));
    }
}
