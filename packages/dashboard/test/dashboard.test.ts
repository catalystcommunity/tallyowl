// The dashboard against a head that speaks the real contract.
//
// The fake here is a head, not a mock of the dashboard's own seam. It decodes a
// real CSIL-RPC frame, decodes a real `QueryRequest` out of it, and answers
// with a real `QueryResponse`, so the test proves the wire and not a stub.
//
// The rules under test:
//
// - the dashboard sends a typed query tree and never a query string;
// - the session travels on the connection and reaches the head;
// - a rejected request and an unreachable head are different failures;
// - a callback completes a sign-in exactly once.

import assert from "node:assert/strict";
import { test } from "node:test";

import { RpcRequest, RpcResponse } from "../../../.deps/csilgen/transports/typescript/src/rpc.ts";
import {
  CsilDecimal,
  fromQueryNodeBoxCbor,
  fromQueryRequestCbor,
  toBeginLoginResponseCbor,
  toCompleteLoginResponseCbor,
  toQueryResponseCbor,
  toServiceErrorCbor,
  type QueryRequest,
} from "../src/control-api.ts";
import {
  ATTRIBUTION_MODELS,
  attribution,
  campaignSummary,
  eventBreakdown,
  eventTrend,
  metricNames,
  metricRate,
} from "../src/queries.ts";
import { Control, ControlError, TransportFailure } from "../src/transport.ts";
import * as signIn from "../src/sign-in.ts";
import * as view from "../src/view.ts";

const PROJECT = new Uint8Array(16).fill(9);

/// Enough of a document for the element builders under test.
///
/// The view module builds elements and the test runner has no browser. The
/// alternative is to leave every rendering rule untested, and two of them are
/// rules rather than styling: a campaign with no spend shows a dash rather than
/// a zero, and a report with nothing in it says so rather than drawing an empty
/// table. Both are the difference between "no data" and "this is broken", which
/// `docs/FAILURE_MODES.md` section 2 puts above a stopped request.
///
/// It holds only what those builders call. A shim that grew to a browser would
/// be a second implementation of one.
interface ShimElement {
  tagName: string;
  className: string;
  textContent: string;
  children: ShimElement[];
  append(...nodes: ShimElement[]): void;
  setAttribute(name: string, value: string): void;
  getAttribute(name: string): string | undefined;
}

function shimDocument(): void {
  const create = (tagName: string): ShimElement => {
    const attributes = new Map<string, string>();
    const element: ShimElement = {
      tagName,
      className: "",
      children: [],
      append(...nodes) {
        element.children.push(...nodes);
      },
      // Attributes are recorded, because two of the rules under test are
      // attributes rather than text: a workflow with work in quarantine is
      // marked for a person, and an alert row carries the state it is in.
      setAttribute(name, value) {
        attributes.set(name, value);
      },
      getAttribute(name) {
        return attributes.get(name);
      },
      get textContent(): string {
        return own.length > 0 ? own : element.children.map((c) => c.textContent).join(" ");
      },
      set textContent(value: string) {
        own = value;
      },
    };
    let own = "";
    return element;
  };
  (globalThis as { document?: unknown }).document = {
    createElement: create,
    createElementNS: (_namespace: string, tagName: string) => create(tagName),
  };
}

shimDocument();

/// What one call to the fake head recorded.
interface Seen {
  readonly op: string;
  readonly auth: string | undefined;
  readonly payload: Uint8Array;
}

/// A head that answers one CSIL-RPC frame per request, over `fetch`.
function head(answer: (op: string, payload: Uint8Array) => RpcResponse): {
  fetch: typeof globalThis.fetch;
  seen: Seen[];
} {
  const seen: Seen[] = [];
  const send = (async (_input: unknown, init?: RequestInit) => {
    const request = RpcRequest.decode(new Uint8Array(init!.body as ArrayBuffer));
    const headers = init!.headers as Record<string, string>;
    const authorization = headers["authorization"];
    seen.push({
      op: request.op,
      auth: authorization?.replace("Bearer ", ""),
      payload: request.payload,
    });
    assert.equal(request.service, "TallyOwlControl", "the dashboard names one service");
    const response = answer(request.op, request.payload).withId(request.id);
    const body = response.encode();
    return {
      status: 200,
      arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
    } as unknown as Response;
  }) as unknown as typeof globalThis.fetch;
  return { fetch: send, seen };
}

