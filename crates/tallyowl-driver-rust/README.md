# The Rust app driver

The Rust app driver sends telemetry from a Rust service to a TallyOwl
collector. It sends events, errors, spans, and metrics.

You need two values from the person who operates TallyOwl:

- the address of a collector, for example `127.0.0.1:5100`;
- a key for your application. A key starts with `tow_`.

## 1. Add the dependency

Add this line to `Cargo.toml`. Use the newest release tag.

```toml
tallyowl-driver-rust = { git = "https://github.com/CatalystCommunity/tallyowl", tag = "v0.2.1" }
```

This one line is sufficient. The app driver exports each type that you use.

## 2. Send the first items

```rust
use tallyowl_driver_rust::metrics::{labels, Meter};
use tallyowl_driver_rust::{Capture, Driver, Settings, SpanContext, SpanKind};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let settings = Settings::new("127.0.0.1:5100", std::env::var("TALLYOWL_KEY")?)
        .with_property("service", "checkout")
        .with_on_error(|error| eprintln!("tallyowl: {}", error.message));
    let driver = Driver::new(settings);
    let flusher = driver.spawn_flusher()?;

    driver.capture(Capture::event("checkout-started"))?;

    if let Err(failure) = std::fs::read("/a/file/that/is/not/there") {
        driver.capture(Capture::from_error(&failure, true))?;
    }

    let span = driver.start_span(&SpanContext::root(), "GET /orders/{id}", SpanKind::Server);
    // Do the work that the span measures.
    span.end();

    let meter = Meter::new();
    meter.counter("orders_total", None, Some("Orders that this service accepted."));
    meter.increment("orders_total", &labels(&[("plan", "pro")]))?;
    driver.publish_metrics(&meter)?;

    let unsent = flusher.stop();
    println!("{unsent} items did not reach the collector.");
    Ok(())
}
```

The same program is in `examples/quickstart.rs`. Run it with this command:

```sh
TALLYOWL_COLLECTOR=127.0.0.1:5100 TALLYOWL_KEY=tow_... \
    cargo run -p tallyowl-driver-rust --example quickstart
```

## 3. Start the flusher

`capture` puts an item in a buffer. `capture` does not send the item.

The flusher is one thread that sends the buffer. Start the flusher one time,
immediately after you make the driver:

```rust
let flusher = driver.spawn_flusher()?;
```

If you do not start the flusher, no item leaves the process. The buffer then
becomes full, and `capture` returns an error.

Stop the flusher before the process stops:

```rust
let unsent = flusher.stop();
```

`stop` sends the remaining items. It returns the number of items that did not
reach the collector. It returns in 2 seconds or less.

A `Driver` is a handle. Clone the driver and give one clone to each thread.

If your service has a scheduler, you can use it as an alternative to the
flusher. Call `driver.tick()` from the scheduler each 50 milliseconds. At
shutdown, call `driver.shutdown()`.

## 4. Connect securely

The app driver selects the connection type from the address:

| Address | Connection |
| --- | --- |
| `unix:/path/to/socket` | Plaintext. Nothing crosses a network |
| `127.0.0.1:5100`, `[::1]:5100`, `localhost:5100` | Plaintext. Nothing crosses a network |
| Any other address | TLS. The app driver checks the collector certificate against the trusted authorities of the operating system |

The project key identifies your application. The TLS certificate identifies
the collector. TLS also keeps the key off the network.

If the collector certificate comes from a private authority, give the app
driver the certificate of that authority:

```rust
let transport = Transport::default().with_roots_pem(&std::fs::read("authority.pem")?)?;
let settings = Settings::new(address, key).with_transport(transport);
```

If the address is not the name on the certificate, use
`Transport::with_server_name`. To send plaintext to a network address, use
`Transport::allowing_plaintext`. Do this only on a network that something
else protects.

A unix socket works on Unix only in this release.

## 5. Do a test without a collector

Use a dry run to see the items that your instrumentation makes. In a dry run,
the app driver opens no connection. It writes one line for each item.

```rust
let settings = Settings::new("unused", "unused").with_dry_run(std::io::stdout());
```

```sh
TALLYOWL_DRY_RUN=1 cargo run -p tallyowl-driver-rust --example quickstart
```

## 6. What you see when something is wrong

The app driver tells you about a failure in three places:

- the `on_error` function that you give to `Settings`;
- the error that `flush`, `submit`, or `drain` returns;
- `driver.stats()`, which includes the last error and the item counts.

