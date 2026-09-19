package tallyowl

import (
	"crypto/rand"
	"fmt"
	"sync/atomic"
	"time"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
)

// SDKName and SDKVersion travel on every envelope, so a support conversation
// can tell which client produced an item.
const (
	SDKName    = "tallyowl-driver-go"
	SDKVersion = "0.2.0"
)

// ProtocolVersion is the wire protocol this driver speaks. It travels on every
// batch, and a collector and the head each accept it and the version before
// it. It changes when the meaning of the wire changes, which is not the same
// as SDKVersion changing. See docs/DECISIONS.md D31.
const ProtocolVersion = 1

// Capture is one telemetry item, before the driver seals it into a batch.
type Capture struct {
	item     api.TelemetryItem
	critical bool
}

func nowMs() int64 { return time.Now().UnixMilli() }

var lastID atomic.Int64

// NewEventID makes a UUIDv7: a millisecond timestamp followed by random bytes,
// so an ID sorts by time and does not repeat inside one millisecond.
func NewEventID() []byte {
	out := make([]byte, 16)
	ms := nowMs()
	out[0] = byte(ms >> 40)
	out[1] = byte(ms >> 32)
	out[2] = byte(ms >> 24)
	out[3] = byte(ms >> 16)
	out[4] = byte(ms >> 8)
	out[5] = byte(ms)
	if _, err := rand.Read(out[6:]); err != nil {
		// A failure of the system random source is not something a telemetry
		// driver can fix, and a repeated ID would suppress a later legitimate
		// event, so fall back to a counter rather than to a constant.
		n := lastID.Add(1)
		for i := 6; i < 16; i++ {
			out[i] = byte(n >> uint(8*(i-6)))
		}
	}
	out[6] = (out[6] & 0x0f) | 0x70
	out[8] = (out[8] & 0x3f) | 0x80
	return out
}

func newEnvelope(kind api.TelemetryKind) api.Envelope {
	return api.Envelope{
		EventId:       NewEventID(),
		Kind:          kind,
		SchemaVersion: 1,
		OccurredAt:    api.Timestamp(nowMs()),
		// The collector stamps the receive time and the tenancy. A driver that
		// set them would be claiming something it cannot know.
		SdkName:    SDKName,
		SdkVersion: SDKVersion,
		Properties: api.PropertyList{},
	}
}

// Event is a named product or behavior event.
func Event(name string) *Capture {
	payload := api.EventPayload{Name: name}
	return &Capture{item: api.TelemetryItem{
		Envelope: newEnvelope("event"),
		Event:    &payload,
	}}
}

// PageView is a page view or a screen view.
func PageView(route string) *Capture {
	payload := api.PageViewPayload{Route: route}
	return &Capture{item: api.TelemetryItem{
		Envelope: newEnvelope("page-view"),
		PageView: &payload,
	}}
}

// SessionStart begins a session. The client library issues the ID; a person
// cannot select it. See D11.
func SessionStart(sessionID string) *Capture {
	payload := api.SessionStartPayload{}
	c := &Capture{item: api.TelemetryItem{
		Envelope:     newEnvelope("session-start"),
		SessionStart: &payload,
	}}
	return c.WithSession(sessionID)
}

// SessionEnd closes a session and says why.
func SessionEnd(sessionID string, reason string) *Capture {
	payload := api.SessionEndPayload{Reason: reason}
	c := &Capture{item: api.TelemetryItem{
		Envelope:   newEnvelope("session-end"),
		SessionEnd: &payload,
	}}
	return c.WithSession(sessionID)
}

// Conversion records a business outcome. Money travels as an exact decimal and
// never as a float.
func Conversion(goal string, value *Value, currency string) *Capture {
	payload := api.ConversionPayload{Goal: goal}
	if value != nil && value.Kind == KindDecimal {
		d := value.Decimal
		payload.Value = &d
	}
	if currency != "" {
		c := currency
		payload.Currency = &c
	}
	return &Capture{
		item: api.TelemetryItem{
			Envelope:   newEnvelope("conversion"),
			Conversion: &payload,
		},
		// A conversion is the first priority class in DELIVERY.md section 8,
		// so it seals its batch rather than waiting behind page views.
		critical: true,
	}
}

// Order records a business outcome that names the order it came from.
//
// **This is the idempotent form and an application with orders should use it.**
// A checkout that retried, a webhook that arrived twice, and a person who
// refreshed the receipt page all produce the same order, and TallyOwl counts one
// conversion and one value for one goal and order pair. Without an order
// identifier a repeat is a second conversion, because nothing says otherwise.
func Order(goal string, value *Value, currency, orderID string) *Capture {
	c := Conversion(goal, value, currency)
	if orderID != "" {
		id := orderID
		c.item.Conversion.OrderId = &id
	}
	return c
}

