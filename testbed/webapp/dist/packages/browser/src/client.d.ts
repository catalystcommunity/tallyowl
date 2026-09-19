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
    /** A release name that every item carries. */
    release?: string;
}
export declare const defaultSettings: Settings;
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
    private sealNow;
    private dropped;
    constructor(router: Router, settings?: Partial<Settings>);
    /** How many items are waiting. */
    get buffered(): number;
    /** How many items this client dropped because the buffer was full. A rising
     * count is the signal that a browser is producing faster than the host can
     * take. */
    get droppedCount(): number;
    /**
     * Buffer one item. This does not reach the host and makes no durability
     * claim.
     *
     * A browser cannot block a person's interaction to wait for capacity, so a
     * full buffer drops the oldest ordinary item and counts the drop. It never
     * reports success for something it discarded without counting it.
     */
    capture(capture: Capture): void;
    /** Whether the buffer has reached a seal condition. */
    get shouldFlush(): boolean;
    private take;
    /**
     * Send everything buffered on the host's connection, best effort.
     *
     * Success means the host's carrier accepted the frame. It does not mean the
     * data is durable, and the result says so. See DELIVERY.md section 1.
     */
    flush(): Promise<CaptureResult | undefined>;
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
     * `send` is the host's own route. It is never a TallyOwl address.
     */
    attachUnloadFlush(send: (payload: Uint8Array) => void, target: {
        addEventListener(type: string, listener: () => void): void;
        visibilityState?: string;
    }): () => void;
    private throwIfServiceError;
}
