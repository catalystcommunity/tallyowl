// What the browser client does when the host does not take what it is given.
//
// A browser cannot block and cannot retry for ever, so the rule is narrower
// than a backend's: nothing is lost without being counted, nothing rejects
// into the host page unless the host asked for a promise, and a critical item
// is the last thing a full buffer gives up.

import assert from "node:assert/strict";
import { mock, test } from "node:test";

import * as ingest from "../../../generated/typescript/tallyowl-ingest-api/codec.gen.ts";

import {
  BrowserClient,
  Capture,
  ServiceError,
  captureGlobalErrors,
  parseStack,
  parseTrace,
  type Router,
} from "../src/index.ts";

/** A host whose carrier can be switched off, and that records what it took. */
class SwitchableHost implements Router {
  down = false;
  refuseWith: { code: string; message: string; retryable: boolean } | undefined;
  readonly batches: string[][] = [];

  async call(_service: string, _op: string, payload: Uint8Array) {
    if (this.down) throw new Error("The host's carrier is closed.");
    if (this.refuseWith !== undefined) {
      return {
        variant: "ServiceError",
        payload: ingest.toServiceErrorCbor(this.refuseWith as never),
      };
    }
    const decoded = ingest.fromCaptureRequestCbor(payload);
    this.batches.push(decoded.items.map((item) => item.event?.name ?? item.envelope.kind));
    return {
      variant: "CaptureResponse",
      payload: ingest.toCaptureResponseCbor({ accepted: decoded.items.length }),
    };
  }
}

test("a flush the host did not take keeps its items, in order, for the next one", async () => {
  const host = new SwitchableHost();
  const client = new BrowserClient(host);
  client.capture(Capture.event("a"));
  client.capture(Capture.event("b"));

  host.down = true;
  await assert.rejects(() => client.flush());
  assert.equal(client.buffered, 2, "a failed flush lost its items");
  assert.equal(client.droppedCount, 0);

  client.capture(Capture.event("c"));
  host.down = false;
  await client.flush();
  assert.deepEqual(host.batches, [["a", "b", "c"]]);
  assert.equal(client.stats().sent, 3);
  assert.equal(client.stats().lastError, undefined);
});

test("items put back after a failed flush still respect the buffer bound, and the overflow is counted", async () => {
  const host = new SwitchableHost();
  host.down = true;
  const client = new BrowserClient(host, { maxBuffered: 3, maxItems: 3 });
  for (const name of ["a", "b", "c"]) client.capture(Capture.event(name));

  const pending = client.flush();
  // These arrive while the flush is in flight, so the buffer holds them when
  // the three come back.
  client.capture(Capture.event("d"));
  client.capture(Capture.event("e"));
  await assert.rejects(() => pending);

  assert.equal(client.buffered, 3);
  assert.equal(client.droppedCount, 2);
});

test("a retryable rejection keeps the items and a permanent one drops and counts them", async () => {
  const host = new SwitchableHost();
  const client = new BrowserClient(host);

  host.refuseWith = { code: "unavailable", message: "Try again.", retryable: true };
  client.capture(Capture.event("a"));
  await assert.rejects(() => client.flush(), ServiceError);
  assert.equal(client.buffered, 1);
  assert.equal(client.droppedCount, 0);

  host.refuseWith = { code: "invalid-argument", message: "That item is not valid.", retryable: false };
  await assert.rejects(() => client.flush(), ServiceError);
  assert.equal(client.buffered, 0, "a permanent rejection would be sent again for ever");
  assert.equal(client.droppedCount, 1);
});

test("a full buffer gives up ordinary items before a critical one", () => {
  const client = new BrowserClient(new SwitchableHost(), { maxBuffered: 3 });
  client.capture(Capture.conversion("purchase"));
  client.capture(Capture.event("a"));
  client.capture(Capture.event("b"));
  client.capture(Capture.event("c"));
  client.capture(Capture.event("d"));

  assert.equal(client.droppedCount, 2);
  const sent: Uint8Array[] = [];
  client.flushOnUnload((payload) => sent.push(payload) > 0);
  const kinds = sent.flatMap((p) => ingest.fromCaptureRequestCbor(p).items.map((i) => i.envelope.kind));
  assert.ok(kinds.includes("conversion"), "the oldest item was a conversion and it was evicted first");
});

