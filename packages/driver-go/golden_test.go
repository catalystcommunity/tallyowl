package tallyowl

// The Go half of the cross-language agreement.
//
// `docs/PLAN.md` Phase 2 says the contract is not trustworthy until every
// maintained language encodes each golden vector to identical bytes. The
// vectors are built in Rust and written to `golden/vectors.json`. This file
// builds the same values in Go and compares.
//
// Every vector name here must appear in the file, and every name in the file
// must appear here. A vector that only one language knows about proves nothing.

import (
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	collector "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
	control "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-control-api"
	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
)

const at int64 = 1_785_628_800_000

type goldenFile struct {
	Vectors []goldenVector `json:"vectors"`
}

type goldenVector struct {
	Name    string `json:"name"`
	Type    string `json:"type"`
	Package string `json:"package"`
	Bytes   string `json:"bytes"`
}

func readGolden(t *testing.T) goldenFile {
	t.Helper()
	path := filepath.Join("..", "..", "golden", "vectors.json")
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("golden/vectors.json could not be read: %v", err)
	}
	var out goldenFile
	if err := json.Unmarshal(raw, &out); err != nil {
		t.Fatalf("golden/vectors.json could not be parsed: %v", err)
	}
	return out
}

func id(fill byte) []byte {
	out := make([]byte, 16)
	for i := range out {
		out[i] = fill
	}
	return out
}

func text(s string) *string { return &s }

func minimalEnvelope() collector.Envelope {
	return collector.Envelope{
		EventId:       id(1),
		Kind:          "event",
		SchemaVersion: 1,
		OccurredAt:    collector.Timestamp(at),
		SdkName:       "tallyowl-driver-rust",
		SdkVersion:    "0.0.0",
		Properties:    collector.PropertyList{},
	}
}

func fullEnvelope(t *testing.T) collector.Envelope {
	t.Helper()
	env := minimalEnvelope()
	observed := collector.Timestamp(at + 1)
	received := collector.Timestamp(at + 2)
	sequence := uint64(42)
	session := collector.SessionId("s-1")
	trace := collector.TraceId(id(3))
	span := collector.SpanId([]byte{4, 4, 4, 4, 4, 4, 4, 4})
	workspace := collector.WorkspaceId(id(8))
	project := collector.ProjectId(id(9))
	source := collector.SourceId(id(7))

	env.ObservedAt = &observed
	env.ReceivedAt = &received
	env.WorkspaceId = &workspace
	env.ProjectId = &project
	env.SourceId = &source
	env.Sequence = &sequence
	env.Release = text("2026.8.1")
	env.ServiceName = text("checkout")
	env.RequestId = text("r-1")
	env.SessionId = &session
	env.EndUserId = text("u-1")
	env.AnonymousId = text("a-1")
	env.TraceId = &trace
	env.SpanId = &span
	env.Consent = &collector.Consent{
		Marketing:     "denied",
		Analytics:     "granted",
		PolicyVersion: text("2026-01"),
	}
	env.Properties = collector.PropertyList{
		Property("region", Text("us-west2"), "collector"),
		Property("attempts", Uint(3), "driver"),
		Property("ratio", Float(0.5), "client"),
	}
	render, err := Measurement("render", Float(12.5), "ms")
	if err != nil {
		t.Fatalf("a float is a number: %v", err)
	}
	measurements := collector.MeasurementList{render}
	env.Measurements = &measurements
	return env
}

