package tallyowl

import (
	"bytes"
	"testing"
)

func TestAChildKeepsTheTraceAndTakesANewSpan(t *testing.T) {
	// The trace ID is what routes every span of one trace to one tablet, so it
	// never changes down a trace. See D16 and D35.
	root := RootContext()
	child := root.Child()
	if !bytes.Equal(child.TraceID, root.TraceID) {
		t.Fatal("a child left its trace")
	}
	if bytes.Equal(child.SpanID, root.SpanID) {
		t.Fatal("two spans took one identity")
	}
	if !bytes.Equal(child.ParentSpanID, root.SpanID) {
		t.Fatal("the child does not name its parent")
	}
}

func TestAContextSurvivesTheHeaderFormOtherSystemsRead(t *testing.T) {
	outbound := RootContext()
	header := outbound.Traceparent()
	if len(header) != 55 {
		t.Fatalf("the W3C form is fixed width, and this is %d: %s", len(header), header)
	}

	inbound, ok := ParseTraceparent(header)
	if !ok {
		t.Fatal("the header did not read")
	}
	if !bytes.Equal(inbound.TraceID, outbound.TraceID) {
		t.Fatal("the trace did not survive the header")
	}
	// The incoming span becomes this one's parent. Reusing its ID would give
	// two spans one identity and break every waterfall holding them.
	if !bytes.Equal(inbound.ParentSpanID, outbound.SpanID) {
		t.Fatal("the incoming span is not the parent")
	}
	if bytes.Equal(inbound.SpanID, outbound.SpanID) {
		t.Fatal("the incoming span ID was reused")
	}
	if !inbound.Sampled {
		t.Fatal("the sampled flag did not survive")
	}
}

func TestTheSampledFlagSurvivesTheHeader(t *testing.T) {
	context := RootContext()
	context.Sampled = false
	header := context.Traceparent()
	if header[len(header)-2:] != "00" {
		t.Fatalf("an unsampled trace should end in 00: %s", header)
	}
	read, ok := ParseTraceparent(header)
	if !ok || read.Sampled {
		t.Fatal("an unsampled trace read as sampled")
	}
}

func TestAHeaderThisVersionCannotReadStartsANewTrace(t *testing.T) {
	// Joining a trace from a header nobody could parse would put two unrelated
	// traces together, which is worse than starting one.
	for _, header := range []string{
		"",
		"not a header",
		"01-aabb-ccdd-01",
		"00-00000000000000000000000000000000-1111111111111111-01",
		"00-11111111111111111111111111111111-0000000000000000-01",
		"00-zzzz-1111111111111111-01",
	} {
		if _, ok := ParseTraceparent(header); ok {
			t.Fatalf("%q should not read as a trace", header)
		}
		started := ContinueOrStart(header)
		if len(started.ParentSpanID) != 0 {
			t.Fatalf("%q produced a parent from nothing", header)
		}
		if len(started.TraceID) != 16 {
			t.Fatalf("%q did not start a usable trace", header)
		}
	}
}

func TestASpanCarriesItsContextOntoTheWire(t *testing.T) {
	root := RootContext()
	child := root.Child()
	capture := Span(child, "GET /orders", "server", 1000, 12)

	if capture.item.Envelope.TraceId == nil ||
		!bytes.Equal(*capture.item.Envelope.TraceId, child.TraceID) {
		t.Fatal("the span left its trace behind")
	}
	if capture.item.Envelope.SpanId == nil ||
		!bytes.Equal(*capture.item.Envelope.SpanId, child.SpanID) {
		t.Fatal("the span has no identity")
	}
	if capture.item.Span.ParentSpanId == nil ||
		!bytes.Equal(*capture.item.Span.ParentSpanId, root.SpanID) {
		t.Fatal("the span does not name its parent")
	}
	if capture.item.Span.DurationMs != 12 {
		t.Fatal("the duration did not travel")
	}
}

func TestAFailedSpanNamesTheErrorThatExplainsIt(t *testing.T) {
	context := RootContext()
	errorID := NewEventID()
	capture := Span(context, "charge", "client", 1, 5).Failed(errorID)
	if capture.item.Span.Status != "error" {
		t.Fatal("a failed span reported ok")
	}
	if capture.item.Span.ErrorEventId == nil ||
		!bytes.Equal(*capture.item.Span.ErrorEventId, errorID) {
		t.Fatal("the link to the error did not travel")
	}
}

func TestAnErrorCarriesItsFramesAndSaysWhichAreTheApplicationsOwn(t *testing.T) {
	capture := Error("Timeout", "took too long", false).WithFrames([]Frame{
		LibraryFrame("net/http", "serve"),
		InAppFrame("checkout", "charge").At("/app/checkout.go", 40),
	})
	frames := capture.item.Error.Frames
	if len(frames) != 2 {
		t.Fatalf("expected two frames, got %d", len(frames))
	}
	if frames[0].InApp {
		t.Fatal("a library frame reported as the application's own")
	}
	if !frames[1].InApp {
		t.Fatal("an application frame reported as a library's")
	}
	if frames[1].Line == nil || *frames[1].Line != 40 {
		t.Fatal("the line did not travel")
	}
}

func TestAnErrorInsideASpanJoinsItsTrace(t *testing.T) {
	context := RootContext()
	capture := Error("Timeout", "slow", false).InSpan(context)
	if capture.item.Envelope.TraceId == nil ||
		!bytes.Equal(*capture.item.Envelope.TraceId, context.TraceID) {
		t.Fatal("the error is not in the trace")
	}
}

func TestASpanNeverSealsABatch(t *testing.T) {
	// A batch sealed by a span would send one span at a time under load, which
	// is the opposite of what a high-volume diagnostic path needs.
	if Span(RootContext(), "op", "internal", 1, 1).critical {
		t.Fatal("a span sealed its batch")
	}
	// An unhandled error still does.
	if !Error("Timeout", "slow", false).critical {
		t.Fatal("an unhandled error did not seal its batch")
	}
}

func TestTwoTracesNeverShareAnIdentifier(t *testing.T) {
	seen := map[string]bool{}
	for range 200 {
		id := string(RootContext().TraceID)
		if seen[id] {
			t.Fatal("two traces took one identifier")
		}
		seen[id] = true
	}
}