| Condition | What you see | What to do |
| --- | --- | --- |
| The key is incorrect or revoked. | `on_error` receives an `Unauthenticated` error. The message tells you to examine the key. `stats().lost` increases. | Give the correct key to `Settings`. Ask the operator if the key was revoked. |
| The collector certificate is from an authority that the host does not trust. | `on_error` receives a `FailedPrecondition` error. The message says that no trusted authority signed the certificate, and tells you to use `Transport::with_roots_pem`. `stats().unacknowledged` increases. `stats().lost` does not change. | Give the certificate of the authority to `Transport::with_roots_pem`. |
| The collector does not use TLS on a network address. | `on_error` receives an error that says that the collector did not answer with TLS, or that it closed the connection during the TLS handshake. | Ask the operator to give the collector certificates. |
| The collector is not reachable. | `on_error` receives an `Unavailable` error that contains the address. `stats().unacknowledged` increases. `stats().lost` does not change. | Make sure that the address is correct and that the collector operates. The app driver keeps the items and tries again. |
| The collector accepts the connection and does not answer. | Each call returns an `Unavailable` error after 10 seconds. | Tell the operator. The app driver keeps the items and tries again. |
| The buffer is full. | `capture` returns a `ResourceExhausted` error. The message says that the event was not recorded. `stats().refused` increases. | Find why items do not leave. Read `stats().last_error`. If the collector is slow, send fewer items. |
| One item is larger than one frame. | `capture` returns an error that gives the size and the limit. | Send fewer or smaller properties on that item. |
| The flusher was not started. | `stats().accepted` stays at 0 and `stats().buffered` increases. | Start the flusher. See section 3. |

The app driver never reports success for an item that it discarded.

## 7. How the app driver sends

1. The app driver seals a batch from the buffer. One batch has a maximum of
   256 items and 512 KiB. Each batch has an ID.
2. The app driver sends the batch. It keeps the batch until the collector
   acknowledges it.
3. If an attempt fails, the app driver keeps the batch and its ID. It waits
   before the next attempt. The first wait is between 50 and 100 milliseconds.
   Each subsequent wait is two times longer, to a maximum of 30 seconds. A
   random part in each wait prevents many applications from trying again at
   the same moment.
4. During the wait, `flush`, `submit`, and `drain` return immediately. The
   error contains the wait. `retry_after(&error)` reads it. The app driver
   does not sleep on your thread.
5. The buffer and the sealed batches together have a limit of 8 MiB. At the
   limit, `capture` refuses the item. Thus an outage cannot increase the
   memory that your service uses.

The app driver discards a batch only in these two conditions. In each
condition, it adds the items to `stats().lost` and calls `on_error`.

- The collector refused the batch, and the refusal says that a new attempt
  cannot help. An incorrect key is an example.
- The batch left five times, and no answer came back.

An attempt that cannot connect does not count as one of the five. An answer of
"try again" from the collector does not count.

Delivery is at-least-once. A batch can arrive two times after a lost answer.
TallyOwl storage uses the batch ID to keep one copy.

## 8. Helpers

| Helper | Use |
| --- | --- |
| `Capture::from_error(&error, handled)` | Records an error value. The message includes each cause. |
| `capture_panics(&driver)` | Records each panic as an unhandled error. Call it one time. |
| `driver.start_span(&context, operation, kind)` | Starts a span. The span records its duration when it ends. |
| `SpanContext::continue_or_start(header)` | Continues a trace from a `traceparent` header. |
| `context.traceparent()` | Gives the `traceparent` header for a service that you call. |
| `Session::start()` | Makes a session ID. |
| `Meter` | Adds counters, gauges, and histograms in the process. `publish_metrics` sends one point for each series. |

Use the route template for a span name, for example `GET /orders/{id}`. Do not
use the requested address, because it contains IDs and a query string.

The app driver records only the values that you give it. Do not put a
password, a token, a request body, or personal data in a property or in an
error message.

## 9. Defaults

You can change each default in `Settings`.

| Setting | Field | Default |
| --- | --- | --- |
| Items in one batch | `max_items` | 256 |
| Bytes in one batch | `max_batch_bytes` | 512 KiB |
| Time before a batch seals | `linger` | 100 ms |
| Largest frame | `max_frame_bytes` | 1 MiB |
| Limit for the buffer and the sealed batches | `max_unacknowledged_bytes` | 8 MiB |
| Batches in flight at one time | `max_in_flight_batches` | 4 |
| Deadline for one send or one answer | `call_timeout` | 10 s |
| First wait after a failed attempt | `retry_backoff_min` | 100 ms |
| Longest wait between attempts | `retry_backoff_max` | 30 s |
| Attempts with no answer before a batch is discarded | `max_batch_attempts` | 5 |
| Deadline for shutdown | `shutdown_flush_deadline` | 2 s |
| How the app driver connects | `transport` | TLS with the authorities of the operating system for a network address. See section 4 |

A conversion and an unhandled error seal the batch immediately.