function emptyResult(): RpcResponse {
  return RpcResponse.ok(
    "QueryResponse",
    toQueryResponseCbor({
      columns: ["bucket", "events"],
      rows: [],
      metadata: {
        algebraVersion: 1,
        commitWatermark: 1,
        freshnessMs: 0,
        complete: true,
        exactness: [],
        scannedBytes: 0,
        scannedSegments: 0,
        tombstoneGeneration: 0,
      },
    }),
  );
}

/// A `sessionStorage` that is a map. The real one ends with the tab; this one
/// ends with the test.
function storage(): signIn.Storage {
  const held = new Map<string, string>();
  return {
    getItem: (key) => held.get(key) ?? null,
    setItem: (key, value) => void held.set(key, value),
    removeItem: (key) => void held.delete(key),
  };
}

// ---------------------------------------------------------------------------
// The query is a tree, never a string
// ---------------------------------------------------------------------------

test("the dashboard sends a typed query tree and no query text", async () => {
  const { fetch, seen } = head(() => emptyResult());
  const control = new Control({ fetch }).withSession("tows_abc");

  await control.call("run-query", toQueryRequestCborOf(eventTrend(PROJECT, range(), 3_600_000)));

  assert.equal(seen.length, 1);
  // It decodes as a real request, which is the whole point: nothing here is a
  // string the head would have to parse.
  const decoded = fromQueryRequestCbor(seen[0]!.payload);
  assert.equal(decoded.form, "node");
  assert.equal(decoded.allowPartial, false, "a dashboard never draws a partial answer as a whole one");
  assert.ok(decoded.node !== undefined, "the tree travels as an encoded node");
});

test("the breakdown is a limit over a sort over an aggregate", async () => {
  const request = eventBreakdown(PROJECT, range(), 10);
  assert.equal(request.form, "node");
  assert.ok(request.node !== undefined);
});

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

test("the session travels on the connection and reaches the head", async () => {
  const { fetch, seen } = head(() => emptyResult());
  const control = new Control({ fetch }).withSession("tows_abc");
  await control.call("run-query", toQueryRequestCborOf(eventTrend(PROJECT, range(), 60_000)));
  await control.call("run-query", toQueryRequestCborOf(eventTrend(PROJECT, range(), 60_000)));
  assert.deepEqual(
    seen.map((call) => call.auth),
    ["tows_abc", "tows_abc"],
  );
});

test("a dashboard with no session sends none rather than an empty one", async () => {
  const { fetch, seen } = head(() => emptyResult());
  const control = new Control({ fetch }).withSession("");
  await control.call("run-query", toQueryRequestCborOf(eventTrend(PROJECT, range(), 60_000)));
  assert.equal(seen[0]!.auth, undefined);
});

// ---------------------------------------------------------------------------
// Failure, and telling the two kinds apart
// ---------------------------------------------------------------------------

test("a rejected request and an unreachable head are different failures", async () => {
  const rejecting = head(() =>
    RpcResponse.ok(
      "ServiceError",
      toServiceErrorCbor({
        code: "permission-denied",
        message: "You cannot read that project.",
        retryable: false,
      }),
    ),
  );
  const control = new Control({ fetch: rejecting.fetch }).withSession("tows_abc");
  await assert.rejects(
    () => control.call("run-query", toQueryRequestCborOf(eventTrend(PROJECT, range(), 60_000))),
    (error: unknown) => {
      assert.ok(error instanceof ControlError, "a rejection is an application error");
      assert.equal(error.code, "permission-denied");
      assert.equal(error.retryable, false);
      return true;
    },
  );

  const unreachable = new Control({
    fetch: (async () => {
      throw new Error("connection refused");
    }) as unknown as typeof globalThis.fetch,
  });
  await assert.rejects(
    () => unreachable.call("run-query", new Uint8Array()),
    (error: unknown) => error instanceof TransportFailure,
  );
});

