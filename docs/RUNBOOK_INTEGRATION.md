# Integration runbook

This runbook tells a developer how to connect an application to a TallyOwl
installation. The app driver is the supported path; the browser package rides
the application's own connection.

## 1. Get a key

Ask the operator for a key, or make one:

```sh
tallyowl-head --config <file> provision <project>
```

The key is printed one time. The application reads it from its own secret
store. A key has project scope: the application does not know about
workspaces, and tenancy comes from the key rather than from anything in a
payload.

## 2. The app driver

An app driver sends telemetry from a backend service to a collector. There is
a Go app driver and a Rust app driver. Each app driver has a README with a
complete first program, the failure table, and the defaults:

- [Go app driver](../packages/driver-go/README.md)
- [Rust app driver](../crates/tallyowl-driver-rust/README.md)

A Go service starts the driver one time:

```go
settings := tallyowl.NewSettings(collectorAddress, credential)
settings.OnError = func(err error) { log.Printf("tallyowl: %v", err) }
driver := tallyowl.NewDriver(settings)
go driver.Run(ctx) // Sends in the background. Shuts down when ctx ends.

driver.Capture(tallyowl.Event("checkout-started").
    WithSession(sessionID).
    WithRequest(requestID).
    WithProperty("route", tallyowl.Text("/checkout")))
```

`Capture` only puts an item in a buffer. `Run` sends the buffer. A service
that does not call `Run`, `Flush`, or `Submit` sends nothing until `Shutdown`.

To examine the items before a collector is available, set
`settings.DryRun = os.Stdout`. The driver then opens no connection.

These rules are important:

- **Use the envelope for correlation IDs.** `WithRequest`, `WithSession`, and
  the span helpers set the indexed envelope fields. A property with the name
  `request_id` is a different field, and a filter on that name reads the
  envelope field.
- **An acknowledgement means durable.** The acknowledgement means that the
  durable queue of the installation accepted the batch. Delivery is
  at-least-once with stable IDs. Ingestion is logically idempotent.
- **A failure is a later attempt, not a loss.** The driver keeps a sealed batch
  until the collector acknowledges it. The driver waits between attempts. The
  wait increases to a maximum of 30 seconds and includes a random part.
- **Backpressure is a refusal at the driver.** When the unacknowledged bound
  is full, `Capture` returns an error and the application selects the data to
  discard. TallyOwl does not discard data silently.
- **`Shutdown` returns the number of items that did not go.** Write this
  number to the application log.

### 2.1 The connection to the collector

The app driver selects the connection type from the collector address (D62):

| Address | Connection |
| --- | --- |
| `unix:/path/to/socket` | Plaintext. Nothing crosses a network |
| `127.0.0.1:5100`, `[::1]:5100`, `localhost:5100` | Plaintext |
| Any other address | TLS. The driver checks the collector certificate against the authorities of the operating system |

The project key identifies the application. The certificate identifies the
collector. TLS also keeps the key off the network.

- **A private authority.** If the operator made the collector certificate from
  a private authority, give the driver that authority. In Go, set
  `settings.Transport = tallyowl.Transport{TLS: &tls.Config{RootCAs: pool}}`.
  In Rust, use `Transport` with `with_roots_pem`. Each driver README gives the
  details.
- **A different name.** If the address is not a name on the certificate, set
  the server name in the same TLS setting.
- **Plaintext on a network.** Set `AllowPlaintext` in Go or
  `allowing_plaintext` in Rust. Do this only on a network that something else
  protects. The collector must also allow it.
- **A failed handshake** sends nothing. The driver keeps the batch, uses no
  attempt, and reports a message that says what to change.

A unix socket works on Linux and macOS. The Rust app driver does not support a
unix socket on Windows in this release.

## 3. The browser

The browser package uses the existing same-origin CSIL connection of the
application. It does not open a TallyOwl connection, and it does not know a
TallyOwl address. The [browser package README](../packages/browser/README.md)
gives the first program, the unload route that the host must supply, and the
defaults.

The application receives each browser item on its own connection and gives
the item to the app driver:

```go
capture, err := tallyowl.FromBrowser(item)
if err == nil {
    err = driver.Capture(capture)
}
```

