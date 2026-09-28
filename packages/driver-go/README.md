# TallyOwl Go app driver

The Go app driver sends telemetry from a Go service to a TallyOwl collector.
It sends events, errors, spans, and metrics.

You need two values. The operator of your TallyOwl installation gives you
both.

- The collector address, for example `collector.internal:5100`.
- A project key.

## 1. Install

```sh
go get github.com/CatalystCommunity/tallyowl/packages/driver-go
```

## 2. Send the first items

```go
package main

import (
	"context"
	"errors"
	"log"
	"os"
	"os/signal"

	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
)

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()

	settings := tallyowl.NewSettings(os.Getenv("TALLYOWL_COLLECTOR"), os.Getenv("TALLYOWL_KEY"))
	settings.OnError = func(err error) { log.Printf("tallyowl: %v", err) }
	driver := tallyowl.NewDriver(settings)
	go driver.Run(ctx) // Sends in the background. Stops when ctx ends.

	_ = driver.Capture(tallyowl.Event("signed-up"))                           // an event
	_ = driver.Capture(tallyowl.ErrorFrom(errors.New("card declined"), true)) // an error

	_, span := driver.StartSpan(ctx, "charge-card", "client") // a span
	_ = span.End()

	meter := tallyowl.NewMeter() // a metric
	meter.Counter("orders_total", "", "Orders that were placed.")
	_ = meter.Increment("orders_total", tallyowl.Labels{"plan": "pro"})
	_, _ = driver.PublishMetrics(meter)

	<-ctx.Done()
}
```

`Capture` only puts an item in a buffer. It does not send. One of these calls
sends:

- `go driver.Run(ctx)` sends in the background. Use it in a service.
- `driver.Flush()` sends and waits for the acknowledgement. Use it in a
  short program.
- `driver.Submit()` sends and does not wait. Call `driver.ShouldFlush()` from
  your own scheduler, and call `Submit` when the result is true.

If the host calls none of them, the driver sends nothing until `Shutdown`.

Call `PublishMetrics` on your own timer, for example one time each minute.
The driver does not start a timer for metrics.

## 3. Connect securely

The driver selects the connection type from the address:

| Address | Connection |
| --- | --- |
| `unix:/path/to/socket` | Plaintext. Nothing crosses a network |
| `127.0.0.1:5100`, `[::1]:5100`, `localhost:5100` | Plaintext. Nothing crosses a network |
| Any other address | TLS. The driver checks the collector certificate against the trusted authorities of the operating system |

The project key identifies your application. The TLS certificate identifies
the collector. TLS also keeps the key off the network.

If the collector certificate comes from a private authority, give the driver
the certificate of that authority:

```go
roots := x509.NewCertPool()
roots.AppendCertsFromPEM(authorityPEM)
settings.Transport = tallyowl.Transport{TLS: &tls.Config{RootCAs: roots}}
```

If the address is not the name on the certificate, set
`TLS.ServerName`. To send plaintext to a network address, set
`Transport.AllowPlaintext`. Do this only on a network that something else
protects.

## 4. Make sure that it works before you have a collector

Set `DryRun`. The driver then writes each item as one line of JSON and opens
no connection.

```go
settings.DryRun = os.Stdout
```

## 5. Record HTTP requests

```go
mux := http.NewServeMux()
mux.HandleFunc("GET /orders/{id}", showOrder)
http.ListenAndServe(":8080", driver.Middleware(mux))
```

The middleware records one span for each request.

- The span continues the trace in the `traceparent` request header.
- The operation is the method and the route pattern, for example
  `GET /orders/{id}`. The driver does not record the path.
- The driver does not record a request body, a query string, or a header
  value.
- A panic becomes an unhandled error. The panic then continues to your own
  recovery code.

In a handler, `driver.StartSpan(r.Context(), ...)` makes a child span.
`tallyowl.Event(...).InSpan(span.Context())` puts an item in the trace.

## 6. Send browser items

A browser does not connect to TallyOwl. The browser package sends its items
to your application on your own connection. Your application gives each item
to the driver:

```go
capture, err := tallyowl.FromBrowser(item)
if err == nil {
	err = driver.Capture(capture)
}
```

