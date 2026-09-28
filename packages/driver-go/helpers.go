package tallyowl

import (
	"context"
	"fmt"
	"net/http"
	"runtime"
	"strings"
	"time"
)

// The common cases, so that a host does not write them by hand: a span that
// times itself, an error with its stack, and an HTTP server that records both.
//
// None of these record a request body, a query string, a header value, or a
// path with its identifiers in it. A host that wants more adds it with
// WithProperty, and owns that choice.

type spanContextKey struct{}

// ContextWithSpan returns a context that carries a span context.
func ContextWithSpan(ctx context.Context, span SpanContext) context.Context {
	return context.WithValue(ctx, spanContextKey{}, span)
}

// SpanFromContext is the span context a context carries, if it carries one.
func SpanFromContext(ctx context.Context) (SpanContext, bool) {
	span, ok := ctx.Value(spanContextKey{}).(SpanContext)
	return span, ok
}

// ActiveSpan is a span that has started and has not ended.
type ActiveSpan struct {
	driver    *Driver
	context   SpanContext
	operation string
	kind      string
	startedAt time.Time
	failure   []byte
	ended     bool
}

// StartSpan starts a span and returns a context that carries it.
//
// The span is a child of the span the context carries, or the root of a new
// trace when the context carries none. End records it. Every item captured
// with InSpan(span.Context()) joins the same trace.
func (d *Driver) StartSpan(ctx context.Context, operation, kind string) (context.Context, *ActiveSpan) {
	var span SpanContext
	if parent, ok := SpanFromContext(ctx); ok {
		span = parent.Child()
	} else {
		span = RootContext()
	}
	active := &ActiveSpan{
		driver:    d,
		context:   span,
		operation: operation,
		kind:      kind,
		startedAt: d.now(),
	}
	return ContextWithSpan(ctx, span), active
}

// Context is the identity of this span.
func (s *ActiveSpan) Context() SpanContext { return s.context }

// Fail records the error that ended this span, and links the two. It returns
// the error from Capture, which is a backpressure refusal or nil.
func (s *ActiveSpan) Fail(err error) error {
	capture := ErrorFrom(err, true).InSpan(s.context)
	s.failure = capture.EventID()
	return s.driver.Capture(capture)
}

// End records the span with the time it took. A second call does nothing.
func (s *ActiveSpan) End() error {
	if s.ended {
		return nil
	}
	s.ended = true
	duration := s.driver.now().Sub(s.startedAt).Milliseconds()
	capture := Span(s.context, s.operation, s.kind, s.startedAt.UnixMilli(), duration)
	if s.failure != nil {
		capture = capture.Failed(s.failure)
	}
	return s.driver.Capture(capture)
}

// ErrorFrom records a Go error with the stack of the caller.
//
// The error type is the type of the innermost error in the chain, because that
// is what two occurrences of one defect share. The frames decide the group, so
// a record with them groups more precisely than one without. See D39.
func ErrorFrom(err error, handled bool) *Capture {
	if err == nil {
		err = fmt.Errorf("an error value was nil")
	}
	innermost := err
	for {
		next, ok := innermost.(interface{ Unwrap() error })
		if !ok || next.Unwrap() == nil {
			break
		}
		innermost = next.Unwrap()
	}
	return Error(fmt.Sprintf("%T", innermost), err.Error(), handled).
		WithFrames(callerFrames(3))
}

// callerFrames reads the stack above the caller. `skip` counts the frames of
// this package to leave out.
func callerFrames(skip int) []Frame {
	const depth = 50
	pcs := make([]uintptr, depth)
	count := runtime.Callers(skip, pcs)
	frames := runtime.CallersFrames(pcs[:count])
	var out []Frame
	for {
		frame, more := frames.Next()
		if frame.Function != "" {
			module, function := splitFunction(frame.Function)
			out = append(out, Frame{
				Module:   module,
				Function: function,
				File:     frame.File,
				Line:     uint64(frame.Line),
				InApp:    inApp(frame.Function),
			})
		}
		if !more {
			break
		}
	}
	return out
}

// splitFunction splits `example.com/shop/cart.(*Cart).Add` into its package and
// the rest.
func splitFunction(name string) (module, function string) {
	slash := strings.LastIndex(name, "/")
	dot := strings.Index(name[slash+1:], ".")
	if dot < 0 {
		return "", name
	}
	return name[:slash+1+dot], name[slash+1+dot+1:]
}

// inApp reports whether a function is the application's own. The standard
// library has no dot in the first element of its package path, and this driver
// is never the application.
func inApp(function string) bool {
	if strings.HasPrefix(function, "github.com/CatalystCommunity/tallyowl/packages/driver-go.") {
		return false
	}
	first := function
	if slash := strings.Index(function, "/"); slash >= 0 {
		first = function[:slash]
	} else if dot := strings.Index(function, "."); dot >= 0 {
		first = function[:dot]
	}
	return strings.Contains(first, ".") || first == "main"
}

// statusRecorder remembers the status a handler wrote.
type statusRecorder struct {
	http.ResponseWriter
	status int
}

func (r *statusRecorder) WriteHeader(status int) {
	r.status = status
	r.ResponseWriter.WriteHeader(status)
}

func (r *statusRecorder) Unwrap() http.ResponseWriter { return r.ResponseWriter }

// Middleware records one server span for each request, and an error for each
// panic.
//
// The span continues the trace in the request's `traceparent` header, or starts
// one. The handler's context carries the span, so StartSpan inside the handler
// makes a child. The operation is the method and the route pattern the request
// matched, for example `GET /orders/{id}`, and never the path, because a path
// carries identifiers. The status is a property. A panic is recorded as an
// unhandled error and then continues to the host's own recovery.
//
// A telemetry failure never reaches the handler: a refused capture is counted
// in Stats and the request goes on.
func (d *Driver) Middleware(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		span := ContinueOrStart(r.Header.Get("traceparent"))
		startedAt := d.now()
		recorder := &statusRecorder{ResponseWriter: w, status: http.StatusOK}
		r = r.WithContext(ContextWithSpan(r.Context(), span))

		defer func() {
			panicked := recover()
			operation := r.Method + " " + routeOf(r)
			duration := d.now().Sub(startedAt).Milliseconds()
			capture := Span(span, operation, "server", startedAt.UnixMilli(), duration)
			if panicked != nil {
				recorder.status = http.StatusInternalServerError
				failure := Error("panic", fmt.Sprint(panicked), false).
					WithFrames(callerFrames(4)).InSpan(span)
				capture = capture.Failed(failure.EventID())
				_ = d.Capture(failure)
			} else if recorder.status >= 500 {
				capture = capture.Failed(nil)
			}
			_ = d.Capture(capture.WithProperty("http.status", Int(int64(recorder.status))))
			if panicked != nil {
				panic(panicked)
			}
		}()
		next.ServeHTTP(recorder, r)
	})
}

// routeOf is the pattern a request matched. A request that matched none is
// named as such, because its path is whatever a stranger typed.
func routeOf(r *http.Request) string {
	if r.Pattern == "" {
		return "unmatched"
	}
	// A pattern may begin with its method. The operation already names it.
	if space := strings.Index(r.Pattern, " "); space >= 0 {
		return r.Pattern[space+1:]
	}
	return r.Pattern
}
