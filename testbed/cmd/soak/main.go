// The soak driver: sustained concurrent load, for days rather than seconds.
//
// docs/PLAN.md Phase 11 asks for cross-cluster collector soak tests, and L132
// points them at the one open defect: an append-log hang that reproduced at
// about one run in twenty under concurrent commits and has not reproduced in
// 1,600 runs since. A soak is the first thing in this project's history that
// runs that concurrency for days, which is the regime a run count cannot reach.
//
// This program is the load half. The orchestrator in
// tools/tallyowl_tools/soak.py starts the cluster, injects the outage windows,
// and watches the stall signature; this offers events through the maintained Go
// app driver — the path a customer uses — and continuously proves three things:
//
//  1. nothing acknowledged is lost: a count over each closed window equals what
//     the producers captured in it, once the delivery queue has drained;
//  2. nothing is duplicated: the same count counts logical events, so a
//     duplicate would read as a surplus;
//  3. rows stay retrievable: random acknowledged events from earlier in the run
//     answer an exact lookup with exactly one row.
//
// It writes its state to a JSON file on an interval, atomically, so the
// orchestrator and a person can read progress without stopping anything.
//
// Environment:
//
//	SOAK_COLLECTORS  comma-separated collector addresses (required)
//	SOAK_HEAD        head address, for queries (required)
//	SOAK_CREDENTIAL  the source key (required)
//	SOAK_SESSION     an operator session, for queries (required)
//	SOAK_STATUS_FILE where to write status JSON (required)
//	SOAK_RATE        events each second across all producers (default 500)
//	SOAK_PRODUCERS   producers for each collector (default 4)
//	SOAK_SEED        the seed (default: the start time, so a restarted
//	                 incarnation never re-issues an earlier one's request IDs)
//	SOAK_CHECK_SECONDS   seconds between reconciliation passes (default 300)
//	SOAK_CHECK_AGE_SECONDS  how old a window must be before it is checked
//	                        (default 900)
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
)

func main() {
	config, err := read()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(2)
	}

	// `SOAK_CHECK_ONLY` answers one count and a handful of exact lookups and
	// exits. The disaster-recovery drill uses it to ask, after a restore,
	// whether what was acknowledged is what a query answers.
	if os.Getenv("SOAK_CHECK_ONLY") != "" {
		os.Exit(checkOnly(config))
	}

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGTERM, syscall.SIGINT)

	soak := newSoak(config)
	var workers sync.WaitGroup
	for index, collector := range config.collectors {
		for producer := range config.producers {
			workers.Add(1)
			id := index*config.producers + producer
			address := collector
			go func() {
				defer workers.Done()
				soak.produce(id, address)
			}()
		}
	}

	workers.Add(1)
	go func() {
		defer workers.Done()
		soak.watch()
	}()

	<-stop
	soak.stopping.Store(true)
	workers.Wait()
	soak.reconcileOnce(true)
	soak.writeStatus()
}

// checkOnly counts every event the project holds and looks up the request IDs
// named in `SOAK_CHECK_IDS`, then prints one JSON object.
func checkOnly(c config) int {
	control := NewControlClient(c.head, c.session)
	defer control.Close()
	project, err := projectFromResolve(c.head, c.credential)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	count, err := control.CountIn(project, 0, 1<<46)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		return 1
	}
	lookups := map[string]int64{}
	for _, id := range strings.Split(os.Getenv("SOAK_CHECK_IDS"), ",") {
		id = strings.TrimSpace(id)
		if id == "" {
			continue
		}
		rows, err := control.LookupCount(project, id)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			return 1
		}
		lookups[id] = rows
	}
	raw, _ := json.Marshal(map[string]any{"count": count, "lookups": lookups})
	fmt.Println(string(raw))
	return 0
}

type config struct {
	collectors []string
	head       string
	credential string
	session    string
	statusFile string
	rate       int
	producers  int
	seed       int64
	linger     time.Duration
	checkEvery time.Duration
	checkAge   time.Duration
}

