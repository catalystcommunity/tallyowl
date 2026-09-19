# Phase 9 report: campaigns and business outcomes

What was built, what was tested, and what was not built. Read
[IMPLEMENTATION_LOG.md](IMPLEMENTATION_LOG.md) L113 to L131 beside this: it
holds the reasoning and this holds the state.

**All four Phase 9 exit criteria pass.** Section 5 states each one and names
where it is proved.

**The collection policy now reaches collectors.** The Phase 8 report and the
implementation prompt both named this as the first thing to build next, and it
was built first. L113. The head applies the policy at the commit, as before, and
a collector applies the same compiled snapshot at intake, so a blocked event
costs no batch, no queue write, and no delivery.

**One defect was found by writing the test rather than by reading the code.**
The first run of the reference-application assertion compared two empty answers
and passed: the query range stopped at ten days and the marketing journey spans
fourteen, so every conversion was outside it. An assertion that passes against
nothing is the failure this test bed exists to avoid, and it is recorded in L123
rather than quietly fixed.

**Three more were found by running the loop, and every test in the suite had
missed all three.** A clean installation refused a collection policy every fetch interval,
for ever, because the head sent the compiled defaults at version 0 and the
collector was right to refuse them. A log field named `version` replaced the
software version on the same line. And a declared metric was refused for its
name and dropped in silence, which is the same defect the alpha report records
against the storage instruments. L124 holds all three and says what it means:
the tests in this run are good at the rules and blind to the wiring.

**The owner reviewed every open decision, and this report is written after
it.** Section 11 records what was settled. Two things came out of that review
that were defects rather than choices, and both are fixed: the word `capture`
named two different things at once (L126), and six technical nouns this phase
introduced were missing from the glossary that `docs/DOCUMENTATION.md` requires
them to be in. The second was found by the owner asking what two of the words
meant.

## 1. What happened to Phase 8's open items

`docs/PHASE8_REPORT.md` section 6 ranked seven things it had not built, and its
section 7 listed eight entries marked `Revisit: yes`. This is what became of
each.

| Item | State |
| --- | --- |
| **The collection policy is not distributed to collectors** (ranked first, and named in the implementation prompt as the first thing to build next) | **Built.** L113. The head answers `fetch-policy`, a collector fetches on a timer, caches, and applies at a batch boundary, and both ends compile from one snapshot |
| **Attribution is refused by name** | **Built.** This phase. Six models, the window, and the settings |
| **Nothing drives a segment copy on a timer** | **Unchanged.** A copy is still a call rather than a background task, and a replica that falls behind past the purge point still needs an operator |
| **The general aggregate is not pushed down** (L101) | **Unchanged.** It is the performance half of the fan-out and it needs measures mapped onto partial states |
| **A retention month is 28 days** (L110) | **Unchanged.** This build still carries no timezone database |
| **A copy onto a target that already holds overlapping data is refused** (L097) | **Unchanged.** Reconciling still needs a way to tell two replicas' segments apart, and nothing carries one |
| **The charts still have no templates** | **Unchanged** |

Of the eight entries Phase 8 marked for revisit, **L107 is closed by L113** and
**L106 was already closed by L112**. L097, L099, L101, L103, L104, and L110 are
open and their recommendations are unchanged. L104 is worth naming: event-time
identity resolution is implemented and still nothing on the contract asks for
it, and Phase 9 did not change that — attribution correlates by latest-known
identity for the reason L104 gives about funnels.

## 2. What was built

| Path | Holds |
| --- | --- |
| `crates/tallyowl-head/src/attribution.rs` | The six models, the window, the idempotent order fold, the consent rule, the campaign report, and the durable per-project settings |
| `crates/tallyowl-head/src/campaign.rs` | The channel classifier and the referrer host it reads |
| `crates/tallyowl-head/src/money.rs` | An exact decimal amount, and the largest-remainder division that makes credited parts add up |
| `crates/tallyowl-head/src/policy.rs` | Campaign linking, the consent setting, and the compiled snapshot a collector applies |
| `crates/tallyowl-head/src/query.rs` | The `attribution` and `campaign-summary` query forms |
| `crates/tallyowl-head/src/service.rs` | `fetch-policy` for a collector, and the two attribution-settings operations |
| `crates/tallyowl-collector/src/policy.rs` | The fetch loop, the held snapshot, the staleness, and what intake applies |
| `crates/tallyowl-head/src/starter.rs` | The starter campaign dashboard: six panels, offered once for each project, and a delete that sticks |
| `crates/tallyowl-store/src/control.rs` | The attribution settings record, the campaign-linking and consent fields on a policy record, and a source lookup by identifier |
| `csil/tallyowl-control.csil` | `AttributionModel`, `AttributionSettings`, `CampaignSummaryQuery`, `CampaignLinking`, and two operations at wire IDs 23 and 24 |
| `csil/tallyowl-collector.csil` | `CampaignLinking`, and the blocked names, blocked keys, and linking level on `CollectionPolicy` |
| `csil/tallyowl-ingest.csil` | The campaign and touch links on a conversion, and the whole referrer on a touch |
| `packages/driver-go/item.go` | `Order`, `CampaignTouch`, `CampaignCost`, and `WithConsent` |
| `packages/dashboard/src` | The attribution and campaign-report queries, the model chooser, and the report table |
| `testbed/simulator`, `testbed/ledger` | The marketing journey, and what every model must credit |
| `crates/tallyowl-driver-rust/examples/put_policy.rs` | A small helper that sets a project's policy over the real control socket, so the distribution can be checked through the running loop rather than only in a test |

