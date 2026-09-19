// Package ledger holds the expected result of a scenario.
//
// The simulator is the source of truth. Before it sends anything, it writes a
// ledger recording the exact expected result of every analysis the test bed
// checks. The tests then run the equivalent TallyOwl query and compare. A
// mismatch is a failure, which makes analytics correctness a test result rather
// than a judgment. See docs/TESTBED.md section 5.
//
// The ledger uses exact decimal text for money. It never uses a floating-point
// comparison for revenue, because 19.99 through a float is 19.989999999999998
// and a revenue total built from that disagrees with the customer's own records.
package ledger

import (
	"encoding/json"
	"fmt"
	"math/big"
	"os"
	"sort"
	"strconv"
)

// Ledger is the whole expected result of one scenario run.
type Ledger struct {
	// Scenario names the scenario, and Seed reproduces it. A result nobody can
	// reproduce is not a result.
	Scenario string `json:"scenario"`
	Seed     int64  `json:"seed"`

	// The time span the scenario covers, in milliseconds since the epoch.
	RangeStart int64 `json:"range_start"`
	RangeEnd   int64 `json:"range_end"`

	// Every project the scenario writes to, so a tenant-isolation assertion has
	// something to compare against.
	Projects []Project `json:"projects"`
}

// Project is the expected result for one application's project.
type Project struct {
	// Credential is what the backend authenticates with. TallyOwl resolves the
	// project from it; the application never sends a project ID.
	Credential string `json:"credential"`

	TotalEvents int `json:"total_events"`

	// CountByKind and CountByName are the trend and breakdown answers.
	CountByKind map[string]int `json:"count_by_kind"`
	CountByName map[string]int `json:"count_by_name"`

	// CountByMinute is the trend a dashboard draws, keyed by bucket start.
	CountByMinute map[string]int `json:"count_by_minute"`

	// ConversionValue is the exact decimal total, as text. Never a float.
	ConversionValue string `json:"conversion_value"`
	ConversionCount int    `json:"conversion_count"`

	// EventIDs is every identifier the scenario produced, in the order it
	// produced them, so an exact lookup has something to look up.
	EventIDs []string `json:"event_ids"`

	// DuplicateEventIDs are identifiers the scenario sent more than once. A
	// primary query counts a logical event once, whatever the physical rows.
	DuplicateEventIDs []string `json:"duplicate_event_ids"`

	// ErrorsByDefect counts the occurrences of each defect the scenario
	// simulated, keyed by the name the scenario gave it.
	//
	// The ledger deliberately does not compute a fingerprint. D39 says the
	// projector computes it and a producer never controls its group, so a
	// ledger that predicted the digest would be asserting that TallyOwl agrees
	// with a second implementation of TallyOwl. What the ledger knows exactly
	// is how many distinct defects there were and how many times each happened,
	// and that is what the assertion compares.
	ErrorsByDefect map[string]int `json:"errors_by_defect"`

	// Traces is the exact shape of each trace: how many spans it holds and how
	// deep it goes. Keyed by the trace identifier as hexadecimal.
	Traces map[string]TraceShape `json:"traces"`

	// Identity is what the identity graph must resolve to after the scenario
	// runs. Phase 8.
	//
	// The scenario knows exactly who did what, because it decided. A person who
	// used three client surfaces has three anonymous identifiers and one known
	// one, and every event on all three belongs to that one person.
	Identity IdentityShape `json:"identity"`

	// Funnel is the step-by-step result the scenario produced, for the
	// sequence a person passes through: a page view, a checkout, and a
	// purchase. Correlated by end user, so the anonymous prefix of a timeline
	// counts as the person it turned out to be.
	Funnel []FunnelStepShape `json:"funnel"`

	// Retention is the cohort-by-period matrix. Every person signs up in the
	// same period, so there is one cohort, and the counts say how many returned
	// in each later period.
	Retention RetentionShape `json:"retention"`

	// TimelineByEndUser is how many items each person's whole timeline holds,
	// across every surface they used. Keyed by the known end-user identifier.
	TimelineByEndUser map[string]int `json:"timeline_by_end_user"`

	// funnelSeen remembers which person has already been counted for which
	// step or period, so a person who did a step twice is counted once. It is
	// not part of the written ledger, which is why it is unexported: what a
	// reader needs is the answer, not the bookkeeping that produced it.
	funnelSeen map[string]bool

	// Attribution is what each attribution model must credit each campaign
	// with, after the marketing journey. Phase 9.
	//
	// The scenario knows the answer because it built the journey to have one:
	// the touches sit exactly one decay half-life apart, so every model divides
	// the conversion value into whole units and there is nothing to round. See
	// the marketing journey in the simulator.
	Attribution AttributionShape `json:"attribution"`

	// Metrics is what each metric series the application published must hold.
	// Keyed by the metric name.
	//
	// The ledger predicts the value because the application aggregated it in
	// process: a counter that was incremented 12 times holds 12, whatever the
	// batching and whatever the collector merged. That is the assertion worth
	// making, because it is the one a merge or a budget could break.
	Metrics map[string]MetricShape `json:"metrics"`
}

