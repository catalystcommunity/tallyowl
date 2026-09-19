// Package api is the seedstore backend.
//
// It is the only surface that holds a TallyOwl credential. Every client surface
// sends telemetry through it, which is the central integration rule and the
// thing the test bed exists to prove at every client type. See
// docs/TESTBED.md section 2.
//
// The backend does two jobs:
//
//   - it serves `TallyOwlIngest` on its own connection, so the browser package
//     reaches TallyOwl without opening one or naming a TallyOwl address;
//   - it forwards what it receives with the Go app driver, which is the only
//     component here that knows a collector address.
package api

import (
	"fmt"
	"net"
	"sync"

	collector "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// Backend is one instrumented application.
type Backend struct {
	driver   *tallyowl.Driver
	listener net.Listener

	mu          sync.Mutex
	connections []net.Conn
	received    int
	failures    []string
}

// New builds a backend that forwards to one collector with one credential.
func New(collectorAddress, credential, serviceName string) *Backend {
	settings := tallyowl.NewSettings(collectorAddress, credential).
		WithProperty("service", serviceName)
	// The test bed decides when a batch goes, so a linger would only add
	// waiting to every case.
	settings.Linger = 0
	return &Backend{driver: tallyowl.NewDriver(settings)}
}

// Driver is the app driver this backend forwards with. A surface that produces
// telemetry from inside the backend uses it directly.
func (b *Backend) Driver() *tallyowl.Driver { return b.driver }

// Listen starts serving `TallyOwlIngest` on a loopback port.
//
// The address returned is the host application's own address. A browser reaches
// this, never a TallyOwl domain.
func (b *Backend) Listen() (string, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return "", err
	}
	b.listener = listener
	go b.accept()
	return listener.Addr().String(), nil
}

func (b *Backend) accept() {
	for {
		conn, err := b.listener.Accept()
		if err != nil {
			return
		}
		b.mu.Lock()
		b.connections = append(b.connections, conn)
		b.mu.Unlock()
		go b.serve(conn)
	}
}

func (b *Backend) serve(conn net.Conn) {
	defer conn.Close()
	carrier, err := transport.NewStreamCarrierWithMaxFrame(conn, 16*1024*1024)
	if err != nil {
		return
	}
	server := transport.NewRpcServer(carrier)
	for {
		served, err := server.ServeOne(b.handle)
		if err != nil || !served {
			return
		}
	}
}

// handle answers the browser package's operations.
//
// `capture` is best effort: it returns when the backend accepts the frame and
// makes no durability claim. `capture-critical` waits for the collector's
// durability boundary and reports what it actually reached. See D5.
func (b *Backend) handle(request *transport.RpcRequest) transport.HandlerOutcome {
	switch request.Op {
	case "capture":
		decoded, err := ingest.DecodeCaptureRequest(request.Payload)
		if err != nil {
			return transport.Transport(transport.StatusMalformedEnvelope, err.Error())
		}
		accepted, failure := b.forward(decoded.Items, false)
		if failure != nil {
			return b.serviceError("unavailable", failure.Error(), true)
		}
		return transport.Reply("CaptureResponse",
			ingest.EncodeCaptureResponse(ingest.CaptureResponse{Accepted: accepted}))

	case "capture-critical":
		decoded, err := ingest.DecodeCaptureCriticalRequest(request.Payload)
		if err != nil {
			return transport.Transport(transport.StatusMalformedEnvelope, err.Error())
		}
		accepted, failure := b.forward(decoded.Items, true)
		if failure != nil {
			return b.serviceError("unavailable", failure.Error(), true)
		}
		return transport.Reply("CaptureCriticalResponse",
			ingest.EncodeCaptureCriticalResponse(ingest.CaptureCriticalResponse{
				Accepted: accepted,
				// True only because the flush below returned, and a flush
				// returns only after Corndogs durably accepted the batch.
				Durable: true,
			}))

	case "policy-version":
		return transport.Reply("PolicyVersionResponse",
			ingest.EncodePolicyVersionResponse(ingest.PolicyVersionResponse{
				PolicyVersion: 1,
				SamplingRate:  1.0,
				EnabledKinds:  []ingest.TelemetryKind{"event", "page-view", "conversion", "error"},
			}))
	}
	return transport.Transport(transport.StatusUnknownServiceOrOp, "no such operation")
}

// forward hands browser items to the app driver.
//
// The backend performs only cheap validation and hands the value on, which is
// what DELIVERY.md section 3 asks of an application's generated route.
func (b *Backend) forward(items []ingest.TelemetryItem, critical bool) (uint64, error) {
	accepted := uint64(0)
	for _, item := range items {
		capture, err := fromIngest(item)
		if err != nil {
			b.mu.Lock()
			b.failures = append(b.failures, err.Error())
			b.mu.Unlock()
			continue
		}
		if critical {
			capture = capture.Critical()
		}
		if err := b.driver.Capture(capture); err != nil {
			return accepted, err
		}
		accepted++
	}
	b.mu.Lock()
	b.received += int(accepted)
	b.mu.Unlock()

	if critical {
		if _, err := b.driver.Flush(); err != nil {
			return accepted, err
		}
	}
	return accepted, nil
}

