package tallyowl

// The driver against a collector that speaks the real contract.
//
// The fake here is a collector, not a mock of the driver's own storage seam.
// It decodes a real `SubmitBatchRequest` off a real CSIL-RPC frame and answers
// with a real `SubmitBatchResponse`, so the test proves the wire and not a stub.

import (
	"net"
	"sync"
	"testing"
	"time"

	collector "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// fakeCollector accepts CSIL-RPC on a loopback port and records what arrived.
type fakeCollector struct {
	listener net.Listener

	mu          sync.Mutex
	batches     []collector.Batch
	auth        []string
	failWith    *collector.ServiceError
	connections []net.Conn
}

func startFakeCollector(t *testing.T) *fakeCollector {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("the fake collector could not listen: %v", err)
	}
	f := &fakeCollector{listener: listener}
	go f.accept()
	t.Cleanup(func() { _ = listener.Close() })
	return f
}

func (f *fakeCollector) address() string { return f.listener.Addr().String() }

func (f *fakeCollector) accept() {
	for {
		conn, err := f.listener.Accept()
		if err != nil {
			return
		}
		go f.serve(conn)
	}
}

func (f *fakeCollector) serve(conn net.Conn) {
	f.mu.Lock()
	f.connections = append(f.connections, conn)
	f.mu.Unlock()
	defer conn.Close()
	carrier, err := transport.NewStreamCarrierWithMaxFrame(conn, 16*1024*1024)
	if err != nil {
		return
	}
	server := transport.NewRpcServer(carrier)
	for {
		served, err := server.ServeOne(f.handle)
		if err != nil || !served {
			return
		}
	}
}

func (f *fakeCollector) handle(request *transport.RpcRequest) transport.HandlerOutcome {
	if request.Op != "submit-batch" {
		return transport.Transport(transport.StatusUnknownServiceOrOp, "no such operation")
	}
	decoded, err := collector.DecodeSubmitBatchRequest(request.Payload)
	if err != nil {
		return transport.Transport(transport.StatusMalformedEnvelope, err.Error())
	}

	f.mu.Lock()
	f.batches = append(f.batches, decoded.Batch)
	if request.Auth != nil {
		f.auth = append(f.auth, *request.Auth)
	} else {
		f.auth = append(f.auth, "")
	}
	failure := f.failWith
	f.mu.Unlock()

	if failure != nil {
		return transport.Reply("ServiceError", collector.EncodeServiceError(*failure))
	}
	return transport.Reply("SubmitBatchResponse", collector.EncodeSubmitBatchResponse(
		collector.SubmitBatchResponse{
			BatchId:       decoded.Batch.BatchId,
			Accepted:      uint64(len(decoded.Batch.Items)),
			DurableCopies: 1,
			QueuedAt:      collector.Timestamp(nowMs()),
		}))
}

// stop closes the listener and every accepted connection. Closing a listener
// alone leaves established connections serving, which would make a reconnection
// test pass without a reconnection.
func (f *fakeCollector) stop() {
	_ = f.listener.Close()
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, conn := range f.connections {
		_ = conn.Close()
	}
}

func (f *fakeCollector) received() []collector.Batch {
	f.mu.Lock()
	defer f.mu.Unlock()
	out := make([]collector.Batch, len(f.batches))
	copy(out, f.batches)
	return out
}

