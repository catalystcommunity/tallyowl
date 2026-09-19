// Counter, gauge, and histogram instruments for an application.
//
// # What this is, and what it is not
//
// TallyOwl's own services expose operational instruments through a registry
// with strict naming rules. This is the other side: the instruments an
// application declares. An application names its own metrics, so no prefix rule
// applies, and docs/DATA_MODEL.md section 3.4 permits a request, trace,
// session, or end-user ID as a label, so no label is forbidden either. What
// that section does require is that TallyOwl never silently puts a
// high-cardinality series in an overflow series, and that is what the budget
// below does: it keeps every series it admitted correct, and refuses a new one
// in the open rather than folding it into a bucket nobody asked for.
//
// # Aggregation happens here
//
// An application calls Add a million times and the meter sends one point for
// each series in each period. The driver batches; the meter aggregates.
//
// # Restart and reset
//
// A cumulative counter reports the value since StartAt, and StartAt is when the
// meter created the series. A restarted process therefore reports a fresh
// StartAt beside a value that begins again at zero, and that pair is what makes
// a reset visible: a lower value with the same StartAt is a defect, and a lower
// value with a later StartAt is a restart.
package tallyowl

import (
	"fmt"
	"sort"
	"strings"
	"sync"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
)

// Temporality says whether a point reports the total since the meter started,
// or what happened inside one period.
const (
	// Cumulative reports the total since StartAt. A restart is visible as a new
	// StartAt.
	Cumulative = "cumulative"
	// Delta reports what happened since the previous snapshot, and resets.
	Delta = "delta"
)

// Budget is what a meter refuses, and when. These are the application-side half
// of the budgets in docs/DATA_MODEL.md section 3.4. The collector enforces the
// same shape at the trust boundary; this side exists so that an application
// learns at the call site, where the label that caused it is still in scope.
type Budget struct {
	// MaxSeriesForEachMetric bounds the active series for one metric name. A
	// new series past this is refused.
	MaxSeriesForEachMetric int
	// MaxLabels bounds the labels on one series.
	MaxLabels int
	// MaxLabelValueBytes bounds one label value.
	MaxLabelValueBytes int
}

// DefaultBudget is enough for a per-route or per-status breakdown of a large
// application, and small enough that a label holding a request ID reaches it in
// seconds rather than filling memory quietly.
func DefaultBudget() Budget {
	return Budget{
		MaxSeriesForEachMetric: 2000,
		MaxLabels:              16,
		MaxLabelValueBytes:     256,
	}
}

// Labels are the labels of one series. A meter sorts them, so one series has
// one key.
type Labels map[string]string

func (l Labels) key() string {
	if len(l) == 0 {
		return ""
	}
	names := make([]string, 0, len(l))
	for name := range l {
		names = append(names, name)
	}
	sort.Strings(names)
	var b strings.Builder
	for _, name := range names {
		b.WriteString(name)
		b.WriteByte('\x00')
		b.WriteString(l[name])
		b.WriteByte('\x00')
	}
	return b.String()
}

// Pressure is how much a meter refused, and why. An application reads this to
// find out that a label it added is costing it series.
type Pressure struct {
	SeriesRefused uint64
	LabelsRefused uint64
	ValueRefused  uint64
}

type histogramState struct {
	bounds []float64
	counts []uint64
	sum    float64
	count  uint64
}

type seriesState struct {
	labels Labels
	// startAt is when this series began counting. A cumulative point carries
	// it, and a restart is visible because a new process produces a new one.
	startAt int64
	// updatedAt is the last time this series changed.
	updatedAt int64
	value     float64
	histogram *histogramState
	// exemplarTraceID is the trace of one recent observation, so a chart can
	// jump from a point to a trace. The most recent one wins: a reader
	// following an exemplar wants a trace that still exists.
	exemplarTraceID []byte
	dirty           bool
}

type family struct {
	kind        api.MetricKind
	unit        string
	description string
	bounds      []float64
	order       []string
	series      map[string]*seriesState
}

// Meter holds every instrument one application declares.
type Meter struct {
	temporality string
	budget      Budget
	serviceName string
	release     string

	mu              sync.Mutex
	families        map[string]*family
	order           []string
	pressure        Pressure
	periodStartedAt int64
}

// NewMeter returns a cumulative meter. Cumulative is the default because it
// survives a lost batch: a missing delta is a hole in a total that nothing can
// rebuild, and a missing cumulative point costs one sample of resolution.
func NewMeter() *Meter { return NewMeterWithTemporality(Cumulative) }

// NewMeterWithTemporality returns a meter that reports the named temporality.
func NewMeterWithTemporality(temporality string) *Meter {
	return &Meter{
		temporality:     temporality,
		budget:          DefaultBudget(),
		families:        map[string]*family{},
		periodStartedAt: nowMs(),
	}
}