// Campaign is the set of parameters a marketing link carries.
//
// An application reads these out of the address a person arrived at. It does
// not name a channel: TallyOwl classifies the channel from the source, the
// medium, and the referring site, because a producer that could name its own
// channel could put paid traffic in the organic column.
type Campaign struct {
	Source   string
	Medium   string
	Name     string
	Term     string
	Content  string
	ClickID  string
	Referrer string
	Landing  string
}

func (c Campaign) parameters() api.CampaignParameters {
	out := api.CampaignParameters{}
	set := func(value string, into **string) {
		if value != "" {
			held := value
			*into = &held
		}
	}
	set(c.Source, &out.Source)
	set(c.Medium, &out.Medium)
	set(c.Name, &out.Campaign)
	set(c.Term, &out.Term)
	set(c.Content, &out.Content)
	set(c.ClickID, &out.ClickId)
	return out
}

// CampaignTouch records one touch: somebody arrived from somewhere.
//
// A touch with no campaign and no referrer is a direct arrival, and it is worth
// sending: an attribution model that skips direct touches can only skip one it
// was told about.
func CampaignTouch(campaign Campaign) *Capture {
	payload := api.CampaignTouchPayload{Campaign: campaign.parameters()}
	if campaign.Referrer != "" {
		referrer := campaign.Referrer
		payload.Referrer = &referrer
	}
	if campaign.Landing != "" {
		landing := campaign.Landing
		payload.LandingRoute = &landing
	}
	return &Capture{item: api.TelemetryItem{
		Envelope:      newEnvelope("campaign-touch"),
		CampaignTouch: &payload,
	}}
}

// CampaignCost imports what a campaign cost over a period.
//
// It is a separate typed import rather than a property on an event, because a
// return query must not need a cost value on every conversion. DATA_MODEL.md
// section 3.6.
func CampaignCost(
	campaign, platform string,
	cost Value,
	currency string,
	periodStart, periodEnd int64,
) (*Capture, error) {
	if cost.Kind != KindDecimal {
		return nil, fmt.Errorf(
			"a campaign cost is an exact decimal and this one is a %s. Money never travels as a float",
			cost.Kind)
	}
	payload := api.CampaignCostPayload{
		Campaign:    campaign,
		Cost:        cost.Decimal,
		Currency:    currency,
		PeriodStart: api.Timestamp(periodStart),
		PeriodEnd:   api.Timestamp(periodEnd),
	}
	if platform != "" {
		held := platform
		payload.Platform = &held
	}
	return &Capture{item: api.TelemetryItem{
		Envelope:     newEnvelope("campaign-cost"),
		CampaignCost: &payload,
	}}, nil
}

// WithConsent attaches the consent state this item was collected under.
//
// TallyOwl stores it and does not act on it by default. D30: consent applies to
// personal data, TallyOwl does not guess a jurisdiction, and the applicable
// collection policy decides what a denial means. Storing the state is what lets
// a later policy act on data that arrived before it.
func (c *Capture) WithConsent(marketing, analytics string) *Capture {
	c.item.Envelope.Consent = &api.Consent{
		Marketing: api.ConsentState(marketing),
		Analytics: api.ConsentState(analytics),
	}
	return c
}

// Identify links an anonymous timeline to a known end user, from this point.
//
// The trusted app backend supplies the known identifier; TallyOwl never derives
// one. DATA_MODEL.md section 3.5.
//
// The anonymous identifier goes on the envelope, because that is what the link
// is from: an Identify with no anonymous identifier links nothing, and the
// caller has to attach one with WithAnonymous.
func Identify(endUserID string) *Capture {
	payload := api.IdentifyPayload{EndUserId: endUserID}
	c := &Capture{item: api.TelemetryItem{
		Envelope: newEnvelope("identify"),
		Identify: &payload,
	}}
	return c.WithEndUser(endUserID)
}

// Alias merges two known identifiers.
//
// It is an explicit, auditable merge edge and it does not rewrite raw events.
// A query follows the edge; the stored rows keep what they were sent with.
func Alias(fromID, toID string) *Capture {
	payload := api.AliasPayload{FromId: fromID, ToId: toID}
	return &Capture{item: api.TelemetryItem{
		Envelope: newEnvelope("alias"),
		Alias:    &payload,
	}}
}

// Group associates the current end user with an organization, account, or team.
func Group(groupID, groupKind string) *Capture {
	payload := api.GroupPayload{GroupId: groupID}
	if groupKind != "" {
		k := groupKind
		payload.GroupKind = &k
	}
	return &Capture{item: api.TelemetryItem{
		Envelope: newEnvelope("group"),
		Group:    &payload,
	}}
}

// WithEndUser attaches the known end-user identifier this item belongs to.
func (c *Capture) WithEndUser(endUserID string) *Capture {
	s := endUserID
	c.item.Envelope.EndUserId = &s
	return c
}