// built returns every vector this language produces, by name.
func built(t *testing.T) map[string][]byte {
	t.Helper()
	out := map[string][]byte{}

	typed := func(name string, v Value) {
		out[name] = collector.EncodeTypedValue(v.Wire())
	}
	typed("typed-value-null", Null())
	typed("typed-value-bool", Bool(true))
	typed("typed-value-int", Int(-3))
	typed("typed-value-uint", Uint(3))
	typed("typed-value-float", Float(0.5))
	typed("typed-value-decimal", MustDecimal("19.99"))
	typed("typed-value-text", Text("us-west2"))
	typed("typed-value-bytes", Bytes([]byte{0xde, 0xad, 0xbe, 0xef}))

	out["property-collector-origin"] = collector.EncodeProperty(
		Property("region", Text("us-west2"), "collector"))

	amount, err := Measurement("amount", MustDecimal("-0.01"), "USD")
	if err != nil {
		t.Fatalf("a decimal is a number: %v", err)
	}
	out["measurement-decimal-with-unit"] = collector.EncodeMeasurement(amount)

	out["envelope-minimal"] = collector.EncodeEnvelope(minimalEnvelope())
	out["envelope-full"] = collector.EncodeEnvelope(fullEnvelope(t))

	event := collector.EventPayload{Name: "checkout-started", Route: text("/checkout")}
	out["item-event"] = collector.EncodeTelemetryItem(collector.TelemetryItem{
		Envelope: minimalEnvelope(), Event: &event,
	})

	pageEnvelope := minimalEnvelope()
	pageEnvelope.Kind = "page-view"
	pageView := collector.PageViewPayload{
		Route:     "/pricing",
		PageTitle: text("Pricing"),
		Campaign: &collector.CampaignParameters{
			Source:   text("newsletter"),
			Medium:   text("email"),
			Campaign: text("spring"),
		},
	}
	out["item-page-view-with-campaign"] = collector.EncodeTelemetryItem(collector.TelemetryItem{
		Envelope: pageEnvelope, PageView: &pageView,
	})

	conversionEnvelope := minimalEnvelope()
	conversionEnvelope.Kind = "conversion"
	money := MustDecimal("19.99").Decimal
	conversion := collector.ConversionPayload{
		Goal: "purchase", Value: &money, Currency: text("USD"),
	}
	out["item-conversion-exact-money"] = collector.EncodeTelemetryItem(collector.TelemetryItem{
		Envelope: conversionEnvelope, Conversion: &conversion,
	})

	errorEnvelope := minimalEnvelope()
	errorEnvelope.Kind = "error"
	line := uint64(42)
	errorPayload := collector.ErrorPayload{
		ErrorType: "TypeError",
		Message:   "x is not a function",
		Handled:   false,
		Severity:  "fatal",
		Runtime:   text("node"),
		Frames: []collector.StackFrame{{
			Module:   text("checkout"),
			Function: text("submit"),
			Line:     &line,
			InApp:    true,
		}},
	}
	out["item-error-with-frames"] = collector.EncodeTelemetryItem(collector.TelemetryItem{
		Envelope: errorEnvelope, Error: &errorPayload,
	})

	heartbeatEnvelope := minimalEnvelope()
	heartbeatEnvelope.Kind = "session-heartbeat"
	out["item-session-heartbeat-no-payload"] = collector.EncodeTelemetryItem(
		collector.TelemetryItem{Envelope: heartbeatEnvelope})

	a := collector.EventPayload{Name: "a"}
	b := collector.EventPayload{Name: "b"}
	compression := collector.Compression("zstd")
	out["batch-two-items"] = collector.EncodeBatch(collector.Batch{
		BatchId: id(2),
		Items: []collector.TelemetryItem{
			{Envelope: minimalEnvelope(), Event: &a},
			{Envelope: minimalEnvelope(), Event: &b},
		},
		SealedAt:    collector.Timestamp(at),
		Compression: &compression,
	})

	deduplicated := false
	out["commit-batch-response-with-rejection"] = collector.EncodeCommitBatchResponse(
		collector.CommitBatchResponse{
			BatchId:          id(2),
			Accepted:         1,
			CommittedAt:      collector.Timestamp(at),
			SatisfiedPolicy:  "local-one",
			CommitWatermark:  7,
			ProtocolVersion:  1,
			ProjectorVersion: 1,
			Rejected: []collector.RejectedItem{{
				EventId: id(5),
				Code:    "invalid-argument",
				Message: "This item carries no project.",
			}},
			Deduplicated: &deduplicated,
		})

	out["service-error-retryable"] = collector.EncodeServiceError(collector.ServiceError{
		Code:      "unavailable",
		Message:   "We could not reach the durable store.",
		Retryable: true,
	})

	ingestEvent := ingest.EventPayload{Name: "checkout-started", Route: text("/checkout")}
	out["capture-request-one-event"] = ingest.EncodeCaptureRequest(ingest.CaptureRequest{
		Items: []ingest.TelemetryItem{{
			Envelope: ingest.Envelope{
				EventId:       id(1),
				Kind:          "event",
				SchemaVersion: 1,
				OccurredAt:    ingest.Timestamp(at),
				SdkName:       "tallyowl-browser",
				SdkVersion:    "0.0.0",
				Properties:    ingest.PropertyList{},
			},
			Event: &ingestEvent,
		}},
	})

	timeRange := control.TimeRange{
		RangeStart: control.Timestamp(at),
		RangeEnd:   control.Timestamp(at + 3_600_000),
		Basis:      "occurred_at",
	}
	scan := control.ScanNode{Scan: "events", ProjectId: id(9), Range: timeRange}
	scanBox := control.QueryNodeBox{Node: "scan", Scan: &scan}
	out["query-node-scan"] = control.EncodeQueryNodeBox(scanBox)

	fieldRef := control.FieldRef{Name: "route", ValueType: text("text")}
	fieldNode := control.ExpressionNode{Expression: "field", Field: &fieldRef}
	literalValue := Text("/pricing").Wire()
	literalNode := control.ExpressionNode{
		Expression: "literal",
		Literal: &control.TypedValue{
			Kind:      control.TypedValueKind(literalValue.Kind),
			TextValue: literalValue.TextValue,
		},
	}
	compare := control.CompareExpr{
		Compare: "eq",
		Left:    control.EncodeExpressionNode(fieldNode),
		Right:   control.EncodeExpressionNode(literalNode),
	}
	out["expression-compare"] = control.EncodeExpressionNode(
		control.ExpressionNode{Expression: "compare", Compare: &compare})

	interval := control.Interval{FixedMs: func() *control.DurationMs {
		v := control.DurationMs(60_000)
		return &v
	}()}
	aggregate := control.AggregateNode{
		Dimensions: []control.Dimension{},
		Measures:   []control.Measure{{Kind: "count", Alias: "events"}},
		Interval:   &interval,
		Input:      control.EncodeQueryNodeBox(scanBox),
	}
	aggregateBox := control.QueryNodeBox{Node: "aggregate", Aggregate: &aggregate}
	node := control.QueryNodeRef(control.EncodeQueryNodeBox(aggregateBox))
	out["query-request-trend"] = control.EncodeQueryRequest(control.QueryRequest{
		AlgebraVersion: 1,
		Consistency:    "committed",
		AllowPartial:   false,
		Form:           "node",
		Node:           &node,
	})

	return out
}

