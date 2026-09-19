// The load harness: what the alpha report measures.
//
// The implementation prompt section 11 asks for seven measurements against a
// running home profile. This produces them, using the maintained Go app driver,
// because a measurement taken through a path a customer does not use is a
// measurement of something else.
//
//	go run ./cmd/load <collector-address> <head-address> <credential> <session>
//
// The ramp runs several producers at once, because that is what a backend is:
// many request handlers producing telemetry into one driver. One synchronous
// producer measures the round trip to the durable boundary rather than the
// ingest ceiling, and this reports both.
//
// Every measurement states what it measured rather than what it assumed. A
// number this harness could not take is reported as absent, never as zero.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"sort"
	"strconv"
	"strings"
	"time"

	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
)

// The seed. A result nobody can reproduce is not a result.
const seed = 20260803

type report struct {
	// The ramp: one entry for each rate the harness held.
	Steps []step `json:"steps"`
	// The highest rate that lost nothing and refused nothing.
	SustainedPerSecond int `json:"sustained_events_each_second"`
	// What stopped it: "loss", "refusal", "latency", or "reached the ceiling
	// of the ramp without failing".
	CeilingReason string `json:"ceiling_reason"`
	// The burst the system absorbed above the sustained rate without loss.
	BurstMultiplier float64 `json:"burst_multiplier"`
	// What the unpaced producers actually offered, accepted or not. The
	// multiplier is only meaningful beside this: absorbing everything offered
	// says nothing if the offer was small.
	BurstOfferedPerSecond float64 `json:"burst_offered_each_second"`
	// Events the burst refused. A refusal here is the driver's unacknowledged
	// bound, which is the boundary being measured rather than a fault.
	BurstRefused int `json:"burst_refused"`
	// Query latency, in milliseconds.
	PointLookupP50 float64 `json:"point_lookup_p50_ms"`
	PointLookupP99 float64 `json:"point_lookup_p99_ms"`
	// How many of the timed point lookups found no rows. The alpha report
	// published latencies where every one of these was a miss and the number
	// that would have said so did not exist (L153). Anything above zero means
	// the latencies above measured the cost of finding nothing.
	PointLookupsEmpty int     `json:"point_lookups_that_found_nothing"`
	AggregateP50      float64 `json:"aggregate_p50_ms"`
	AggregateP99      float64 `json:"aggregate_p99_ms"`
	EventsSent        int     `json:"events_sent"`
	EventsAccepted    int     `json:"events_accepted"`
	Producers         int     `json:"producers"`
	// OneProducerPerSecond is the rate one synchronous producer reaches. It is
	// the round trip to the durable boundary rather than the ingest ceiling,
	// and both numbers matter: an application with one worker gets the first.
	OneProducerPerSecond float64 `json:"one_producer_events_each_second"`
	QueriesRun           int     `json:"queries_run"`
	Seed                 int     `json:"seed"`
}

type step struct {
	TargetPerSecond   int     `json:"target_events_each_second"`
	AchievedPerSecond float64 `json:"achieved_events_each_second"`
	Accepted          int     `json:"accepted"`
	Refused           int     `json:"refused"`
	P50Ms             float64 `json:"submit_p50_ms"`
	P99Ms             float64 `json:"submit_p99_ms"`
}