// WithAnonymous attaches the project-scoped anonymous identifier.
//
// An anonymous identifier is random and belongs to one project. The same text
// in two projects is two different people, and TallyOwl treats it that way.
func (c *Capture) WithAnonymous(anonymousID string) *Capture {
	s := anonymousID
	c.item.Envelope.AnonymousId = &s
	return c
}

// Error records an error occurrence. The producer never supplies a group; the
// projector computes the fingerprint. See D39.
func Error(errorType, message string, handled bool) *Capture {
	severity := "fatal"
	if handled {
		severity = "error"
	}
	payload := api.ErrorPayload{
		ErrorType: errorType,
		Message:   message,
		Handled:   handled,
		Severity:  severity,
	}
	return &Capture{
		item: api.TelemetryItem{
			Envelope: newEnvelope("error"),
			Error:    &payload,
		},
		// An unhandled error is priority class two, and a handled one is class
		// three. Only the unhandled one seals the batch.
		critical: !handled,
	}
}

// Critical seals the current batch as soon as this item enters it.
func (c *Capture) Critical() *Capture { c.critical = true; return c }

// At sets the producer time.
func (c *Capture) At(occurredAt int64) *Capture {
	c.item.Envelope.OccurredAt = api.Timestamp(occurredAt)
	return c
}

// WithEventID replaces the generated identifier. A browser-supplied ID is
// namespaced or replaced before a batch is sealed; see DELIVERY.md section 2.
func (c *Capture) WithEventID(id []byte) *Capture {
	c.item.Envelope.EventId = id
	return c
}

// WithSession attaches the session this item belongs to.
func (c *Capture) WithSession(sessionID string) *Capture {
	s := api.SessionId(sessionID)
	c.item.Envelope.SessionId = &s
	return c
}

// WithRequest attaches a request correlation ID.
func (c *Capture) WithRequest(requestID string) *Capture {
	s := requestID
	c.item.Envelope.RequestId = &s
	return c
}

// WithTrace attaches a trace ID.
func (c *Capture) WithTrace(traceID []byte) *Capture {
	t := api.TraceId(traceID)
	c.item.Envelope.TraceId = &t
	return c
}

// WithService names the service that produced the item.
func (c *Capture) WithService(name string) *Capture {
	s := name
	c.item.Envelope.ServiceName = &s
	return c
}

// WithRelease names the release that produced the item.
func (c *Capture) WithRelease(release string) *Capture {
	s := release
	c.item.Envelope.Release = &s
	return c
}

// WithProperty adds a typed property from the calling code, at the event site.
func (c *Capture) WithProperty(key string, value Value) *Capture {
	c.item.Envelope.Properties = append(
		c.item.Envelope.Properties, Property(key, value, "client"))
	return c
}

// WithMeasurement adds a number with a unit. A value that is not a number is
// refused rather than converted.
func (c *Capture) WithMeasurement(key string, value Value, unit string) (*Capture, error) {
	m, err := Measurement(key, value, unit)
	if err != nil {
		return c, err
	}
	if c.item.Envelope.Measurements == nil {
		c.item.Envelope.Measurements = &api.MeasurementList{}
	}
	*c.item.Envelope.Measurements = append(*c.item.Envelope.Measurements, m)
	return c, nil
}

// MetricPoint is the metric this capture carries, or nil when it carries
// something else. A host reads it to check what a meter produced without
// keeping a second tally that could drift.
func (c *Capture) MetricPoint() *api.MetricPointPayload { return c.item.MetricPoint }

// EventID is the identifier this item travels under.
func (c *Capture) EventID() []byte { return c.item.Envelope.EventId }

// payloadName reports which payload field an item carries, for a message a
// person reads. An item with none, or with more than one, is a fault.
func payloadName(item api.TelemetryItem) (string, error) {
	found := ""
	count := 0
	check := func(name string, present bool) {
		if present {
			found = name
			count++
		}
	}
	check("event", item.Event != nil)
	check("page-view", item.PageView != nil)
	check("session-start", item.SessionStart != nil)
	check("session-end", item.SessionEnd != nil)
	check("interaction", item.Interaction != nil)
	check("feature-exposure", item.FeatureExposure != nil)
	check("identify", item.Identify != nil)
	check("alias", item.Alias != nil)
	check("group", item.Group != nil)
	check("conversion", item.Conversion != nil)
	check("error", item.Error != nil)
	check("span", item.Span != nil)
	check("metric-point", item.MetricPoint != nil)
	check("campaign-touch", item.CampaignTouch != nil)
	check("campaign-cost", item.CampaignCost != nil)

	declared := string(item.Envelope.Kind)
	switch {
	case count > 1:
		return "", fmt.Errorf("one item carries more than one kind of detail")
	case count == 0 && declared == "session-heartbeat":
		return declared, nil
	case count == 0:
		return "", fmt.Errorf("one item says it is %s and carries no %s details", declared, declared)
	case found != declared:
		return "", fmt.Errorf("one item says it is %s and carries %s details instead", declared, found)
	}
	return found, nil
}
