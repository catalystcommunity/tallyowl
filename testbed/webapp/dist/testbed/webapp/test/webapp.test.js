// seedstore's web application against its own backend's route.
//
// The rules under test are the integration rules the test bed exists to prove,
// from AGENTS.md and docs/TESTBED.md section 2:
//
// - the browser opens no TallyOwl connection and names no TallyOwl address;
// - the browser never sends a workspace, a project, or a credential;
// - an unload flush reaches the host's own route and nothing else;
// - a conversion and an unhandled error take the durable path.
//
// The fake here is seedstore's route, not a mock of the browser package. It
// decodes a real CSIL-RPC frame and a real `CaptureRequest`, so the test proves
// the wire and not a stub.
import assert from "node:assert/strict";
import { test } from "node:test";
import { RpcRequest, RpcResponse } from "../../../.deps/csilgen/transports/typescript/src/rpc.js";
import { fromCaptureRequestCbor, toCaptureCriticalResponseCbor, toCaptureResponseCbor, } from "../../../generated/typescript/tallyowl-ingest-api/codec.gen.js";
import { FUNNEL, close, fail, open, purchase, walk } from "../src/index.js";
import { HostRouter, defaultRoutes, unloadSender } from "../src/router.js";
/// seedstore's route. It records every URL it was asked for, because "never
/// contacts a TallyOwl domain" is only proved by looking at the addresses.
function seedstore() {
    const urls = [];
    const items = [];
    const ops = [];
    const send = (async (url, init) => {
        urls.push(String(url));
        const frame = RpcRequest.decode(new Uint8Array(init.body));
        ops.push(frame.op);
        assert.equal(frame.service, "TallyOwlIngest");
        const request = fromCaptureRequestCbor(frame.payload);
        items.push(...request.items);
        const body = frame.op === "capture-critical"
            ? RpcResponse.ok("CaptureCriticalResponse", toCaptureCriticalResponseCbor({
                accepted: request.items.length,
                durable: true,
            })).withId(frame.id)
            : RpcResponse.ok("CaptureResponse", toCaptureResponseCbor({ accepted: request.items.length })).withId(frame.id);
        const encoded = body.encode();
        return {
            status: 200,
            arrayBuffer: async () => encoded.buffer.slice(encoded.byteOffset, encoded.byteOffset + encoded.byteLength),
        };
    });
    return { urls, items, ops, router: new HostRouter(defaultRoutes, send) };
}
test("a browser event reaches the collector through the application's own route", async () => {
    const host = seedstore();
    const storefront = open(host.router);
    await walk(storefront, FUNNEL);
    await close(storefront);
    // Every address is seedstore's. This is the rule in AGENTS.md, and the only
    // way to prove it is to look at what was contacted.
    assert.ok(host.urls.length > 0);
    for (const url of host.urls) {
        assert.equal(url, "/telemetry", `the browser reached ${url}`);
    }
    const names = host.items.map((item) => item.event?.name).filter(Boolean);
    for (const step of FUNNEL) {
        assert.ok(names.includes(step), `${step} never arrived`);
    }
});
test("the browser never sends a workspace, a project, or a credential", async () => {
    // The collector resolves tenancy from the backend's credential and stamps it.
    // A browser that sent tenancy would be claiming something it cannot know.
    const host = seedstore();
    const storefront = open(host.router);
    await walk(storefront, ["catalogue-viewed"]);
    for (const item of host.items) {
        assert.equal(item.envelope.workspaceId, undefined);
        assert.equal(item.envelope.projectId, undefined);
        assert.equal(item.envelope.sourceId, undefined);
        assert.equal(item.envelope.receivedAt, undefined);
    }
});
test("every item carries the session and the release", async () => {
    // Phase 5's exit criterion needs an error that correlates to a session and a
    // release, and a correlation that is missing on some items is not one.
    const host = seedstore();
    const storefront = open(host.router, "seedstore-9.9.9");
    await walk(storefront, ["catalogue-viewed"]);
    await fail(storefront, "TypeError", "seeds is not iterable");
    const errors = host.items.filter((item) => item.error !== undefined);
    assert.equal(errors.length, 1);
    assert.equal(errors[0].envelope.sessionId, storefront.session.id);
    assert.equal(errors[0].envelope.release, "seedstore-9.9.9");
    assert.equal(errors[0].error?.handled, false);
});
test("a conversion and an unhandled error take the durable path", async () => {
    // Both are critical, so they use `capture-critical` and reach the collector's
    // durability boundary rather than the best-effort buffer.
    const host = seedstore();
    const storefront = open(host.router);
    await purchase(storefront, "order-1", "19.99", "USD");
    await fail(storefront, "TypeError", "seeds is not iterable");
    assert.deepEqual(host.ops, ["capture-critical", "capture-critical"]);
    const conversion = host.items.find((item) => item.conversion !== undefined);
    assert.equal(conversion?.conversion?.goal, "purchase");
    assert.equal(conversion?.conversion?.currency, "USD");
    // Money is an exact decimal and never a float: 1999 at exponent -2.
    assert.equal(conversion?.conversion?.value?.mantissa, 1999n);
    assert.equal(conversion?.conversion?.value?.exponent, -2);
});
test("an unload flush reaches the host's own route and nothing else", async () => {
    // D34: the unload flush is the one browser-only HTTP use permitted for
    // telemetry, and it reaches the host's route, never a TallyOwl domain.
    const beaconed = [];
    const host = seedstore();
    const storefront = open(host.router);
    const flushNow = storefront.client.attachUnloadFlush(unloadSender(defaultRoutes, (url, payload) => {
        beaconed.push({ url, bytes: payload.size });
        return true;
    }), { addEventListener: () => { }, visibilityState: "visible" });
    await walk(storefront, ["catalogue-viewed"]);
    storefront.client.capture((await import("../../../packages/browser/src/index.js")).Capture.event("tab-closing"));
    flushNow();
    assert.equal(beaconed.length, 1, "the unload flush did not go");
    assert.equal(beaconed[0].url, "/telemetry/unload");
    assert.ok(beaconed[0].bytes > 0);
    // And an empty buffer sends nothing, so a closing tab with no events does not
    // produce a request.
    flushNow();
    assert.equal(beaconed.length, 1);
});
test("a rejection from the host arrives as a typed failure", async () => {
    const failing = (async () => {
        const response = RpcResponse.transportError(7, "seedstore is not taking events.");
        const encoded = response.encode();
        return {
            status: 200,
            arrayBuffer: async () => encoded.buffer.slice(encoded.byteOffset, encoded.byteOffset + encoded.byteLength),
        };
    });
    const storefront = open(new HostRouter(defaultRoutes, failing));
    await assert.rejects(() => walk(storefront, ["catalogue-viewed"]));
});