test("a transport status that is not ok never reads as a result", async () => {
  const { fetch } = head(() => RpcResponse.transportError(2, "no such operation"));
  const control = new Control({ fetch });
  await assert.rejects(
    () => control.call("invent", new Uint8Array()),
    (error: unknown) => error instanceof TransportFailure,
  );
});

// ---------------------------------------------------------------------------
// Signing in
// ---------------------------------------------------------------------------

test("a callback completes a sign-in exactly once", async () => {
  const { fetch, seen } = head((op) => {
    if (op === "begin-login") {
      return RpcResponse.ok(
        "BeginLoginResponse",
        toBeginLoginResponseCbor({
          redirectUrl: "https://linkkeys.example/authorize?x=1",
          loginId: "login-1",
          expiresAt: 1,
        }),
      );
    }
    return RpcResponse.ok(
      "CompleteLoginResponse",
      toCompleteLoginResponseCbor({
        sessionToken: "tows_new",
        subject: "someone@example.com",
        expiresAt: 2,
        memberships: [],
      }),
    );
  });
  const control = new Control({ fetch });
  const held = storage();

  const redirect = await signIn.begin(
    control,
    held,
    "example.com",
    "https://tallyowl.example/sign-in/callback",
  );
  assert.equal(redirect, "https://linkkeys.example/authorize?x=1");

  const arrived = "https://tallyowl.example/sign-in/callback?encrypted_token=abc";
  const completed = await signIn.complete(control, held, arrived);
  assert.equal(completed?.sessionToken, "tows_new");
  assert.equal(control.session(), "tows_new", "the session is presented from now on");

  // Taken rather than read: the pending login is gone, so the same callback
  // cannot be replayed.
  assert.equal(await signIn.complete(control, held, arrived), undefined);
  assert.equal(seen.filter((call) => call.op === "complete-login").length, 1);
});

test("a page that is not a callback completes nothing", async () => {
  const { fetch, seen } = head(() => emptyResult());
  const control = new Control({ fetch });
  assert.equal(await signIn.complete(control, storage(), "https://tallyowl.example/"), undefined);
  assert.equal(seen.length, 0);
});

// ---------------------------------------------------------------------------
// The chart
// ---------------------------------------------------------------------------

test("a trend result reads into points in time order", () => {
  const points = view.pointsFrom(
    ["bucket", "events"],
    [
      { values: [{ kind: "int", intValue: 3_000 }, { kind: "uint", uintValue: 7 }] },
      { values: [{ kind: "int", intValue: 1_000 }, { kind: "uint", uintValue: 4 }] },
    ],
  );
  assert.deepEqual(points, [
    { at: 1_000, count: 4 },
    { at: 3_000, count: 7 },
  ]);
});

test("a value the chart cannot read is left out rather than drawn as a zero", () => {
  // A chart that drew a zero for a value it could not read would be inventing
  // data, which is worse than showing less.
  const points = view.pointsFrom(
    ["bucket", "events"],
    [
      { values: [{ kind: "text", textValue: "not a time" }, { kind: "uint", uintValue: 4 }] },
      { values: [{ kind: "int", intValue: 1_000 }, { kind: "uint", uintValue: 9 }] },
    ],
  );
  assert.deepEqual(points, [{ at: 1_000, count: 9 }]);
});

function range() {
  return { rangeStart: 0, rangeEnd: 86_400_000, basis: "occurred_at" as const };
}

function toQueryRequestCborOf(request: QueryRequest): Uint8Array {
  return toQueryRequestCborImport(request);
}

import { toQueryRequestCbor as toQueryRequestCborImport } from "../src/control-api.ts";

