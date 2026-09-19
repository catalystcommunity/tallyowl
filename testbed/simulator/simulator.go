// Package simulator expands a declarative scenario into an ordered event
// stream and the ledger that says what the stream must produce.
//
// Two properties matter more than the size of the scenario, and
// docs/TESTBED.md section 6 names both:
//
//   - **Seeded.** One seed produces one identical run, so a failure reproduces.
//   - **Virtual clock.** A scenario simulates a span of days in a short real
//     time, because retention, cohort, and attribution windows need it.
//
// The simulator deliberately sends late events and duplicate identifiers. The
// ledger records the correct logical result, not the physical one.
package simulator

import (
	"encoding/hex"
	"fmt"
	"net/url"

	tallyowl "github.com/CatalystCommunity/tallyowl/packages/driver-go"
	"github.com/CatalystCommunity/tallyowl/testbed/ledger"
)

// Scenario is the declarative definition of one run.
type Scenario struct {
	Name string `json:"name"`
	Seed int64  `json:"seed"`

	// StartAt is the virtual clock's origin, in milliseconds since the epoch.
	StartAt int64 `json:"start_at"`

	// Applications each hold one credential. A second application in one
	// installation is what proves tenant isolation with real traffic. See D8.
	Applications []Application `json:"applications"`
}

// Application is one instrumented product in the installation.
type Application struct {
	Name       string `json:"name"`
	Credential string `json:"credential"`

	// EndUsers is how many people the scenario simulates.
	EndUsers int `json:"end_users"`
	// SessionsEachUser is how many visits each of them makes.
	SessionsEachUser int `json:"sessions_each_user"`
	// ConvertEvery names how often a session ends in a purchase. One in this
	// many sessions converts, and the scenario counts them exactly.
	ConvertEvery int `json:"convert_every"`
	// DuplicateEvery sends one event twice, one session in this many, so a
	// duplicate delivery has a ledger entry that expects one logical event.
	DuplicateEvery int `json:"duplicate_every"`
	// LateEvery sends one event with a producer time well before the rest, so
	// a late arrival has a ledger entry too.
	LateEvery int `json:"late_every"`
	// TraceEvery gives one session in this many a backend trace. A trace is a
	// root span, a child, and a grandchild, so the waterfall has a shape to
	// assert rather than a list.
	TraceEvery int `json:"trace_every"`
	// ErrorEvery gives one session in this many an error occurrence. The
	// scenario alternates between two defects, so a grouping assertion has
	// something to separate.
	ErrorEvery int `json:"error_every"`
	// SurfacesEachUser is how many client surfaces each person uses: the web
	// application, the rich client, and the mobile client. Each surface has its
	// own project-scoped anonymous identifier and each one identifies to the
	// same known end user, which is what makes one person out of three
	// timelines. Zero keeps the older behaviour of one surface and no identity
	// events.
	//
	// This is Phase 8's exit criterion: "the reference application end user
	// uses the web, rich, and mobile clients, and the resulting funnel,
	// retention, and timeline results match the ledger."
	SurfacesEachUser int `json:"surfaces_each_user"`
	// ReturnDays is how many days apart a person's visits are, so a retention
	// matrix has cohorts and periods rather than one bucket.
	ReturnDays int `json:"return_days"`
	// MarketingUsers is how many people take the marketing journey: three
	// campaign touches, a sign-in, and a purchase with an order identifier.
	// Zero leaves the journey out.
	//
	// This is Phase 9's exit criterion: "every attribution model matches the
	// ledger for traffic that arrives from the reference marketing site landing
	// pages."
	MarketingUsers int `json:"marketing_users"`
	// Metrics turns on in-process metric aggregation for this application. It
	// counts checkouts by route and observes each span's duration, then
	// publishes one snapshot at the end of the run.
	//
	// One snapshot rather than one for each session is the point: a meter
	// aggregates in process, so a million calls become one point for each
	// series. The ledger predicts the aggregate, which is what a merge or a
	// series budget could break.
	Metrics bool `json:"metrics"`
}

// The instruments the reference application publishes.
const (
	checkoutsMetric = "seedstore_checkouts_total"
	durationMetric  = "seedstore_span_seconds"
)

