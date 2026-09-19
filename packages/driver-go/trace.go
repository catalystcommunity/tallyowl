// Spans, error stacks, and trace context propagation.
//
// A trace crosses a service TallyOwl does not own, so the identifiers travel in
// the W3C `traceparent` header form that other tracing systems already read.
// The text form exists only at that boundary: csil/types/common.csil says an ID
// travels as raw bytes on the ingest path, because 16 bytes costs 53 percent
// less than 36 characters after encoding and compression.
package tallyowl

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"strconv"
	"strings"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
)

// SpanContext is where one span sits in its trace.
type SpanContext struct {
	TraceID      []byte
	SpanID       []byte
	ParentSpanID []byte
	// Sampled is whether the producer decided to record this trace. The head
	// decides again after the trace is complete; this is the head-sampling
	// half. See D35.
	Sampled bool
}

// RootContext starts a new trace.
func RootContext() SpanContext {
	return SpanContext{
		TraceID: randomBytes(16),
		SpanID:  randomBytes(8),
		Sampled: true,
	}
}

// Child is a span inside this one.
//
// The trace ID never changes down a trace: it is what routes every span of one
// trace to one tablet, so one tablet can make the tail decision without any
// component buffering a trace across collectors. See D16 and D35.
func (c SpanContext) Child() SpanContext {
	return SpanContext{
		TraceID:      c.TraceID,
		SpanID:       randomBytes(8),
		ParentSpanID: c.SpanID,
		Sampled:      c.Sampled,
	}
}

// Traceparent is the W3C header value for this context.
func (c SpanContext) Traceparent() string {
	flags := "00"
	if c.Sampled {
		flags = "01"
	}
	return fmt.Sprintf("00-%s-%s-%s",
		hex.EncodeToString(c.TraceID), hex.EncodeToString(c.SpanID), flags)
}

// ParseTraceparent reads an incoming header.
//
// It reports false for anything this version cannot read, and a caller then
// starts a new trace. Continuing a trace from a header nobody could parse would
// join two unrelated traces, which is worse than starting one.
func ParseTraceparent(header string) (SpanContext, bool) {
	parts := strings.Split(strings.TrimSpace(header), "-")
	if len(parts) < 4 || parts[0] != "00" {
		return SpanContext{}, false
	}
	traceID, err := hex.DecodeString(parts[1])
	if err != nil || len(traceID) != 16 || isZero(traceID) {
		return SpanContext{}, false
	}
	spanID, err := hex.DecodeString(parts[2])
	if err != nil || len(spanID) != 8 || isZero(spanID) {
		return SpanContext{}, false
	}
	flags, err := strconv.ParseUint(parts[3], 16, 8)
	if err != nil {
		return SpanContext{}, false
	}
	return SpanContext{
		TraceID: traceID,
		// The incoming span is this one's parent. A service that reused the
		// incoming span ID would give two spans one identity and break every
		// waterfall that held them.
		SpanID:       randomBytes(8),
		ParentSpanID: spanID,
		Sampled:      flags&1 == 1,
	}, true
}

// ContinueOrStart continues an incoming trace, or starts one when there is no
// usable header.
func ContinueOrStart(header string) SpanContext {
	if context, ok := ParseTraceparent(header); ok {
		return context
	}
	return RootContext()
}

// Span records one span of a trace.
//
// The span's own identity comes from the context, so a caller passes the
// context it already has rather than assembling three identifiers.
func Span(context SpanContext, operation, kind string, startAt, durationMs int64) *Capture {
	payload := api.SpanPayload{
		Operation:  operation,
		Kind:       api.SpanKind(kind),
		StartAt:    api.Timestamp(startAt),
		DurationMs: api.DurationMs(durationMs),
		Status:     "ok",
	}
	if len(context.ParentSpanID) > 0 {
		parent := api.SpanId(context.ParentSpanID)
		payload.ParentSpanId = &parent
	}
	item := api.TelemetryItem{
		Envelope: newEnvelope("span"),
		Span:     &payload,
	}
	item.Envelope.OccurredAt = api.Timestamp(startAt)
	trace := api.TraceId(context.TraceID)
	item.Envelope.TraceId = &trace
	span := api.SpanId(context.SpanID)
	item.Envelope.SpanId = &span
	return &Capture{
		item: item,
		// A diagnostic span is the lowest priority class. It never seals a
		// batch: a batch sealed by a span would send one span at a time under
		// load, which is the opposite of what that path needs.
		critical: false,
	}
}

// Failed marks a span as failed and links it to the error that explains it.
//
// The link goes both ways without a join: the error carries the trace ID, and
// the span carries the error's event ID.
func (c *Capture) Failed(errorEventID []byte) *Capture {
	if c.item.Span == nil {
		return c
	}
	c.item.Span.Status = "error"
	if len(errorEventID) > 0 {
		id := api.EventId(errorEventID)
		c.item.Span.ErrorEventId = &id
	}
	return c
}

// InSpan puts this item in a trace.
func (c *Capture) InSpan(context SpanContext) *Capture {
	trace := api.TraceId(context.TraceID)
	c.item.Envelope.TraceId = &trace
	span := api.SpanId(context.SpanID)
	c.item.Envelope.SpanId = &span
	return c
}

// Frame is one stack frame, in the shape the group fingerprint reads.
//
// InApp is the field that matters most. Two defects in one application throw
// through the same framework, and a fingerprint over the top frames alone would
// group them together. See D39.
type Frame struct {
	Module   string
	Function string
	File     string
	Line     uint64
	InApp    bool
}

// InAppFrame is a frame in the application's own code.
func InAppFrame(module, function string) Frame {
	return Frame{Module: module, Function: function, InApp: true}
}

// LibraryFrame is a frame in a library or the runtime.
func LibraryFrame(module, function string) Frame {
	return Frame{Module: module, Function: function, InApp: false}
}

// At names the file and line a frame came from. Neither reaches the group
// fingerprint, because a reformatting change shifts every line in a file.
func (f Frame) At(file string, line uint64) Frame {
	f.File = file
	f.Line = line
	return f
}

// WithFrames attaches the stack this error carried.
//
// The frames decide the group, so a producer that has them should send them:
// without frames the projector falls back to the message, which groups less
// precisely. See D39.
func (c *Capture) WithFrames(frames []Frame) *Capture {
	if c.item.Error == nil {
		return c
	}
	out := make([]api.StackFrame, 0, len(frames))
	for _, frame := range frames {
		wire := api.StackFrame{InApp: frame.InApp}
		if frame.Module != "" {
			module := frame.Module
			wire.Module = &module
		}
		if frame.Function != "" {
			function := frame.Function
			wire.Function = &function
		}
		if frame.File != "" {
			file := frame.File
			wire.File = &file
		}
		if frame.Line != 0 {
			line := frame.Line
			wire.Line = &line
		}
		out = append(out, wire)
	}
	c.item.Error.Frames = out
	return c
}

func randomBytes(n int) []byte {
	out := make([]byte, n)
	if _, err := rand.Read(out); err != nil {
		// A span ID has to be unpredictable as well as unique: a guessable one
		// lets a caller attach a span to somebody else's trace. There is no
		// safe fallback, so this stops rather than producing a weak identifier.
		panic("the system random source is unavailable: " + err.Error())
	}
	return out
}

func isZero(bytes []byte) bool {
	for _, b := range bytes {
		if b != 0 {
			return false
		}
	}
	return true
}