test("flushSafely never rejects and reports the failure", async () => {
  const host = new SwitchableHost();
  host.down = true;
  const seen: unknown[] = [];
  const client = new BrowserClient(host, {
    onError: (error) => {
      seen.push(error);
      throw new Error("a handler that throws must not reach the page");
    },
  });
  client.capture(Capture.event("a"));

  assert.equal(await client.flushSafely(), undefined);
  assert.equal(seen.length, 1);
  assert.match(String(client.stats().lastError), /carrier is closed/);
  assert.equal(client.buffered, 1);
});

test("a flush sends sealed batches, not one frame of any size", async () => {
  const host = new SwitchableHost();
  const client = new BrowserClient(host, { maxItems: 2 });
  for (const name of ["a", "b", "c", "d", "e"]) client.capture(Capture.event(name));
  const result = await client.flush();
  assert.deepEqual(host.batches, [["a", "b"], ["c", "d"], ["e"]]);
  assert.equal(result?.accepted, 5);
});

test("the flush timer sends when a seal condition is reached, and stop ends it", async () => {
  mock.timers.enable({ apis: ["setInterval", "Date"] });
  try {
    const host = new SwitchableHost();
    const client = new BrowserClient(host, { lingerMs: 2_000 });
    client.start();
    client.capture(Capture.event("a"));

    mock.timers.tick(1_000);
    await settle();
    assert.equal(host.batches.length, 0, "the buffer had not lingered long enough");

    mock.timers.tick(1_000);
    await settle();
    assert.deepEqual(host.batches, [["a"]]);

    client.stop();
    client.capture(Capture.event("b"));
    mock.timers.tick(10_000);
    await settle();
    assert.equal(host.batches.length, 1, "a stopped timer still flushed");
  } finally {
    mock.timers.reset();
  }
});

test("a timer flush that fails stays inside the client", async () => {
  mock.timers.enable({ apis: ["setInterval", "Date"] });
  try {
    const host = new SwitchableHost();
    host.down = true;
    const seen: unknown[] = [];
    const client = new BrowserClient(host, { lingerMs: 100, onError: (e) => seen.push(e) });
    client.start();
    client.capture(Capture.event("a"));
    mock.timers.tick(100);
    await settle();
    assert.equal(seen.length, 1);
    assert.equal(client.buffered, 1);
    client.stop();
  } finally {
    mock.timers.reset();
  }
});

test("an unload flush goes in small requests, and a refused one is counted", () => {
  const client = new BrowserClient(new SwitchableHost(), { maxBeaconBytes: 400 });
  for (let i = 0; i < 10; i++) client.capture(Capture.event(`event-number-${i}`));

  const sizes: number[] = [];
  let calls = 0;
  const handed = client.flushOnUnload((payload) => {
    sizes.push(payload.length);
    calls += 1;
    // The browser's quota runs out after the first request.
    return calls === 1;
  });

  assert.ok(sizes.length > 1, "ten items went as one request");
  for (const size of sizes) assert.ok(size <= 400, `a request of ${size} bytes passed the bound`);
  assert.equal(client.buffered, 0);
  assert.equal(handed + client.droppedCount, 10);
  assert.ok(client.droppedCount > 0, "a refused request was not counted");
});

test("an unload flush sends a session end before ordinary items", () => {
  const client = new BrowserClient(new SwitchableHost(), { maxBeaconBytes: 1 });
  client.capture(Capture.event("ordinary"));
  client.capture(Capture.sessionEnd("0123456789abcdef0123456789abcdef", "explicit").asCritical());

  const kinds: string[] = [];
  client.flushOnUnload((payload) => {
    kinds.push(...ingest.fromCaptureRequestCbor(payload).items.map((i) => i.envelope.kind));
    return true;
  });
  assert.deepEqual(kinds, ["session-end", "event"]);
});

test("two unload events send an item once, and detaching removes the listeners", () => {
  const client = new BrowserClient(new SwitchableHost());
  const listeners = new Map<string, () => void>();
  const target = {
    visibilityState: "hidden",
    addEventListener: (type: string, listener: () => void) => void listeners.set(type, listener),
    removeEventListener: (type: string, listener: () => void) => {
      if (listeners.get(type) === listener) listeners.delete(type);
    },
  };
  const sent: Uint8Array[] = [];
  const detach = client.attachUnloadFlush((payload) => sent.push(payload) > 0, target);

  client.capture(Capture.event("exit"));
  listeners.get("pagehide")?.();
  listeners.get("visibilitychange")?.();
  assert.equal(sent.length, 1);

  detach();
  assert.equal(listeners.size, 0);
});