// WithBudget sets the series and label budgets.
func (m *Meter) WithBudget(budget Budget) *Meter { m.budget = budget; return m }

// WithService names the service every point carries. A metric without a service
// is hard to read on a dashboard that holds more than one.
func (m *Meter) WithService(name string) *Meter { m.serviceName = name; return m }

// WithRelease names the release every point carries.
func (m *Meter) WithRelease(release string) *Meter { m.release = release; return m }

// Temporality is what this meter reports.
func (m *Meter) Temporality() string { return m.temporality }

// Pressure reports what this meter refused.
func (m *Meter) Pressure() Pressure {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.pressure
}

// Counter declares a counter. A counter only rises.
func (m *Meter) Counter(name, unit, description string) {
	m.declare(name, "counter", unit, description, nil)
}

// Gauge declares a gauge. A gauge is an observation at a moment, and it can
// fall.
func (m *Meter) Gauge(name, unit, description string) {
	m.declare(name, "gauge", unit, description, nil)
}

// Histogram declares a histogram over explicit upper bounds. The bounds travel
// with every point, so two producers with different bounds stay mergeable: the
// query side merges what it can and says so when it cannot.
func (m *Meter) Histogram(name string, bounds []float64, unit, description string) {
	m.declare(name, "histogram", unit, description, bounds)
}

func (m *Meter) declare(name string, kind api.MetricKind, unit, description string, bounds []float64) {
	sorted := append([]float64(nil), bounds...)
	sort.Float64s(sorted)
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, seen := m.families[name]; seen {
		return
	}
	m.families[name] = &family{
		kind:        kind,
		unit:        unit,
		description: description,
		bounds:      sorted,
		series:      map[string]*seriesState{},
	}
	m.order = append(m.order, name)
}

// Add adds to a counter.
//
// A negative amount is refused rather than applied, because a counter that
// falls makes every rate over it wrong and a reader cannot tell that from a
// restart.
func (m *Meter) Add(name string, labels Labels, delta float64) error {
	if delta < 0 {
		return fmt.Errorf(
			"the counter %q was given a negative amount; a counter only rises, so use a gauge for a value that falls", name)
	}
	return m.record(name, labels, "counter", func(s *seriesState, at int64) {
		s.value += delta
		s.updatedAt = at
	})
}

// Increment adds one to a counter.
func (m *Meter) Increment(name string, labels Labels) error { return m.Add(name, labels, 1) }

// Set puts a gauge at what it is now.
func (m *Meter) Set(name string, labels Labels, value float64) error {
	return m.record(name, labels, "gauge", func(s *seriesState, at int64) {
		s.value = value
		s.updatedAt = at
		// A gauge reports the moment it was read, so the period it covers has
		// no width. A reader that averaged it over a period the value was not
		// held for would report a number nothing observed.
		s.startAt = at
	})
}

// Observe records one observation in a histogram.
func (m *Meter) Observe(name string, labels Labels, value float64) error {
	return m.ObserveInTrace(name, labels, value, nil)
}

// ObserveInTrace records one observation and the trace it came from. This is
// the exemplar: a chart of a latency histogram becomes a way into one slow
// request rather than a shape a reader has to go and look for.
func (m *Meter) ObserveInTrace(name string, labels Labels, value float64, traceID []byte) error {
	return m.record(name, labels, "histogram", func(s *seriesState, at int64) {
		s.updatedAt = at
		if s.histogram != nil {
			s.histogram.sum += value
			s.histogram.count++
			for i, bound := range s.histogram.bounds {
				if value <= bound {
					s.histogram.counts[i]++
				}
			}
		}
		if len(traceID) > 0 {
			s.exemplarTraceID = append([]byte(nil), traceID...)
		}
	})
}

func (m *Meter) record(name string, labels Labels, expected api.MetricKind, apply func(*seriesState, int64)) error {
	at := nowMs()
	m.mu.Lock()
	defer m.mu.Unlock()

	if len(labels) > m.budget.MaxLabels {
		m.pressure.LabelsRefused++
		return fmt.Errorf(
			"a metric series carries %d labels and the budget is %d; send fewer labels, or raise the label budget on this meter",
			len(labels), m.budget.MaxLabels)
	}
	for _, value := range labels {
		if len(value) > m.budget.MaxLabelValueBytes {
			m.pressure.ValueRefused++
			return fmt.Errorf(
				"a metric label value is %d bytes and the budget is %d; send a shorter value, or raise the label-value budget on this meter",
				len(value), m.budget.MaxLabelValueBytes)
		}
	}

	f, declared := m.families[name]
	if !declared {
		return fmt.Errorf(
			"the metric %q was used before it was declared; declare it with Counter, Gauge, or Histogram first", name)
	}
	if f.kind != expected {
		return fmt.Errorf("the metric %q is a %s, and this call records a %s", name, f.kind, expected)
	}

	key := labels.key()
	series, known := f.series[key]
	if !known {
		if len(f.series) >= m.budget.MaxSeriesForEachMetric {
			// Not an overflow series, and not a silent drop. The application is
			// told, at the call site, that this label set costs more series
			// than the budget permits. Every series already admitted keeps
			// counting correctly.
			m.pressure.SeriesRefused++
			return fmt.Errorf(
				"the metric %q would reach %d series and the budget is %d; remove a label that takes many values, or raise the series budget on this meter",
				name, len(f.series)+1, m.budget.MaxSeriesForEachMetric)
		}
		copied := Labels{}
		for k, v := range labels {
			copied[k] = v
		}
		series = &seriesState{labels: copied, startAt: at, updatedAt: at}
		if f.kind == "histogram" {
			series.histogram = &histogramState{
				bounds: append([]float64(nil), f.bounds...),
				counts: make([]uint64, len(f.bounds)),
			}
		}
		f.series[key] = series
		f.order = append(f.order, key)
	}
	apply(series, at)
	series.dirty = true
	return nil
}