func (b *Backend) serviceError(code, message string, retryable bool) transport.HandlerOutcome {
	return transport.Reply("ServiceError", ingest.EncodeServiceError(ingest.ServiceError{
		Code:      ingest.ErrorCode(code),
		Message:   message,
		Retryable: retryable,
	}))
}

// Flush sends everything the backend is holding and waits for the durable
// acknowledgement.
func (b *Backend) Flush() (*tallyowl.Receipt, error) { return b.driver.Flush() }

// recordFailure notes a failure that has nowhere else to go. The unload flush
// uses it: the tab is already going away, so nothing can be reported to it, and
// a failure nobody recorded would look exactly like a flush that worked.
func (b *Backend) recordFailure(message string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	b.failures = append(b.failures, message)
}

// Received is how many browser items this backend accepted.
func (b *Backend) Received() int {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.received
}

// Failures are the browser items the backend could not read. A rising count is
// a contract problem, not a load problem.
func (b *Backend) Failures() []string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return append([]string(nil), b.failures...)
}

// Close stops serving and drains the driver.
func (b *Backend) Close() int {
	if b.listener != nil {
		_ = b.listener.Close()
	}
	b.mu.Lock()
	for _, conn := range b.connections {
		_ = conn.Close()
	}
	b.mu.Unlock()
	return b.driver.Shutdown()
}

// fromIngest converts a browser item into the shape the collector takes.
//
// The two packages hold their own copies of the same shared types, so this is a
// field-by-field copy rather than a cast. The golden vectors prove the copies
// encode identically; this is where a drift would show up as a compile failure
// instead of as a wrong byte.
func fromIngest(item ingest.TelemetryItem) (*tallyowl.Capture, error) {
	envelope := item.Envelope
	var capture *tallyowl.Capture

	switch {
	case item.Event != nil:
		capture = tallyowl.Event(item.Event.Name)
	case item.PageView != nil:
		capture = tallyowl.PageView(item.PageView.Route)
	case item.Conversion != nil:
		var value *tallyowl.Value
		if item.Conversion.Value != nil {
			v := tallyowl.Value{Kind: tallyowl.KindDecimal, Decimal: toCollectorDecimal(*item.Conversion.Value)}
			value = &v
		}
		currency := ""
		if item.Conversion.Currency != nil {
			currency = *item.Conversion.Currency
		}
		capture = tallyowl.Conversion(item.Conversion.Goal, value, currency)
	case item.Error != nil:
		capture = tallyowl.Error(item.Error.ErrorType, item.Error.Message, item.Error.Handled)
	case item.SessionStart != nil:
		capture = tallyowl.SessionStart(sessionOf(envelope))
	case item.SessionEnd != nil:
		capture = tallyowl.SessionEnd(sessionOf(envelope), item.SessionEnd.Reason)
	default:
		return nil, fmt.Errorf(
			"the backend does not forward a %s item yet", envelope.Kind)
	}

	// A browser supplies an untrusted identifier. The app driver namespaces or
	// replaces it before it seals a batch; here the backend keeps it, because
	// the test bed's ledger names it and the simulator is the producer. A real
	// application replaces it. See DELIVERY.md section 2.
	capture = capture.WithEventID(envelope.EventId).At(int64(envelope.OccurredAt))
	if envelope.SessionId != nil {
		capture = capture.WithSession(string(*envelope.SessionId))
	}
	if envelope.RequestId != nil {
		capture = capture.WithRequest(*envelope.RequestId)
	}
	if envelope.Release != nil {
		capture = capture.WithRelease(*envelope.Release)
	}
	for _, property := range envelope.Properties {
		value, err := tallyowl.ReadValue(collector.TypedValue{
			Kind:         collector.TypedValueKind(property.Value.Kind),
			BoolValue:    property.Value.BoolValue,
			IntValue:     property.Value.IntValue,
			UintValue:    property.Value.UintValue,
			FloatValue:   property.Value.FloatValue,
			DecimalValue: optionalDecimal(property.Value.DecimalValue),
			TextValue:    property.Value.TextValue,
			BytesValue:   property.Value.BytesValue,
		})
		if err != nil {
			return nil, fmt.Errorf("the property `%s` could not be read: %w", property.Key, err)
		}
		capture = capture.WithProperty(property.Key, value)
	}
	return capture, nil
}

// The two generated packages each carry their own copy of the exact-decimal
// type. The value is the same integers either way, so the conversion is exact
// and no digit is lost.
func toCollectorDecimal(value ingest.CsilDecimal) collector.CsilDecimal {
	return collector.CsilDecimal{Exponent: value.Exponent, Mantissa: value.Mantissa}
}

func optionalDecimal(value *ingest.CsilDecimal) *collector.CsilDecimal {
	if value == nil {
		return nil
	}
	out := toCollectorDecimal(*value)
	return &out
}

func sessionOf(envelope ingest.Envelope) string {
	if envelope.SessionId == nil {
		return ""
	}
	return string(*envelope.SessionId)
}