`FromBrowser` accepts all item kinds. A browser is not a trusted source, thus
the driver discards the tenancy fields, gives each property the origin
`client`, and replaces the event ID with an ID that is specific to the
browser session.

Sessions come from `startSession`. TallyOwl drops an event that has an
invalid session and counts the drop.

## 4. Compatibility receivers

A collector can scrape Prometheus and OpenMetrics endpoints. A collector can
also receive OpenTelemetry metrics and traces. Both functions are off until an
operator enables them. The collector normalizes the data at the edge. Each
later hop uses native CSIL.

Use this path to monitor an application that already publishes metrics. You do
not change the application.

### 4.1 Where the data goes

The compatibility data goes to the project of `collector.apiKey`. A scrape
target and an OpenTelemetry exporter do not send a TallyOwl key. The collector
sends its own key. Thus, when you set `collector.apiKey`, you select the
project.

To send compatibility data to a different project, run a second collector with
a key for that project.

### 4.2 Scrape a metrics endpoint

1. Add each target to the collector configuration.

   ```yaml
   compatibility:
     prometheus:
       targets:
         - http://10.0.4.12:9100/metrics
         - http://checkout.internal:8080/metrics
         - http://[fd00::21]:9100/metrics
       interval: 60s
       timeout: 5s
       maxBodyBytes: 16MiB
       workers: 8
   ```

2. Run `tallyowl-collector config check`. The check refuses a target that the
   collector cannot use, and it names the target.
3. Start the collector again.

These rules apply:

- Write each target as `http://host:port/metrics`. The scrape uses HTTP only.
  The configuration check refuses a target that starts with `https://`. Keep
  each target in a network that you control.
- The list of targets is static. The collector does not find targets
  automatically.
- A target cannot use a bearer token, a password, or a client certificate.
- `timeout` is the limit for one full scrape. The limit includes the
  connection, the request, and all of the response.
- `maxBodyBytes` is the limit for one response. If a response is larger, the
  scrape fails and the collector keeps none of it.
- `workers` is the number of targets that the collector reads at one time. A
  slow target uses one worker for a maximum of `timeout`. The other targets
  continue.
- The collector does not scrape all targets at the same moment. It spreads the
  targets across one `interval`.
- One scrape can be larger than `collector.maxBatchBytes`. The collector
  divides the scrape into as many batches as are necessary.

The collector adds an `instance` label to each scraped series. The value is the
host and the port of the target, for example `10.0.4.12:9100`. If the target
published an `instance` label, the collector keeps that value as
`exported_instance`. Group by `instance` to see each copy of an application.

### 4.3 Find a target that is down

The collector records two gauges for each target on each scrape. It records
them also when the scrape fails.

| Metric | Value |
| --- | --- |
| `up` | 1 when the last scrape was successful. 0 when it was not. |
| `scrape_duration_seconds` | The time that the last scrape took. |

Each gauge has the `instance` label. To find a target that is down, query `up`
and filter for the value 0. To get a notification, make an alert rule for the
same condition.

The collector reports the cause in two more places:

- The log has the line "A scrape target could not be read." The line gives the
  `target` and the `reason`.
- The collector metrics endpoint has these counters.

| Counter | Meaning |
| --- | --- |
| `tallyowl_scrape_failures_total` | Scrapes that did not get a response. |
| `tallyowl_scrape_overruns_total` | Scrapes that were not complete when the next scrape was due. |
| `tallyowl_scrape_line_faults_total` | Lines that the collector could not read. The other lines of that target arrived. |
| `tallyowl_scrape_panics_total` | Scrapes that stopped because of a defect in TallyOwl. Report each one. |
| `tallyowl_compat_items_rejected_total` | Items that the collector offered and did not keep. The log line gives the first reason. |

### 4.4 Receive OpenTelemetry metrics and traces

1. Enable the receiver on the collector.

   ```yaml
   compatibility:
     openTelemetry:
       enabled: true
       listen: 0.0.0.0:4318
   tls:
     certificateDirectories:
       - /etc/tallyowl/collector-tls
   ```

   On a network address the receiver uses TLS, with the certificate of
   collector intake. On a loopback address it can use plaintext.

