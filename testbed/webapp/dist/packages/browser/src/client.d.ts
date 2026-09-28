import type { TelemetryItem } from "./ingest-api.ts";
import { Capture } from "./capture.ts";
/**
 * The seam the host provides.
 *
 * A host that already speaks CSIL to its own backend implements this with its
 * existing router or multiplexed carrier. The browser package never builds one.
 */
export interface Router {
    /**
     * Send one already-encoded request on the host's own connection and return
     * the reply.
     *
     * `variant` names which arm of the operation's output choice `payload`
     * decodes to, so a caller can tell a typed result from a typed error.
     */
    call(service: string, op: string, payload: Uint8Array): Promise<{
        variant?: string;
        payload: Uint8Array;
    }>;
}
/** A typed rejection from the host or the collector behind it. */
export declare class ServiceError extends Error {
    readonly code: string;
    readonly retryable: boolean;
    constructor(code: string, message: string, retryable: boolean);
}
/** What one send produced. */
export interface CaptureResult {
    accepted: number;
    /** True only for a critical send that reached the collector's durability
     * boundary. An ordinary capture is best effort and reports false. */
    durable: boolean;
    rejected: {
        eventId: Uint8Array;
        code: string;
        message: string;
    }[];
}
/** Browser package settings. There is no address here, and there is not going
 * to be one. */
export interface Settings {
    /** Seal a buffer at this many items. */
    maxItems: number;
    /** Seal a buffer after this long. */
    lingerMs: number;
    /** Hold at most this many items before refusing. A browser cannot block, so
     * the oldest ordinary item is dropped and the drop is counted. */
    maxBuffered: number;
    /** Send at most this many encoded bytes in one unload flush request. A
     * browser refuses a beacon above its own quota, which is about 64 KiB, so a
     * large buffer goes as several small requests. */
    maxBeaconBytes: number;
    /** A release name that every item carries. */
    release?: string;
    /** Receives every failure that `flushSafely`, the flush timer, and the
     * unload flush would otherwise keep to themselves. */
    onError?: (error: unknown) => void;
    /** Log each captured item. `true` writes to the console. This only logs: it
     * sends nothing and it changes no seal condition. */
    debug?: boolean | ((item: TelemetryItem) => void);
}
export declare const defaultSettings: Settings;
/**
 * The host's unload route. It returns what `sendBeacon` returned. Only `false`
 * means the browser refused the request, so a host that returns nothing still
 * works and only loses the count of what was refused.
 */
export type UnloadSend = (payload: Uint8Array) => unknown;
/** What this client has done since it was built. Every count is in items. */
export interface Stats {
    /** Items `capture` accepted. */
    captured: number;
    /** Items the host took, on its carrier or on its unload route. */
    sent: number;
    /** Items this client gave up on: the buffer was full, the host refused them
     * for good, or the browser refused the unload request. */
    dropped: number;
    /** Items waiting now. */
    buffered: number;
    /** The most recent failure, until a send succeeds. */
    lastError: unknown;
}
/**
 * The session lifecycle, which works on every surface including one with no
 * browser storage.
 *
 * The library issues the identifier; a person cannot select it. See D11.
 */
export declare class Session {
    readonly id: string;
    private constructor();
    static start(): Session;
}
/**
 * The browser client.
 *
 * `capture` buffers and makes no durability claim. `sendCritical` waits for the
 * host to reach the collector's durability boundary, and reports what it
 * actually got rather than assuming.
 */
export declare class BrowserClient {
    private readonly router;
    private readonly settings;
    private buffer;
    private openedAt;
    private captured;
    private sent;
    private dropped;
    private lastError;
    private timer;
    private flushing;
    constructor(router: Router, settings?: Partial<Settings>);
    /** How many items are waiting. */
    get buffered(): number;
    /** How many items this client dropped. A rising count is the signal that a
     * browser is producing faster than the host can take, or that the host is
     * refusing what it is given. */
    get droppedCount(): number;
    /** What this client has captured, sent, and dropped. */
    stats(): Stats;
    /**
     * Buffer one item. This does not reach the host and makes no durability
     * claim.
     *
     * A browser cannot block a person's interaction to wait for capacity, so a
     * full buffer drops the oldest ordinary item and counts the drop. A critical
     * item goes only when nothing ordinary is left. It never reports success for
     * something it discarded without counting it.
     */
    capture(capture: Capture): void;
    /** Drop until the buffer fits, oldest ordinary item first. */
    private evictToFit;
    private log;
    /** Whether the buffer has reached a seal condition. */
    get shouldFlush(): boolean;
    /** Take the oldest items, at most one sealed batch of them. */
    private take;
    /**
     * Put items the host did not take back where they were.
     *
     * They go to the front, so order holds. The buffer bound still holds, and
     * what no longer fits is dropped and counted.
     */
    private restore;
    /**
     * Send every sealed batch with `send`, and keep what the host did not take.
     *
     * A failure on the carrier and a retryable rejection keep the items for the
     * next flush. A permanent rejection drops them and counts them, because
     * sending the same items again gets the same answer.
     */
    private drain;
    /**
     * Send everything buffered on the host's connection, best effort.
     *
     * Success means the host's carrier accepted the frame. It does not mean the
     * data is durable, and the result says so. See DELIVERY.md section 1.
     *
     * A failed send rejects, and the items stay buffered for the next flush. A
     * caller that does not want a rejection uses `flushSafely`.
     */
    flush(): Promise<CaptureResult | undefined>;
    /**
     * Flush, and never reject.
     *
     * This is the form for an event handler and for a timer, where a rejection
     * would reach the host page as an unhandled one. The failure goes to
     * `onError` and to `stats().lastError`, and the items stay buffered.
     */
    flushSafely(): Promise<CaptureResult | undefined>;
    private report;
    /**
     * Flush on a timer whenever a seal condition is reached.
     *
     * This is opt-in. A host that already has a scheduler calls `flushSafely`
     * from it and never calls this. The default period is half of `lingerMs`.
     */
    start(intervalMs?: number): void;
    /** Stop the flush timer. What is buffered stays buffered. */
    stop(): void;
    /**
     * Send everything buffered and wait for the collector's durability boundary.
     *
     * A host chooses whether to expose this. The result reports the durability it
     * actually reached, so a caller never has to assume. See D5.
     */
    sendCritical(): Promise<CaptureResult | undefined>;
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
    attachUnloadFlush(send: UnloadSend, target: {
        addEventListener(type: string, listener: () => void): void;
        removeEventListener?(type: string, listener: () => void): void;
        visibilityState?: string;
    }): () => void;
    /**
     * Hand everything buffered to the host's unload route now, and report how
     * many items the browser took.
     *
     * The buffer empties before the first request, so a `pagehide` and a
     * `visibilitychange` that both fire cannot send one item twice. Critical
     * items go first, because a browser stops taking requests when its quota is
     * used and a session end is what D34 exists to save.
     */
    flushOnUnload(send: UnloadSend): number;
    /** Split items into requests that each stay under `maxBeaconBytes`. An item
     * that is larger than the bound goes alone. */
    private beaconChunks;
    private throwIfServiceError;
}