`FromBrowser` accepts all item kinds. A browser is not a trusted source, thus
the driver changes the item:

- The driver discards the tenancy fields and the receive time.
- The driver gives each property the origin `client`.
- The driver replaces the event ID with an ID that is specific to the
  browser session. One browser cannot hide the events of a different
  browser. A retry from the same browser stays one event.

## 7. What you see when there is a problem

| Problem | What you see | What to do |
| --- | --- | --- |
| The key is incorrect. | `OnError` reports that the collector refused the items, and tells you to check the credential. `Stats().Lost` increases. | Get a correct key from the operator. |
| The collector certificate is from an authority that the host does not trust. | `OnError` reports that no trusted authority signed the certificate. `Stats().Unacknowledged` increases. `Stats().Lost` does not increase. | Put the certificate of the authority in `Transport.TLS.RootCAs`. |
| The collector does not use TLS on a network address. | `OnError` reports that the collector closed the connection during the TLS handshake. | Ask the operator to give the collector certificates. |
| The collector is not available. | `OnError` reports the address. `Flush` and `Submit` return an error. `Stats().Unacknowledged` increases. `Stats().Lost` does not increase. | No action is necessary. The driver keeps the data and sends it again. |
| The collector is not available for a long time. | `Capture` returns `ErrBackpressure`. `Stats().Refused` increases. | Decide which items your application discards. The driver does not discard an item silently. |
| One item is too large for a frame. | `Capture` returns an error that gives the two sizes. | Record a smaller item, or increase `MaxFrameBytes`. |
| The collector rejects some items. | `OnError` reports the count and the first reason. The `Receipt` lists each rejected item. | Correct the items that the reason identifies. |
| The process stops. | `Shutdown` returns the number of items that did not go. | Write this number to your log. |

`driver.Stats()` gives these counts at any time: captured, refused, accepted,
rejected, lost, buffered, and unacknowledged. It also gives the last error.

## 8. What the driver guarantees

- The driver keeps a sealed batch until the collector acknowledges it. A
  failure causes a later attempt. A failure does not cause a loss.
- A batch keeps the same ID for each attempt. TallyOwl stores one copy.
- An acknowledgement means that the installation accepted the batch durably.
- The wait between failed attempts starts at 100 ms. It doubles to a maximum
  of 30 s. It includes a random part. `Flush` and `Submit` return a
  `RetryLaterError` immediately during the wait. The driver does not block
  your code during the wait.
- Each call to the collector has a time limit. `Shutdown` returns before its
  deadline for all collector conditions.
- The memory that the driver uses has a limit. The buffer and the batches
  that are not acknowledged use `MaxUnacknowledgedBytes` together.
- The driver gives up on a batch in two conditions only. The collector
  refuses the batch permanently. Or, the driver sent the batch
  `MaxBatchAttempts` times and the connection broke each time before the
  answer came. The driver counts each such item in `Stats().Lost` and reports
  it to `OnError`.

## 9. Defaults

| Setting | Default | Function |
| --- | --- | --- |
| `MaxItems` | 256 | The maximum number of items in one batch. |
| `MaxBatchBytes` | 512 KiB | The maximum size of the items in one batch. |
| `Linger` | 100 ms | The age at which a buffer is ready to send. |
| `MaxFrameBytes` | 1 MiB | The maximum size of one frame. |
| `MaxUnacknowledgedBytes` | 8 MiB | The maximum data that the driver holds. |
| `CallTimeout` | 10 s | The time limit for one call to the collector. |
| `RetryBackoffMin` | 100 ms | The first wait after a failure. |
| `RetryBackoffMax` | 30 s | The longest wait after a failure. |
| `MaxBatchAttempts` | 5 | The number of broken attempts before the driver gives up on a batch. |
| `MaxInFlightBatches` | 4 | The number of batches that `Submit` sends before it waits for an acknowledgement. |
| `ShutdownFlushDeadline` | 2 s | The time limit for `Shutdown`. |
| `Transport` | TLS with the system authorities for a network address | How the driver connects. See section 3. |

Use `WithRequest`, `WithSession`, and the span helpers for correlation IDs.
These helpers set the indexed envelope fields. A property with the name
`request_id` is a different field.
