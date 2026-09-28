package tallyowl

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
)

// buffered is what a driver holds, which is what a helper recorded.
func buffered(driver *Driver) []api.TelemetryItem {
	driver.mu.Lock()
	defer driver.mu.Unlock()
	return append([]api.TelemetryItem(nil), driver.items...)
}

type cartError struct{}

func (cartError) Error() string { return "the cart is empty" }

func TestAStartedSpanTimesItselfAndItsChildJoinsTheTrace(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	clock := onFakeTime(driver)

	ctx, parent := driver.StartSpan(context.Background(), "checkout", "internal")
	_, child := driver.StartSpan(ctx, "charge-card", "client")
	clock.advance(40 * time.Millisecond)
	if err := child.End(); err != nil {
		t.Fatalf("end: %v", err)
	}
	clock.advance(10 * time.Millisecond)
	if err := parent.End(); err != nil {
		t.Fatalf("end: %v", err)
	}
	_ = parent.End()

	items := buffered(driver)
	if len(items) != 2 {
		t.Fatalf("two spans ended once each, and %d items were recorded", len(items))
	}
	if items[0].Span.DurationMs != 40 || items[1].Span.DurationMs != 50 {
		t.Errorf("the spans took %d and %d ms", items[0].Span.DurationMs, items[1].Span.DurationMs)
	}
	if string(*items[0].Envelope.TraceId) != string(*items[1].Envelope.TraceId) {
		t.Error("the child is in another trace")
	}
	if items[0].Span.ParentSpanId == nil ||
		string(*items[0].Span.ParentSpanId) != string(*items[1].Envelope.SpanId) {
		t.Error("the child does not name its parent")
	}
}

func TestAFailedSpanLinksToTheErrorThatEndedIt(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	onFakeTime(driver)

	_, span := driver.StartSpan(context.Background(), "checkout", "internal")
	if err := span.Fail(fmt.Errorf("checkout: %w", cartError{})); err != nil {
		t.Fatalf("fail: %v", err)
	}
	_ = span.End()

	items := buffered(driver)
	failure, recorded := items[0], items[1]
	if failure.Error == nil || failure.Error.ErrorType != "tallyowl.cartError" {
		t.Fatalf("the error type should be the innermost error's, got %+v", failure.Error)
	}
	if failure.Error.Message != "checkout: the cart is empty" {
		t.Errorf("the message lost its context: %q", failure.Error.Message)
	}
	if len(failure.Error.Frames) == 0 {
		t.Error("the error carries no stack, so it groups by message alone")
	}
	if recorded.Span.Status != "error" || recorded.Span.ErrorEventId == nil ||
		string(*recorded.Span.ErrorEventId) != string(failure.Envelope.EventId) {
		t.Error("the span does not name the error that ended it")
	}
}

func TestAStackSaysWhichFramesAreTheApplicationsOwn(t *testing.T) {
	for function, want := range map[string]bool{
		"main.main":                         true,
		"example.com/shop/cart.(*Cart).Add": true,
		"net/http.(*ServeMux).ServeHTTP":    false,
		"runtime.gopanic":                   false,
		"github.com/CatalystCommunity/tallyowl/packages/driver-go.ErrorFrom": false,
	} {
		if got := inApp(function); got != want {
			t.Errorf("inApp(%s) = %v", function, got)
		}
	}
	module, function := splitFunction("example.com/shop/cart.(*Cart).Add")
	if module != "example.com/shop/cart" || function != "(*Cart).Add" {
		t.Errorf("split into %q and %q", module, function)
	}
}

func TestTheMiddlewareRecordsTheRoutePatternAndNeverThePath(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	onFakeTime(driver)
	mux := http.NewServeMux()
	mux.HandleFunc("GET /orders/{id}", func(w http.ResponseWriter, r *http.Request) {
		if _, ok := SpanFromContext(r.Context()); !ok {
			t.Error("the handler's context carries no span")
		}
		w.WriteHeader(http.StatusTeapot)
	})

	request := httptest.NewRequest("GET", "/orders/alice@example.com?card=4111", nil)
	request.Header.Set("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01")
	driver.Middleware(mux).ServeHTTP(httptest.NewRecorder(), request)

	items := buffered(driver)
	if len(items) != 1 || items[0].Span == nil {
		t.Fatalf("one request is one span, got %d items", len(items))
	}
	if got := items[0].Span.Operation; got != "GET /orders/{id}" {
		t.Errorf("the operation is %q", got)
	}
	if fmt.Sprintf("%x", []byte(*items[0].Envelope.TraceId)) != "0af7651916cd43dd8448eb211c80319c" {
		t.Error("the span did not continue the caller's trace")
	}
	encoded := string(api.EncodeTelemetryItem(items[0]))
	if contains(encoded, "alice") || contains(encoded, "4111") {
		t.Error("a path identifier or a query string reached the telemetry")
	}

	stranger := httptest.NewRequest("GET", "/wp-admin/secret", nil)
	driver.Middleware(mux).ServeHTTP(httptest.NewRecorder(), stranger)
	if got := buffered(driver)[1].Span.Operation; got != "GET unmatched" {
		t.Errorf("a path nobody routed was recorded as %q", got)
	}
}

func TestTheMiddlewareRecordsAPanicAndLetsItContinue(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "key-a"))
	onFakeTime(driver)
	handler := driver.Middleware(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		panic(errors.New("nil map write"))
	}))

	func() {
		defer func() {
			if recover() == nil {
				t.Error("the middleware swallowed the host's panic")
			}
		}()
		handler.ServeHTTP(httptest.NewRecorder(), httptest.NewRequest("GET", "/", nil))
	}()

	items := buffered(driver)
	if len(items) != 2 || items[0].Error == nil || items[1].Span == nil {
		t.Fatalf("a panic is one error and one span, got %d items", len(items))
	}
	if items[0].Error.Handled || items[1].Span.Status != "error" {
		t.Error("the panic should be an unhandled error on a failed span")
	}
}

func TestATelemetryRefusalNeverReachesTheHandler(t *testing.T) {
	settings := NewSettings("127.0.0.1:1", "key-a")
	settings.MaxUnacknowledgedBytes = 1
	driver := NewDriver(settings)
	served := false
	handler := driver.Middleware(http.HandlerFunc(func(http.ResponseWriter, *http.Request) {
		served = true
	}))
	handler.ServeHTTP(httptest.NewRecorder(), httptest.NewRequest("GET", "/", nil))
	if !served || driver.Stats().Refused != 1 {
		t.Fatalf("served=%v, stats=%+v", served, driver.Stats())
	}
}
