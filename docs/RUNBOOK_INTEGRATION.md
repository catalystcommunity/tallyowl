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

The Go app driver is the maintained backend driver:

```go
settings := tallyowl.NewSettings(collectorAddress, credential)
driver := tallyowl.NewDriver(settings)
defer driver.Shutdown()

driver.Capture(tallyowl.Event("checkout-started").
    WithSession(sessionID).
    WithRequest(requestID).
    WithProperty("route", tallyowl.Text("/checkout")))
```

Three rules that matter:

- **use the envelope for correlation IDs.** `WithRequest`, `WithSession`, and
  the trace helpers set the envelope's own columns, which the exact index
  answers. A property named `request_id` is a different thing from the
  request column, and a filter on that name reads the column.
- **a receipt means durable.** The driver's acknowledgement means the
  installation's durable queue accepted the batch. Delivery is at-least-once
  with stable IDs; ingestion is logically idempotent.
- **backpressure is a refusal at the driver.** When the unacknowledged bound
  fills, `Capture` returns an error and the application chooses what to
  drop. TallyOwl never drops quietly.

## 3. The browser

The browser package uses the application's existing same-origin CSIL
connection. It never opens a TallyOwl connection and never learns a TallyOwl
address. Sessions come from `startSession`; an event with an invalid session
is dropped and counted.

## 4. Compatibility receivers

A collector can receive OpenTelemetry metrics and traces, and can scrape
Prometheus endpoints. Both are off until the operator turns them on: no port
opens because the binary contains the feature. The collector normalizes at
the edge, and every later hop is native CSIL.

## 5. Receiving alerts

Two channels deliver an alert notification:

- **a webhook**, signed over the timestamp and the body with a keyed hash.
  The address must be `https` or an address TallyOwl can reach in the clear
  is refused. Verify the signature and the timestamp.
- **the native callback**: declare `TallyOwlAlertReceiver.notify` on a
  service that already speaks CSIL to the installation. The connection's
  mutual TLS is the authentication, and the body is the same body a webhook
  receives.

A failed delivery never changes the alert state, and delivery retries with
capped jitter.

## 6. Querying

Control operations need an operator session, not an application key. The
dashboard signs in through LinkKeys; an installation without LinkKeys issues
a session with `tallyowl-head session create`. A query travels as the
algebra QUERY.md defines, and the driver examples under
`crates/tallyowl-driver-rust/examples/` show the shapes.
