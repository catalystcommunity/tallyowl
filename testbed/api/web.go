package api

// The seedstore web surface.
//
// This is the host application's own HTTP server. It serves the web
// application's document and modules, it carries the browser package's CSIL
// frames on the host's own route, and it takes the unload flush.
//
// The rule this file exists to hold, from AGENTS.md: "Browser instrumentation
// uses an application's existing same-origin CSIL connection. It must not
// create a TallyOwl connection or contact a TallyOwl domain." Every address
// here belongs to seedstore. The browser never learns a TallyOwl address,
// never learns a workspace or a project, and never holds a credential; the
// backend holds the one credential and forwards with the app driver.
//
// The unload flush is the one browser-only HTTP use the design permits for
// telemetry, and it is not an exception to that rule: it reaches this route,
// which is seedstore's own. See D34 and docs/TESTBED.md section 2.

import (
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"

	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
	transport "github.com/catalystcommunity/csilgen/transports/go"
)

// The largest browser request this surface reads. A capture buffer seals at 64
// items, so anything larger is a mistake or an attempt, and both are refused
// before allocation.
const maxBrowserBodyBytes = 1 << 20

// Web is the seedstore web surface in front of one backend.
type Web struct {
	backend  *Backend
	assets   string
	listener net.Listener
	server   *http.Server

	mu            sync.Mutex
	unloadFlushes int
	browserCalls  int
}

// NewWeb builds the web surface. `assets` is where the built web application
// is; `./tools.sh build` writes it.
func NewWeb(backend *Backend, assets string) *Web {
	return &Web{backend: backend, assets: assets}
}

// Listen starts the web surface on a loopback port and returns its address.
func (w *Web) Listen() (string, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return "", err
	}
	w.listener = listener

	mux := http.NewServeMux()
	mux.HandleFunc("/", w.document)
	mux.HandleFunc("/assets/", w.asset)
	mux.HandleFunc("/telemetry", w.carry)
	mux.HandleFunc("/telemetry/unload", w.unload)

	w.server = &http.Server{Handler: mux}
	go func() { _ = w.server.Serve(listener) }()
	return listener.Addr().String(), nil
}

// Close stops the web surface.
func (w *Web) Close() {
	if w.server != nil {
		_ = w.server.Close()
	}
}

// UnloadFlushes reports how many unload flushes arrived. A test asserts on it,
// because an unload flush that quietly did nothing would look exactly like one
// that worked.
func (w *Web) UnloadFlushes() int {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.unloadFlushes
}

// BrowserCalls reports how many browser CSIL calls arrived on the host route.
func (w *Web) BrowserCalls() int {
	w.mu.Lock()
	defer w.mu.Unlock()
	return w.browserCalls
}

func (w *Web) document(response http.ResponseWriter, request *http.Request) {
	if request.URL.Path != "/" {
		http.NotFound(response, request)
		return
	}
	response.Header().Set("content-type", "text/html; charset=utf-8")
	response.Header().Set("cache-control", "no-store")
	response.Header().Set("x-content-type-options", "nosniff")
	_, _ = response.Write([]byte(document))
}

func (w *Web) asset(response http.ResponseWriter, request *http.Request) {
	file, ok := assetPath(w.assets, request.URL.Path)
	if !ok {
		http.NotFound(response, request)
		return
	}
	bytes, err := os.ReadFile(file)
	if err != nil {
		http.Error(response,
			"The web application is not built. Run `./tools.sh build`.",
			http.StatusServiceUnavailable)
		return
	}
	response.Header().Set("content-type", "text/javascript; charset=utf-8")
	response.Header().Set("cache-control", "no-store")
	_, _ = response.Write(bytes)
}

// assetPath resolves one asset under the bundle directory.
//
// A component that is not a plain name is refused. Without that a request for
// `/assets/../../etc/passwd` would read whatever the process can read, and this
// surface faces a browser.
func assetPath(assets, path string) (string, bool) {
	rest, found := strings.CutPrefix(path, "/assets/")
	if !found || rest == "" {
		return "", false
	}
	file := assets
	for _, component := range strings.Split(rest, "/") {
		if component == "" || component == "." || component == ".." {
			return "", false
		}
		if strings.ContainsAny(component, `\:`) {
			return "", false
		}
		file = filepath.Join(file, component)
	}
	return file, true
}

