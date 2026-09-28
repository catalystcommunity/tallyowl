//! Send one event, one error, one span, and one metric.
//!
//! This is the program to copy into an application. It needs two values:
//!
//! ```text
//! TALLYOWL_COLLECTOR=127.0.0.1:5100 TALLYOWL_KEY=tow_... \
//!     cargo run -p tallyowl-driver-rust --example quickstart
//! ```
//!
//! With no collector, set `TALLYOWL_DRY_RUN=1`. The driver then opens no socket
//! and writes each item as one line, so you can check the instrumentation:
//!
//! ```text
//! TALLYOWL_DRY_RUN=1 cargo run -p tallyowl-driver-rust --example quickstart
//! ```

use tallyowl_driver_rust::metrics::{labels, Meter};
use tallyowl_driver_rust::{Capture, Driver, Settings, SpanContext, SpanKind};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::var("TALLYOWL_COLLECTOR").unwrap_or_else(|_| "127.0.0.1:5100".into());
    let key = std::env::var("TALLYOWL_KEY").unwrap_or_default();

    let mut settings = Settings::new(address, key)
        .with_property("service", "quickstart")
        // Every failure the driver cannot return to a caller arrives here.
        .with_on_error(|error| eprintln!("tallyowl: {}", error.message));
    if std::env::var("TALLYOWL_DRY_RUN").is_ok() {
        settings = settings.with_dry_run(std::io::stdout());
    }

    let driver = Driver::new(settings);
    // `capture` only buffers. The flusher is what sends.
    let flusher = driver.spawn_flusher()?;

    driver.capture(Capture::event("checkout-started"))?;

    let failure = std::fs::read("/a/file/that/is/not/there").unwrap_err();
    driver.capture(Capture::from_error(&failure, true))?;

    let span = driver.start_span(&SpanContext::root(), "GET /orders/{id}", SpanKind::Server);
    // The work the span measures goes here.
    span.end();

    let meter = Meter::new();
    meter.counter("orders_total", None, Some("Orders this service took."));
    meter.increment("orders_total", &labels(&[("plan", "pro")]))?;
    // Call this on a period of your own, for example each 10 seconds.
    driver.publish_metrics(&meter)?;

    // Stop the flusher before the process exits. It sends what is left, and it
    // returns how many items did not reach the collector.
    let unsent = flusher.stop();
    let stats = driver.stats();
    println!(
        "captured {}, accepted {}, lost {}, unsent {unsent}",
        stats.captured, stats.accepted, stats.lost
    );
    if unsent > 0 {
        if let Some(error) = stats.last_error {
            println!("The last failure was: {}", error.message);
        }
    }
    Ok(())
}
