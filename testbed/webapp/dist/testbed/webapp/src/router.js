// seedstore's own route, as the browser package's `Router`.
//
// The browser package builds no carrier and names no address: the host injects
// one `Router` and everything travels on it. This is seedstore's, and every
// address in it is seedstore's. AGENTS.md: browser instrumentation "must not
// create a TallyOwl connection or contact a TallyOwl domain".
//
// There is no TallyOwl address in this file and there is nowhere to configure
// one.
import { RpcRequest, RpcResponse } from "../../../.deps/csilgen/transports/typescript/src/rpc.js";
export const defaultRoutes = {
    telemetry: "/telemetry",
    unload: "/telemetry/unload",
};
/// A router over seedstore's own HTTP route.
export class HostRouter {
    routes;
    send;
    nextId = 1;
    constructor(routes = defaultRoutes, send = globalThis.fetch.bind(globalThis)) {
        this.routes = routes;
        this.send = send;
    }
    async call(service, op, payload) {
        const frame = new RpcRequest(service, op, payload).withId(this.nextId++);
        const http = await this.send(this.routes.telemetry, {
            method: "POST",
            headers: { "content-type": "application/cbor" },
            body: frame.encode(),
        });
        const response = RpcResponse.decode(new Uint8Array(await http.arrayBuffer()));
        if (response.status !== 0) {
            throw new Error(response.error ?? "seedstore could not take that event.");
        }
        return { variant: response.variant, payload: response.payload };
    }
}
/// Send an unload flush to seedstore's own route.
///
/// `sendBeacon` is what survives a closing tab. It sends bytes with no reply,
/// which is why the route takes an encoded `CaptureRequest` rather than an RPC
/// frame. This is best effort by design: D34 says the browser package does not
/// claim durable delivery after a tab closes.
export function unloadSender(routes = defaultRoutes, beacon) {
    const send = beacon ??
        ((url, payload) => globalThis.navigator.sendBeacon(url, payload));
    return (payload) => {
        send(routes.unload, new Blob([payload], { type: "application/cbor" }));
    };
}
