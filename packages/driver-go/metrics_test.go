package tallyowl

import (
	"fmt"
	"testing"
	"time"

	api "github.com/CatalystCommunity/tallyowl/generated/go/tallyowl-collector-api"
)

func point(t *testing.T, capture *Capture) *api.MetricPointPayload {
	t.Helper()
	if capture.item.MetricPoint == nil {
		t.Fatalf("a metric capture carries a metric point")
	}
	return capture.item.MetricPoint
}

func find(t *testing.T, captures []*Capture, name string) *api.MetricPointPayload {
	t.Helper()
	for _, capture := range captures {
		if p := point(t, capture); p.MetricName == name {
			return p
		}
	}
	t.Fatalf("the snapshot holds no metric named %q", name)
	return nil
}

func TestACounterAggregatesInProcessAndSendsOnePoint(t *testing.T) {
	meter := NewMeter()
	meter.Counter("orders_placed_total", "", "Orders placed.")
	route := Labels{"route": "/checkout"}
	for i := 0; i < 1000; i++ {
		if err := meter.Increment("orders_placed_total", route); err != nil {
			t.Fatalf("increment: %v", err)
		}
	}
	captures := meter.Snapshot()
	if len(captures) != 1 {
		t.Fatalf("a thousand calls to one series produce one point, got %d", len(captures))
	}
	p := find(t, captures, "orders_placed_total")
	if p.NumberValue == nil || *p.NumberValue != 1000 {
		t.Fatalf("the point carries the aggregate, got %v", p.NumberValue)
	}
	if !p.Monotonic {
		t.Fatalf("a counter is monotonic")
	}
	if p.Temporality != Cumulative {
		t.Fatalf("the default meter is cumulative, got %s", p.Temporality)
	}
}

func TestACumulativeCounterKeepsItsTotalAndItsStartAcrossSnapshots(t *testing.T) {
	meter := NewMeter()
	meter.Counter("requests_total", "", "")
	none := Labels{}
	if err := meter.Add("requests_total", none, 5); err != nil {
		t.Fatal(err)
	}
	first := find(t, meter.Snapshot(), "requests_total")
	if err := meter.Add("requests_total", none, 3); err != nil {
		t.Fatal(err)
	}
	second := find(t, meter.Snapshot(), "requests_total")

	if *first.NumberValue != 5 || *second.NumberValue != 8 {
		t.Fatalf("a cumulative counter accumulates, got %v then %v", *first.NumberValue, *second.NumberValue)
	}
	// The same run reports the same start. That is what makes a fall in the
	// value a defect rather than a restart.
	if first.StartAt != second.StartAt {
		t.Fatalf("one run keeps one start, got %d then %d", first.StartAt, second.StartAt)
	}
}

func TestARestartReportsANewStartBesideAValueThatBeginsAgain(t *testing.T) {
	first := NewMeter()
	first.Counter("requests_total", "", "")
	none := Labels{}
	if err := first.Add("requests_total", none, 9); err != nil {
		t.Fatal(err)
	}
	before := find(t, first.Snapshot(), "requests_total")

	time.Sleep(2 * time.Millisecond)
	second := NewMeter()
	second.Counter("requests_total", "", "")
	if err := second.Add("requests_total", none, 2); err != nil {
		t.Fatal(err)
	}
	after := find(t, second.Snapshot(), "requests_total")

	if *before.NumberValue != 9 || *after.NumberValue != 2 {
		t.Fatalf("a restarted counter begins again, got %v then %v", *before.NumberValue, *after.NumberValue)
	}
	if after.StartAt <= before.StartAt {
		t.Fatalf("a restarted counter carries a later start, got %d then %d", before.StartAt, after.StartAt)
	}
}

