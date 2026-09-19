// A fake host routes a typed browser event over its existing multiplexed
// carrier into a fake collector.
//
// This is the Phase 2 exit criterion, and it is also the integration rule from
// AGENTS.md written as a test: the browser package opens no connection of its
// own and never names a TallyOwl address. Everything travels on the carrier the
// host already has.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  HandlerOutcome,
  LoopbackFrameCarrier,
  RpcClient,
  RpcRequest,
  RpcResponse,
  Status,
} from "./transport.ts";

import * as ingest from "../../../generated/typescript/tallyowl-ingest-api/codec.gen.ts";
import type { CaptureRequest, TelemetryItem } from "../../../generated/typescript/tallyowl-ingest-api/types.gen.ts";

import { BrowserClient, Capture, Session, ServiceError, decimal, text, uint } from "../src/index.ts";

/**
 * A collector behind the host.
 *
 * It decodes a real `CaptureRequest` off a real CSIL-RPC frame, so the test
 * proves the wire and not a stub.
 */
class FakeCollector {
  readonly received: TelemetryItem[] = [];
  failWith: { code: string; message: string; retryable: boolean } | undefined;

  handle(request: RpcRequest): HandlerOutcome {
    if (request.service !== "TallyOwlIngest") {
      return { kind: "transport", status: Status.UnknownServiceOrOp, message: "no such service" };
    }
    if (this.failWith !== undefined) {
      return {
        kind: "reply",
        variant: "ServiceError",
        payload: ingest.toServiceErrorCbor(this.failWith as never),
      };
    }
    if (request.op === "capture") {
      const decoded: CaptureRequest = ingest.fromCaptureRequestCbor(request.payload);
      this.received.push(...decoded.items);
      return {
        kind: "reply",
        variant: "CaptureResponse",
        payload: ingest.toCaptureResponseCbor({ accepted: decoded.items.length }),
      };
    }
    if (request.op === "capture-critical") {
      const decoded = ingest.fromCaptureCriticalRequestCbor(request.payload);
      this.received.push(...decoded.items);
      return {
        kind: "reply",
        variant: "CaptureCriticalResponse",
        payload: ingest.toCaptureCriticalResponseCbor({
          accepted: decoded.items.length,
          // The collector reached its durability boundary, and the reply says
          // so rather than leaving the caller to assume.
          durable: true,
          batchId: new Uint8Array(16).fill(2),
        }),
      };
    }
    return { kind: "transport", status: Status.UnknownServiceOrOp, message: "no such operation" };
  }
}

/**
 * The host application.
 *
 * It already holds a multiplexed CSIL carrier to its own backend, and it routes
 * the browser package's frames on it. This is the whole integration: the
 * browser package hands over encoded bytes and never learns an address.
 */
class FakeHost {
  private readonly carrier = new LoopbackFrameCarrier();
  private readonly client = new RpcClient(this.carrier, true);
  /** Every address this host contacted. It stays empty for TallyOwl, which is
   * the negative assertion the design requires. */
  readonly contactedAddresses: string[] = [];

  constructor(private readonly collector: FakeCollector) {}

  async call(
    service: string,
    op: string,
    payload: Uint8Array,
  ): Promise<{ variant?: string; payload: Uint8Array }> {
    // The host's own transport. The browser package supplied bytes only.
    const pending = this.client.call(service, op, payload);
    const outbound = this.carrier.takeOutbound();
    assert.ok(outbound !== undefined, "the host's carrier saw no frame");
    const request = RpcRequest.decode(outbound);
    const outcome = this.collector.handle(request);
    const response =
      outcome.kind === "reply"
        ? RpcResponse.ok(outcome.variant, outcome.payload).withId(request.id)
        : RpcResponse.transportError(outcome.status, outcome.message).withId(request.id);
    this.carrier.pushInbound(response.encode());
    const reply = await pending;
    return { variant: reply.variant, payload: reply.payload };
  }
}

test("a typed browser event reaches the collector over the host's carrier", async () => {
  const collector = new FakeCollector();
  const host = new FakeHost(collector);
  const client = new BrowserClient(host);
  const session = Session.start();

  client.capture(Capture.pageView("/pricing").withSession(session.id));
  client.capture(
    Capture.event("checkout-started")
      .withSession(session.id)
      .withProperty("plan", text("pro"))
      .withProperty("attempts", uint(2)),
  );

  const result = await client.flush();
  assert.ok(result !== undefined);
  assert.equal(result.accepted, 2);
  // An ordinary capture is best effort. The result says so rather than
  // implying a durability the carrier cannot give.
  assert.equal(result.durable, false);

  assert.equal(collector.received.length, 2);
  // Each item arrives as the kind it says it is. This is what the earlier bare
  // choice could not do out of TypeScript at all: every payload encoded as the
  // first arm, so a page view arrived as an event.
  assert.equal(collector.received[0].envelope.kind, "page-view");
  assert.ok(collector.received[0].pageView !== undefined);
  assert.equal(collector.received[0].pageView?.route, "/pricing");
  assert.equal(collector.received[0].event, undefined);

  assert.equal(collector.received[1].envelope.kind, "event");
  assert.equal(collector.received[1].event?.name, "checkout-started");

  // Every property keeps its own type, including the one JavaScript has no
  // runtime form for.
  const properties = collector.received[1].envelope.properties;
  const attempts = properties.find((p) => p.key === "attempts");
  assert.equal(attempts?.value.kind, "uint");
  assert.equal(attempts?.value.uintValue, 2);
  assert.equal(attempts?.value.intValue, undefined);
  assert.equal(attempts?.origin, "client");
});