func read() (config, error) {
	need := func(name string) (string, error) {
		value := strings.TrimSpace(os.Getenv(name))
		if value == "" {
			return "", fmt.Errorf("%s is required", name)
		}
		return value, nil
	}
	collectors, err := need("SOAK_COLLECTORS")
	if err != nil {
		return config{}, err
	}
	head, err := need("SOAK_HEAD")
	if err != nil {
		return config{}, err
	}
	credential, err := need("SOAK_CREDENTIAL")
	if err != nil {
		return config{}, err
	}
	session, err := need("SOAK_SESSION")
	if err != nil {
		return config{}, err
	}
	statusFile, err := need("SOAK_STATUS_FILE")
	if err != nil {
		return config{}, err
	}
	number := func(name string, fallback int) int {
		if value, err := strconv.Atoi(strings.TrimSpace(os.Getenv(name))); err == nil && value > 0 {
			return value
		}
		return fallback
	}
	return config{
		collectors: strings.Split(collectors, ","),
		head:       head,
		credential: credential,
		session:    session,
		statusFile: statusFile,
		rate:       number("SOAK_RATE", 500),
		producers:  number("SOAK_PRODUCERS", 4),
		// The seed rides every request ID, and a restarted driver with the
		// same seed re-issues IDs an earlier incarnation already committed —
		// which made every exact lookup answer two rows and read as
		// duplication. The default is therefore the start time, unique for
		// each incarnation, and the status file records whichever seed ran so
		// the result stays reproducible.
		seed:       int64(number("SOAK_SEED", int(time.Now().Unix()))),
		linger:     time.Duration(number("SOAK_LINGER_MS", 2000)) * time.Millisecond,
		checkEvery: time.Duration(number("SOAK_CHECK_SECONDS", 300)) * time.Second,
		checkAge:   time.Duration(number("SOAK_CHECK_AGE_SECONDS", 900)) * time.Second,
	}, nil
}

// A checkpoint pairs a wall-clock time with how many events one producer had
// captured by then. Reconciliation counts committed events in a closed window
// and compares against the difference of two checkpoints, so a lost event is a
// shortfall and a duplicated one is a surplus.
type checkpoint struct {
	At      int64 `json:"at_ms"`
	Counter int64 `json:"counter"`
}

type producerState struct {
	mutex       sync.Mutex
	captured    int64
	acked       int64
	refused     int64
	checkpoints []checkpoint
}

type window struct {
	Start int64 `json:"start_ms"`
	End   int64 `json:"end_ms"`
}

// An edge is one instant's exact reading: the wall clock and how many events
// every producer had captured by then. A window is two consecutive edges, and
// its expected count is the difference of two exact readings. The first
// version interpolated from ten-second checkpoints instead, and its first four
// windows "failed" by exactly the checkpoint staleness while the committed
// counts were perfect to the event.
type edge struct {
	At    int64
	Total int64
}

type soak struct {
	config    config
	stopping  atomic.Bool
	started   int64
	producers []*producerState

	mutex          sync.Mutex
	edges          []edge
	checkedEdges   int
	windowsChecked int64
	windowsClean   int64
	// A window whose count disagreed, kept whole. This is the number that must
	// stay zero, and one entry holds enough to reproduce the query.
	mismatches []mismatch
	lookups    int64
	lookupBad  int64
	deferrals  int64
}

type mismatch struct {
	Window    window `json:"window"`
	Expected  int64  `json:"expected"`
	Committed int64  `json:"committed"`
	At        int64  `json:"at_ms"`
}

func newSoak(c config) *soak {
	producers := make([]*producerState, len(c.collectors)*c.producers)
	for index := range producers {
		producers[index] = &producerState{}
	}
	now := time.Now().UnixMilli()
	return &soak{
		config:    c,
		started:   now,
		producers: producers,
		edges:     []edge{{At: now, Total: 0}},
	}
}