**1,088 Rust tests, 52 Go tests, and 39 TypeScript tests pass**, against 1,021
and 52 at the end of Phase 8. Lint is clean in all three languages,
`cargo fmt --check` passes, and `./tools.sh gen-check` passes.

**The TypeScript figure needs a note.** The Phase 8 report says 38. The dashboard
package held 15 tests before this run and holds 22 now, and the browser package
and the reference web application hold 11 and 6. That is 32 before and 39 after.
The 38 in the earlier report does not reconcile with either, so one of the two
counts was taken differently. 39 is what `./tools.sh test-ts` reports today.

## 3. Deliverable by deliverable

`docs/PLAN.md` Phase 9 lists six.

| Deliverable | State |
| --- | --- |
| Campaign and referrer capture and classification | **Built.** A touch carries the campaign parameters, the whole referrer, and the landing route. TallyOwl derives the referring host and the channel; a producer cannot send a channel. Ten channels, and the classification runs again at read time so a corrected classifier needs no rewrite. L114 |
| Touchpoint, conversion, exact value and currency, and cost import types | **Built.** All three were already in the contract; this projects them, adds the campaign and touch links on a conversion, and adds the whole referrer on a touch. Money is an exact decimal from the wire to the table cell |
| Versioned attribution models and windows | **Built.** Six models, a window bounded by the project's touchpoint retention, and per-project settings in the control catalog. Every result names the model, the model version, and the settings version. L118, L119 |
| Campaign, conversion, value, cost, and return dashboards | **Built.** The `campaign-summary` operator answers all five, plus assisted conversions, and the dashboard package builds the query, the model chooser, and the report table. A **starter dashboard of six panels** is written for each project the first time the head sees it, and `delete-dashboard` removes it for good. L128 |
| Consent-aware collection and attribution behavior | **Built.** Three campaign-linking levels, a consent state stored with every applicable record, and a per-project setting for whether attribution reads a person who refused. An absent consent state is not a refusal. L120 |
| Idempotent conversion and order handling | **Built.** One goal and one order give one conversion, the earliest record wins, and both rows stay stored. L116 |

## 4. What each model does, and what a test proves about it

Every fixture is one a person can work out by hand. The journey they share is
three touches and one purchase of 100:

```text
  day  0   a paid search click        (google / cpc / spring)
  day  3   a link from a partner site (partner.example)
  day  9   the person types the address (direct)
  day 10   they buy, for 100
```

| Model | Credits | Test |
| --- | --- | --- |
| `first-touch` | `spring` 100 | `first_touch_credits_the_paid_click_that_started_it` |
| `last-touch` | direct 100 | `last_touch_credits_the_address_bar` |
| `last-non-direct` | `partner` 100 | `last_non_direct_skips_the_address_bar_and_credits_the_partner` |
| `linear` | 33.333333, 33.333333, 33.333334 | `linear_divides_the_hundred_three_ways_and_loses_nothing` |
| `position` | 40, 20, 40 | `position_gives_the_ends_forty_each_and_the_middle_twenty` |
| `decay` | 100, 200, 400 of 700 over a different journey | `decay_halves_the_credit_for_every_week_before_the_purchase` |

One test covers all six at once: `every_model_credits_the_whole_value_and_never_more`
asserts that what the rows add up to, plus what nothing earned, is the revenue
that arrived. A model that broke that would make a campaign report disagree with
a revenue report, and nobody could say which one was right.

## 5. Exit criteria