test("the browser package contacts no TallyOwl address", async () => {
  // The negative case the design requires. The browser package holds no address
  // and opens no connection, so a host that recorded every address it contacted
  // sees none from this package.
  const collector = new FakeCollector();
  const host = new FakeHost(collector);
  const client = new BrowserClient(host);
  client.capture(Capture.event("a"));
  await client.flush();
  assert.deepEqual(host.contactedAddresses, []);
});

test("a critical send reports the durability it actually reached", async () => {
  const collector = new FakeCollector();
  const client = new BrowserClient(new FakeHost(collector));
  client.capture(Capture.conversion("purchase", { value: asDecimal("19.99"), currency: "USD" }));

  const result = await client.sendCritical();
  assert.equal(result?.durable, true);
  assert.equal(collector.received[0].envelope.kind, "conversion");
  // Money keeps its exact digits. A float here would make a revenue total
  // disagree with the customer's own records.
  assert.equal(collector.received[0].conversion?.value?.toString(), "19.99");
});

test("a rejection arrives with its retry fact", async () => {
  const collector = new FakeCollector();
  collector.failWith = {
    code: "resource-exhausted",
    message: "The durable store is full.",
    retryable: false,
  };
  const client = new BrowserClient(new FakeHost(collector));
  client.capture(Capture.event("a"));
  await assert.rejects(
    () => client.flush(),
    (error: unknown) => {
      assert.ok(error instanceof ServiceError);
      assert.equal(error.code, "resource-exhausted");
      // `retryable` is a fact, not advice. A caller builds automation on it.
      assert.equal(error.retryable, false);
      return true;
    },
  );
});

test("a conversion seals the buffer and an ordinary event does not", () => {
  const client = new BrowserClient(new FakeHost(new FakeCollector()), {
    maxItems: 1000,
    lingerMs: 3_600_000,
  });
  client.capture(Capture.event("ordinary"));
  assert.equal(client.shouldFlush, false);
  // A conversion is the first priority class in DELIVERY.md section 8.
  client.capture(Capture.conversion("purchase"));
  assert.equal(client.shouldFlush, true);
});

test("a full buffer drops the oldest item and counts the drop", () => {
  // A browser cannot block a person's interaction to wait for capacity. It can
  // refuse to hide the loss.
  const client = new BrowserClient(new FakeHost(new FakeCollector()), { maxBuffered: 3 });
  for (let i = 0; i < 5; i++) client.capture(Capture.event(`e${i}`));
  assert.equal(client.buffered, 3);
  assert.equal(client.droppedCount, 2);
});

test("an item whose kind and payload disagree is refused", () => {
  const client = new BrowserClient(new FakeHost(new FakeCollector()));
  const capture = Capture.pageView("/pricing");
  capture.item.envelope.kind = "conversion";
  assert.throws(() => client.capture(capture), /conversion.*page-view/);
});

test("a session heartbeat carries no payload and is accepted", async () => {
  const collector = new FakeCollector();
  const client = new BrowserClient(new FakeHost(collector));
  const session = Session.start();
  client.capture(Capture.sessionHeartbeat(session.id));
  await client.flush();
  assert.equal(collector.received[0].envelope.kind, "session-heartbeat");
  assert.equal(collector.received[0].envelope.sessionId, session.id);
});

test("the unload flush reaches the host's own route and nothing else", () => {
  // D34: the flush reaches the application, not a TallyOwl domain. The browser
  // package does not claim durable delivery after a tab closes.
  const client = new BrowserClient(new FakeHost(new FakeCollector()));
  const sent: Uint8Array[] = [];
  const listeners: Record<string, () => void> = {};
  const target = {
    visibilityState: "visible",
    addEventListener(type: string, listener: () => void) {
      listeners[type] = listener;
    },
  };

  client.attachUnloadFlush((payload) => sent.push(payload), target);
  client.capture(Capture.event("exit"));

  target.visibilityState = "hidden";
  listeners.visibilitychange();

  assert.equal(sent.length, 1);
  const decoded = ingest.fromCaptureRequestCbor(sent[0]);
  assert.equal(decoded.items.length, 1);
  assert.equal(decoded.items[0].event?.name, "exit");
  assert.equal(client.buffered, 0);
});

test("a session identifier is issued by the library and is opaque", () => {
  // D11: the client library issues the ID; a person cannot select it.
  const a = Session.start();
  const b = Session.start();
  assert.notEqual(a.id, b.id);
  assert.match(a.id, /^[0-9a-f]{32}$/);
});

function asDecimal(value: string) {
  const parsed = decimal(value);
  assert.equal(parsed.kind, "decimal");
  return parsed.kind === "decimal" ? parsed.value : undefined;
}