// IdentityShape is what the identity graph must hold. Phase 8.
type IdentityShape struct {
	// AnonymousByEndUser lists every anonymous identifier that resolves to one
	// known end user. One for each client surface that person used.
	AnonymousByEndUser map[string][]string `json:"anonymous_by_end_user"`
	// KnownEndUsers is how many distinct people the scenario produced.
	KnownEndUsers int `json:"known_end_users"`
	// Surfaces is how many client surfaces each of them used.
	Surfaces int `json:"surfaces"`
}

// FunnelStepShape is one step's expected result.
//
// The ledger predicts the count of distinct people, not of events: a funnel
// counts a person once however many times they did a step. That is the
// assertion worth making, because it is the one a correlation defect breaks.
type FunnelStepShape struct {
	Name    string `json:"name"`
	Reached int    `json:"reached"`
}

// RetentionShape is the expected cohort-by-period matrix.
type RetentionShape struct {
	// Period is `day`, `week`, or `month`.
	Period string `json:"period"`
	// Periods is how many columns the matrix has.
	Periods int `json:"periods"`
	// CohortSize is how many people entered the one cohort.
	CohortSize int `json:"cohort_size"`
	// ReturnedByPeriod is how many of them came back in each period, index 0
	// being the period they signed up in.
	ReturnedByPeriod []int `json:"returned_by_period"`
}

// AttributionShape is what every attribution model must produce.
//
// It is exact decimal text, never a float, for the same reason the revenue
// total is: a credited value that arrived through a float would disagree with
// the revenue it was divided from.
type AttributionShape struct {
	// Goal is the conversion the models are run against.
	Goal string `json:"goal"`
	// LookbackMs is the window the assertion asks for. It has to reach the
	// first touch, or the ledger and the query would be answering two
	// different questions.
	LookbackMs int64 `json:"lookback_ms"`
	// Conversions is how many distinct conversions the journey produced. A
	// repeat delivery of one order is not a second conversion.
	Conversions int `json:"conversions"`
	// OrderRepeats is how many conversion rows carried an order another row
	// already carried. TallyOwl must fold each of them.
	OrderRepeats int `json:"order_repeats"`
	// ByModel maps a model name to the credited value of each campaign, as
	// exact decimal text.
	ByModel map[string]map[string]string `json:"by_model"`
	// CostByCampaign is the spend imported for each campaign.
	CostByCampaign map[string]string `json:"cost_by_campaign"`
	// TouchesByChannel counts the touches of each classified channel. It is
	// what proves the classifier put paid traffic in the paid column.
	TouchesByChannel map[string]int `json:"touches_by_channel"`
}

// RecordCredit adds an exact amount to what one model credits one campaign.
func (p *Project) RecordCredit(model, campaign, amount string) error {
	if p.Attribution.ByModel == nil {
		p.Attribution.ByModel = map[string]map[string]string{}
	}
	held := p.Attribution.ByModel[model]
	if held == nil {
		held = map[string]string{}
		p.Attribution.ByModel[model] = held
	}
	sum, err := addExact(held[campaign], amount)
	if err != nil {
		return err
	}
	held[campaign] = sum
	return nil
}

// RecordCampaignCost adds an exact amount to one campaign's imported spend.
func (p *Project) RecordCampaignCost(campaign, amount string) error {
	if p.Attribution.CostByCampaign == nil {
		p.Attribution.CostByCampaign = map[string]string{}
	}
	sum, err := addExact(p.Attribution.CostByCampaign[campaign], amount)
	if err != nil {
		return err
	}
	p.Attribution.CostByCampaign[campaign] = sum
	return nil
}