func TestABatchReachesTheCollectorAndComesBackAcknowledged(t *testing.T) {
	fake := startFakeCollector(t)
	driver := NewDriver(NewSettings(fake.address(), "key-a").WithProperty("service", "checkout"))

	if err := driver.Capture(Event("checkout-started")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if err := driver.Capture(PageView("/pricing")); err != nil {
		t.Fatalf("capture: %v", err)
	}

	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("flush: %v", err)
	}
	if receipt == nil {
		t.Fatal("a flush with two items returned no receipt")
	}
	if receipt.Accepted != 2 {
		t.Errorf("the collector accepted %d items, and two were sent", receipt.Accepted)
	}
	// The acknowledgement names the durable copies it reached. A caller must
	// never have to assume which durability it got.
	if receipt.DurableCopies != 1 {
		t.Errorf("the receipt reported %d durable copies", receipt.DurableCopies)
	}

	batches := fake.received()
	if len(batches) != 1 {
		t.Fatalf("the collector saw %d batches", len(batches))
	}
	if len(batches[0].Items) != 2 {
		t.Fatalf("the batch held %d items", len(batches[0].Items))
	}
	if batches[0].Items[0].Event == nil || batches[0].Items[0].Event.Name != "checkout-started" {
		t.Error("the first item did not arrive as an event")
	}
	// The payload the second item carries is the one its kind names. This is
	// what the earlier bare choice could not do out of a dynamically typed
	// language at all.
	if batches[0].Items[1].PageView == nil {
		t.Error("the second item did not arrive as a page view")
	}
	if batches[0].Items[1].Event != nil {
		t.Error("a page view arrived carrying event details")
	}
}

