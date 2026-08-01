# Query algebra

This document uses these abbreviations: CBOR Service Interface Language (CSIL),
Concise Binary Object Representation (CBOR), and universally unique identifier
(UUID). [DOCUMENTATION.md](DOCUMENTATION.md) holds the full list.

## 1. Purpose

The query algebra is the public contract for every TallyOwl analytic question.
The dashboard uses it. An operator tool uses it. A compatibility adapter
compiles into it.

The algebra is a versioned typed CSIL structure, not a text language. A caller
sends an operator tree. TallyOwl does not parse a string from a browser.

This document defines the operators, the types, the exactness rules, the
pushdown contract, and the result envelope. It does not define the storage
layout. See [STORAGE.md](STORAGE.md).

## 2. Principles

1. A query is a typed value. It has a schema and a version.
2. The caller never names a workspace or a project. The session determines
   scope.
3. Every result declares its exactness, its freshness, and its cost.
4. An operator that cannot give a correct answer fails. It does not guess.
5. A partial aggregate must merge. The coordinator never pulls raw rows to
   compute an aggregate.
6. Every operator has a bound. No operator can run without a limit.
7. The same tree runs on one node and on many nodes.

## 3. Scope and authorization

The session supplies the workspace. The query supplies a project ID from the
projects that the session can read.

TallyOwl refuses a project that the session cannot read. It returns an
authorization error. It does not return an empty result, because an empty
result tells a caller that the project exists.

A query reads one project. A query that reads more than one project needs an
explicit cross-project identity policy. See [DESIGN.md](DESIGN.md) section 6.

## 4. Type system

### 4.1 Value types

- null
- boolean
- integer, signed and unsigned, 64-bit
- decimal, for money and exact fractional values
- float, 64-bit
- timestamp and duration
- text and bytes
- UUID and fixed identifier
- Internet Protocol address

A decimal never becomes a float in an aggregate. Money keeps its exact value.

### 4.2 Typed variants

One field name can hold more than one type. D20 keeps typed variants.

A field reference selects one type. A caller can request an explicit
conversion. TallyOwl never converts without that request.

A field reference that matches more than one type without a selection is an
error. The error lists the available types.

### 4.3 Determinism

- An integer sum that overflows is an error. It does not wrap.
- A float sum merges in a fixed order: segment ID, then row ID. The same data
  therefore gives the same result.
- A comparison between two types is an error unless the caller requests a
  conversion.
- Null never equals null in a filter. A caller uses an explicit null test.

## 5. Expressions

An expression is a typed tree. The permitted nodes are:

- a literal;
- a field reference, with an optional type selection;
- a comparison: equal, not equal, less, less or equal, greater, greater or
  equal;
- a set test: in, not in, with a bounded list;
- a text test: starts with, ends with, contains, with a bounded pattern;
- a null test: is null, is not null;
- a logical operator: and, or, not;
- an arithmetic operator: add, subtract, multiply, divide;
- a time function: truncate to interval, extract part, shift by duration;
- a conversion function, named and explicit.

There is no user-defined function. There is no regular expression in the first
release. There is no arbitrary code. See the out-of-scope list in
[DESIGN.md](DESIGN.md).

Expression nesting has a configurable maximum depth. The default is 16.

## 6. Core operators

| Operator | Purpose |
| --- | --- |
| `scan` | Read one dataset in one project, with a time range |
| `filter` | Keep rows that satisfy an expression |
| `project` | Select columns and computed expressions |
| `aggregate` | Group by dimensions and compute measures |
| `sort` | Order rows by keys |
| `limit` | Bound the row count, with an offset or a cursor |
| `join` | Correlate two inputs on an exact ID, with a bound |
| `union` | Combine inputs with the same output schema |

### 6.1 scan

A scan names a dataset, a project, a time range, and a time basis. The datasets
are the canonical views in [DATA_MODEL.md](DATA_MODEL.md), such as `events`,
`spans`, `error_occurrences`, and `metric_points`.

A scan always has a time range. A scan without a time range is an error.

### 6.2 join

A join stays bounded and exact. It correlates on a built-in correlation ID,
such as a trace ID, a request ID, a session ID, or an end-user ID.

Rules:

- both sides read the same project;
- both sides have a time range;
- the join key is an exact-indexed field;
- the join has a maximum row count on each side;
- the join fails when either side exceeds its bound.

There is no join on an arbitrary expression. There is no cross join.

## 7. Aggregates and partial states

A storage node computes a partial state. The coordinator merges partial states.
Each partial state has a bounded size.

| Measure | Partial state | Exactness |
| --- | --- | --- |
| `count` | integer | exact |
| `sum` | typed sum | exact |
| `min`, `max` | typed value | exact |
| `avg` | sum and count | exact |
| `count_distinct` | value set under a cap | exact until the cap |
| `count_distinct_approx` | named sketch | approximate, with an error bound |
| `quantile` | sorted values under a cap | exact until the cap |
| `quantile_approx` | named sketch | approximate, with an error bound |
| `histogram_merge` | aligned bucket counts | exact when buckets align |
| `top_k` | counts under a cap | exact until the cap |
| `top_k_approx` | named sketch | approximate, with an error bound |
| `rate`, `increase` | per-series values and reset marks | exact |