// RecordTouch counts one touch of one classified channel.
//
// The scenario names the channel it intends, so a classifier that put a paid
// click in the organic column fails here rather than showing up as a marketing
// budget somebody argues about.
func (p *Project) RecordTouch(channel string) {
	if p.Attribution.TouchesByChannel == nil {
		p.Attribution.TouchesByChannel = map[string]int{}
	}
	p.Attribution.TouchesByChannel[channel]++
}

// addExact adds two decimal texts and returns the exact sum, with no trailing
// zeros. An empty running total starts at zero.
func addExact(running, amount string) (string, error) {
	if running == "" {
		running = "0"
	}
	total, ok := new(big.Rat).SetString(running)
	if !ok {
		return "", fmt.Errorf("the running total %q is not a number", running)
	}
	next, ok := new(big.Rat).SetString(amount)
	if !ok {
		return "", fmt.Errorf("%q is not a number", amount)
	}
	return exactText(total.Add(total, next)), nil
}

// exactText renders a rational as decimal text with no trailing zeros.
//
// It carries eight decimal places rather than the two `ratText` uses for money,
// because a credited value is money divided by a weight and the division
// carries guard digits. A ledger that rounded to two would disagree with an
// exact answer that did not.
func exactText(value *big.Rat) string {
	if value.IsInt() {
		return value.Num().String()
	}
	text := value.FloatString(8)
	for len(text) > 0 && text[len(text)-1] == '0' {
		text = text[:len(text)-1]
	}
	if len(text) > 0 && text[len(text)-1] == '.' {
		text = text[:len(text)-1]
	}
	return text
}

// MetricShape is what one metric name must hold after the scenario runs.
type MetricShape struct {
	Kind string `json:"kind"`
	// Series is how many distinct label sets the application published.
	Series int `json:"series"`
	// Total is the sum of every series value, for a counter or a gauge, or the
	// total observation count for a histogram.
	Total float64 `json:"total"`
	// Sum is the total of the observations, for a histogram only.
	Sum float64 `json:"sum"`
}

// TraceShape is the exact parent and child shape of one trace.
type TraceShape struct {
	Spans    int `json:"spans"`
	MaxDepth int `json:"max_depth"`
	// Errors is how many error occurrences belong to this trace. An error is
	// not a span, and a waterfall shows both.
	Errors int `json:"errors"`
}

// NewProject starts an empty expectation for one credential.
func NewProject(credential string) *Project {
	return &Project{
		Credential:      credential,
		CountByKind:     map[string]int{},
		CountByName:     map[string]int{},
		CountByMinute:   map[string]int{},
		ConversionValue: "0",
		EventIDs:        []string{},
		ErrorsByDefect:  map[string]int{},
		Traces:          map[string]TraceShape{},
		Metrics:         map[string]MetricShape{},
		Identity: IdentityShape{
			AnonymousByEndUser: map[string][]string{},
		},
		TimelineByEndUser: map[string]int{},
	}
}

// RecordSurface remembers that one person used one client surface under one
// anonymous identifier.
//
// A person who used three surfaces has three of these and one known
// identifier, and every event on all three belongs to that one person. An
// identity defect that failed to join them would show as three people here.
func (p *Project) RecordSurface(endUserID, anonymousID string) {
	held := p.Identity.AnonymousByEndUser[endUserID]
	for _, seen := range held {
		if seen == anonymousID {
			return
		}
	}
	p.Identity.AnonymousByEndUser[endUserID] = append(held, anonymousID)
	p.Identity.KnownEndUsers = len(p.Identity.AnonymousByEndUser)
	if len(p.Identity.AnonymousByEndUser[endUserID]) > p.Identity.Surfaces {
		p.Identity.Surfaces = len(p.Identity.AnonymousByEndUser[endUserID])
	}
}

// RecordFunnelStep counts one person as having reached one step.
//
// It is idempotent for one person and one step, because a funnel counts a
// person once however many times they did the step.
func (p *Project) RecordFunnelStep(step int, name, endUserID string) {
	for len(p.Funnel) <= step {
		p.Funnel = append(p.Funnel, FunnelStepShape{})
	}
	p.Funnel[step].Name = name
	key := name + "\x00" + endUserID
	if p.funnelSeen == nil {
		p.funnelSeen = map[string]bool{}
	}
	if p.funnelSeen[key] {
		return
	}
	p.funnelSeen[key] = true
	p.Funnel[step].Reached++
}