func main() {
	if len(os.Args) < 4 {
		fmt.Fprintln(os.Stderr,
			"usage: load <collector-address> <head-address> <credential> [<session>]")
		os.Exit(2)
	}
	collector, head, credential := os.Args[1], os.Args[2], os.Args[3]
	session := ""
	if len(os.Args) > 4 {
		session = os.Args[4]
	}

	out := report{Seed: seed, Producers: producers}

	// `LOAD_QUERIES_ONLY` measures the query half against a store that already
	// holds data. A query measurement taken while the head is still committing
	// a backlog measures the backlog, and separating the two is what
	// docs/BENCHMARKS.md section 16 asked for after the previous run could not
	// tell three causes apart.
	if os.Getenv("LOAD_QUERIES_ONLY") != "" {
		if session != "" {
			measureQueries(head, session, credential, collector, &out)
		}
		raw, _ := json.MarshalIndent(out, "", "  ")
		fmt.Println(string(raw))
		return
	}

	// The ramp. Each step holds its rate for a fixed time, so a step that could
	// not reach its rate says so through `achieved` rather than by running
	// longer.
	const holdFor = 3 * time.Second
	rates := []int{500, 1000, 2000, 5000, 10000, 20000, 40000, 80000}
	// `LOAD_RATES` runs a shorter ramp. A defect that only appears at 80,000
	// events each second is hard to read; one that reproduces at 500 is not,
	// and the first thing to find out about any of them is which it is.
	if named := os.Getenv("LOAD_RATES"); named != "" {
		rates = nil
		for _, part := range strings.Split(named, ",") {
			if rate, err := strconv.Atoi(strings.TrimSpace(part)); err == nil {
				rates = append(rates, rate)
			}
		}
	}

	// One producer first, so the report can say what a single synchronous
	// worker gets as well as what the installation gets. `LOAD_RATES` skips it:
	// a short diagnostic ramp wants one variable, not two.
	if os.Getenv("LOAD_RATES") == "" {
		single := hold(collector, credential, 1, 20_000, 2*time.Second)
		out.OneProducerPerSecond = single.AchievedPerSecond
		out.EventsSent += single.Accepted + single.Refused
		out.EventsAccepted += single.Accepted
	}

	out.CeilingReason = "reached the ceiling of the ramp without failing"
	// The best rate any step actually achieved. The ceiling is measured against
	// this rather than against the last step's target.
	best := 0.0
	for _, rate := range rates {
		measured := hold(collector, credential, producers, rate, holdFor)
		out.Steps = append(out.Steps, measured)
		out.EventsSent += measured.Accepted + measured.Refused
		out.EventsAccepted += measured.Accepted

		if measured.Refused > 0 {
			out.CeilingReason = "refusal"
			break
		}
		// A step that could not reach its target **and did not improve on the
		// best rate so far** is the ceiling: the system stopped scaling.
		//
		// Missing a target alone is not enough. The first step of the ramp is
		// the shortest and the most exposed to scheduler jitter, and a run that
		// achieved 444 against a target of 500 once aborted the whole ramp and
		// published 444 as the sustained rate. A ceiling is a system that
		// stopped going faster, not one sample that came in one percent low.
		if measured.AchievedPerSecond < float64(rate)*0.9 &&
			measured.AchievedPerSecond <= best {
			out.CeilingReason = "the system stopped scaling"
			out.SustainedPerSecond = int(best)
			break
		}
		if measured.AchievedPerSecond > best {
			best = measured.AchievedPerSecond
		}
		out.SustainedPerSecond = int(best)
	}

	// The burst. **Unpaced**: every producer offers as fast as it can, so the
	// harness is no longer the slower half. A refusal is the answer rather than
	// a failure — it is the driver's unacknowledged bound saying the path is
	// full, which is exactly the boundary the multiplier is about.
	if out.SustainedPerSecond > 0 && os.Getenv("LOAD_RATES") == "" {
		measured := hold(collector, credential, producers, 0, time.Second)
		out.BurstOfferedPerSecond = measured.AchievedPerSecond +
			float64(measured.Refused)/time.Second.Seconds()
		out.BurstRefused = measured.Refused
		if measured.AchievedPerSecond > 0 {
			out.BurstMultiplier = measured.AchievedPerSecond / float64(out.SustainedPerSecond)
		}
		out.EventsSent += measured.Accepted + measured.Refused
		out.EventsAccepted += measured.Accepted
	}

	if session != "" {
		measureQueries(head, session, credential, collector, &out)
	}

	raw, _ := json.MarshalIndent(out, "", "  ")
	fmt.Println(string(raw))
}

// How many producers the ramp runs at once. A backend is many request
// handlers, not one loop, and the driver's flush is synchronous: a single
// producer measures the durable round trip rather than the ingest ceiling.
const producers = 8