test("a metric rate result reads into points in time order", () => {
  const points = view.ratePointsFrom(
    ["bucket", "each_second"],
    [
      { values: [{ kind: "int", intValue: 2_000 }, { kind: "float", floatValue: 0.5 }] },
      { values: [{ kind: "int", intValue: 1_000 }, { kind: "float", floatValue: 1.5 }] },
    ],
  );
  assert.deepEqual(points, [
    { at: 1_000, count: 1.5 },
    { at: 2_000, count: 0.5 },
  ]);
});

test("a bucket with no rate is left out rather than drawn as a zero", () => {
  // A rate needs two readings, so the first bucket of a series has none. A zero
  // there would say the metric stopped when it had not started.
  const points = view.ratePointsFrom(
    ["bucket", "each_second"],
    [
      { values: [{ kind: "int", intValue: 1_000 }, { kind: "null" }] },
      { values: [{ kind: "int", intValue: 2_000 }, { kind: "float", floatValue: 2 }] },
    ],
  );
  assert.deepEqual(points, [{ at: 2_000, count: 2 }]);
});

test("the metric names come from the head rather than from a configured list", () => {
  const names = view.metricNamesFrom(
    ["metric_name", "points"],
    [
      {
        values: [
          { kind: "text", textValue: "seedstore_checkouts_total" },
          { kind: "uint", uintValue: 4 },
        ],
      },
      {
        values: [
          { kind: "text", textValue: "seedstore_span_seconds" },
          { kind: "uint", uintValue: 3 },
        ],
      },
    ],
  );
  assert.deepEqual(names, ["seedstore_checkouts_total", "seedstore_span_seconds"]);
});

test("a metric rate query filters by name and asks for the rate operator", () => {
  // The dashboard never subtracts two readings for itself. QUERY.md section
  // 12.7: `rate` handles a counter reset, and a subtraction would draw a spike
  // every time a service restarted.
  const request = metricRate(PROJECT, "requests_total", range(), 3_600_000);
  const decoded = fromQueryRequestCbor(toQueryRequestCborOf(request));
  const node = fromQueryNodeBoxCbor(decoded.node!);
  assert.equal(node.node, "aggregate");
  assert.equal(node.aggregate?.measures[0]?.kind, "rate");
  const input = fromQueryNodeBoxCbor(node.aggregate!.input);
  assert.equal(input.node, "filter");
  const scan = fromQueryNodeBoxCbor(input.filter!.input);
  assert.equal(scan.scan?.scan, "metric_points");
});

test("a metric name list reads over the metric points and never over events", () => {
  const request = metricNames(PROJECT, range(), 20);
  const decoded = fromQueryRequestCbor(toQueryRequestCborOf(request));
  const limit = fromQueryNodeBoxCbor(decoded.node!);
  const sorted = fromQueryNodeBoxCbor(limit.limit!.input);
  const grouped = fromQueryNodeBoxCbor(sorted.sort!.input);
  const scan = fromQueryNodeBoxCbor(grouped.aggregate!.input);
  assert.equal(scan.scan?.scan, "metric_points");
});

// ---------------------------------------------------------------------------
// Campaigns and business outcomes. Phase 9.
// ---------------------------------------------------------------------------

test("an attribution question names its model and never its weights", () => {
  // D40 makes a weight the operator's configuration for the project. A
  // dashboard that could send weights could make one campaign outrank another
  // by asking differently, and the contract deliberately gives it no way to.
  const request = attribution(PROJECT, range(), "purchase", "last-non-direct", 2_592_000_000);
  const decoded = fromQueryRequestCbor(toQueryRequestCborOf(request));
  assert.equal(decoded.form, "attribution");
  assert.equal(decoded.attribution?.model, "last-non-direct");
  assert.equal(decoded.attribution?.conversionGoal, "purchase");
  assert.equal(decoded.attribution?.lookbackMs, 2_592_000_000);
  assert.equal(decoded.node, undefined, "an attribution question carries no tree");
  assert.equal(decoded.allowPartial, false);
});