An exact measure that reaches its cap fails with a typed error. The error names
the cap and names the approximate measure that the caller can select.

An exact measure never becomes approximate on its own. D21 requires this.

`histogram_merge` fails when bucket boundaries do not align. The caller must
request an explicit rebucket. TallyOwl does not silently rebucket.

## 8. Time semantics

A query selects one time basis:

- `occurred_at`, the producer event time;
- `received_at`, the trusted intake time;
- `committed_at`, the storage commit time.

A caller supplies a timezone for interval truncation. TallyOwl uses
Coordinated Universal Time when the caller supplies none.

An interval is a fixed duration or a calendar interval. A calendar interval
uses the supplied timezone.

A comparison window repeats the same tree over a second time range. The result
carries both ranges. TallyOwl does not align two ranges of different lengths.

Late data can change a result for a past range. A result reports the lateness
horizon that applied.

## 9. Consistency, freshness, and completeness

A query selects a consistency mode. D18 defines them.

- `committed` routes to a voting replica at the requested commit watermark.
- `bounded-stale` uses a read replica whose watermark satisfies the request.

Every result gives its watermark. A dashboard can accept a watermark that is
not more than five seconds old. That limit is configurable.

A missing tablet causes a typed incomplete-result error by default. A caller
must request partial mode explicitly. A partial result names the missing
tablets and the missing time ranges. A partial result can never mark itself
complete.

### Provisional spans

Tail sampling decides after commit. See D35. A query can therefore find a span
that is still in the provisional retention class.

Rules:

- a detail query and an exact lookup return provisional spans, because storage
  already committed them;
- an aggregate over spans marks a result that covers an open decision window;
- the result names the time range that is still open.

A caller that needs a stable trace aggregate waits for the decision window to
close. TallyOwl reports the window instead of hiding it.

## 10. Pushdown contract

A storage node evaluates locally:

- time and project pruning;
- segment pruning by statistics and Bloom filters;
- exact index lookup;
- filter expressions;
- column selection;
- partial aggregate states;
- local sort and local limit.

The coordinator evaluates:

- the merge of partial states;
- the global sort and the global limit;
- the join of bounded inputs;
- the comparison window;
- the domain operator finalization.

The coordinator never pulls raw rows to compute an aggregate. It pulls raw rows
only for a detail query, a trace assembly, or an exact lookup.

Fan-out has a configurable maximum. A query that needs more tablets than the
maximum fails with a typed error.

## 11. Field access classes

D20 gives each dynamic field a physical access class. The algebra respects it.

| Class | Filter | Group by | Cost |
| --- | --- | --- | --- |
| `lookup` | exact index | scan | low for equality |
| `facet` | exact index | typed column | low |
| `stored` | scan | scan | high |

A group by a `stored` field is legal. It is not cheap. The planner reports the
estimated cost, and the budget can refuse it.

A property is a dimension whatever its origin. A query can also filter on the
origin itself. See D38.

## 12. Domain operators

A simple aggregate cannot express these questions. Each domain operator has a
typed input, a bounded cost, and a defined result.

### 12.1 funnel

Input: an ordered list of step definitions, a window duration, and a
correlation basis.

- the correlation basis is an end user, a session, or a group;
- a step is an event definition with an optional filter;
- ordered mode requires the steps in order; unordered mode does not;
- an exclusion step voids a sequence when it occurs inside the window;
- the first matching sequence for one correlation key counts once.

Result: a count for each step, the conversion time distribution, and an
optional breakdown by one dimension.

### 12.2 retention

Input: an initial event definition, a returning event definition, a period, and
a number of periods.

- a period is daily, weekly, or monthly, in the supplied timezone;
- the cohort key is the first period in which the initial event occurred;
- first-time and recurring semantics are explicit in the request.

Result: a matrix of cohort by period, with counts and rates.

### 12.3 path

Input: an anchor event definition, a direction, a depth, and a minimum
frequency.

- the direction is previous or next;
- depth has a configurable maximum;
- a node is a semantic page or a semantic interaction;
- loop collapsing is explicit in the request.

Result: a bounded tree of nodes with counts.

### 12.4 trace assembly

Input: a trace ID.

Result: the spans of that trace with parent and child structure, the critical
path, and any linked error event IDs.

Trace assembly uses the exact index. It does not scan.

### 12.5 timeline

Input: an end-user ID or a session ID, a time range, and a set of telemetry kinds.

Result: the merged envelopes in event-time order, with bounded pagination.

### 12.6 attribution

Input: a conversion definition, a model name, a lookback window, and a
touchpoint filter.

Result: credited value for each touchpoint dimension.