// The bucket layout every application in the scenario uses. One layout is what
// makes two applications' histograms merge; `histogram_merge` refuses two
// layouts rather than rebucketing them.
var durationBounds = []float64{0.01, 0.05, 0.1, 0.5, 1.0}

// Item is one thing the scenario sends, and which application sends it.
type Item struct {
	Credential string
	Capture    *tallyowl.Capture
}

/// A small deterministic generator. A scenario records its seed, so a failure
/// reproduces exactly, and nothing here reads the wall clock.
type rng struct{ state uint64 }

func newRng(seed int64) *rng {
	return &rng{state: uint64(seed) ^ 0x9e37_79b9_7f4a_7c15}
}

func (r *rng) next() uint64 {
	x := r.state
	x ^= x >> 12
	x ^= x << 25
	x ^= x >> 27
	r.state = x
	return x * 0x2545_f491_4f6c_dd1d
}

func (r *rng) below(n uint64) uint64 {
	if n == 0 {
		return 0
	}
	return r.next() % n
}

// eventID makes a deterministic identifier from the virtual clock and a
// counter, so a run reproduces byte for byte.
func eventID(at int64, counter uint64) []byte {
	out := make([]byte, 16)
	for i := 0; i < 6; i++ {
		out[i] = byte(at >> (8 * (5 - i)))
	}
	for i := 0; i < 10; i++ {
		out[6+i] = byte(counter >> (8 * i))
	}
	out[6] = (out[6] & 0x0f) | 0x70
	out[8] = (out[8] & 0x3f) | 0x80
	return out
}

var routes = []string{"/", "/pricing", "/features", "/checkout"}

// The defects the scenario simulates. Two of them, with different in-app
// frames, so a grouping assertion has something to separate. The messages
// differ between occurrences on purpose: `fingerprint_v1` replaces literals
// with placeholders, so one defect stays one group. See D39.
var defects = []struct {
	name      string
	errorType string
	message   string
	frames    []tallyowl.Frame
}{
	{
		name:      "charge-timeout",
		errorType: "Timeout",
		message:   "the payment provider took 30 seconds",
		frames: []tallyowl.Frame{
			tallyowl.LibraryFrame("net/http", "serve"),
			tallyowl.InAppFrame("checkout", "charge").At("/app/checkout.go", 88),
		},
	},
	{
		name:      "cart-missing",
		errorType: "KeyError",
		message:   "no cart \"cart-4471\" for this session",
		frames: []tallyowl.Frame{
			tallyowl.LibraryFrame("net/http", "serve"),
			tallyowl.InAppFrame("cart", "load").At("/app/cart.go", 21),
		},
	},
}

// A deterministic trace identifier, so a run reproduces byte for byte and the
// ledger can name the traces it expects.
func traceID(counter uint64) []byte {
	out := make([]byte, 16)
	for i := range 8 {
		out[i] = byte(counter >> (8 * i))
	}
	// A byte that is never zero, so an all-zero identifier can never appear.
	out[15] = 0x5a
	return out
}

func spanID(counter uint64) []byte {
	out := make([]byte, 8)
	for i := range 7 {
		out[i] = byte(counter >> (8 * i))
	}
	out[7] = 0x5b
	return out
}