func TestADeltaMeterReportsThePeriodAndResets(t *testing.T) {
	meter := NewMeterWithTemporality(Delta)
	meter.Counter("requests_total", "", "")
	none := Labels{}
	if err := meter.Add("requests_total", none, 5); err != nil {
		t.Fatal(err)
	}
	if got := *find(t, meter.Snapshot(), "requests_total").NumberValue; got != 5 {
		t.Fatalf("the first period is 5, got %v", got)
	}
	if err := meter.Add("requests_total", none, 3); err != nil {
		t.Fatal(err)
	}
	if got := *find(t, meter.Snapshot(), "requests_total").NumberValue; got != 3 {
		t.Fatalf("the second period is 3 and not 8, got %v", got)
	}
}

func TestADeltaMeterSendsNothingForASeriesThatDidNotMove(t *testing.T) {
	meter := NewMeterWithTemporality(Delta)
	meter.Counter("requests_total", "", "")
	if err := meter.Add("requests_total", Labels{}, 1); err != nil {
		t.Fatal(err)
	}
	if len(meter.Snapshot()) != 1 {
		t.Fatalf("the period that moved sends one point")
	}
	if len(meter.Snapshot()) != 0 {
		t.Fatalf("an idle period sends nothing")
	}
}

func TestAGaugeFallsAndACounterRefusesTo(t *testing.T) {
	meter := NewMeter()
	meter.Gauge("queue_depth_count", "", "")
	meter.Counter("requests_total", "", "")
	none := Labels{}
	if err := meter.Set("queue_depth_count", none, 12); err != nil {
		t.Fatal(err)
	}
	if err := meter.Set("queue_depth_count", none, 4); err != nil {
		t.Fatal(err)
	}
	if got := *find(t, meter.Snapshot(), "queue_depth_count").NumberValue; got != 4 {
		t.Fatalf("a gauge holds the last observation, got %v", got)
	}
	if err := meter.Add("requests_total", none, -1); err == nil {
		t.Fatalf("a counter refuses a negative amount")
	}
}

func TestAHistogramCarriesItsBoundsAndItsCounts(t *testing.T) {
	meter := NewMeter()
	meter.Histogram("request_seconds", []float64{0.01, 0.1, 1.0}, "s", "")
	none := Labels{}
	for _, value := range []float64{0.005, 0.05, 0.5, 5.0} {
		if err := meter.Observe("request_seconds", none, value); err != nil {
			t.Fatal(err)
		}
	}
	h := find(t, meter.Snapshot(), "request_seconds").HistogramValue
	if h == nil {
		t.Fatalf("a histogram point carries buckets")
	}
	if h.Count != 4 {
		t.Fatalf("four observations, got %d", h.Count)
	}
	want := []uint64{1, 2, 3}
	for i, count := range want {
		if h.Counts[i] != count {
			t.Fatalf("bucket %d holds %d, want %d", i, h.Counts[i], count)
		}
	}
	if diff := h.Sum - 5.555; diff > 1e-9 || diff < -1e-9 {
		t.Fatalf("the sum is 5.555, got %v", h.Sum)
	}
}

func TestAnExemplarCarriesTheTraceTheObservationCameFrom(t *testing.T) {
	meter := NewMeter()
	meter.Histogram("request_seconds", []float64{1.0}, "", "")
	trace := make([]byte, 16)
	for i := range trace {
		trace[i] = 7
	}
	if err := meter.ObserveInTrace("request_seconds", Labels{}, 2.0, trace); err != nil {
		t.Fatal(err)
	}
	captures := meter.Snapshot()
	p := find(t, captures, "request_seconds")
	if p.ExemplarTraceId == nil || string(*p.ExemplarTraceId) != string(trace) {
		t.Fatalf("the point carries its exemplar trace")
	}
	// The envelope carries it too, so a stored point joins to a trace without
	// reading the payload.
	if captures[0].item.Envelope.TraceId == nil {
		t.Fatalf("the envelope carries the exemplar trace")
	}
}

