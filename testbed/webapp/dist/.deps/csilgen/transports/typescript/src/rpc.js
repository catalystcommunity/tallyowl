// CSIL-RPC transport — request/response/push envelopes — see `csil-rpc-transport.md`.
import { Status, TransportError, VERSION, canonMap, checkVersion, decodeValue, encodeValue, getInt, getText, getTextOpt, getUint, getUintOpt, intValue, mapGet, statusIsOk, tag24, textValue, untag24, } from "./conventions.js";
// A CSIL-RPC request (client → server). `payload` is the opaque CBOR(request type)
// bytes; the transport wraps it in tag 24 on the wire and never inspects it.
export class RpcRequest {
    service;
    op;
    id;
    payload;
    auth;
    constructor(service, op, payload) {
        this.service = service;
        this.op = op;
        this.payload = payload;
    }
    withId(id) {
        this.id = id;
        return this;
    }
    withAuth(auth) {
        this.auth = auth;
        return this;
    }
    encode() {
        const entries = [
            ["v", intValue(VERSION)],
            ["service", textValue(this.service)],
            ["op", textValue(this.op)],
            ["payload", tag24(this.payload)],
        ];
        if (this.id !== undefined)
            entries.push(["id", intValue(this.id)]);
        if (this.auth !== undefined)
            entries.push(["auth", textValue(this.auth)]);
        return encodeValue(canonMap(entries));
    }
    static decode(bytes) {
        const v = decodeValue(bytes);
        checkVersion(getUint(v, "v"));
        const payloadField = mapGet(v, "payload");
        if (!payloadField)
            throw TransportError.malformed("missing 'payload'");
        const req = new RpcRequest(getText(v, "service"), getText(v, "op"), untag24(payloadField));
        req.id = getUintOpt(v, "id");
        req.auth = getTextOpt(v, "auth");
        return req;
    }
}
// A CSIL-RPC response (server → client). `status` is the transport outcome (a
// registry code); `variant` names the declared output-choice arm `payload`
// decodes to when `status` is 0. `payload` is empty when `status` is non-zero.
export class RpcResponse {
    id;
    status;
    variant;
    error;
    payload;
    constructor(status, payload) {
        this.status = status;
        this.payload = payload;
    }
    // A successful (status Ok) typed reply.
    static ok(variant, payload) {
        const r = new RpcResponse(Status.Ok, payload);
        r.variant = variant;
        return r;
    }
    // A transport-level failure (no typed payload).
    static transportError(status, message) {
        const r = new RpcResponse(status, new Uint8Array(0));
        r.error = message;
        return r;
    }
    withId(id) {
        this.id = id;
        return this;
    }
    encode() {
        const entries = [
            ["v", intValue(VERSION)],
            ["status", intValue(this.status)],
            ["payload", tag24(this.payload)],
        ];
        if (this.id !== undefined)
            entries.push(["id", intValue(this.id)]);
        if (this.variant !== undefined)
            entries.push(["variant", textValue(this.variant)]);
        if (this.error !== undefined)
            entries.push(["error", textValue(this.error)]);
        return encodeValue(canonMap(entries));
    }
    static decode(bytes) {
        const v = decodeValue(bytes);
        checkVersion(getUint(v, "v"));
        // payload is present but may be an empty byte string on error.
        const payloadField = mapGet(v, "payload");
        const payload = payloadField ? untag24(payloadField) : new Uint8Array(0);
        const r = new RpcResponse(getInt(v, "status"), payload);
        r.id = getUintOpt(v, "id");
        r.variant = getTextOpt(v, "variant");
        r.error = getTextOpt(v, "error");
        return r;
    }
    // Throw `TransportError` for a non-ok response so callers surface transport
    // failures distinctly from application errors (which ride as status-0 variants).
    intoTransportError() {
        if (statusIsOk(this.status))
            return this;
        throw TransportError.status(this.status, this.error);
    }
}
// A CSIL-RPC server push (server → client) for `<-` operations. No id (not a
// reply) and no status (cannot fail in the request/response sense).
export class RpcPush {
    service;
    event;
    payload;
    constructor(service, event, payload) {
        this.service = service;
        this.event = event;
        this.payload = payload;
    }
    encode() {
        return encodeValue(canonMap([
            ["v", intValue(VERSION)],
            ["service", textValue(this.service)],
            ["event", textValue(this.event)],
            ["payload", tag24(this.payload)],
        ]));
    }
    static decode(bytes) {
        const v = decodeValue(bytes);
        checkVersion(getUint(v, "v"));
        const payloadField = mapGet(v, "payload");
        if (!payloadField)
            throw TransportError.malformed("missing 'payload'");
        return new RpcPush(getText(v, "service"), getText(v, "event"), untag24(payloadField));
    }
}
export const reply = (variant, payload) => ({
    kind: "reply",
    variant,
    payload,
});
export const transport = (status, message) => ({
    kind: "transport",
    status,
    message,
});
// A CSIL-RPC client over a frame carrier. The carrier is injected (bring your own);
// the client owns the envelope and a per-connection monotonic correlation id.
export class RpcClient {
    nextId = 1;
    carrier;
    multiplexed;
    // `multiplexed` true assigns a correlation id to every request (required on WS /
    // pipelined streams); false omits it (strictly one-in-flight carriers).
    constructor(carrier, multiplexed) {
        this.carrier = carrier;
        this.multiplexed = multiplexed;
    }
    // Invoke `service/op` with an encoded request payload, returning the decoded
    // response. A non-zero transport status is thrown as `TransportError`.
    async call(service, op, payload, auth) {
        const req = new RpcRequest(service, op, payload);
        req.auth = auth;
        if (this.multiplexed) {
            req.id = this.nextId;
            this.nextId += 1;
        }
        await this.carrier.sendFrame(req.encode());
        const frame = await this.carrier.recvFrame();
        if (frame === null) {
            throw TransportError.carrier("connection closed before response");
        }
        return RpcResponse.decode(frame).intoTransportError();
    }
}
// A CSIL-RPC server over a frame carrier. The host supplies a handler mapping
// `(service, op, request-payload)` to an outcome; the generated router is the
// natural implementation of that handler.
export class RpcServer {
    carrier;
    constructor(carrier) {
        this.carrier = carrier;
    }
    // Read one request, dispatch it through `handler`, and write the response.
    // Returns `false` at a clean end of stream.
    async serveOne(handler) {
        const frame = await this.carrier.recvFrame();
        if (frame === null)
            return false;
        let resp;
        try {
            const req = RpcRequest.decode(frame);
            const outcome = await handler(req);
            resp =
                outcome.kind === "reply"
                    ? RpcResponse.ok(outcome.variant, outcome.payload).withId(req.id)
                    : RpcResponse.transportError(outcome.status, outcome.message).withId(req.id);
        }
        catch (e) {
            const message = e instanceof Error ? e.message : String(e);
            resp = RpcResponse.transportError(Status.MalformedEnvelope, message);
        }
        await this.carrier.sendFrame(resp.encode());
        return true;
    }
}
