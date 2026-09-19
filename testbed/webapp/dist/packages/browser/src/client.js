// The browser package, and the connection it is not allowed to open.
//
// Browser instrumentation rides the host application's existing same-origin
// CSIL connection. It must not create a TallyOwl connection and must not
// contact a TallyOwl domain. That rule is in AGENTS.md, and this module holds
// it in code: there is no socket here, no fetch to a TallyOwl address, and no
// address configuration at all. The host injects one `Router`, and everything
// travels on it.
//
// The unload flush is the one exception the design permits, and it is not an
// exception to that rule. It reaches the host application's own same-origin
// route, never a TallyOwl domain. See D34.
import { fromCaptureCriticalResponseCbor, fromCaptureResponseCbor, fromServiceErrorCbor, toCaptureCriticalRequestCbor, toCaptureRequestCbor, } from "./ingest-api.js";
import { nowMs, payloadName } from "./capture.js";
const SERVICE = "TallyOwlIngest";
/** A typed rejection from the host or the collector behind it. */
export class ServiceError extends Error {
    code;
    retryable;
    constructor(code, message, retryable) {
        super(message);
        this.code = code;
        this.retryable = retryable;
        this.name = "ServiceError";
    }
}
export const defaultSettings = {
    maxItems: 64,
    lingerMs: 2_000,
    maxBuffered: 512,
};
/**
 * The session lifecycle, which works on every surface including one with no
 * browser storage.
 *
 * The library issues the identifier; a person cannot select it. See D11.
 */
export class Session {
    id;
    constructor(id) {
        this.id = id;
    }
    static start() {
        const bytes = new Uint8Array(16);
        if (typeof globalThis.crypto?.getRandomValues === "function") {
            globalThis.crypto.getRandomValues(bytes);
        }
        else {
            for (let i = 0; i < bytes.length; i++)
                bytes[i] = Math.floor(Math.random() * 256);
        }
        return new Session(Array.from(bytes)
            .map((b) => b.toString(16).padStart(2, "0"))
            .join(""));
    }
}
/**
 * The browser client.
 *
 * `capture` buffers and makes no durability claim. `sendCritical` waits for the
 * host to reach the collector's durability boundary, and reports what it
 * actually got rather than assuming.
 */
export class BrowserClient {
    router;
    settings;
    buffer = [];
    openedAt;
    sealNow = false;
    dropped = 0;
    constructor(router, settings = {}) {
        this.router = router;
        this.settings = { ...defaultSettings, ...settings };
    }
    /** How many items are waiting. */
    get buffered() {
        return this.buffer.length;
    }
    /** How many items this client dropped because the buffer was full. A rising
     * count is the signal that a browser is producing faster than the host can
     * take. */
    get droppedCount() {
        return this.dropped;
    }
    /**
     * Buffer one item. This does not reach the host and makes no durability
     * claim.
     *
     * A browser cannot block a person's interaction to wait for capacity, so a
     * full buffer drops the oldest ordinary item and counts the drop. It never
     * reports success for something it discarded without counting it.
     */
    capture(capture) {
        const item = capture.item;
        payloadName(item);
        if (this.settings.release !== undefined && item.envelope.release === undefined) {
            item.envelope.release = this.settings.release;
        }
        if (this.buffer.length >= this.settings.maxBuffered) {
            this.buffer.shift();
            this.dropped += 1;
        }
        if (this.openedAt === undefined)
            this.openedAt = nowMs();
        this.buffer.push(item);
        if (capture.critical)
            this.sealNow = true;
    }
    /** Whether the buffer has reached a seal condition. */
    get shouldFlush() {
        if (this.buffer.length === 0)
            return false;
        if (this.sealNow || this.buffer.length >= this.settings.maxItems)
            return true;
        return this.openedAt !== undefined && nowMs() - this.openedAt >= this.settings.lingerMs;
    }
    take() {
        const items = this.buffer;
        this.buffer = [];
        this.openedAt = undefined;
        this.sealNow = false;
        return items;
    }
    /**
     * Send everything buffered on the host's connection, best effort.
     *
     * Success means the host's carrier accepted the frame. It does not mean the
     * data is durable, and the result says so. See DELIVERY.md section 1.
     */
    async flush() {
        if (this.buffer.length === 0)
            return undefined;
        const items = this.take();
        const reply = await this.router.call(SERVICE, "capture", toCaptureRequestCbor({ items }));
        this.throwIfServiceError(reply);
        const response = fromCaptureResponseCbor(reply.payload);
        return {
            accepted: response.accepted,
            durable: false,
            rejected: (response.rejected ?? []).map((r) => ({
                eventId: r.eventId,
                code: r.code,
                message: r.message,
            })),
        };
    }
    /**
     * Send everything buffered and wait for the collector's durability boundary.
     *
     * A host chooses whether to expose this. The result reports the durability it
     * actually reached, so a caller never has to assume. See D5.
     */
    async sendCritical() {
        if (this.buffer.length === 0)
            return undefined;
        const items = this.take();
        const reply = await this.router.call(SERVICE, "capture-critical", toCaptureCriticalRequestCbor({ items }));
        this.throwIfServiceError(reply);
        const response = fromCaptureCriticalResponseCbor(reply.payload);
        return {
            accepted: response.accepted,
            durable: response.durable,
            rejected: (response.rejected ?? []).map((r) => ({
                eventId: r.eventId,
                code: r.code,
                message: r.message,
            })),
        };
    }
    /**
     * Attach the unload flush to the host's own same-origin route.
     *
     * A browser loses buffered events when a person closes a tab, and session end
     * and exit events are exactly the ones a funnel and a session-duration
     * analysis need. The flush is best effort and this package does not claim
     * durable delivery after a tab closes. See D34.
     *
     * `send` is the host's own route. It is never a TallyOwl address.
     */
    attachUnloadFlush(send, target) {
        const flushNow = () => {
            if (this.buffer.length === 0)
                return;
            send(toCaptureRequestCbor({ items: this.take() }));
        };
        const onVisibility = () => {
            if (target.visibilityState === "hidden")
                flushNow();
        };
        target.addEventListener("pagehide", flushNow);
        target.addEventListener("visibilitychange", onVisibility);
        return flushNow;
    }
    throwIfServiceError(reply) {
        if (reply.variant !== "ServiceError")
            return;
        const wire = fromServiceErrorCbor(reply.payload);
        throw new ServiceError(wire.code, wire.message, wire.retryable);
    }
}