// Expand turns a scenario into the ordered stream and the ledger it must
// produce. Nothing is sent here; a caller decides how the stream travels.
func Expand(scenario Scenario) ([]Item, *ledger.Ledger, error) {
	random := newRng(scenario.Seed)
	book := &ledger.Ledger{
		Scenario:   scenario.Name,
		Seed:       scenario.Seed,
		RangeStart: scenario.StartAt,
	}

	var stream []Item
	var counter uint64
	latest := scenario.StartAt

	for _, application := range scenario.Applications {
		project := ledger.NewProject(application.Credential)
		// The meter aggregates in process. The driver batches; the meter
		// counts. See packages/driver-go/metrics.go.
		meter := tallyowl.NewMeter().WithService(application.Name).WithRelease("2026.8.1")
		if application.Metrics {
			meter.Counter(checkoutsMetric, "", "Checkouts started, by route.")
			meter.Histogram(durationMetric, durationBounds, "s", "How long a backend span took.")
		}
		// The virtual clock advances for each application from the same origin,
		// so the two applications overlap in time. Tenant isolation then has to
		// come from the credential rather than from a time range.
		at := scenario.StartAt

		for user := 0; user < application.EndUsers; user++ {
			endUser := fmt.Sprintf("u-%03d", user)
			for visit := 0; visit < application.SessionsEachUser; visit++ {
				session := fmt.Sprintf("s-%s-%03d-%03d", application.Name, user, visit)
				at += 1_000 + int64(random.below(4_000))

				add := func(capture *tallyowl.Capture, kind, name string, occurredAt int64) {
					counter++
					id := eventID(occurredAt, counter)
					capture = capture.
						WithEventID(id).
						At(occurredAt).
						WithSession(session).
						WithProperty("end_user", tallyowl.Text(endUser))
					stream = append(stream, Item{Credential: application.Credential, Capture: capture})
					project.Record(hex.EncodeToString(id), kind, name, occurredAt)
					if occurredAt > latest {
						latest = occurredAt
					}
				}

				add(tallyowl.SessionStart(session), "session-start", "session-start", at)

				route := routes[random.below(uint64(len(routes)))]
				at += 500
				add(tallyowl.PageView(route), "page-view", route, at)

				at += 500
				add(
					tallyowl.Event("checkout-started").WithProperty("route", tallyowl.Text(route)),
					"event", "checkout-started", at)
				if application.Metrics {
					if err := meter.Increment(
						checkoutsMetric, tallyowl.Labels{"route": route}); err != nil {
						return nil, nil, err
					}
				}

				// A late event: the producer time sits well before the rest of
				// the session. It is accepted, and retention applies at
				// projection rather than at intake. See DELIVERY.md section 9.
				if application.LateEvery > 0 && visit%application.LateEvery == 0 {
					add(
						tallyowl.Event("app-opened").WithProperty("late", tallyowl.Bool(true)),
						"event", "app-opened", scenario.StartAt-3_600_000)
				}

				if application.ConvertEvery > 0 && visit%application.ConvertEvery == 0 {
					value := tallyowl.MustDecimal("19.99")
					at += 500
					counter++
					id := eventID(at, counter)
					capture := tallyowl.Conversion("purchase", &value, "USD").
						WithEventID(id).
						At(at).
						WithSession(session).
						WithProperty("end_user", tallyowl.Text(endUser))
					stream = append(stream, Item{Credential: application.Credential, Capture: capture})
					project.Record(hex.EncodeToString(id), "conversion", "purchase", at)
					if err := project.RecordConversionValue("19.99"); err != nil {
						return nil, nil, err
					}
					if at > latest {
						latest = at
					}
				}

				at += 500
				// A backend trace: one request, one charge inside it, and one
				// query inside that. The ledger records the exact shape.
				if application.TraceEvery > 0 && visit%application.TraceEvery == 0 {
					root := tallyowl.RootContext()
					root.TraceID = traceID(counter + 1)
					root.SpanID = spanID(counter + 1)
					child := root.Child()
					child.SpanID = spanID(counter + 2)
					grandchild := child.Child()
					grandchild.SpanID = spanID(counter + 3)

					traceText := hex.EncodeToString(root.TraceID)
					for depth, span := range []struct {
						context   tallyowl.SpanContext
						operation string
						duration  int64
					}{
						{root, "GET /checkout", 90},
						{child, "charge", 40},
						{grandchild, "SELECT orders", 5},
					} {
						at += 10
						counter++
						id := eventID(at, counter)
						capture := tallyowl.
							Span(span.context, span.operation, "server", at, span.duration).
							WithEventID(id).
							WithSession(session).
							WithService("seedstore-api").
							WithProperty("end_user", tallyowl.Text(endUser))
						stream = append(stream, Item{
							Credential: application.Credential,
							Capture:    capture,
						})
						project.Record(
							hex.EncodeToString(id), "span", span.operation, at)
						project.RecordSpan(traceText, depth)
						if application.Metrics {
							// The exemplar: the observation carries the trace it
							// came from, so a chart of the histogram becomes a
							// way into one slow request.
							if err := meter.ObserveInTrace(
								durationMetric,
								tallyowl.Labels{"operation": span.operation},
								float64(span.duration)/1000.0,
								root.TraceID,
							); err != nil {
								return nil, nil, err
							}
						}
						if at > latest {
							latest = at
						}
					}

					// One error inside the trace, for the sessions that have
					// one. The error carries the trace ID, so an error and its
					// trace meet without a join.
					if application.ErrorEvery > 0 && visit%application.ErrorEvery == 0 {
						defect := defects[int(random.below(uint64(len(defects))))]
						at += 10
						counter++
						id := eventID(at, counter)
						capture := tallyowl.
							Error(defect.errorType, defect.message, false).
							WithFrames(defect.frames).
							InSpan(grandchild).
							WithEventID(id).
							At(at).
							WithSession(session).
							WithService("seedstore-api").
							WithRelease("2026.8.1").
							WithProperty("end_user", tallyowl.Text(endUser))
						stream = append(stream, Item{
							Credential: application.Credential,
							Capture:    capture,
						})
						project.Record(
							hex.EncodeToString(id), "error", defect.errorType, at)
						project.RecordError(defect.name)
						project.RecordTraceError(traceText)
						if at > latest {
							latest = at
						}
					}
				}

				add(tallyowl.SessionEnd(session, "explicit"), "session-end", "session-end", at)

				// A duplicate delivery: the same identifier travels twice. The
				// ledger already counted it once, and a primary query must
				// agree however many physical rows exist.
				if application.DuplicateEvery > 0 && visit%application.DuplicateEvery == 0 {
					last := stream[len(stream)-1]
					stream = append(stream, last)
					project.Record(
						hex.EncodeToString(last.Capture.EventID()),
						"session-end", "session-end", at)
				}
			}
		}
		// The identity journey. Phase 8.
		//
		// One person, three client surfaces, and one known identifier. Each
		// surface has its own project-scoped anonymous identifier, and each one
		// identifies to the same person; the funnel, the retention matrix, and
		// the timeline then all have to see one person rather than three.
		//
		// It runs beside the sessions above rather than inside them, because
		// the sessions above carry no envelope identity and the two would
		// otherwise interfere: a funnel by end user must count the journey and
		// nothing else, and a fixture that mixed them would be one nobody could
		// work out by hand.
		if application.SurfacesEachUser > 0 {
			journey := expandJourney(
				application, scenario.StartAt, project, &counter, &stream)
			if journey > latest {
				latest = journey
			}
		}

		// The marketing journey. Phase 9.
		if application.MarketingUsers > 0 {
			marketing, err := expandMarketing(
				application, scenario.StartAt, project, &counter, &stream)
			if err != nil {
				return nil, nil, err
			}
			if marketing > latest {
				latest = marketing
			}
		}

		// One snapshot for the whole run. Every call above became a reading in
		// one point for each series, which is what in-process aggregation
		// means.
		if application.Metrics {
			for _, capture := range meter.Snapshot() {
				counter++
				id := eventID(latest, counter)
				capture = capture.WithEventID(id).At(latest)
				stream = append(stream, Item{
					Credential: application.Credential,
					Capture:    capture,
				})
				name, kind, value, sum := metricReading(capture)
				project.Record(hex.EncodeToString(id), "metric-point", name, latest)
				project.RecordMetric(name, kind, value, sum)
			}
		}

		book.Projects = append(book.Projects, *project)
	}

	book.RangeEnd = latest + 1
	// The late events sit before the origin, so the range has to reach them or
	// a trend would count fewer than the ledger expects.
	book.RangeStart = scenario.StartAt - 7_200_000
	return stream, book, nil
}