// hold offers events at `rate` across `workers` producers for `duration`.
func hold(collector, credential string, workers, rate int, duration time.Duration) step {
	// A rate of zero is the unpaced burst and must travel through as zero. A
	// clamp to one paced every producer at one event each second, which is how
	// the first unpaced run reported a burst of 7.5 events each second.
	each := 0
	if rate > 0 {
		each = rate / workers
		if each < 1 {
			each = 1
		}
	}
	results := make(chan step, workers)
	for w := range workers {
		go func(worker int) {
			results <- holdOne(collector, credential, worker, each, duration)
		}(w)
	}
	combined := step{TargetPerSecond: rate}
	var latencies []float64
	for range workers {
		one := <-results
		combined.Accepted += one.Accepted
		combined.Refused += one.Refused
		combined.AchievedPerSecond += one.AchievedPerSecond
		latencies = append(latencies, one.P50Ms, one.P99Ms)
	}
	combined.P50Ms = quantile(latencies, 0.50)
	combined.P99Ms = quantile(latencies, 0.99)
	return combined
}

func holdOne(collector, credential string, worker, rate int, duration time.Duration) step {
	settings := tallyowl.NewSettings(collector, credential)
	// The batch defaults are D19's, because those are the ones a customer runs.
	driver := tallyowl.NewDriver(settings)
	defer driver.Shutdown()

	var latencies []float64
	// When each outstanding batch was sealed. A receipt frees the oldest, which
	// is what makes a latency meaningful once several batches are in flight.
	var sealed []time.Time
	accepted, refused := 0, 0
	// A rate of zero means **do not pace**: offer as fast as this producer can,
	// which is what a burst measurement needs and what every previous harness
	// could not do. A paced producer sleeps between events and therefore can
	// never outrun the collector, which is why the burst multiplier came back
	// below 1 for three runs. See L051, L055, and L082.
	unpaced := rate <= 0
	interval := time.Nanosecond
	if !unpaced {
		interval = time.Second / time.Duration(rate)
	}
	started := time.Now()
	deadline := started.Add(duration)
	next := started
	counter := 0

	for time.Now().Before(deadline) {
		// Pace by a deadline rather than by sleeping for the interval. Sleeping
		// accumulates the scheduler's error and reports a rate the harness
		// never actually offered.
		if !unpaced {
			next = next.Add(interval)
			if wait := time.Until(next); wait > 0 {
				time.Sleep(wait)
			}
		}
		counter++
		capture := tallyowl.Event("checkout-started").
			WithSession(fmt.Sprintf("s-%d", counter%1000)).
			WithProperty("route", tallyowl.Text(routes[counter%len(routes)])).
			// A high-cardinality value on every event, because that is what
			// the capacity envelope is actually about. See D20. It rides the
			// envelope's request column: a field reference on `request_id`
			// resolves to the column, so the earlier client-property form made
			// every point lookup a miss — the alpha lookup latencies measured
			// misses. See the Phase 11 report.
			WithRequest(fmt.Sprintf("r-%d-%d-%d", seed, worker, counter))
		if err := driver.Capture(capture); err != nil {
			refused++
			continue
		}
		if driver.ShouldFlush() {
			// `Submit` sends without waiting for this batch's acknowledgement,
			// so a producer offers at its target rate instead of being paced by
			// the round trip. The previous harness used `Flush` and therefore
			// could not offer faster than the system took, which is why the
			// burst multiplier came back below 1. See L051 and L055.
			at := time.Now()
			sealed = append(sealed, at)
			receipts, err := driver.Submit()
			if err != nil {
				refused++
				continue
			}
			for range receipts {
				// A receipt is for a batch sealed earlier in the window, so the
				// latency is measured from that batch's seal.
				if len(sealed) > 0 {
					latencies = append(latencies,
						float64(time.Since(sealed[0]).Microseconds())/1000.0)
					sealed = sealed[1:]
				}
			}
			for _, receipt := range receipts {
				accepted += int(receipt.Accepted)
			}
		}
	}

	// Everything still in flight, plus whatever is still buffered.
	if receipts, err := driver.Submit(); err == nil {
		for _, receipt := range receipts {
			accepted += int(receipt.Accepted)
		}
	} else {
		refused++
	}
	if receipts, err := driver.Drain(); err == nil {
		for range receipts {
			if len(sealed) > 0 {
				latencies = append(latencies,
					float64(time.Since(sealed[0]).Microseconds())/1000.0)
				sealed = sealed[1:]
			}
		}
		for _, receipt := range receipts {
			accepted += int(receipt.Accepted)
		}
	} else {
		refused++
	}

	elapsed := time.Since(started).Seconds()
	return step{
		TargetPerSecond:   rate,
		AchievedPerSecond: float64(accepted) / elapsed,
		Accepted:          accepted,
		Refused:           refused,
		P50Ms:             quantile(latencies, 0.50),
		P99Ms:             quantile(latencies, 0.99),
	}
}