func TestASeriesBudgetRefusesInTheOpenAndKeepsEveryAdmittedSeriesCorrect(t *testing.T) {
	budget := DefaultBudget()
	budget.MaxSeriesForEachMetric = 2
	meter := NewMeter().WithBudget(budget)
	meter.Counter("requests_total", "", "")
	if err := meter.Add("requests_total", Labels{"route": "/a"}, 3); err != nil {
		t.Fatal(err)
	}
	if err := meter.Add("requests_total", Labels{"route": "/b"}, 4); err != nil {
		t.Fatal(err)
	}
	if err := meter.Add("requests_total", Labels{"route": "/c"}, 5); err == nil {
		t.Fatalf("the third series is refused")
	}
	if meter.Pressure().SeriesRefused != 1 {
		t.Fatalf("the refusal is counted, got %d", meter.Pressure().SeriesRefused)
	}

	// The two admitted series still count, and nothing was folded into an
	// overflow series.
	captures := meter.Snapshot()
	if len(captures) != 2 {
		t.Fatalf("two series survive, got %d", len(captures))
	}
	for _, capture := range captures {
		for _, label := range point(t, capture).Labels {
			if label.Key == "overflow" {
				t.Fatalf("a refused series never becomes an overflow series")
			}
		}
	}
}

func TestTooManyLabelsIsRefusedAtTheCallSite(t *testing.T) {
	budget := DefaultBudget()
	budget.MaxLabels = 2
	meter := NewMeter().WithBudget(budget)
	meter.Counter("requests_total", "", "")
	err := meter.Add("requests_total", Labels{"a": "1", "b": "2", "c": "3"}, 1)
	if err == nil {
		t.Fatalf("a series wider than the budget is refused")
	}
	if meter.Pressure().LabelsRefused != 1 {
		t.Fatalf("the refusal is counted")
	}
}

func TestAMetricUsedBeforeItIsDeclaredIsRefused(t *testing.T) {
	meter := NewMeter()
	if err := meter.Add("requests_total", Labels{}, 1); err == nil {
		t.Fatalf("an undeclared metric is refused")
	}
}

func TestACounterCallAgainstAGaugeIsRefused(t *testing.T) {
	meter := NewMeter()
	meter.Gauge("queue_depth_count", "", "")
	if err := meter.Add("queue_depth_count", Labels{}, 1); err == nil {
		t.Fatalf("a counter call against a gauge is refused")
	}
}

func TestAHighCardinalityLabelIsAdmittedRatherThanRewritten(t *testing.T) {
	// DATA_MODEL.md section 3.4 permits a request ID as a label. The meter keeps
	// it exactly, because the alternative is a value nobody can join.
	meter := NewMeter()
	meter.Counter("requests_total", "", "")
	for i := 0; i < 500; i++ {
		if err := meter.Add("requests_total", Labels{"request_id": fmt.Sprintf("r-%d", i)}, 1); err != nil {
			t.Fatal(err)
		}
	}
	if got := len(meter.Snapshot()); got != 500 {
		t.Fatalf("every distinct label set is its own series, got %d", got)
	}
}

func TestAMeterStampsTheServiceAndTheReleaseOnEveryPoint(t *testing.T) {
	meter := NewMeter().WithService("checkout").WithRelease("1.4.0")
	meter.Counter("requests_total", "", "")
	if err := meter.Increment("requests_total", Labels{}); err != nil {
		t.Fatal(err)
	}
	envelope := meter.Snapshot()[0].item.Envelope
	if envelope.ServiceName == nil || *envelope.ServiceName != "checkout" {
		t.Fatalf("the point names its service")
	}
	if envelope.Release == nil || *envelope.Release != "1.4.0" {
		t.Fatalf("the point names its release")
	}
}

func TestPublishMetricsBuffersEveryPoint(t *testing.T) {
	driver := NewDriver(NewSettings("127.0.0.1:1", "k"))
	meter := NewMeter()
	meter.Counter("requests_total", "", "")
	for _, route := range []string{"/a", "/b", "/c"} {
		if err := meter.Increment("requests_total", Labels{"route": route}); err != nil {
			t.Fatal(err)
		}
	}
	published, err := driver.PublishMetrics(meter)
	if err != nil {
		t.Fatalf("publish: %v", err)
	}
	if published != 3 || driver.Buffered() != 3 {
		t.Fatalf("three points reach the buffer, got %d published and %d buffered", published, driver.Buffered())
	}
}