// produce offers events at this producer's share of the rate, for ever.
//
// A refusal is counted and offered again later: during a head outage the
// driver's unacknowledged bound fills, and that backpressure is the behavior
// under test rather than a failure of it.
func (s *soak) produce(id int, collector string) {
	state := s.producers[id]
	settings := tallyowl.NewSettings(strings.TrimSpace(collector), s.config.credential)
	// A paced producer under the default 100ms linger seals a handful of
	// events into every batch, and the head's commit ceiling is batches, not
	// events. A soak holds its rate for days, so it fills real batches the way
	// a busy backend does rather than paying the whole ceiling in seal
	// overhead. `SOAK_LINGER_MS` sets it; two seconds is the default.
	settings.Linger = s.config.linger
	driver := tallyowl.NewDriver(settings)
	defer driver.Shutdown()

	each := s.config.rate / len(s.producers)
	if each < 1 {
		each = 1
	}
	interval := time.Second / time.Duration(each)
	next := time.Now()

	// Batches in flight, oldest first: the last counter each sealed batch
	// carries. A receipt is for the oldest, so its last counter becomes the
	// acknowledged watermark.
	var sealed []int64
	var lastCheckpoint time.Time

	for !s.stopping.Load() {
		next = next.Add(interval)
		if wait := time.Until(next); wait > 0 {
			time.Sleep(wait)
		} else if wait < -time.Second {
			// The producer fell behind, probably against a full driver during
			// an outage. Offering the missed events in a burst afterwards
			// would turn recovery into a self-made spike.
			next = time.Now()
		}

		state.mutex.Lock()
		counter := state.captured + 1
		state.mutex.Unlock()

		// The request ID rides the envelope's own correlation column, which is
		// what the exact index answers. A client property named `request_id`
		// is a different thing: the field reference resolves to the column and
		// the property is unreachable, which the first drill run demonstrated
		// with four lookups that answered zero rows each.
		capture := tallyowl.Event("soak-event").
			WithSession(fmt.Sprintf("s-%d", counter%1000)).
			WithRequest(requestID(s.config.seed, id, counter)).
			WithProperty("route", tallyowl.Text(routes[counter%int64(len(routes))]))
		if err := driver.Capture(capture); err != nil {
			state.mutex.Lock()
			state.refused++
			state.mutex.Unlock()
			continue
		}

		now := time.Now()
		state.mutex.Lock()
		state.captured = counter
		// One checkpoint every ten seconds bounds the memory of a run that
		// lasts weeks, and reconciliation never needs a finer edge.
		if now.Sub(lastCheckpoint) >= 10*time.Second {
			state.checkpoints = append(state.checkpoints, checkpoint{
				At:      now.UnixMilli(),
				Counter: counter,
			})
			lastCheckpoint = now
		}
		state.mutex.Unlock()

		if driver.ShouldFlush() {
			sealedAt := counter
			receipts, err := driver.Submit()
			if err == nil {
				sealed = append(sealed, sealedAt)
				for range receipts {
					if len(sealed) > 0 {
						state.mutex.Lock()
						state.acked = sealed[0]
						state.mutex.Unlock()
						sealed = sealed[1:]
					}
				}
			} else {
				state.mutex.Lock()
				state.refused++
				state.mutex.Unlock()
			}
		}
	}

	if receipts, err := driver.Drain(); err == nil {
		for range receipts {
			if len(sealed) > 0 {
				state.mutex.Lock()
				state.acked = sealed[0]
				state.mutex.Unlock()
				sealed = sealed[1:]
			}
		}
	}
}

var routes = []string{"/", "/pricing", "/features", "/checkout"}

func requestID(seed int64, producer int, counter int64) string {
	return fmt.Sprintf("r-%d-%d-%d", seed, producer, counter)
}

// watch runs the reconciliation loop and the status file.
func (s *soak) watch() {
	status := time.NewTicker(30 * time.Second)
	defer status.Stop()
	check := time.NewTicker(s.config.checkEvery)
	defer check.Stop()
	for !s.stopping.Load() {
		select {
		case <-status.C:
			s.writeStatus()
		case <-check.C:
			s.reconcileOnce(false)
		}
	}
}