| Criterion | State |
| --- | --- |
| First, last, non-direct, linear, position, and decay fixtures are stable | **Passes.** The six in section 4, plus the edge cases each model has: a journey that is direct from end to end, a position split over one touch and over two, a touch outside the window, and a touch after the conversion |
| Model changes recompute from immutable facts | **Passes.** `changing_the_weights_changes_the_answer_and_not_one_stored_row` answers one set of rows twice under two settings, compares the two answers, and asserts the rows are what they were. The settings version travels in the result, so two answers under different weights cannot be mistaken for one number |
| Missing consent excludes data according to policy | **Passes.** `a_person_who_denied_marketing_consent_is_left_out_when_the_policy_asks` runs the same rows with the setting off and on: two conversions and 200, then one and 100, with the refusal counted. `an_absent_consent_state_is_not_a_denial` proves the other half, because treating silence as a refusal would make every project that has not instrumented consent report no revenue the day the setting was turned on |
| Every attribution model matches the ledger for traffic that arrives from the reference marketing site landing pages | **Passes.** `every_attribution_model_matches_the_ledger_for_the_marketing_site_traffic`. The simulator parses real landing-page addresses, writes the ledger before it sends anything, and this asks TallyOwl the same six questions through the head's own query executor. It also asserts the classified channels, the order fold, and the imported cost and return |

## 6. What is not built, and where it would go

The owner reviewed each of these. Four are now **scheduled for Phase 10** rather
than deferred again, on the reasoning that measuring early beats waiting for
evidence that nothing is generating.

**Decided to stay as it is:**

1. **A collector's readiness does not fail on staleness.** The age of the last
   successful fetch is on the health record so an operator can alert on it, and
   readiness stays passing. A collector that stopped accepting telemetry when it
   lost the head would turn a control-plane outage into a data-plane one, and
   the last good policy is still in force. L113.
2. **A cost never divides by channel.** A cost import names a campaign and a
   period, so a report grouped by channel, source, medium, or content shows an
   empty cost column and says why. Dividing the spend evenly would be TallyOwl
   inventing the number a person is reading the report to find out. L121.
3. **The engine and network lists in the classifier are short and
   hand-written.** A search engine or a social network that is not on them lands
   in `referral`, which is wrong rather than harmful. Grow them from real
   traffic. L114.

**Scheduled for Phase 10:**

4. **Calendar periods, and a retention month that is 28 days.** L110. The owner
   added the shape: keep storing everything in UTC, and convert only where a UTC
   comparison will not do, such as a daylight-saving boundary. Evaluate whether
   a full timezone database is needed or a conversion library covers it.
5. **The identity graph is rebuilt on every question.** L103. The cost grows
   with the installation's age rather than with the query, which makes it the
   one item that gets worse with no operator action.
6. **The general aggregate is not pushed down.** L101. Correctness holds and the
   design's intent does not.
7. **The consensus-log constants.** L099. The snapshot threshold and the log to
   keep become settings.

**Open, and the most important thing on this list:**

8. **An intermittent hang in the append log.** L131. It reproduced at about one
   run in twenty before this work and at none in four hundred after, and **the
   root cause was never identified** — two independent fixes each moved the
   rate and neither removed it alone, which is a narrowed window rather than a
   closed one. No commit was ever lost; it is liveness, not durability. Closing
   it needs a stack, which needs `kernel.yama.ptrace_scope` relaxed. Look at the
   lock ordering between the WAL mutex and the catalog and segment locks in
   `SegmentedStore::commit` first.

**Not a deferral a decision can lift:**

9. **A copy onto a target that already holds overlapping data is refused.**
   L097. Reconciling needs a way to tell two replicas' segments apart, and
   neither the segment format nor the manifest carries one. This is a format
   change rather than something waiting on evidence, and it was not among the
   options the owner was shown.

**Unchanged:**

10. **The charts still have no templates.** True before Phase 7 and still true.

## 7. Design documents that changed, and why

| Document | Change |
| --- | --- |
| `docs/QUERY.md` | Section 12.6 now states the attribution rules, the model table, and that a request names a model and never a weight. Section 12.7 is the new campaign summary. The metric operators moved from 12.7 to 12.8 |
| `docs/POLICY.md` | Section 7 says both ends apply the policy and why each end has a different reason. Sections 7.1 and 7.2 are new: the three campaign-linking levels and the consent setting. Two required tests were added |
| `docs/DECISIONS.md` | D40 holds the shipped default values and the reason each one holds, which is what D40 said Phase 9 would do. D30 gained a section on how the implementation reads it. Open item 11 is closed |
| `docs/DATA_MODEL.md` | Section 3.6 says a producer never sends a channel, that the classification also runs at read time, and that first and last touch position is not a field |
| `docs/TESTBED.md` | Build-order step 7 is marked built and says what the marketing journey does |

No measurement contradicted a document in this run.

## 8. Everything marked `Revisit: yes`, with a recommendation

Seven of the twelve entries added in this run are marked for revisit.
`docs/PHASE8_REPORT.md` section 7 holds the same list for Phase 8,
`docs/PHASE7_REPORT.md` section 8 for Phase 7, and `docs/ALPHA_REPORT.md`
section 5 for Phases 1 to 6.