// RecordReturn counts one person as having come back in one period.
//
// Also idempotent: two visits in one period are one person on that period.
func (p *Project) RecordReturn(period int, endUserID string) {
	for len(p.Retention.ReturnedByPeriod) <= period {
		p.Retention.ReturnedByPeriod = append(p.Retention.ReturnedByPeriod, 0)
	}
	key := "return\x00" + endUserID + "\x00" + itoa(period)
	if p.funnelSeen == nil {
		p.funnelSeen = map[string]bool{}
	}
	if p.funnelSeen[key] {
		return
	}
	p.funnelSeen[key] = true
	p.Retention.ReturnedByPeriod[period]++
}

// RecordTimelineItem counts one more item on one person's timeline.
func (p *Project) RecordTimelineItem(endUserID string) {
	p.TimelineByEndUser[endUserID]++
}

func itoa(value int) string {
	return strconv.Itoa(value)
}

// RecordMetric adds one expected metric series reading.
func (p *Project) RecordMetric(name, kind string, value, sum float64) {
	shape := p.Metrics[name]
	shape.Kind = kind
	shape.Series++
	shape.Total += value
	shape.Sum += sum
	p.Metrics[name] = shape
}

// Record adds one expected event.
//
// `eventID` is the logical identity. Recording the same identifier twice counts
// it once and remembers the duplicate, because a duplicate delivery must give
// one logical event however many physical rows exist.
func (p *Project) Record(eventID, kind, name string, occurredAt int64) {
	for _, seen := range p.EventIDs {
		if seen == eventID {
			p.DuplicateEventIDs = append(p.DuplicateEventIDs, eventID)
			return
		}
	}
	p.EventIDs = append(p.EventIDs, eventID)
	p.TotalEvents++
	p.CountByKind[kind]++
	p.CountByName[name]++
	bucket := occurredAt - mod(occurredAt, 60_000)
	p.CountByMinute[fmt.Sprintf("%d", bucket)]++
}

// RecordError adds one expected occurrence of a named defect.
func (p *Project) RecordError(defect string) {
	p.ErrorsByDefect[defect]++
}

// RecordSpan adds one expected span to a trace, at a known depth.
func (p *Project) RecordSpan(traceID string, depth int) {
	shape := p.Traces[traceID]
	shape.Spans++
	if depth > shape.MaxDepth {
		shape.MaxDepth = depth
	}
	p.Traces[traceID] = shape
}

// RecordTraceError adds one expected error occurrence to a trace.
func (p *Project) RecordTraceError(traceID string) {
	shape := p.Traces[traceID]
	shape.Errors++
	p.Traces[traceID] = shape
}

// RecordConversionValue adds an exact amount to the revenue total.
func (p *Project) RecordConversionValue(amount string) error {
	total, ok := new(big.Rat).SetString(p.ConversionValue)
	if !ok {
		return fmt.Errorf("the running total %q is not a number", p.ConversionValue)
	}
	next, ok := new(big.Rat).SetString(amount)
	if !ok {
		return fmt.Errorf("%q is not a number", amount)
	}
	total.Add(total, next)
	p.ConversionValue = ratText(total)
	p.ConversionCount++
	return nil
}

// ratText renders an exact rational as decimal text with no trailing zeros. A
// money total that printed as `19.990000` would not compare equal to `19.99`.
func ratText(value *big.Rat) string {
	if value.IsInt() {
		return value.Num().String()
	}
	// Two decimal places covers currency. A scenario that needs more sets it
	// deliberately rather than discovering the limit.
	text := value.FloatString(2)
	for len(text) > 0 && text[len(text)-1] == '0' {
		text = text[:len(text)-1]
	}
	if len(text) > 0 && text[len(text)-1] == '.' {
		text = text[:len(text)-1]
	}
	return text
}

func mod(value, by int64) int64 {
	remainder := value % by
	if remainder < 0 {
		remainder += by
	}
	return remainder
}

// Write puts the ledger on disk, sorted so two runs of one seed produce one
// identical file.
func (l *Ledger) Write(path string) error {
	sort.Slice(l.Projects, func(a, b int) bool {
		return l.Projects[a].Credential < l.Projects[b].Credential
	})
	raw, err := json.MarshalIndent(l, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(path, append(raw, '\n'), 0o644)
}

// Read loads a ledger a test compares against.
func Read(path string) (*Ledger, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var out Ledger
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, err
	}
	return &out, nil
}