2. Point the exporter of the application at the collector.

   ```sh
   export OTEL_EXPORTER_OTLP_ENDPOINT=https://collector.internal:4318
   export OTEL_EXPORTER_OTLP_CERTIFICATE=/path/to/authority.crt   # a private authority only
   export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
   export OTEL_EXPORTER_OTLP_COMPRESSION=none
   export OTEL_LOGS_EXPORTER=none
   ```

   For an OpenTelemetry Collector, use the `otlphttp` exporter.

   ```yaml
   exporters:
     otlphttp:
       endpoint: https://collector.internal:4318
       compression: none
       tls:
         ca_file: /path/to/authority.crt   # a private authority only
   ```

**The receiver has no authentication.** TLS protects the data in transit. It
does not identify the client. Each client that can connect to the address can
write into the project of `collector.apiKey`. Permit only your
applications to connect. Use a network policy or a firewall rule. Do not make
the address available on the internet.

The receiver accepts one encoding: protocol buffers over HTTP, not compressed,
with a `Content-Length` header. The receiver does not acknowledge a push that
it cannot read. The response tells you what to change.

| Response | Cause | Correction |
| --- | --- | --- |
| 415 | The body is compressed. | Set `compression: none`. |
| 415 | The body is JSON. | Set the protocol to `http/protobuf`. |
| 411 | The push has no `Content-Length`, or it uses chunked transfer. | Send a `Content-Length`. |
| 413 | The push is larger than 4 MiB. | Send smaller batches. |
| 431 | The request headers are larger than 16 KiB. | Remove the headers that are not necessary. |
| 400 | The body is not an OpenTelemetry request message. | Examine the exporter configuration. |
| 501 | The push contains log records. | TallyOwl does not collect log records. Send them to a log system. |
| 503 | The durable store did not accept the push, or the receiver has too many connections. | The exporter sends the push again. Examine Corndogs. |

`tallyowl_compat_unreadable_total` counts these responses by `reason`.

A response of 200 with an empty body tells the exporter that all the data
arrived. A response of 200 with a partial-success message tells the exporter
how many items did not arrive, and gives the first reason. Look for that
message in the log of the exporter.

One push can be larger than `collector.maxBatchBytes`. The collector divides
the push into as many batches as are necessary.

The collector keeps `service.name` in two places: as the service name of each
item, and as a label on each metric series. Thus, two services that send the
same metric name stay separate series.

## 5. Receiving alerts

Two channels deliver an alert notification:

- **a webhook**, signed over the timestamp and the body with a keyed hash.
  Use an `https` address. The head refuses a private, loopback, or link-local
  address unless its host is in `alerts.allowedPrivateTargets`. Verify the
  signature, and refuse a timestamp that is more than five minutes from your
  clock. [ALERTS.md section 6.1](ALERTS.md) gives the calculation.
- **the native callback**: declare `TallyOwlAlertReceiver` from
  `csil/tallyowl-ingest.csil` on a service that already speaks CSIL. Give the
  target a `secret_ref`, and give the receiver the same secret. The head
  signs each callback. The receiver verifies it before it reads the body:

  ```go
  func (r *Receiver) Notify(ctx context.Context, req ingest.AlertNotifyRequest) (ingest.AlertNotifyResponse, error) {
      body, err := tallyowl.VerifyAlertCallback(r.secret, req, time.Now())
      if err != nil {
          return ingest.AlertNotifyResponse{}, err // Do not read the body.
      }
      // body is the notification as JSON.
      return ingest.AlertNotifyResponse{}, nil
  }
  ```

  If your router can return a typed `ServiceError`, return the code
  `unauthenticated`, not retryable. The head then does not send that
  notification again.

  In Rust, call `tallyowl_driver_rust::verify_alert_callback(secret,
  &request, now_ms)`. The body is the same JSON that a webhook receives. The
  head uses plaintext for a loopback or `unix:` receiver, and TLS for any
  other. It checks the receiver's certificate against the trusted authorities
  of the operating system. TLS proves the receiver to the head. The signature
  proves the head to the receiver.

A failed delivery never changes the alert state, and delivery retries with
capped jitter.

## 6. Querying

Control operations need an operator session, not an application key. The
dashboard signs in through LinkKeys; an installation without LinkKeys issues
a session with `tallyowl-head session create`. A query travels as the
algebra QUERY.md defines, and the driver examples under
`crates/tallyowl-driver-rust/examples/` show the shapes.