test("stats count what was captured, sent, and dropped, and debug only logs", async () => {
  const logged: string[] = [];
  const host = new SwitchableHost();
  const client = new BrowserClient(host, {
    maxBuffered: 2,
    debug: (item) => logged.push(item.event?.name ?? ""),
  });
  for (const name of ["a", "b", "c"]) client.capture(Capture.event(name));
  assert.deepEqual(logged, ["a", "b", "c"]);
  assert.equal(host.batches.length, 0, "debug sent something");

  await client.flush();
  assert.deepEqual(
    { ...client.stats(), lastError: undefined },
    { captured: 3, sent: 2, dropped: 1, buffered: 0, lastError: undefined },
  );
});

test("an unhandled error is captured with its frames and without a query string", () => {
  const client = new BrowserClient(new SwitchableHost());
  const listeners = new Map<string, (event: unknown) => void>();
  const target = {
    addEventListener: (type: string, listener: (event: unknown) => void) => void listeners.set(type, listener),
    removeEventListener: (type: string) => void listeners.delete(type),
  };
  const detach = captureGlobalErrors(client, {
    target,
    sessionId: "0123456789abcdef0123456789abcdef",
    origin: "https://shop.example",
  });

  const error = new TypeError("cart is undefined");
  error.stack = [
    "TypeError: cart is undefined",
    "    at checkout (https://shop.example/assets/app.js?token=secret#frag:10:5)",
    "    at https://cdn.example/lib.js:3:1",
  ].join("\n");
  listeners.get("error")?.({ error });
  listeners.get("unhandledrejection")?.({ reason: "plain text" });

  const sent: Uint8Array[] = [];
  client.flushOnUnload((payload) => sent.push(payload) > 0);
  const items = sent.flatMap((p) => ingest.fromCaptureRequestCbor(p).items);
  assert.equal(items.length, 2);
  assert.equal(items[0].error?.errorType, "TypeError");
  assert.equal(items[0].error?.handled, false);
  assert.equal(items[0].envelope.sessionId, "0123456789abcdef0123456789abcdef");
  // The decoder names every optional field, so compare the ones that were set.
  const frames = (items[0].error?.frames ?? []).map((f) => [f.function, f.file, f.line, f.inApp]);
  assert.deepEqual(frames, [
    ["checkout", "/assets/app.js", 10, true],
    [undefined, "/lib.js", 3, false],
  ]);
  assert.ok(!JSON.stringify(items[0].error).includes("secret"));
  assert.equal(items[1].error?.message, "plain text");
  assert.equal(items[1].error?.mechanism, "unhandledrejection");

  detach();
  assert.equal(listeners.size, 0);
});

test("the other stack format reads the same way", () => {
  assert.deepEqual(parseStack("checkout@https://shop.example/app.js?v=3:10:5", "https://shop.example"), [
    { function: "checkout", file: "/app.js", line: 10, inApp: true },
  ]);
});

test("a trace joins from an ID or a traceparent, and an unreadable one joins nothing", () => {
  const hex = "0af7651916cd43dd8448eb211c80319c";
  const joined = Capture.event("a").withTrace(`00-${hex}-b7ad6b7169203331-01`);
  assert.equal(Buffer.from(joined.item.envelope.traceId!).toString("hex"), hex);
  assert.equal(Buffer.from(joined.item.envelope.spanId!).toString("hex"), "b7ad6b7169203331");

  assert.equal(Buffer.from(Capture.event("a").withTrace(hex).item.envelope.traceId!).toString("hex"), hex);
  assert.equal(Capture.event("a").withTrace("not-a-trace").item.envelope.traceId, undefined);
  assert.equal(parseTrace("0".repeat(32)), undefined);
  assert.equal(parseTrace(`01-${hex}-b7ad6b7169203331-01`), undefined);
});

/** Let the promises a timer callback started run to completion. */
async function settle(): Promise<void> {
  for (let i = 0; i < 10; i++) await Promise.resolve();
}