test("a campaign report asks for the campaign dimension by default", () => {
  const request = campaignSummary(PROJECT, range(), "purchase", "linear", 2_592_000_000);
  const decoded = fromQueryRequestCbor(toQueryRequestCborOf(request));
  assert.equal(decoded.form, "campaign-summary");
  assert.equal(decoded.campaignSummary?.dimension, "campaign");
  assert.equal(decoded.campaignSummary?.model, "linear");
});

test("every model the chooser offers is one the contract carries", () => {
  // A chooser that offered a model the head does not answer would be a refusal
  // a person cannot act on.
  assert.deepEqual(
    [...ATTRIBUTION_MODELS],
    ["first-touch", "last-touch", "last-non-direct", "linear", "position", "decay"],
  );
  for (const model of ATTRIBUTION_MODELS) {
    const request = attribution(PROJECT, range(), "purchase", model, 1_000);
    const decoded = fromQueryRequestCbor(toQueryRequestCborOf(request));
    assert.equal(decoded.attribution?.model, model);
  }
});

test("a credited value stays exact all the way to the table cell", () => {
  // Money never becomes a float. 23.333334 through a JavaScript number is
  // still 23.333334, but a value with more digits than a double holds is not,
  // and the point of keeping it as text is that this never has to be checked.
  const rows = view.campaignRowsFrom(
    ["campaign", "touches", "people", "sessions", "conversions", "value", "cost", "return"],
    [
      {
        values: [
          { kind: "text", textValue: "spring" },
          { kind: "uint", uintValue: 2 },
          { kind: "uint", uintValue: 2 },
          { kind: "uint", uintValue: 2 },
          { kind: "float", floatValue: 0.6666667 },
          { kind: "decimal", decimalValue: new CsilDecimal(-6, 23333334n) },
          { kind: "decimal", decimalValue: new CsilDecimal(0, 100n) },
          { kind: "float", floatValue: 0.23333334 },
        ],
      },
    ],
  );
  assert.equal(rows[0]?.value, "23.333334");
  assert.equal(rows[0]?.cost, "100");
  assert.equal(rows[0]?.people, 2);
});

test("a campaign with no spend shows no return rather than a zero", () => {
  // A zero would read as "this campaign earned nothing for its spend", and a
  // campaign with no spend recorded earned everything for nothing.
  const rows = view.campaignRowsFrom(
    ["campaign", "touches", "people", "sessions", "conversions", "value", "cost", "return"],
    [
      {
        values: [
          { kind: "text", textValue: "organic" },
          { kind: "uint", uintValue: 5 },
          { kind: "uint", uintValue: 5 },
          { kind: "uint", uintValue: 5 },
          { kind: "float", floatValue: 1 },
          { kind: "decimal", decimalValue: new CsilDecimal(0, 70n) },
          { kind: "decimal", decimalValue: new CsilDecimal(0, 0n) },
          { kind: "null" },
        ],
      },
    ],
  );
  assert.equal(rows[0]?.returnOnSpend, undefined);

  const table = view.campaignReport(rows);
  assert.match(table.textContent ?? "", /—/);
});

test("a campaign report with nothing in it says so rather than drawing an empty table", () => {
  const section = view.campaignReport([]);
  assert.match(section.textContent ?? "", /No campaign traffic/);
});

test("what a result said about itself reaches the person reading it", () => {
  // An attribution answer names its model and its settings version, says how
  // much revenue nothing earned, and says how many people a consent policy left
  // out. A report that hid those would be a number without its footnotes.
  const notes = view.resultNotes([
    "This uses the `linear` model at version 1, under settings version 3.",
    "2 conversions worth 140 had no touch inside the window.",
  ]);
  assert.notEqual(notes, undefined);
  assert.match(notes!.textContent ?? "", /settings version 3/);
  assert.match(notes!.textContent ?? "", /no touch inside the window/);

  assert.equal(view.resultNotes(undefined), undefined);
  assert.equal(view.resultNotes([]), undefined);
});

// ---------------------------------------------------------------------------
// The operator surface. Phase 10.
// ---------------------------------------------------------------------------

