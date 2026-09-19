// The two queries the dashboard runs, built as typed trees.
//
// `docs/QUERY.md`: the query algebra is a typed tree, not a text language.
// TallyOwl never parses a query string from a browser, so the dashboard builds
// nodes and the head evaluates them. Nothing here concatenates a string into a
// query, and there is nothing in the head that would read one.
//
// A child node travels as `bytes` holding the encoded child. `tallyowl-control.csil`
// says why: a reader can bound a subtree before it decodes one, which is what a
// query budget needs, and a nesting depth becomes a decode count rather than a
// type, so a hostile tree cannot exhaust a stack during parsing.

import {
  toExpressionNodeCbor,
  toQueryNodeBoxCbor,
  type AttributionModel,
  type CampaignSummaryQuery,
  type ExpressionNode,
  type QueryNodeBox,
  type QueryRequest,
  type TimeRange,
} from "./control-api.ts";

/// Every model this build answers, in the order a person compares them.
///
/// First and last are the two arguments; the rest are the ways of settling it.
/// The list is here rather than in the view, because a chooser that offered a
/// model the head does not answer would be a refusal a person cannot act on.
export const ATTRIBUTION_MODELS: readonly AttributionModel[] = [
  "first-touch",
  "last-touch",
  "last-non-direct",
  "linear",
  "position",
  "decay",
];

/// Which touch dimension a campaign report groups by. It is the field's own
/// type rather than a second list, so a value this build cannot send does not
/// compile.
export type CampaignDimension = NonNullable<CampaignSummaryQuery["dimension"]>;

export type { AttributionModel };

function node(box: QueryNodeBox): Uint8Array {
  return toQueryNodeBoxCbor(box);
}

function expression(node: ExpressionNode): Uint8Array {
  return toExpressionNodeCbor(node);
}

/// Read one project's events over a time range. Every query below starts here.
function scan(projectId: Uint8Array, range: TimeRange): QueryNodeBox {
  return {
    node: "scan",
    scan: { scan: "events", projectId, range },
  };
}

/// Count events for each bucket of time. This is the chart.
export function eventTrend(
  projectId: Uint8Array,
  range: TimeRange,
  bucketMs: number,
): QueryRequest {
  return request({
    node: "aggregate",
    aggregate: {
      input: node(scan(projectId, range)),
      measures: [{ kind: "count", alias: "events" }],
      dimensions: [],
      interval: { fixedMs: bucketMs },
    },
  });
}

/// The most frequent event names in the range. This is the breakdown beside
/// the chart, and it is a `limit` over a `sort` over an `aggregate` rather than
/// anything the dashboard computes for itself.
export function eventBreakdown(
  projectId: Uint8Array,
  range: TimeRange,
  limit: number,
): QueryRequest {
  const grouped: QueryNodeBox = {
    node: "aggregate",
    aggregate: {
      input: node(scan(projectId, range)),
      measures: [{ kind: "count", alias: "events" }],
      dimensions: [{ field: { name: "name" }, alias: "name" }],
    },
  };
  const sorted: QueryNodeBox = {
    node: "sort",
    sort: {
      input: node(grouped),
      sort: [{ alias: "events", direction: "desc" }],
    },
  };
  return request({
    node: "limit",
    limit: { input: node(sorted), limit },
  });
}

/// Read one project's metric points over a time range.
function metricScan(projectId: Uint8Array, range: TimeRange): QueryNodeBox {
  return {
    node: "scan",
    scan: { scan: "metric_points", projectId, range },
  };
}

/// The rate of every metric series in the range, for each bucket of time.
///
/// `rate` is the operator, not a subtraction the dashboard does for itself.
/// QUERY.md section 12.8 says `rate` handles a counter reset, and a dashboard
/// that subtracted two readings would draw a spike every time a service
/// restarted.
export function metricRate(
  projectId: Uint8Array,
  metricName: string,
  range: TimeRange,
  bucketMs: number,
): QueryRequest {
  const named: QueryNodeBox = {
    node: "filter",
    filter: {
      input: node(metricScan(projectId, range)),
      filter: expression({
        expression: "compare",
        compare: {
          compare: "eq",
          left: expression({ expression: "field", field: { name: "metric_name" } }),
          right: expression({
            expression: "literal",
            literal: { kind: "text", textValue: metricName },
          }),
        },
      }),
    },
  };
  return request({
    node: "aggregate",
    aggregate: {
      input: node(named),
      measures: [{ kind: "rate", alias: "each_second" }],
      dimensions: [],
      interval: { fixedMs: bucketMs },
    },
  });
}

/// Which metric names this project holds, and how many points each has.
///
/// A dashboard cannot chart a metric it does not know the name of, and asking
/// the head is better than configuring a list that goes stale.
export function metricNames(
  projectId: Uint8Array,
  range: TimeRange,
  limit: number,
): QueryRequest {
  const grouped: QueryNodeBox = {
    node: "aggregate",
    aggregate: {
      input: node(metricScan(projectId, range)),
      measures: [{ kind: "count", alias: "points" }],
      dimensions: [{ field: { name: "metric_name" }, alias: "metric_name" }],
    },
  };
  const sorted: QueryNodeBox = {
    node: "sort",
    sort: {
      input: node(grouped),
      sort: [{ alias: "points", direction: "desc" }],
    },
  };
  return request({
    node: "limit",
    limit: { input: node(sorted), limit },
  });
}

/// A request that states its own consistency and refuses a partial answer.
///
/// `allowPartial` is false on purpose. A dashboard that quietly drew a smaller
/// number would be the failure `docs/FAILURE_MODES.md` section 2 ranks worst:
/// an incomplete answer presented as a complete one.
function request(box: QueryNodeBox): QueryRequest {
  return {
    algebraVersion: 1,
    consistency: "committed",
    allowPartial: false,
    form: "node",
    node: node(box),
  };
}

// ---------------------------------------------------------------------------
// Campaigns and business outcomes. Phase 9.
//
// These are domain operators rather than trees. `docs/QUERY.md` section 12: "A
// simple aggregate cannot express these questions." A campaign report joins
// touches, conversions, and an imported cost, and the credited value is a
// division of money by a weight, so nothing the dashboard could assemble out of
// scans would produce it.
//
// The model and the window travel; the **weights do not**. D40 makes a weight
// an operator's configuration for the project, and a dashboard that could send
// weights could make one campaign outrank another by asking differently.
// ---------------------------------------------------------------------------

/// Which touch earned each conversion, and how much of its value.
export function attribution(
  projectId: Uint8Array,
  range: TimeRange,
  goal: string,
  model: AttributionModel,
  lookbackMs: number,
  breakdown?: string,
): QueryRequest {
  return {
    algebraVersion: 1,
    consistency: "committed",
    allowPartial: false,
    form: "attribution",
    attribution: {
      projectId,
      range,
      conversionGoal: goal,
      model,
      lookbackMs,
      breakdown:
        breakdown === undefined
          ? undefined
          : { field: { name: breakdown }, alias: breakdown },
    },
  };
}

/// The campaign report: touches, people, conversions, value, cost, and return.
export function campaignSummary(
  projectId: Uint8Array,
  range: TimeRange,
  goal: string,
  model: AttributionModel,
  lookbackMs: number,
  dimension: CampaignDimension = "campaign",
): QueryRequest {
  return {
    algebraVersion: 1,
    consistency: "committed",
    allowPartial: false,
    form: "campaign-summary",
    campaignSummary: {
      projectId,
      range,
      conversionGoal: goal,
      model,
      lookbackMs,
      dimension,
    },
  };
}
