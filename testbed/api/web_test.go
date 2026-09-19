package api

// The seedstore web surface, against a collector that speaks the real
// contract.
//
// The fake here is a collector, not a mock of seedstore's own seam. It decodes
// a real `SubmitBatchRequest` off a real CSIL-RPC frame, so the test proves the
// whole path: browser bytes, seedstore's own HTTP route, the app driver, and a
// collector.
//
// This is what the Phase 4 exit criterion "an unload flush reaches the
// collector through the application" asks for.

import (
	"bytes"
	"net"
	"net/http"
	"sync"
	"testing"
	"time"

	collector "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
	transport "github.com/catalystcommunity/csilgen/transports/go"
)

type fakeCollector struct {
	listener net.Listener

	mu    sync.Mutex
	items []collector.TelemetryItem
	auth  []string
}

func startCollector(t *testing.T) *fakeCollector {
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

func (f *fakeCollector) accept() {
	for {
		conn, err := f.listener.Accept()
		if err != nil {
			return
		}
		go func() {
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
		}()
	}
}

func (f *fakeCollector) handle(request *transport.RpcRequest) transport.HandlerOutcome {
	decoded, err := collector.DecodeSubmitBatchRequest(request.Payload)
	if err != nil {
		return transport.Transport(transport.StatusMalformedEnvelope, err.Error())
	}
	f.mu.Lock()
	f.items = append(f.items, decoded.Batch.Items...)
	if request.Auth != nil {
		f.auth = append(f.auth, *request.Auth)
	}
	f.mu.Unlock()
	return transport.Reply("SubmitBatchResponse", collector.EncodeSubmitBatchResponse(
		collector.SubmitBatchResponse{
			BatchId:       decoded.Batch.BatchId,
			Accepted:      uint64(len(decoded.Batch.Items)),
			DurableCopies: 1,
			QueuedAt:      collector.Timestamp(time.Now().UnixMilli()),
		}))
}

func (f *fakeCollector) received() []collector.TelemetryItem {
	f.mu.Lock()
	defer f.mu.Unlock()
	out := make([]collector.TelemetryItem, len(f.items))
	copy(out, f.items)
	return out
}

/// One seedstore: a backend with a credential, and its web surface.
func seedstore(t *testing.T) (*Backend, *Web, string, *fakeCollector) {
	t.Helper()
	fake := startCollector(t)
	backend := New(fake.listener.Addr().String(), "tow_key_abc", "seedstore")
	web := NewWeb(backend, "testbed/webapp/dist")
	address, err := web.Listen()
	if err != nil {
		t.Fatalf("the web surface could not listen: %v", err)
	}
	t.Cleanup(func() {
		web.Close()
		backend.Close()
	})
	return backend, web, "http://" + address, fake
}

/// One browser event, encoded the way the browser package encodes it.
func captureRequest(name, session string) []byte {
	occurred := ingest.Timestamp(time.Now().UnixMilli())
	sessionID := ingest.SessionId(session)
	item := ingest.TelemetryItem{
		Envelope: ingest.Envelope{
			EventId:    make([]byte, 16),
			Kind:       "event",
			OccurredAt: occurred,
			SessionId:  &sessionID,
			SdkName:    "@tallyowl/browser",
			SdkVersion: "0.0.0",
		},
		Event: &ingest.EventPayload{Name: name},
	}
	item.Envelope.EventId[15] = 1
	return ingest.EncodeCaptureRequest(ingest.CaptureRequest{Items: []ingest.TelemetryItem{item}})
}

func TestAnUnloadFlushReachesTheCollectorThroughTheApplication(t *testing.T) {
	// The Phase 4 exit criterion. The browser sends bytes to seedstore's own
	// route with no reply, and seedstore forwards them with the app driver.
	backend, web, address, fake := seedstore(t)

	response, err := http.Post(
		address+"/telemetry/unload",
		"application/cbor",
		bytes.NewReader(captureRequest("tab-closing", "session-1")),
	)
	if err != nil {
		t.Fatalf("the unload flush did not go: %v", err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusNoContent {
		t.Fatalf("the unload route answered %d", response.StatusCode)
	}
	if web.UnloadFlushes() != 1 {
		t.Fatalf("the surface saw %d unload flushes", web.UnloadFlushes())
	}

	// It is buffered into the app driver like every other browser event, so a
	// flush is what sends it on.
	if _, err := backend.Flush(); err != nil {
		t.Fatalf("the batch did not reach the collector: %v", err)
	}

	items := fake.received()
	if len(items) != 1 {
		t.Fatalf("the collector received %d items", len(items))
	}
	if items[0].Event == nil || items[0].Event.Name != "tab-closing" {
		t.Fatalf("the wrong event arrived: %+v", items[0])
	}
	if len(backend.Failures()) != 0 {
		t.Fatalf("the backend recorded failures: %v", backend.Failures())
	}
}

func TestABrowserEventTakesTheSamePathOverTheHostRoute(t *testing.T) {
	backend, web, address, fake := seedstore(t)

	frame := transport.NewRpcRequest("TallyOwlIngest", "capture", captureRequest("seed-added", "session-2")).
		WithID(1)
	encoded, err := frame.Encode()
	if err != nil {
		t.Fatalf("encode: %v", err)
	}
	response, err := http.Post(address+"/telemetry", "application/cbor", bytes.NewReader(encoded))
	if err != nil {
		t.Fatalf("the browser call did not go: %v", err)
	}
	defer response.Body.Close()
	if web.BrowserCalls() != 1 {
		t.Fatalf("the surface saw %d browser calls", web.BrowserCalls())
	}

	if _, err := backend.Flush(); err != nil {
		t.Fatalf("the batch did not reach the collector: %v", err)
	}
	items := fake.received()
	if len(items) != 1 || items[0].Event.Name != "seed-added" {
		t.Fatalf("the wrong events arrived: %+v", items)
	}

	// The credential is the backend's, and it travels on the connection. The
	// browser never held one.
	fake.mu.Lock()
	auth := append([]string(nil), fake.auth...)
	fake.mu.Unlock()
	if len(auth) == 0 || auth[0] != "tow_key_abc" {
		t.Fatalf("the collector saw credentials %v", auth)
	}
}

func TestTheWebSurfaceRefusesWhatItShould(t *testing.T) {
	_, _, address, _ := seedstore(t)

	for _, probe := range []struct {
		name   string
		path   string
		body   []byte
		expect int
	}{
		{"a body that is not a capture request", "/telemetry/unload", []byte("not cbor"), http.StatusBadRequest},
		{"a body that is not a frame", "/telemetry", []byte("not a frame"), http.StatusBadRequest},
	} {
		response, err := http.Post(address+probe.path, "application/cbor", bytes.NewReader(probe.body))
		if err != nil {
			t.Fatalf("%s: %v", probe.name, err)
		}
		_ = response.Body.Close()
		if response.StatusCode != probe.expect {
			t.Fatalf("%s answered %d, expected %d", probe.name, response.StatusCode, probe.expect)
		}
	}

	// A GET on the carrier is not a carrier call.
	response, err := http.Get(address + "/telemetry")
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	_ = response.Body.Close()
	if response.StatusCode != http.StatusMethodNotAllowed {
		t.Fatalf("a GET on the carrier answered %d", response.StatusCode)
	}
}

func TestAnAssetPathCannotEscapeTheBundleDirectory(t *testing.T) {
	// This surface faces a browser, so a request that walks out of the bundle
	// directory would read whatever the process can read.
	if file, ok := assetPath("/srv/seedstore", "/assets/testbed/webapp/src/boot.js"); !ok ||
		file != "/srv/seedstore/testbed/webapp/src/boot.js" {
		t.Fatalf("a real asset did not resolve: %q %v", file, ok)
	}
	for _, attempt := range []string{
		"/assets/../../etc/passwd",
		"/assets/a/../../etc/passwd",
		"/assets/./../secrets",
		"/assets/",
		"/assets//etc/passwd",
	} {
		if _, ok := assetPath("/srv/seedstore", attempt); ok {
			t.Fatalf("%s escaped the bundle directory", attempt)
		}
	}
}