/// Every `tr` under a section, in order. The shim holds a tree and no
/// selectors, which is deliberate: a shim that grew selectors would be a second
/// implementation of a browser.
function rowsOf(element: HTMLElement | ShimElement): ShimElement[] {
  const out: ShimElement[] = [];
  const walk = (node: ShimElement): void => {
    if (node.tagName === "tr" && node.getAttribute("data-state") !== undefined) out.push(node);
    else if (node.tagName === "tr" && node.getAttribute("data-workflow") !== undefined) out.push(node);
    else if (node.tagName === "tr" && node.getAttribute("data-delivered") !== undefined) out.push(node);
    for (const child of node.children) walk(child);
  };
  walk(element as unknown as ShimElement);
  return out;
}

function cellsOf(row: ShimElement | undefined): (string | undefined)[] {
  return (row?.children ?? []).map((cell) => cell.textContent);
}

test("an alert that could not be answered is not shown as a quiet `ok`", () => {
  // `unknown` means TallyOwl could not answer, which is a different fact from
  // a value that did not cross a threshold. An interface that ran the two
  // together would let an operator read a broken query as a healthy system.
  const section = view.alertList([
    { ruleId: "a", name: "Checkout errors", state: "unknown", since: 1, reason: "" },
  ]);
  assert.match(section.textContent ?? "", /could not answer/);
  assert.equal(rowsOf(section)[0]?.getAttribute("data-state"), "unknown");
});

test("an alert with no value shows no value rather than a zero", () => {
  // A `no-data` alert has no value at all. Drawing a zero would say the
  // measure was zero, which is the reading that makes a dead pipeline look
  // healthy.
  const section = view.alertList([
    { ruleId: "a", name: "Orders", state: "no-data", since: 1 },
  ]);
  assert.equal(cellsOf(rowsOf(section)[0])[3], "—");
});

test("no alert rules says so rather than drawing an empty table", () => {
  assert.match(view.alertList([]).textContent ?? "", /Nothing is being watched/);
});

test("a workflow with work in quarantine is marked for a person", () => {
  // Quarantine is the one that never resolves itself. Lag comes back when the
  // workers catch up and a failure is retried; quarantined work waits.
  const section = view.workflowTable([
    { kind: "notification", pending: 0, inFlight: 0, quarantined: 2, lagMs: 0, failures: 3, lastFailure: "nothing answers at example.test" },
    { kind: "alert-evaluation", pending: 1, inFlight: 0, quarantined: 0, lagMs: 90_000, failures: 0 },
  ]);
  const stuck = rowsOf(section).find(
    (row) => row.getAttribute("data-workflow") === "notification",
  );
  assert.equal(stuck?.getAttribute("data-needs-attention"), "quarantine");
  assert.match(section.textContent ?? "", /waiting for a person/);
  assert.match(section.textContent ?? "", /Nothing was dropped/);
  // And the failure says why, rather than sending somebody to a log file.
  assert.match(section.textContent ?? "", /nothing answers at example.test/);
});

test("a workflow that is keeping up reports no lag rather than the age of the epoch", () => {
  const section = view.workflowTable([
    { kind: "retention", pending: 0, inFlight: 0, quarantined: 0, lagMs: 0, failures: 0 },
  ]);
  assert.equal(cellsOf(rowsOf(section)[0])[3], "none");
});

test("a lag is shown in the unit a person reads", () => {
  assert.equal(view.duration(0), "none");
  assert.equal(view.duration(450), "450 ms");
  assert.equal(view.duration(90_000), "1.5 min");
  assert.equal(view.duration(5_400_000), "1.5 h");
});

test("a failed notification attempt says why it failed", () => {
  const section = view.deliveryList([
    { ruleId: "busy", target: "webhook http://example.test/alerts", attempts: 3, delivered: false, lastFailure: "The receiver answered 503.", at: 1 },
  ]);
  assert.match(section.textContent ?? "", /answered 503/);
  assert.equal(rowsOf(section)[0]?.getAttribute("data-delivered"), "false");
});

test("no notification attempt yet says so", () => {
  assert.match(view.deliveryList([]).textContent ?? "", /No notification has been attempted/);
});
