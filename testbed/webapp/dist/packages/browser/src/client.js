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
    maxBeaconBytes: 32 * 1024,
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
    captured = 0;
    sent = 0;
    dropped = 0;
    lastError;
    timer;
    flushing = false;
    constructor(router, settings = {}) {
        this.router = router;
        this.settings = { ...defaultSettings, ...settings };
    }
    /** How many items are waiting. */
    get buffered() {
        return this.buffer.length;
    }
    /** How many items this client dropped. A rising count is the signal that a
     * browser is producing faster than the host can take, or that the host is
     * refusing what it is given. */
    get droppedCount() {
        return this.dropped;
    }
    /** What this client has captured, sent, and dropped. */
    stats() {
        return {
            captured: this.captured,
            sent: this.sent,
            dropped: this.dropped,
            buffered: this.buffer.length,
            lastError: this.lastError,
        };
    }
    /**
     * Buffer one item. This does not reach the host and makes no durability
     * claim.
     *
     * A browser cannot block a person's interaction to wait for capacity, so a
     * full buffer drops the oldest ordinary item and counts the drop. A critical
     * item goes only when nothing ordinary is left. It never reports success for
     * something it discarded without counting it.
     */
    capture(capture) {
        const item = capture.item;
        payloadName(item);
        if (this.settings.release !== undefined && item.envelope.release === undefined) {
            item.envelope.release = this.settings.release;
        }
        this.captured += 1;
        this.log(item);
        if (this.openedAt === undefined)
            this.openedAt = nowMs();
        this.buffer.push({ item, critical: capture.critical });
        this.evictToFit();
    }
    /** Drop until the buffer fits, oldest ordinary item first. */
    evictToFit() {
        const limit = Math.max(1, this.settings.maxBuffered);
        while (this.buffer.length > limit) {
            const ordinary = this.buffer.findIndex((held) => !held.critical);
            this.buffer.splice(ordinary === -1 ? 0 : ordinary, 1);
            this.dropped += 1;
        }
        if (this.buffer.length === 0)
            this.openedAt = undefined;
    }
    log(item) {
        const debug = this.settings.debug;
        if (debug === undefined || debug === false)
            return;
        try {
            if (typeof debug === "function")
                debug(item);
            else
                globalThis.console?.debug("tallyowl captured", item);
        }
        catch {
            // A logger that throws must not cost the host its event.
        }
    }
    /** Whether the buffer has reached a seal condition. */
    get shouldFlush() {
        if (this.buffer.length === 0)
            return false;
        if (this.buffer.length >= this.settings.maxItems)
            return true;
        if (this.buffer.some((held) => held.critical))
            return true;
        return this.openedAt !== undefined && nowMs() - this.openedAt >= this.settings.lingerMs;
    }
    /** Take the oldest items, at most one sealed batch of them. */
    take(limit = Math.max(1, this.settings.maxItems)) {
        const taken = this.buffer.splice(0, limit);
        this.openedAt = this.buffer.length === 0 ? undefined : nowMs();
        return taken;
    }
    /**
     * Put items the host did not take back where they were.
     *
     * They go to the front, so order holds. The buffer bound still holds, and
     * what no longer fits is dropped and counted.
     */
    restore(taken) {
        this.buffer = taken.concat(this.buffer);
        if (this.openedAt === undefined && this.buffer.length > 0)
            this.openedAt = nowMs();
        this.evictToFit();
    }
    /**
     * Send every sealed batch with `send`, and keep what the host did not take.
     *
     * A failure on the carrier and a retryable rejection keep the items for the
     * next flush. A permanent rejection drops them and counts them, because
     * sending the same items again gets the same answer.
     */
    async drain(send) {
        let total;
        while (this.buffer.length > 0) {
            const taken = this.take();
            let result;
            try {
                result = await send(taken.map((held) => held.item));
            }
            catch (error) {
                if (error instanceof ServiceError && !error.retryable)
                    this.dropped += taken.length;
                else
                    this.restore(taken);
                this.lastError = error;
                throw error;
            }
            this.sent += taken.length;
            this.lastError = undefined;
            total =
                total === undefined
                    ? result
                    : {
                        accepted: total.accepted + result.accepted,
                        durable: total.durable && result.durable,
                        rejected: total.rejected.concat(result.rejected),
                    };
        }
        return total;
    }
    /**
     * Send everything buffered on the host's connection, best effort.
     *
     * Success means the host's carrier accepted the frame. It does not mean the
     * data is durable, and the result says so. See DELIVERY.md section 1.
     *
     * A failed send rejects, and the items stay buffered for the next flush. A
     * caller that does not want a rejection uses `flushSafely`.
     */
    async flush() {
        return this.drain(async (items) => {
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
        });
    }
    /**
     * Flush, and never reject.
     *
     * This is the form for an event handler and for a timer, where a rejection
     * would reach the host page as an unhandled one. The failure goes to
     * `onError` and to `stats().lastError`, and the items stay buffered.
     */
    async flushSafely() {
        try {
            return await this.flush();
        }
        catch (error) {
            this.report(error);
            return undefined;
        }
    }
    report(error) {
        this.lastError = error;
        try {
            this.settings.onError?.(error);
        }
        catch {
            // A failure handler that throws must not reach the host page either.
        }
    }
    /**
     * Flush on a timer whenever a seal condition is reached.
     *
     * This is opt-in. A host that already has a scheduler calls `flushSafely`
     * from it and never calls this. The default period is half of `lingerMs`.
     */
    start(intervalMs) {
        if (this.timer !== undefined)
            return;
        const period = Math.max(50, intervalMs ?? this.settings.lingerMs / 2);
        this.timer = setInterval(() => {
            if (this.flushing || !this.shouldFlush)
                return;
            this.flushing = true;
            void this.flushSafely().finally(() => {
                this.flushing = false;
            });
        }, period);
        // A timer must not keep a process that is not a browser alive.
        this.timer.unref?.();
    }
    /** Stop the flush timer. What is buffered stays buffered. */
    stop() {
        if (this.timer === undefined)
            return;
        clearInterval(this.timer);
        this.timer = undefined;
    }
    /**
     * Send everything buffered and wait for the collector's durability boundary.
     *
     * A host chooses whether to expose this. The result reports the durability it
     * actually reached, so a caller never has to assume. See D5.
     */
    async sendCritical() {
        return this.drain(async (items) => {
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
        });
    }
    /**
     * Attach the unload flush to the host's own same-origin route.
     *
     * A browser loses buffered events when a person closes a tab, and session end
     * and exit events are exactly the ones a funnel and a session-duration
     * analysis need. The flush is best effort and this package does not claim
     * durable delivery after a tab closes. See D34.
     *
     * `send` is the host's own route. It is never a TallyOwl address. It returns
     * what `sendBeacon` returned: `false` means the browser refused the request,
     * and those items are counted as dropped.
     *
     * The result detaches the listeners.
     */
    attachUnloadFlush(send, target) {
        const onPageHide = () => {
            this.flushOnUnload(send);
        };
        const onVisibility = () => {
            if (target.visibilityState === "hidden")
                this.flushOnUnload(send);
        };
        target.addEventListener("pagehide", onPageHide);
        target.addEventListener("visibilitychange", onVisibility);
        return () => {
            target.removeEventListener?.("pagehide", onPageHide);
            target.removeEventListener?.("visibilitychange", onVisibility);
        };
    }
    /**
     * Hand everything buffered to the host's unload route now, and report how
     * many items the browser took.
     *
     * The buffer empties before the first request, so a `pagehide` and a
     * `visibilitychange` that both fire cannot send one item twice. Critical
     * items go first, because a browser stops taking requests when its quota is
     * used and a session end is what D34 exists to save.
     */
    flushOnUnload(send) {
        if (this.buffer.length === 0)
            return 0;
        const taken = this.take(this.buffer.length);
        const ordered = taken
            .filter((held) => held.critical)
            .concat(taken.filter((held) => !held.critical))
            .map((held) => held.item);
        let handed = 0;
        for (const chunk of this.beaconChunks(ordered)) {
            let took;
            try {
                took = send(toCaptureRequestCbor({ items: chunk }));
            }
            catch (error) {
                this.report(error);
                took = false;
            }
            if (took === false) {
                this.dropped += chunk.length;
            }
            else {
                this.sent += chunk.length;
                handed += chunk.length;
            }
        }
        return handed;
    }
    /** Split items into requests that each stay under `maxBeaconBytes`. An item
     * that is larger than the bound goes alone. */
    beaconChunks(items) {
        const limit = Math.max(1, this.settings.maxBeaconBytes);
        const chunks = [];
        let open = [];
        let bytes = 0;
        for (const item of items) {
            // The request wrapper costs a few bytes once, and this counts it for
            // each item, so a chunk never measures larger than this sum.
            const size = toCaptureRequestCbor({ items: [item] }).length;
            if (open.length > 0 && bytes + size > limit) {
                chunks.push(open);
                open = [];
                bytes = 0;
            }
            open.push(item);
            bytes += size;
        }
        if (open.length > 0)
            chunks.push(open);
        return chunks;
    }
    throwIfServiceError(reply) {
        if (reply.variant !== "ServiceError")
            return;
        const wire = fromServiceErrorCbor(reply.payload);
        throw new ServiceError(wire.code, wire.message, wire.retryable);
    }
}