// FastScenario is the small scenario a pull request runs. It is deliberately
// small: enough to prove the ledger mechanism, and quick enough that nobody is
// tempted to skip it. Scenarios grow with the phases. See docs/TESTBED.md
// section 12.
//
// The credentials are issued by TallyOwl and given to the scenario. An
// application holds a key and never knows its project, so the scenario cannot
// invent one either.
func FastScenario(startAt int64, credentials []string) Scenario {
	credential := func(index int) string {
		if index < len(credentials) {
			return credentials[index]
		}
		return ""
	}
	return Scenario{
		Name:    "seedstore-fast",
		Seed:    20260802,
		StartAt: startAt,
		Applications: []Application{
			{
				Name:             "seedstore",
				Credential:       credential(0),
				EndUsers:         4,
				SessionsEachUser: 3,
				ConvertEvery:     3,
				DuplicateEvery:   3,
				LateEvery:        3,
				TraceEvery:       1,
				ErrorEvery:       2,
				Metrics:          true,
				// Phase 8. Three client surfaces for each person, and three
				// return days, so the funnel, the retention matrix, and the
				// timeline each have something exact to compare against.
				SurfacesEachUser: 3,
				ReturnDays:       3,
				// Phase 9. Two people take the marketing journey, so every
				// credited value is a doubling of the table in the journey's
				// note rather than a tally.
				MarketingUsers: 2,
			},
			{
				// The second, smaller application in the same installation. It
				// proves tenant isolation with real traffic rather than only
				// with a negative unit test. See D8 and docs/TESTBED.md.
				Name:             "sidecart",
				Credential:       credential(1),
				EndUsers:         2,
				SessionsEachUser: 2,
				ConvertEvery:     0,
				DuplicateEvery:   0,
				LateEvery:        0,
			},
		},
	}
}

