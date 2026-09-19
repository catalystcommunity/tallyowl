// The TypeScript half of the cross-language agreement.
//
// `docs/PLAN.md` Phase 2 says the contract is not trustworthy until every
// maintained language encodes each golden vector to identical bytes. The
// vectors are built in Rust and written to `golden/vectors.json`. This file
// builds the same values in TypeScript and compares.
//
// Every vector name here must appear in the file, and every name in the file
// must appear here. A vector that only one language knows about proves nothing.

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import * as collector from "../src/collector-api.ts";
import * as ingest from "../../../generated/typescript/tallyowl-ingest-api/codec.gen.ts";
import type * as ingestTypes from "../../../generated/typescript/tallyowl-ingest-api/types.gen.ts";
import * as control from "../../../generated/typescript/tallyowl-control-api/codec.gen.ts";
import type * as controlTypes from "../../../generated/typescript/tallyowl-control-api/types.gen.ts";

import { bool, bytes, decimal, float, int, measurement, nullValue, property, text, uint, write }
  from "../src/value.ts";

const at = 1_785_628_800_000;

const hex = (u: Uint8Array): string =>
  Array.from(u)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");

const id = (fill: number): Uint8Array => new Uint8Array(16).fill(fill);

/**
 * Find `golden/vectors.json` by walking up from this file.
 *
 * The compiled output sits at a different depth from the source, so a fixed
 * number of `..` steps would work in one of the two and not the other.
 */
function readGolden(): { name: string; type: string; package: string; bytes: string }[] {
  let directory = dirname(fileURLToPath(import.meta.url));
  for (let step = 0; step < 10; step++) {
    const candidate = join(directory, "golden", "vectors.json");
    if (existsSync(candidate)) {
      return JSON.parse(readFileSync(candidate, "utf8")).vectors;
    }
    directory = dirname(directory);
  }
  throw new Error("golden/vectors.json is not above this file. Run `./tools.sh gen-golden`.");
}

function minimalEnvelope(): collector.Envelope {
  return {
    eventId: id(1),
    kind: "event",
    schemaVersion: 1,
    occurredAt: at,
    sdkName: "tallyowl-driver-rust",
    sdkVersion: "0.0.0",
    properties: [],
  };
}

function fullEnvelope(): collector.Envelope {
  return {
    ...minimalEnvelope(),
    observedAt: at + 1,
    receivedAt: at + 2,
    workspaceId: id(8),
    projectId: id(9),
    sourceId: id(7),
    sequence: 42,
    release: "2026.8.1",
    serviceName: "checkout",
    requestId: "r-1",
    sessionId: "s-1",
    endUserId: "u-1",
    anonymousId: "a-1",
    traceId: id(3),
    spanId: new Uint8Array(8).fill(4),
    consent: { marketing: "denied", analytics: "granted", policyVersion: "2026-01" },
    properties: [
      property("region", text("us-west2"), "collector"),
      property("attempts", uint(3), "driver"),
      property("ratio", float(0.5), "client"),
    ],
    measurements: [measurement("render", float(12.5), "ms")],
  };
}

