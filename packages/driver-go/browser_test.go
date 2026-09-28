package tallyowl

import (
	"bytes"
	"testing"

	ingest "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-ingest-api"
)

func browserItem(kind string, session, anonymous string) ingest.TelemetryItem {
	envelope := ingest.Envelope{
		EventId:       bytes.Repeat([]byte{7}, 16),
		Kind:          ingest.TelemetryKind(kind),
		SchemaVersion: 1,
		OccurredAt:    1_700_000_000_000,
		SdkName:       "tallyowl-browser",
		SdkVersion:    "0.2.1",
		Properties:    ingest.PropertyList{},
	}
	if session != "" {
		id := ingest.SessionId(session)
		envelope.SessionId = &id
	}
	if anonymous != "" {
		envelope.AnonymousId = &anonymous
	}
	return ingest.TelemetryItem{Envelope: envelope}
}

func TestEveryKindTheBrowserPackageProducesIsTaken(t *testing.T) {
	// A host that wrote this by hand refused an interaction and a heartbeat,
	// and the browser saw only a smaller accepted count.
	items := map[string]ingest.TelemetryItem{}

	item := browserItem("event", "s1", "")
	item.Event = &ingest.EventPayload{Name: "clicked"}
	items["event"] = item

	item = browserItem("page-view", "s1", "")
	item.PageView = &ingest.PageViewPayload{Route: "/cart"}
	items["page-view"] = item

	item = browserItem("interaction", "s1", "")
	item.Interaction = &ingest.InteractionPayload{Target: "buy", Action: "click"}
	items["interaction"] = item

	items["session-heartbeat"] = browserItem("session-heartbeat", "s1", "")

	item = browserItem("session-start", "s1", "")
	item.SessionStart = &ingest.SessionStartPayload{}
	items["session-start"] = item

	item = browserItem("error", "s1", "")
	item.Error = &ingest.ErrorPayload{
		ErrorType: "TypeError", Message: "x is undefined", Severity: "fatal",
		Frames: []ingest.StackFrame{{}},
	}
	items["error"] = item

	for kind, item := range items {
		capture, err := FromBrowser(item)
		if err != nil {
			t.Errorf("a %s item was refused: %v", kind, err)
			continue
		}
		if got := string(capture.item.Envelope.Kind); got != kind {
			t.Errorf("a %s item arrived as %s", kind, got)
		}
	}
	if got := mustFromBrowser(t, items["interaction"]).item.Interaction; got == nil || got.Target != "buy" {
		t.Errorf("the interaction detail did not survive: %+v", got)
	}
	unhandled := mustFromBrowser(t, items["error"])
	if len(unhandled.item.Error.Frames) != 1 {
		t.Error("the stack frames the browser recorded were discarded")
	}
	if !unhandled.critical {
		t.Error("an unhandled browser error should seal the batch, as the driver's own does")
	}
	if mustFromBrowser(t, items["event"]).critical {
		t.Error("an ordinary browser event sealed the batch")
	}
}

func TestTheEnvelopeFieldsABrowserSetsSurvive(t *testing.T) {
	item := browserItem("event", "s1", "anon-9")
	item.Event = &ingest.EventPayload{Name: "clicked"}
	trace := ingest.TraceId(bytes.Repeat([]byte{3}, 16))
	endUser := "user-4"
	item.Envelope.TraceId = &trace
	item.Envelope.EndUserId = &endUser
	item.Envelope.Consent = &ingest.Consent{Marketing: "denied", Analytics: "granted"}

	envelope := mustFromBrowser(t, item).item.Envelope
	if envelope.AnonymousId == nil || *envelope.AnonymousId != "anon-9" {
		t.Error("the anonymous ID was discarded")
	}
	if envelope.TraceId == nil || !bytes.Equal(*envelope.TraceId, trace) {
		t.Error("the trace ID was discarded, so the browser item cannot join its backend trace")
	}
	if envelope.EndUserId == nil || *envelope.EndUserId != "user-4" {
		t.Error("the end user was discarded")
	}
	if envelope.Consent == nil || envelope.Consent.Marketing != "denied" {
		t.Error("the consent was discarded")
	}
	if envelope.OccurredAt != 1_700_000_000_000 {
		t.Error("the time the browser recorded was replaced")
	}
}