// metricReading reads back what one metric capture holds, so the ledger
// predicts the same number the store will hold.
//
// It reads the capture rather than keeping a second tally beside the meter,
// because a second tally is a second implementation and the two would drift.
func metricReading(capture *tallyowl.Capture) (name, kind string, value, sum float64) {
	point := capture.MetricPoint()
	if point == nil {
		return "", "", 0, 0
	}
	name = point.MetricName
	kind = string(point.MetricKind)
	switch {
	case point.HistogramValue != nil:
		value = float64(point.HistogramValue.Count)
		sum = point.HistogramValue.Sum
	case point.NumberValue != nil:
		value = *point.NumberValue
	}
	return name, kind, value, sum
}

// The client surfaces one person uses. Phase 8's exit criterion names all three
// by name, and each one is a separate anonymous timeline until an `identify`
// joins it to the person.
var surfaces = []string{"web", "rich", "mobile"}

// One day, in milliseconds. The retention matrix counts in these.
const dayMs = 86_400_000

// The names the journey uses.
//
// They are distinct from the names the sessions above use, so a funnel over the
// journey counts the journey. A fixture whose steps could also be matched by
// other traffic is one nobody can work out by hand.
const (
	journeyHome     = "journey-home"
	journeyCheckout = "journey-checkout"
	journeyPurchase = "journey-purchase"
	journeySignUp   = "journey-signed-up"
	journeyVisit    = "journey-visit"
)