func TestEveryGoldenVectorEncodesToTheSameBytes(t *testing.T) {
	golden := readGolden(t)
	made := built(t)

	if len(golden.Vectors) == 0 {
		t.Fatal("golden/vectors.json holds no vectors")
	}

	for _, vector := range golden.Vectors {
		bytes, ok := made[vector.Name]
		if !ok {
			t.Errorf("this language does not build the vector `%s`", vector.Name)
			continue
		}
		if got := hex.EncodeToString(bytes); got != vector.Bytes {
			t.Errorf("`%s` (%s):\n  Rust: %s\n  Go:   %s",
				vector.Name, vector.Type, vector.Bytes, got)
		}
		delete(made, vector.Name)
	}

	for name := range made {
		t.Errorf("this language builds `%s` and the golden file does not hold it", name)
	}
}

func TestEveryValueKindSurvivesAWriteAndARead(t *testing.T) {
	values := []Value{
		Null(), Bool(true), Int(-3), Uint(9), Float(0.5),
		MustDecimal("19.99"), Text("us-west2"), Bytes([]byte{1, 2, 3}),
	}
	for _, value := range values {
		back, err := ReadValue(value.Wire())
		if err != nil {
			t.Fatalf("%s did not read back: %v", value.Kind, err)
		}
		if back.Kind != value.Kind || back.String() != value.String() {
			t.Errorf("%s came back as %s (%q against %q)",
				value.Kind, back.Kind, back.String(), value.String())
		}
	}
}

func TestAnUnsignedValueStaysUnsigned(t *testing.T) {
	// This is the distinction the earlier bare choice could not carry in a
	// dynamically typed language. It is the reason the shape changed.
	wire := Uint(9).Wire()
	if wire.Kind != "uint" {
		t.Fatalf("an unsigned value travels as %q", wire.Kind)
	}
	if wire.IntValue != nil {
		t.Error("an unsigned value carried a signed number as well")
	}
	if wire.UintValue == nil || *wire.UintValue != 9 {
		t.Error("an unsigned value did not carry its number")
	}
}

func TestAKindThatNamesAnAbsentValueIsRefused(t *testing.T) {
	wire := Text("x").Wire()
	wire.TextValue = nil
	if _, err := ReadValue(wire); err == nil {
		t.Error("a value that says it holds text and carries none was accepted")
	}
}

func TestMoneyKeepsItsExactDigits(t *testing.T) {
	for _, want := range []string{"19.99", "-0.01", "0.1", "1000", "-12345.6789"} {
		value := MustDecimal(want)
		if got := value.String(); got != want {
			t.Errorf("%q came back as %q", want, got)
		}
	}
	if _, err := Decimal("not a number"); err == nil {
		t.Error("text that is not a number was accepted as one")
	}
}

func TestAMeasurementRefusesAValueThatIsNotANumber(t *testing.T) {
	if _, err := Measurement("duration", Float(1.5), "ms"); err != nil {
		t.Errorf("a float is a number: %v", err)
	}
	if _, err := Measurement("name", Text("x"), ""); err == nil {
		t.Error("text was accepted as a measurement")
	}
}