// Snapshot takes one reading of every series that moved, as captures the driver
// can send.
//
// A delta meter resets its accumulators here. A cumulative meter does not, so a
// lost batch costs one sample rather than a permanent hole.
func (m *Meter) Snapshot() []*Capture {
	endAt := nowMs()
	m.mu.Lock()
	defer m.mu.Unlock()
	periodStart := m.periodStartedAt
	m.periodStartedAt = endAt

	out := []*Capture{}
	for _, name := range m.order {
		f := m.families[name]
		for _, key := range f.order {
			series := f.series[key]
			if !series.dirty && m.temporality == Delta {
				continue
			}

			// A gauge has no period either way: it reports the moment it was
			// read, so both ends of it are that moment.
			gauge := f.kind == "gauge"
			startAt := series.startAt
			pointEnd := endAt
			switch {
			case gauge:
				startAt = series.updatedAt
				pointEnd = series.updatedAt
			case m.temporality == Delta:
				startAt = periodStart
			}

			payload := api.MetricPointPayload{
				MetricName:  name,
				MetricKind:  f.kind,
				Monotonic:   f.kind == "counter",
				Temporality: m.temporality,
				StartAt:     api.Timestamp(startAt),
				EndAt:       api.Timestamp(pointEnd),
				Labels:      api.PropertyList{},
			}
			if f.unit != "" {
				unit := f.unit
				payload.Unit = &unit
			}
			if f.description != "" {
				description := f.description
				payload.Description = &description
			}
			names := make([]string, 0, len(series.labels))
			for label := range series.labels {
				names = append(names, label)
			}
			sort.Strings(names)
			for _, label := range names {
				payload.Labels = append(payload.Labels,
					Property(label, Text(series.labels[label]), "client"))
			}
			if f.kind == "histogram" {
				payload.HistogramValue = &api.HistogramValue{
					Count:  series.histogram.count,
					Sum:    series.histogram.sum,
					Bounds: append([]float64(nil), series.histogram.bounds...),
					Counts: append([]uint64(nil), series.histogram.counts...),
				}
			} else {
				value := series.value
				payload.NumberValue = &value
			}
			if len(series.exemplarTraceID) > 0 {
				trace := api.TraceId(append([]byte(nil), series.exemplarTraceID...))
				payload.ExemplarTraceId = &trace
			}

			capture := &Capture{item: api.TelemetryItem{
				Envelope:    newEnvelope("metric-point"),
				MetricPoint: &payload,
			}}
			capture.item.Envelope.OccurredAt = api.Timestamp(pointEnd)
			if m.serviceName != "" {
				capture.WithService(m.serviceName)
			}
			if m.release != "" {
				capture.WithRelease(m.release)
			}
			if len(series.exemplarTraceID) > 0 {
				capture.WithTrace(series.exemplarTraceID)
			}
			out = append(out, capture)

			series.dirty = false
			if m.temporality == Delta {
				series.value = 0
				series.startAt = endAt
				if series.histogram != nil {
					series.histogram.sum = 0
					series.histogram.count = 0
					for i := range series.histogram.counts {
						series.histogram.counts[i] = 0
					}
				}
			}
		}
	}
	return out
}

// PublishMetrics takes one snapshot of a meter and buffers every point it
// produced.
//
// A host calls this on its own period. The driver does not own a timer, because
// a host that already has one would then have two, and the two would disagree
// about when a period ended.
//
// A point refused for backpressure stops the snapshot and returns the error.
// The rest of the snapshot is still in the meter, and a cumulative meter reports
// it whole in the next period. That is why cumulative is the default.
func (d *Driver) PublishMetrics(meter *Meter) (int, error) {
	published := 0
	for _, capture := range meter.Snapshot() {
		if err := d.Capture(capture); err != nil {
			return published, err
		}
		published++
	}
	return published, nil
}