func TestTheCredentialTravelsWithTheBatch(t *testing.T) {
	fake := startFakeCollector(t)
	driver := NewDriver(NewSettings(fake.address(), "key-secret"))
	if err := driver.Capture(Event("a")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if _, err := driver.Flush(); err != nil {
		t.Fatalf("flush: %v", err)
	}
	fake.mu.Lock()
	defer fake.mu.Unlock()
	if len(fake.auth) != 1 || fake.auth[0] != "key-secret" {
		t.Errorf("the collector saw the credential %q", fake.auth)
	}
}

func TestADriverNeverSetsTenancyOrAReceiveTime(t *testing.T) {
	// The collector stamps both. A driver that set them would be claiming
	// something it cannot know, and the collector would discard it anyway.
	fake := startFakeCollector(t)
	driver := NewDriver(NewSettings(fake.address(), "key-a"))
	if err := driver.Capture(Event("a")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if _, err := driver.Flush(); err != nil {
		t.Fatalf("flush: %v", err)
	}
	envelope := fake.received()[0].Items[0].Envelope
	if envelope.WorkspaceId != nil || envelope.ProjectId != nil || envelope.ReceivedAt != nil {
		t.Error("the driver sent tenancy or a receive time")
	}
}

func TestADriverPropertyArrivesWithADriverOrigin(t *testing.T) {
	fake := startFakeCollector(t)
	driver := NewDriver(NewSettings(fake.address(), "key-a").WithProperty("service", "checkout"))
	if err := driver.Capture(Event("a").WithProperty("plan", Text("pro"))); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if _, err := driver.Flush(); err != nil {
		t.Fatalf("flush: %v", err)
	}
	properties := fake.received()[0].Items[0].Envelope.Properties
	origins := map[string]string{}
	for _, p := range properties {
		origins[p.Key] = string(p.Origin)
	}
	if origins["service"] != "driver" {
		t.Errorf("a driver property arrived with origin %q", origins["service"])
	}
	if origins["plan"] != "client" {
		t.Errorf("a client property arrived with origin %q", origins["plan"])
	}
}

func TestAServiceErrorSurfacesItsRetryFact(t *testing.T) {
	// `retryable` is a fact, not advice. A caller builds automation on it, so a
	// wrong value produces either a retry storm or lost data.
	fake := startFakeCollector(t)
	fake.failWith = &collector.ServiceError{
		Code:      "resource-exhausted",
		Message:   "The durable store is full.",
		Retryable: false,
	}
	driver := NewDriver(NewSettings(fake.address(), "key-a"))
	if err := driver.Capture(Event("a")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	_, err := driver.Flush()
	serviceError, ok := err.(*ServiceError)
	if !ok {
		t.Fatalf("the collector's rejection arrived as %T: %v", err, err)
	}
	if serviceError.Retryable {
		t.Error("a permanent rejection arrived marked retryable")
	}
	if serviceError.Code != "resource-exhausted" {
		t.Errorf("the code arrived as %q", serviceError.Code)
	}
}

func TestAFullBufferRefusesRatherThanDropping(t *testing.T) {
	// A driver that reported success for a discarded event would make every
	// count downstream wrong in a way nobody can find.
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.MaxUnacknowledgedBytes = 200
	driver := NewDriver(settings)

	var last error
	for i := 0; i < 100; i++ {
		if err := driver.Capture(Event("a-long-enough-name-to-fill-the-buffer")); err != nil {
			last = err
			break
		}
	}
	if last == nil {
		t.Fatal("the buffer never refused")
	}
	if last != ErrBackpressure {
		t.Errorf("the buffer refused with %v", last)
	}
}

func TestACriticalEventSealsTheBatch(t *testing.T) {
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.Linger = time.Hour
	settings.MaxItems = 1000
	driver := NewDriver(settings)

	if err := driver.Capture(Event("ordinary")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if driver.ShouldFlush() {
		t.Error("an ordinary event sealed a batch that should still be open")
	}
	// A conversion is the first priority class in DELIVERY.md section 8.
	if err := driver.Capture(Conversion("purchase", nil, "USD")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if !driver.ShouldFlush() {
		t.Error("a conversion did not seal the batch")
	}
}

func TestAnEmptyFlushReturnsNothingRatherThanAnEmptyBatch(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	receipt, err := driver.Flush()
	if err != nil {
		t.Fatalf("an empty flush should not reach the collector: %v", err)
	}
	if receipt != nil {
		t.Error("an empty flush returned a receipt")
	}
}

func TestAnUnreachableCollectorReturnsAnErrorThatNamesTheAddress(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	if err := driver.Capture(Event("a")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	_, err := driver.Flush()
	if err == nil {
		t.Fatal("an unreachable collector reported success")
	}
	if !contains(err.Error(), "127.0.0.1:1") {
		t.Errorf("the failure does not name the address: %v", err)
	}
}

func TestTheDriverReconnectsAfterTheCollectorRestarts(t *testing.T) {
	fake := startFakeCollector(t)
	address := fake.address()
	driver := NewDriver(NewSettings(address, "key-a"))

	if err := driver.Capture(Event("before")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if _, err := driver.Flush(); err != nil {
		t.Fatalf("flush: %v", err)
	}

	// The collector goes away and comes back on the same address, which is the
	// ordinary case during a rolling upgrade.
	fake.stop()
	listener, err := net.Listen("tcp", address)
	if err != nil {
		t.Skipf("the address did not come free again: %v", err)
	}
	replacement := &fakeCollector{listener: listener}
	go replacement.accept()
	defer listener.Close()

	if err := driver.Capture(Event("after")); err != nil {
		t.Fatalf("capture: %v", err)
	}
	if _, err := driver.Flush(); err != nil {
		t.Fatalf("the driver did not reconnect: %v", err)
	}
	if len(replacement.received()) != 1 {
		t.Error("the replacement collector saw nothing")
	}
}

func TestAnItemWhoseKindAndPayloadDisagreeIsRefused(t *testing.T) {
	// The check the old shape could not make, because the payload carried no
	// name of its own.
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	capture := PageView("/pricing")
	capture.item.Envelope.Kind = "conversion"
	err := driver.Capture(capture)
	if err == nil {
		t.Fatal("an item whose kind and payload disagree was accepted")
	}
	if !contains(err.Error(), "conversion") || !contains(err.Error(), "page-view") {
		t.Errorf("the message names neither kind: %v", err)
	}
}

func contains(haystack, needle string) bool {
	return len(haystack) >= len(needle) && (func() bool {
		for i := 0; i+len(needle) <= len(haystack); i++ {
			if haystack[i:i+len(needle)] == needle {
				return true
			}
		}
		return false
	})()
}