// expandJourney adds one identity journey for each person, and records what the
// funnel, the retention matrix, and the timeline must then say.
//
// Every person does exactly the same thing, so every number here is a
// multiplication rather than a tally, and the ledger states it as one. Returns
// the latest producer time it used.
func expandJourney(
	application Application,
	startAt int64,
	project *ledger.Project,
	counter *uint64,
	stream *[]Item,
) int64 {
	latest := startAt
	howManySurfaces := application.SurfacesEachUser
	if howManySurfaces > len(surfaces) {
		howManySurfaces = len(surfaces)
	}

	project.Retention.Period = "day"
	project.Retention.Periods = application.ReturnDays + 1
	project.Retention.CohortSize = application.EndUsers

	for user := 0; user < application.EndUsers; user++ {
		endUser := fmt.Sprintf("j-%s-%03d", application.Name, user)
		// Everybody starts in the same period, so there is one cohort and the
		// matrix has one row a person can read.
		base := startAt

		add := func(capture *tallyowl.Capture, kind, name string, occurredAt int64) {
			*counter++
			id := eventID(occurredAt, *counter)
			capture = capture.WithEventID(id).At(occurredAt)
			*stream = append(*stream, Item{
				Credential: application.Credential,
				Capture:    capture,
			})
			project.Record(hex.EncodeToString(id), kind, name, occurredAt)
			project.RecordTimelineItem(endUser)
			if occurredAt > latest {
				latest = occurredAt
			}
		}

		for surface := 0; surface < howManySurfaces; surface++ {
			// A project-scoped anonymous identifier, one for each surface. The
			// same person on three devices is three of these.
			anonymous := fmt.Sprintf("anon-%s-%03d-%s", application.Name, user, surfaces[surface])
			session := fmt.Sprintf("j-%s-%03d-%s", application.Name, user, surfaces[surface])
			at := base + int64(surface)*1_000
			project.RecordSurface(endUser, anonymous)

			// Anonymous, before the person is known. This is the step a funnel
			// would lose if identity resolved at event time rather than at
			// latest known.
			add(
				tallyowl.PageView(journeyHome).
					WithAnonymous(anonymous).
					WithSession(session),
				"page-view", journeyHome, at)
			project.RecordFunnelStep(0, journeyHome, endUser)

			// The moment the anonymous timeline joins the person.
			add(
				tallyowl.Identify(endUser).
					WithAnonymous(anonymous).
					WithSession(session),
				"identify", "identify", at+100)

			add(
				tallyowl.Event(journeyCheckout).
					WithEndUser(endUser).
					WithAnonymous(anonymous).
					WithSession(session),
				"event", journeyCheckout, at+200)
			project.RecordFunnelStep(1, journeyCheckout, endUser)
		}

		// The sign-up that starts the retention cohort, and the first visit.
		primary := fmt.Sprintf("j-%s-%03d-%s", application.Name, user, surfaces[0])
		add(
			tallyowl.Event(journeySignUp).WithEndUser(endUser).WithSession(primary),
			"event", journeySignUp, base+400)
		add(
			tallyowl.Event(journeyVisit).WithEndUser(endUser).WithSession(primary),
			"event", journeyVisit, base+500)
		project.RecordReturn(0, endUser)

		// The purchase, which is the last funnel step.
		value := tallyowl.MustDecimal("29.99")
		add(
			tallyowl.Conversion(journeyPurchase, &value, "USD").
				WithEndUser(endUser).
				WithSession(primary),
			"conversion", journeyPurchase, base+600)
		project.RecordFunnelStep(2, journeyPurchase, endUser)
		if err := project.RecordConversionValue("29.99"); err != nil {
			// A decimal that will not parse is a defect in this file rather
			// than in what it is testing, and it cannot happen with a literal.
			panic(err)
		}

		// Coming back on later days, which is what a retention matrix counts.
		for day := 1; day <= application.ReturnDays; day++ {
			at := base + int64(day)*dayMs
			add(
				tallyowl.Event(journeyVisit).WithEndUser(endUser).WithSession(primary),
				"event", journeyVisit, at)
			project.RecordReturn(day, endUser)
		}
	}
	return latest
}

// ---------------------------------------------------------------------------
// The marketing journey. Phase 9.
//
// `docs/TESTBED.md` section 8 names the assertion this exists for: "every
// attribution model matches the ledger for traffic that arrives from the
// reference marketing site landing pages."
//
// **The journey is built so that every model divides the value into whole
// units.** Three touches, exactly one decay half-life apart, and a conversion
// value of 70:
//
//	  day  0   paid search    google / cpc / spring       (paid-search)
//	  day  7   a partner link partner.example / referral  (referral)
//	  day 14   direct         no campaign, no referrer    (direct)
//	  day 14   a purchase, for 70, with an order identifier
//
// The decay half-life is seven days, so the weights are 0.25, 0.5, and 1, which
// share out as one, two, and four sevenths. Seventy divides by seven. Every
// other model divides it more easily:
//
//	| Model            | spring | partner | direct |
//	| ---              | ---    | ---     | ---    |
//	| first-touch      | 70     | 0       | 0      |
//	| last-touch       | 0      | 0       | 70     |
//	| last-non-direct  | 0      | 70      | 0      |
//	| linear           | 23.333334 | 23.333333 | 23.333333 |
//	| position         | 28     | 14      | 28     |
//	| decay            | 10     | 20      | 40     |
//
// Linear is the one that does not divide, and that is deliberate: a third of 70
// is not a number, so the fixture proves the parts still add back up to 70.
//
// The touches are anonymous and the `identify` comes after them, so the journey
// also proves the thing L104 found in the funnel: a person who clicked a
// campaign before they signed in is the person who bought afterwards.
// ---------------------------------------------------------------------------