The operator shape is stable. The model weights are Phase 9 work. The result
always names the model and its version, because a model change recomputes from
immutable facts.

### 12.7 metric operators

- `rate` and `increase` handle a counter reset. A reset is a decrease in a
  cumulative series.
- `last`, `min`, `max`, and `avg` apply to a gauge.
- `quantile` applies to a histogram through `histogram_merge`.
- an exemplar links a point to a trace ID.

A stable hash of the metric identity and its canonical typed labels identifies
a series. TallyOwl verifies the complete label set. See
[HIGH_CARDINALITY.md](HIGH_CARDINALITY.md).

## 13. Ordering and pagination

A sort names keys and a direction. A sort without a total order appends the
event ID as a final key. The order is therefore deterministic.

Pagination uses an opaque cursor. A cursor encodes the sort keys, the catalog
generation, and the tombstone generation.

A cursor is valid only for its generations. TallyOwl refuses a stale cursor
with a typed error. It does not return rows from a different snapshot.

Offset pagination exists for small results. It has a configurable maximum
offset.

## 14. Budgets

Every query carries a budget. The service applies the smaller of the request
budget and the policy budget.

- wall-clock deadline;
- scanned bytes;
- scanned segments;
- returned rows;
- coordinator memory;
- concurrent queries for each project.

A query that exceeds a budget fails with a typed error. The error names the
budget and gives the observed value.

A query never returns a truncated result that looks complete.

## 15. Result envelope

Every result carries:

- the schema and the algebra version;
- the exactness of each measure, with the method and the error bound when the
  measure is approximate;
- the commit watermark and the freshness;
- the completeness, and the missing tablets and ranges when partial;
- scanned bytes, scanned segments, and cold bytes;
- cache information;
- the applied budget and the observed cost;
- the tombstone generation;
- warnings, such as an open tail-sampling window or a type conflict.

A caller can therefore explain any number that TallyOwl returns.

## 16. Versioning

The algebra has a version. The version appears in every request and every
result.

- Add an operator or a field as optional. Assign each wire ID one time.
- Never change the meaning of an existing operator.
- A reader that meets an unknown required operator refuses the query. It does
  not ignore the operator.
- The head accepts the current and the previous algebra version once the
  project reaches a release candidate. See D31.

## 17. Adapters

An adapter compiles an outside language into this algebra. An adapter is not a
second contract.

- A PromQL adapter translates metric queries.
- A bounded SQL adapter is not part of the first storage release.
- An operator who needs unrestricted analysis exports Parquet and uses an
  independent tool.

An adapter uses a compatible MIT or Apache-2.0 parser. See D18.

## 18. Approximate measures

Each approximate measure names one algorithm, and every result reports the
method and the bound that applied. See D50.

| Measure | Algorithm | Bound |
| --- | --- | --- |
| `count_distinct_approx` | HyperLogLog++ | Stated for the configured precision |
| `quantile_approx` | DDSketch | Guaranteed relative error |
| `top_k_approx` | Space-Saving | Bounded by counter count |

Accuracy comes before footprint. Size each sketch for accuracy, and let a
project that prefers a smaller footprint reduce it.

An exact measure never falls back to a sketch.

## 19. Explain

An explain operation returns a plan tree and a plain-language summary, and it
always returns both. See D52.

The tree carries the detail an engineer needs for each node:

- the operator;
- the estimated rows and bytes;
- the segments it would touch;
- whether it reaches the cold tier;
- the indexes it would use;
- the exactness of each measure.

The summary translates that tree. A person uses it to decide whether to run the
query, to adjust it, or to drop it, without reading the tree.

An estimate that the planner cannot make returns unknown. It never returns a
guess.

### The candidate segment count is always shown

A lookup on a high-cardinality value probes the tablet locator, which returns
candidate segments. That count is the cost of the query, and it grows with the
time range: measured at 100 million end users, a one-day range returns 2
candidate segments and a 30-day range returns 60. An unbounded range reads the
whole retention window.

The plan tree therefore carries the candidate segment count for every locator
probe, and the summary states it in plain language with the time range as the
reason. A person who widens a time range sees the cost before they pay it.

TallyOwl never silently narrows a range to keep a query cheap. It shows the
count, applies the budget, and refuses with a typed error when the budget
cannot cover it. See [BENCHMARKS.md](BENCHMARKS.md) section 12b.

## 20. Saved queries

A saved query records the algebra version of its author and executes on the
current algebra. Section 16 forbids a change to the meaning of an existing
operator, so the current version gives the same answer.

Golden query tests replay saved trees across versions and require identical
results. See D51.

## 21. Open items

These need the capacity envelope in D10 before anyone can choose them:

1. the default caps for `count_distinct`, `quantile`, and `top_k`;
2. the default query budgets for the home profile and for a cluster;
3. the maximum fan-out for each profile;
4. the default sketch precision for each approximate measure.

The reference application measures them. See [TESTBED.md](TESTBED.md).