// reconcileOnce closes the window that has aged past the check age, counts it,
// and spot-checks a handful of exact lookups.
//
// A window is only counted after the delivery queue has drained, because until
// then a shortfall is lag rather than loss. A deferred window stays pending and
// is counted on a later pass; `final` counts what it can and leaves the rest
// pending in the status file, honestly.
func (s *soak) reconcileOnce(final bool) {
	now := time.Now().UnixMilli()

	// This pass's edge first: the counters exactly now. A window's expected
	// count is then a difference of two exact readings, and the only slack a
	// comparison needs is the handful of events that can be mid-stamp at an
	// edge instant — at most one for each producer.
	var total int64
	for _, producer := range s.producers {
		producer.mutex.Lock()
		total += producer.captured
		producer.mutex.Unlock()
	}

	s.mutex.Lock()
	s.edges = append(s.edges, edge{At: now, Total: total})
	type candidate struct {
		w        window
		expected int64
		index    int
	}
	var due []candidate
	for i := s.checkedEdges; i+1 < len(s.edges); i++ {
		if now-s.edges[i+1].At < s.config.checkAge.Milliseconds() {
			break
		}
		due = append(due, candidate{
			w:        window{Start: s.edges[i].At, End: s.edges[i+1].At},
			expected: s.edges[i+1].Total - s.edges[i].Total,
			index:    i,
		})
	}
	s.mutex.Unlock()
	if len(due) == 0 {
		return
	}

	// A window is counted only when nothing older than its edge is still
	// waiting. Under live load the queue never reads exactly zero — a handful
	// of batches are always in flight — so the gate is the age of the oldest
	// waiting batch, not the depth: when it is younger than the check age,
	// every batch from before the window edge has been committed.
	depth, oldestMs, stateErr := s.queueState()
	if stateErr != nil || (depth > 0 && (oldestMs == 0 || oldestMs > s.config.checkAge.Milliseconds()/2)) {
		// Either the backlog reaches past the window edge, or its age is
		// unknown (a collector restart loses the age and keeps the depth).
		// A count now could measure the backlog, so say so and try again.
		if !final {
			s.mutex.Lock()
			s.deferrals++
			s.mutex.Unlock()
			return
		}
	}

	control := NewControlClient(s.config.head, s.config.session)
	defer control.Close()
	project, err := projectFromResolve(s.config.head, s.config.credential)
	if err != nil {
		return
	}

	// The stamp race at an edge: a producer can have stamped an event's time
	// just before the edge and recorded its counter just after, so one event
	// for each producer may sit on the other side of the comparison.
	tolerance := int64(len(s.producers))
	for _, held := range due {
		committed, err := control.CountIn(project, held.w.Start, held.w.End)
		if err != nil {
			// Windows are consumed in order, so a failed count stops the pass
			// and the next one asks again from the same place.
			break
		}
		difference := committed - held.expected
		s.mutex.Lock()
		s.windowsChecked++
		if difference >= -tolerance && difference <= tolerance {
			s.windowsClean++
		} else {
			s.mismatches = append(s.mismatches, mismatch{
				Window:    held.w,
				Expected:  held.expected,
				Committed: committed,
				At:        now,
			})
		}
		s.checkedEdges = held.index + 1
		s.mutex.Unlock()
	}

	s.spotCheck(control, project)
}

// counterAt is the highest counter captured at or before `at`. Checkpoints are
// ten seconds apart, and a window edge always falls on the reconciliation
// clock, so interpolation would invent precision the data does not have; the
// same rule at both edges makes the difference exact.
func counterAt(checkpoints []checkpoint, at int64) int64 {
	var last int64
	for _, point := range checkpoints {
		if point.At > at {
			break
		}
		last = point.Counter
	}
	return last
}

// spotCheck asks for a handful of acknowledged events by exact request ID.
// Each must answer exactly one row: zero is a loss, more than one is a
// duplicate, and either is corruption this soak exists to catch.
func (s *soak) spotCheck(control *ControlClient, project []byte) {
	age := s.config.checkAge.Milliseconds()
	for id, producer := range s.producers {
		producer.mutex.Lock()
		acked := producer.acked
		checkpoints := append([]checkpoint(nil), producer.checkpoints...)
		producer.mutex.Unlock()
		old := counterAt(checkpoints, time.Now().UnixMilli()-age)
		if old < 1 {
			continue
		}
		if acked < old {
			old = acked
		}
		if old < 1 {
			continue
		}
		// A deterministic pick beats a random one: a failure names the exact
		// ID, and a rerun asks the same question.
		pick := (old / 2) + 1
		rows, err := control.LookupCount(project, requestID(s.config.seed, id, pick))
		if err != nil {
			continue
		}
		s.mutex.Lock()
		s.lookups++
		if rows != 1 {
			s.lookupBad++
		}
		s.mutex.Unlock()
	}
}