// The campaigns the marketing site runs, and the addresses somebody lands on.
const (
	marketingGoal  = "marketing-purchase"
	marketingValue = "70"
	// The decay half-life the shipped defaults use, and the spacing that makes
	// every weight a power of one half.
	marketingStepDays = 7
)

// The landing pages, exactly as a browser would open them. The campaign
// parameters come out of the address rather than being invented beside it,
// which is what TESTBED.md section 7 means by "campaign parameters arrive from
// a real landing-page URL".
var marketingLandings = []struct {
	// campaign is what the ledger credits.
	campaign string
	// channel is the channel the classifier must reach. A classifier that put
	// the paid click in the organic column fails here.
	channel  string
	url      string
	referrer string
}{
	{
		campaign: "spring",
		channel:  "paid-search",
		url:      "https://seedstore.example/spring?utm_source=google&utm_medium=cpc&utm_campaign=spring",
		referrer: "https://www.google.com/search?q=seed+store",
	},
	{
		campaign: "partner",
		channel:  "referral",
		url:      "https://seedstore.example/partners?utm_source=partner.example&utm_medium=referral&utm_campaign=partner",
		referrer: "https://partner.example/blog/best-seeds",
	},
	{
		// No campaign and no referrer. Somebody typed the address, and the
		// non-direct model exists to skip exactly this.
		campaign: "(none)",
		channel:  "direct",
		url:      "https://seedstore.example/",
		referrer: "",
	},
}

// What each model credits each campaign with, for one person's journey. The
// table in the module note above, as data.
var marketingCredits = map[string]map[string]string{
	"first-touch":     {"spring": "70", "partner": "0", "(none)": "0"},
	"last-touch":      {"spring": "0", "partner": "0", "(none)": "70"},
	"last-non-direct": {"spring": "0", "partner": "70", "(none)": "0"},
	// A third of 70 is not a number. The parts still add back up to 70, and one
	// of them carries the unit that did not divide. The largest remainder goes
	// to the earliest touch on a tie, which is why `spring` holds it.
	"linear":   {"spring": "23.333334", "partner": "23.333333", "(none)": "23.333333"},
	"position": {"spring": "28", "partner": "14", "(none)": "28"},
	"decay":    {"spring": "10", "partner": "20", "(none)": "40"},
}

// What the marketing site spent, for the return column. One import for each
// campaign over the whole period.
var marketingCosts = map[string]string{
	"spring":  "100",
	"partner": "25",
}