/** Every vector this language produces, by name. */
function built(): Map<string, Uint8Array> {
  const out = new Map<string, Uint8Array>();
  const typed = (name: string, value: Parameters<typeof write>[0]) =>
    out.set(name, collector.toTypedValueCbor(write(value)));

  typed("typed-value-null", nullValue());
  typed("typed-value-bool", bool(true));
  typed("typed-value-int", int(-3));
  typed("typed-value-uint", uint(3));
  typed("typed-value-float", float(0.5));
  typed("typed-value-decimal", decimal("19.99"));
  typed("typed-value-text", text("us-west2"));
  typed("typed-value-bytes", bytes(new Uint8Array([0xde, 0xad, 0xbe, 0xef])));

  out.set(
    "property-collector-origin",
    collector.toPropertyCbor(property("region", text("us-west2"), "collector")),
  );
  out.set(
    "measurement-decimal-with-unit",
    collector.toMeasurementCbor(measurement("amount", decimal("-0.01"), "USD")),
  );

  out.set("envelope-minimal", collector.toEnvelopeCbor(minimalEnvelope()));
  out.set("envelope-full", collector.toEnvelopeCbor(fullEnvelope()));

  out.set(
    "item-event",
    collector.toTelemetryItemCbor({
      envelope: minimalEnvelope(),
      event: { name: "checkout-started", route: "/checkout" },
    }),
  );

  out.set(
    "item-page-view-with-campaign",
    collector.toTelemetryItemCbor({
      envelope: { ...minimalEnvelope(), kind: "page-view" },
      pageView: {
        route: "/pricing",
        pageTitle: "Pricing",
        campaign: { source: "newsletter", medium: "email", campaign: "spring" },
      },
    }),
  );

  const money = decimal("19.99");
  out.set(
    "item-conversion-exact-money",
    collector.toTelemetryItemCbor({
      envelope: { ...minimalEnvelope(), kind: "conversion" },
      conversion: {
        goal: "purchase",
        value: money.kind === "decimal" ? money.value : undefined,
        currency: "USD",
      },
    }),
  );

  out.set(
    "item-error-with-frames",
    collector.toTelemetryItemCbor({
      envelope: { ...minimalEnvelope(), kind: "error" },
      error: {
        errorType: "TypeError",
        message: "x is not a function",
        handled: false,
        severity: "fatal",
        runtime: "node",
        frames: [{ module: "checkout", function: "submit", line: 42, inApp: true }],
      },
    }),
  );

  out.set(
    "item-session-heartbeat-no-payload",
    collector.toTelemetryItemCbor({
      envelope: { ...minimalEnvelope(), kind: "session-heartbeat" },
    }),
  );

  out.set(
    "batch-two-items",
    collector.toBatchCbor({
      batchId: id(2),
      items: [
        { envelope: minimalEnvelope(), event: { name: "a" } },
        { envelope: minimalEnvelope(), event: { name: "b" } },
      ],
      sealedAt: at,
      compression: "zstd",
    }),
  );

  out.set(
    "commit-batch-response-with-rejection",
    collector.toCommitBatchResponseCbor({
      batchId: id(2),
      accepted: 1,
      committedAt: at,
      satisfiedPolicy: "local-one",
      commitWatermark: 7,
      protocolVersion: 1,
      projectorVersion: 1,
      rejected: [
        {
          eventId: id(5),
          code: "invalid-argument",
          message: "This item carries no project.",
        },
      ],
      deduplicated: false,
    }),
  );

  out.set(
    "service-error-retryable",
    collector.toServiceErrorCbor({
      code: "unavailable",
      message: "We could not reach the durable store.",
      retryable: true,
    }),
  );

  const ingestEnvelope: ingestTypes.Envelope = {
    eventId: id(1),
    kind: "event",
    schemaVersion: 1,
    occurredAt: at,
    sdkName: "tallyowl-browser",
    sdkVersion: "0.0.0",
    properties: [],
  };
  out.set(
    "capture-request-one-event",
    ingest.toCaptureRequestCbor({
      items: [{ envelope: ingestEnvelope, event: { name: "checkout-started", route: "/checkout" } }],
    }),
  );

  const range: controlTypes.TimeRange = {
    rangeStart: at,
    rangeEnd: at + 3_600_000,
    basis: "occurred_at",
  };
  const scanBox: controlTypes.QueryNodeBox = {
    node: "scan",
    scan: { scan: "events", projectId: id(9), range },
  };
  out.set("query-node-scan", control.toQueryNodeBoxCbor(scanBox));

  const literal = write(text("/pricing"));
  out.set(
    "expression-compare",
    control.toExpressionNodeCbor({
      expression: "compare",
      compare: {
        compare: "eq",
        left: control.toExpressionNodeCbor({
          expression: "field",
          field: { name: "route", valueType: "text" },
        }),
        right: control.toExpressionNodeCbor({
          expression: "literal",
          literal: { kind: literal.kind, textValue: literal.textValue },
        }),
      },
    }),
  );

  out.set(
    "query-request-trend",
    control.toQueryRequestCbor({
      algebraVersion: 1,
      consistency: "committed",
      allowPartial: false,
      form: "node",
      node: control.toQueryNodeBoxCbor({
        node: "aggregate",
        aggregate: {
          dimensions: [],
          measures: [{ kind: "count", alias: "events" }],
          interval: { fixedMs: 60_000 },
          input: control.toQueryNodeBoxCbor(scanBox),
        },
      }),
    }),
  );

  return out;
}

test("every golden vector encodes to the same bytes", () => {
  const golden = readGolden();
  const made = built();
  assert.ok(golden.length > 0, "golden/vectors.json holds no vectors");

  const problems: string[] = [];
  for (const vector of golden) {
    const bytes = made.get(vector.name);
    if (bytes === undefined) {
      problems.push(`this language does not build the vector \`${vector.name}\``);
      continue;
    }
    const got = hex(bytes);
    if (got !== vector.bytes) {
      problems.push(
        `\`${vector.name}\` (${vector.type}):\n  Rust:       ${vector.bytes}\n  TypeScript: ${got}`,
      );
    }
    made.delete(vector.name);
  }
  for (const name of made.keys()) {
    problems.push(`this language builds \`${name}\` and the golden file does not hold it`);
  }
  assert.equal(problems.length, 0, problems.join("\n"));
});