var routes = []string{"/", "/pricing", "/features", "/checkout"}

// quantile is the nearest-rank value. It is exact over the samples in hand and
// says so by being exact rather than interpolating between two of them.
func quantile(values []float64, q float64) float64 {
	if len(values) == 0 {
		return 0
	}
	sorted := append([]float64(nil), values...)
	sort.Float64s(sorted)
	rank := int(q*float64(len(sorted))+0.5) - 1
	if rank < 0 {
		rank = 0
	}
	if rank >= len(sorted) {
		rank = len(sorted) - 1
	}
	return sorted[rank]
}

// measureQueries times the two query shapes the report names: a point lookup on
// a high-cardinality value, and an aggregate over a range.
//
// A query is a control operation, so it needs a session. The credential above
// authenticates an application; this authenticates a person, and neither is a
// substitute for the other.
func measureQueries(head, session, credential, collector string, out *report) {
	client := NewControlClient(head, session)
	defer client.Close()

	project, err := projectFromResolve(head, credential)
	if err != nil {
		fmt.Fprintf(os.Stderr, "the project could not be resolved, so no query was timed: %v\n", err)
		return
	}

	const runs = 50
	// Every `hold` starts its counters at one, so a low counter was written
	// once per ramp step and a lookup on it reads a row from most segments at
	// once. `LOAD_LOOKUP_OFFSET` moves the probe to counters only the largest
	// step reached — a value stored about once, which is what "one exact
	// high-cardinality value" means — or past everything written, which is a
	// deliberate miss. The report's empty-lookup count says which one happened.
	offset := 0
	if named := os.Getenv("LOAD_LOOKUP_OFFSET"); named != "" {
		if parsed, err := strconv.Atoi(named); err == nil {
			offset = parsed
		}
	}
	var lookups, aggregates []float64
	for run := range runs {
		// The point lookup: one exact high-cardinality value. This is the one
		// AGENTS.md says must stay exact, and the one the locator exists for.
		worker := run % producers
		// Only the single-producer phase writes counters past what the ramp
		// reaches, and it writes them as worker zero. `LOAD_LOOKUP_WORKER`
		// pins the probe there, so an offset into that tail measures fifty
		// values stored exactly once instead of seven.
		if named := os.Getenv("LOAD_LOOKUP_WORKER"); named != "" {
			if parsed, err := strconv.Atoi(named); err == nil {
				worker = parsed
			}
		}
		at := time.Now()
		rows, err := client.PointLookup(project, fmt.Sprintf("r-%d-%d-%d", seed, worker, run+1+offset))
		if err != nil {
			fmt.Fprintf(os.Stderr, "a point lookup failed: %v\n", err)
			return
		}
		lookups = append(lookups, float64(time.Since(at).Microseconds())/1000.0)
		if rows == 0 {
			out.PointLookupsEmpty++
		}

		// The aggregate: a count by minute over the whole range, which is the
		// chart a dashboard draws.
		at = time.Now()
		if _, err := client.Trend(project); err != nil {
			fmt.Fprintf(os.Stderr, "an aggregate failed: %v\n", err)
			return
		}
		aggregates = append(aggregates, float64(time.Since(at).Microseconds())/1000.0)
	}

	out.PointLookupP50 = quantile(lookups, 0.50)
	out.PointLookupP99 = quantile(lookups, 0.99)
	out.AggregateP50 = quantile(aggregates, 0.50)
	out.AggregateP99 = quantile(aggregates, 0.99)
	out.QueriesRun = runs * 2
}