// expandMarketing adds the marketing journey for each person and records what
// every attribution model must then produce.
//
// It runs beside the other journeys rather than inside them, for the reason
// L111 gives about the identity journey: a fixture whose touches could also be
// matched by other traffic is one nobody can work out by hand. These people and
// this goal appear nowhere else in the scenario.
func expandMarketing(
	application Application,
	startAt int64,
	project *ledger.Project,
	counter *uint64,
	stream *[]Item,
) (int64, error) {
	latest := startAt
	step := int64(marketingStepDays) * dayMs

	project.Attribution.Goal = marketingGoal
	// The window has to reach the first touch, or the ledger and the query
	// would be answering two different questions. Thirty days is the shipped
	// default and it covers a fourteen-day journey.
	project.Attribution.LookbackMs = 30 * dayMs

	add := func(capture *tallyowl.Capture, kind, name string, occurredAt int64) {
		*counter++
		id := eventID(occurredAt, *counter)
		capture = capture.WithEventID(id).At(occurredAt)
		*stream = append(*stream, Item{
			Credential: application.Credential,
			Capture:    capture,
		})
		project.Record(hex.EncodeToString(id), kind, name, occurredAt)
		if occurredAt > latest {
			latest = occurredAt
		}
	}

	for user := 0; user < application.MarketingUsers; user++ {
		endUser := fmt.Sprintf("m-%s-%03d", application.Name, user)
		anonymous := fmt.Sprintf("manon-%s-%03d", application.Name, user)
		session := fmt.Sprintf("m-%s-%03d", application.Name, user)

		for index, landing := range marketingLandings {
			at := startAt + int64(index)*step
			campaign, err := campaignFromURL(landing.url, landing.referrer)
			if err != nil {
				return 0, err
			}
			// Anonymous. Every touch happens before the person is known, which
			// is what makes the identity join matter.
			add(
				tallyowl.CampaignTouch(campaign).
					WithAnonymous(anonymous).
					WithSession(session).
					WithConsent("granted", "granted"),
				"campaign-touch", touchName(campaign), at)
			project.RecordTouch(landing.channel)
		}

		// The sign-in, after every touch. Latest-known identity is what joins
		// the anonymous clicks to the customer who bought.
		signedInAt := startAt + int64(len(marketingLandings)-1)*step
		add(
			tallyowl.Identify(endUser).
				WithAnonymous(anonymous).
				WithSession(session),
			"identify", "identify", signedInAt+1)

		// The purchase, at the same moment as the last touch, so the decay
		// weight of that touch is exactly one.
		value := tallyowl.MustDecimal(marketingValue)
		order := fmt.Sprintf("order-%s-%03d", application.Name, user)
		purchaseAt := signedInAt
		add(
			tallyowl.Order(marketingGoal, &value, "USD", order).
				WithEndUser(endUser).
				WithAnonymous(anonymous).
				WithSession(session).
				WithConsent("granted", "granted"),
			"conversion", marketingGoal, purchaseAt)
		project.Attribution.Conversions++
		if err := project.RecordConversionValue(marketingValue); err != nil {
			return 0, err
		}

		// The same order again, a minute later. A checkout that retried, a
		// webhook that arrived twice, or a refreshed receipt page. TallyOwl
		// must count one conversion and one value.
		//
		// The event identifier differs, so this is not the duplicate-delivery
		// case: it is a genuinely second row for one order, which only the
		// order identifier can fold.
		add(
			tallyowl.Order(marketingGoal, &value, "USD", order).
				WithEndUser(endUser).
				WithAnonymous(anonymous).
				WithSession(session).
				WithConsent("granted", "granted"),
			"conversion", marketingGoal, purchaseAt+60_000)
		project.Attribution.OrderRepeats++
		// The ledger's revenue total counts logical conversions, and this is
		// the same order. It is deliberately not recorded a second time.

		for model, credits := range marketingCredits {
			for campaign, amount := range credits {
				if err := project.RecordCredit(model, campaign, amount); err != nil {
					return 0, err
				}
			}
		}
	}

	// The imported spend, one record for each campaign over the whole period.
	// A cost import names a campaign and a period, and never an event.
	for _, campaign := range []string{"spring", "partner"} {
		amount := marketingCosts[campaign]
		cost, err := tallyowl.CampaignCost(
			campaign,
			"reference-platform",
			tallyowl.MustDecimal(amount),
			"USD",
			startAt,
			latest,
		)
		if err != nil {
			return 0, err
		}
		add(cost, "campaign-cost", campaign, latest)
		if err := project.RecordCampaignCost(campaign, amount); err != nil {
			return 0, err
		}
	}

	return latest, nil
}

// touchName is the name the head projects a campaign touch under, so the
// ledger's count by name agrees with what a query sees.
func touchName(campaign tallyowl.Campaign) string {
	if campaign.Name != "" {
		return campaign.Name
	}
	return "campaign-touch"
}

// campaignFromURL reads the campaign parameters out of a landing address.
//
// This is what an application does, and doing it here rather than writing the
// parameters out by hand is the point: `docs/TESTBED.md` section 7 requires that
// "campaign parameters arrive from a real landing-page URL". A scenario that
// hand-wrote them would prove that TallyOwl agrees with the scenario rather
// than that it reads what a browser sends.
func campaignFromURL(address, referrer string) (tallyowl.Campaign, error) {
	parsed, err := url.Parse(address)
	if err != nil {
		return tallyowl.Campaign{}, fmt.Errorf("the landing address %q is not one: %w", address, err)
	}
	query := parsed.Query()
	return tallyowl.Campaign{
		Source:   query.Get("utm_source"),
		Medium:   query.Get("utm_medium"),
		Name:     query.Get("utm_campaign"),
		Term:     query.Get("utm_term"),
		Content:  query.Get("utm_content"),
		ClickID:  query.Get("gclid"),
		Referrer: referrer,
		Landing:  parsed.Path,
	}, nil
}