// queueState reads the delivery queue depth and the oldest waiting age each
// collector publishes.
func (s *soak) queueState() (int64, int64, error) {
	var depth, oldest int64
	for _, collector := range s.config.collectors {
		operational := operationalAddress(strings.TrimSpace(collector))
		value, err := scrapeGauge(operational, "tallyowl_delivery_queue_depth_count")
		if err != nil {
			return 0, 0, err
		}
		if value > depth {
			// Both collectors read one queue, so the depth is shared rather
			// than additive.
			depth = value
		}
		age, err := scrapeGauge(operational, "tallyowl_delivery_oldest_waiting_ms")
		if err != nil {
			return 0, 0, err
		}
		if age > oldest {
			oldest = age
		}
	}
	return depth, oldest, nil
}

// operationalAddress is the collector's health-and-metrics address, which is
// its intake port plus one — the convention the soak configuration uses.
func operationalAddress(intake string) string {
	host, port, found := strings.Cut(intake, ":")
	if !found {
		return intake
	}
	number, err := strconv.Atoi(port)
	if err != nil {
		return intake
	}
	return fmt.Sprintf("%s:%d", host, number+1)
}

func scrapeGauge(address, name string) (int64, error) {
	response, err := http.Get(fmt.Sprintf("http://%s/metrics", address))
	if err != nil {
		return 0, err
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return 0, err
	}
	for _, line := range strings.Split(string(body), "\n") {
		if strings.HasPrefix(line, name+" ") || strings.HasPrefix(line, name+"{") {
			fields := strings.Fields(line)
			if len(fields) == 2 {
				value, err := strconv.ParseFloat(fields[1], 64)
				if err != nil {
					return 0, err
				}
				return int64(value), nil
			}
		}
	}
	return 0, nil
}

type status struct {
	StartedAtMs    int64      `json:"started_at_ms"`
	NowMs          int64      `json:"now_ms"`
	Seed           int64      `json:"seed"`
	Rate           int        `json:"rate_each_second"`
	Producers      int        `json:"producers"`
	Captured       int64      `json:"captured"`
	Acked          int64      `json:"acknowledged"`
	Refused        int64      `json:"refused"`
	WindowsChecked int64      `json:"windows_checked"`
	WindowsClean   int64      `json:"windows_clean"`
	WindowsPending int        `json:"windows_pending"`
	Deferrals      int64      `json:"reconciliations_deferred"`
	Mismatches     []mismatch `json:"mismatches"`
	Lookups        int64      `json:"exact_lookups"`
	LookupBad      int64      `json:"exact_lookups_wrong"`
}

func (s *soak) writeStatus() {
	var captured, acked, refused int64
	for _, producer := range s.producers {
		producer.mutex.Lock()
		captured += producer.captured
		acked += producer.acked
		refused += producer.refused
		producer.mutex.Unlock()
	}
	s.mutex.Lock()
	out := status{
		StartedAtMs:    s.started,
		NowMs:          time.Now().UnixMilli(),
		Seed:           s.config.seed,
		Rate:           s.config.rate,
		Producers:      len(s.producers),
		Captured:       captured,
		Acked:          acked,
		Refused:        refused,
		WindowsChecked: s.windowsChecked,
		WindowsClean:   s.windowsClean,
		WindowsPending: len(s.edges) - 1 - s.checkedEdges,
		Deferrals:      s.deferrals,
		Mismatches:     append([]mismatch(nil), s.mismatches...),
		Lookups:        s.lookups,
		LookupBad:      s.lookupBad,
	}
	s.mutex.Unlock()

	raw, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return
	}
	// Atomic, so a reader never sees half a file.
	temporary := s.config.statusFile + ".next"
	if err := os.WriteFile(temporary, raw, 0o644); err != nil {
		return
	}
	_ = os.Rename(temporary, s.config.statusFile)
}