// carry is the host's own route for the browser package's CSIL frames.
//
// It is the same shape the browser package's `Router` seam expects: one
// encoded request in, one reply out. The browser package builds no carrier and
// names no address; seedstore owns this route and forwards what arrives.
func (w *Web) carry(response http.ResponseWriter, request *http.Request) {
	if request.Method != http.MethodPost {
		http.Error(response, "Use POST.", http.StatusMethodNotAllowed)
		return
	}
	body, ok := readBody(response, request)
	if !ok {
		return
	}

	decoded, err := transport.DecodeRpcRequest(body)
	if err != nil {
		http.Error(response, "That was not a CSIL-RPC frame.", http.StatusBadRequest)
		return
	}

	w.mu.Lock()
	w.browserCalls++
	w.mu.Unlock()

	// The backend's own dispatcher, so a browser event takes exactly the path a
	// browser event takes over the TCP carrier. There is no second code path
	// for HTTP, which is what keeps the two from drifting.
	outcome := w.backend.handle(&decoded)
	reply := replyFor(outcome, decoded.ID)
	encoded, err := reply.Encode()
	if err != nil {
		http.Error(response, "The reply could not be encoded.", http.StatusInternalServerError)
		return
	}
	response.Header().Set("content-type", "application/cbor")
	_, _ = response.Write(encoded)
}

// unload takes the browser package's unload flush.
//
// The body is an encoded `CaptureRequest` rather than an RPC frame, because a
// browser that is going away cannot wait for a reply and `sendBeacon` sends
// bytes with no envelope. It is best effort by design: D34 says this package
// does not claim durable delivery after a tab closes, and this route makes no
// such claim either. It returns 204 and does not wait for the collector.
func (w *Web) unload(response http.ResponseWriter, request *http.Request) {
	if request.Method != http.MethodPost {
		http.Error(response, "Use POST.", http.StatusMethodNotAllowed)
		return
	}
	body, ok := readBody(response, request)
	if !ok {
		return
	}

	decoded, err := ingest.DecodeCaptureRequest(body)
	if err != nil {
		http.Error(response, "That was not a capture request.", http.StatusBadRequest)
		return
	}

	w.mu.Lock()
	w.unloadFlushes++
	w.mu.Unlock()

	// Buffered into the app driver, exactly like every other browser event. A
	// separate path would be a second way for an event to reach a collector,
	// and the test bed exists to prove there is only one.
	if _, err := w.backend.forward(decoded.Items, false); err != nil {
		// The tab is going away. Nothing can be reported to it, so the failure
		// is recorded where the test can see it.
		w.backend.recordFailure(fmt.Sprintf("unload flush: %v", err))
	}
	response.WriteHeader(http.StatusNoContent)
}

func readBody(response http.ResponseWriter, request *http.Request) ([]byte, bool) {
	// The limit is enforced while reading, so an oversized body never reaches
	// memory.
	body, err := io.ReadAll(http.MaxBytesReader(response, request.Body, maxBrowserBodyBytes))
	if err != nil {
		http.Error(response, "That request was too large.", http.StatusRequestEntityTooLarge)
		return nil, false
	}
	return body, true
}

func replyFor(outcome transport.HandlerOutcome, id *uint64) transport.RpcResponse {
	if !outcome.IsReply {
		return transport.RpcResponse{
			ID:      id,
			Status:  outcome.Status,
			Error:   &outcome.Message,
			Payload: []byte{},
		}
	}
	variant := outcome.Variant
	return transport.RpcResponse{
		ID:      id,
		Status:  transport.StatusOk,
		Variant: &variant,
		Payload: outcome.Payload,
	}
}

// The seedstore document.
//
// It carries no logic. Everything it does arrives from the module tree, so this
// string never changes when a behaviour does.
const document = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>seedstore</title>
</head>
<body>
<main id="seedstore">Loading seedstore.</main>
<script type="module" src="/assets/testbed/webapp/src/boot.js"></script>
</body>
</html>
`