| Entry | What | Recommendation |
| --- | --- | --- |
| L113 | The collector's policy fetch interval is a setting with a first value of 30 seconds | **Leave it and measure.** It decides how long a kill switch takes to reach a collector, and a fetch with nothing to send costs a few bytes. A real installation will say whether 30 seconds is too slow |
| L114 | The engine and network lists in the classifier are short | **Grow them from real traffic**, not from a list somebody wrote. An unknown site lands in `referral`, which is wrong rather than harmful, and the reference application will not find them |
| L115 | The two-second fold window for one landing is a constant | **Leave it until a real client is measured.** A client that sends a touch and a page view further apart would be counted twice, and nothing has measured what a real one does |
| L118 | Every shipped attribution default | **Review all seven.** Not one is measured. They are the values a person would defend in a meeting, not values this project has evidence for, and D40 says there is no migration cost in changing them |
| L120 | Campaign linking strips the person from a touch and the campaign from everything else | **This is the one to look at first.** The asymmetry is defensible and it is the kind of rule an operator meets once and is surprised by. The consequence — an application that tags only page views records no campaign data at the `unlinked` level — is written down, and a different split is defensible |
| L121 | `docs/QUERY.md` did not define a campaign summary before this run | **Confirm the columns.** Section 12.7 now defines it, and the owner may want a different set |
| L124 | `let _ = metrics.declare(...)` hides a refused metric name at every declaration site | **Close it everywhere.** A helper that panicked in a debug build, or one test in `tallyowl-obs` that walks every declared name, would cost less than the two times this defect has now been found by reading an exposition |

## 9. Anything blocked

Nothing. No credential, no account, no external service, and no csilgen
capability was needed. Every contract change validated and generated for Rust,
Go, and TypeScript on the first attempt.

One thing is worth naming because it is a rule this project holds. `csilgen` on
this machine is built from `ec22e9248d34` and the repository pins `d693a94d5b72`.
`./tools.sh gen` says so on every run and `./tools.sh gen-check` passes, so the
output matches what is checked in. The pin was already stale before this run and
nothing here changed it.

## 10. How this was checked through the running loop

`./tools.sh dev reset`, `./tools.sh dev up`, and then:

```sh
cargo run -p tallyowl-driver-rust --example put_policy -- checkout-completed
cargo run -p tallyowl-driver-rust --example send_one_event
curl -s http://127.0.0.1:5101/metrics | grep dropped_by_policy
```

What it showed, on the release binaries built from this work:

- the collector logged `Applied a collection policy.` with `policy_version` 2,
  30 seconds after `put-policy` returned;
- `send_one_event` sends two events and the collector accepted one. The blocked
  one never reached the durable queue;
- `tallyowl_items_dropped_by_policy_total` reported 1 and
  `tallyowl_policy_fetches_total{outcome="applied"}` reported 1;
- the round trip still works, and the query answers `complete: true`.

Three of the defects in L124 came out of this and none of them broke a test. The
release binaries were rebuilt before each run, because the implementation prompt
records a session that verified a change against a release binary two hours
older than the change.

## 11. What the owner settled at review

Every open decision from this phase and the ones before it, and what was chosen.

| Decision | Outcome | Where |
| --- | --- | --- |
| The seven shipped attribution defaults | **Keep all seven.** Cheap to change and no evidence exists yet | D40, L118 |
| Assisted conversions | **Build them now.** A column on the campaign report and on the attribution operator | L127 |
| Collector policy staleness | **Surface it, never fail readiness.** It was dead code; it is on the health record now | L113 |
| Campaign linking, at the `unlinked` level | **Keep the asymmetric split.** It only affects an operator who opted in, and the alternative that kept the person-to-campaign tie would have failed anyone who chose the setting for a legal reason | L120, L125 |
| Consent generally | **Optional. TallyOwl does not police.** Make the strict path easy, never impose it. Every default was checked against this | L125 |
| `capture` and `touchpoint` | **Rename both.** `capture` already meant the ingest operation, and `touch` is now the one noun | L126 |
| The csilgen pin | **Pin the version**, and pin the transport by its release tag | L129 |
| A default campaign dashboard | **Seed one.** Six panels, offered once, and a delete sticks | L128 |
| `metrics.declare` | **Expect the result at every call site.** A refused name now fails at boot instead of vanishing | L124 |
| Event-time identity | **Put it on the contract.** Optional, defaulting to latest-known | L130 |
| Calendar periods | **Phase 10.** Store UTC; convert only where UTC will not do | L110 |
| Alpha scope | **Ship one product**, with replication dormant on the home profile | L083 |
| L103, L101, L099 | **Phase 10.** Measure early rather than wait | Section 6 |