func TestABrowserCannotClaimTenancyOrATrustedOrigin(t *testing.T) {
	item := browserItem("event", "s1", "")
	item.Event = &ingest.EventPayload{Name: "clicked"}
	workspace := ingest.WorkspaceId(bytes.Repeat([]byte{1}, 16))
	project := ingest.ProjectId(bytes.Repeat([]byte{2}, 16))
	source := ingest.SourceId(bytes.Repeat([]byte{4}, 16))
	received := ingest.Timestamp(5)
	item.Envelope.WorkspaceId = &workspace
	item.Envelope.ProjectId = &project
	item.Envelope.SourceId = &source
	item.Envelope.ReceivedAt = &received
	text := "prod"
	item.Envelope.Properties = ingest.PropertyList{{
		Key:    "environment",
		Value:  ingest.TypedValue{Kind: "text", TextValue: &text},
		Origin: "collector",
	}}

	envelope := mustFromBrowser(t, item).item.Envelope
	if envelope.WorkspaceId != nil || envelope.ProjectId != nil || envelope.SourceId != nil {
		t.Error("tenancy from a browser survived")
	}
	if envelope.ReceivedAt != nil {
		t.Error("a receive time from a browser survived")
	}
	if len(envelope.Properties) != 1 || envelope.Properties[0].Origin != "client" {
		t.Errorf("a browser property kept the origin it claimed: %+v", envelope.Properties)
	}
}

func TestOneBrowserCannotSuppressTheEventsOfAnother(t *testing.T) {
	// Deduplication has project scope. Two browsers that send the same ID must
	// not become one event, and one browser that retries must stay one event.
	first := browserItem("event", "session-a", "")
	first.Event = &ingest.EventPayload{Name: "clicked"}
	second := browserItem("event", "session-b", "")
	second.Event = &ingest.EventPayload{Name: "clicked"}

	a := mustFromBrowser(t, first).EventID()
	again := mustFromBrowser(t, first).EventID()
	b := mustFromBrowser(t, second).EventID()
	if bytes.Equal(a, first.Envelope.EventId) {
		t.Error("the browser's own ID travelled unchanged")
	}
	if !bytes.Equal(a, again) {
		t.Error("a retry of one browser item became a second event")
	}
	if bytes.Equal(a, b) {
		t.Error("two sessions that chose the same ID became one event")
	}
	if len(a) != 16 {
		t.Errorf("an event ID is 16 bytes, got %d", len(a))
	}

	byAnonymous := browserItem("event", "", "anon-1")
	byAnonymous.Event = &ingest.EventPayload{Name: "clicked"}
	if bytes.Equal(mustFromBrowser(t, byAnonymous).EventID(), a) {
		t.Error("an anonymous ID and a session ID share a namespace")
	}

	nobody := browserItem("event", "", "")
	nobody.Event = &ingest.EventPayload{Name: "clicked"}
	one := mustFromBrowser(t, nobody).EventID()
	two := mustFromBrowser(t, nobody).EventID()
	if bytes.Equal(one, two) || bytes.Equal(one, nobody.Envelope.EventId) {
		t.Error("an item with nobody to namespace it to should get a new ID each time")
	}
}

func TestATrustedProducerKeepsItsEventID(t *testing.T) {
	item := browserItem("event", "s1", "")
	item.Event = &ingest.EventPayload{Name: "clicked"}
	capture, err := FromBrowser(item, KeepBrowserEventID())
	if err != nil {
		t.Fatalf("from browser: %v", err)
	}
	if !bytes.Equal(capture.EventID(), item.Envelope.EventId) {
		t.Error("the ID was replaced although the host asked to keep it")
	}
}

func TestAnItemWhoseKindAndDetailDisagreeIsRefused(t *testing.T) {
	item := browserItem("page-view", "s1", "")
	item.Event = &ingest.EventPayload{Name: "clicked"}
	if _, err := FromBrowser(item); err == nil {
		t.Fatal("an item that says one kind and carries another was taken")
	}
}

func mustFromBrowser(t *testing.T, item ingest.TelemetryItem) *Capture {
	t.Helper()
	capture, err := FromBrowser(item)
	if err != nil {
		t.Fatalf("from browser: %v", err)
	}
	return capture
}
