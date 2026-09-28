import type { ErrorPayload, StackFrame } from "./collector-api.ts";
import type { BrowserClient } from "./client.ts";
export interface GlobalErrorOptions {
    /** Where to listen. The default is the page's global object. */
    target?: {
        addEventListener(type: string, listener: (event: unknown) => void): void;
        removeEventListener(type: string, listener: (event: unknown) => void): void;
    };
    /** The session each error belongs to. */
    sessionId?: string | (() => string | undefined);
    /** The origin whose scripts count as the application's own. The default is
     * the page's origin. */
    origin?: string;
}
/**
 * Record every `error` and `unhandledrejection` the page raises.
 *
 * Each one is an unhandled error, so it seals the buffer. This function sends
 * nothing itself: the host's flush, or `client.start()`, does that.
 *
 * The result detaches the listeners.
 */
export declare function captureGlobalErrors(client: BrowserClient, options?: GlobalErrorOptions): () => void;
/** Build the payload for one thrown value. Exported for a host that catches an
 * error itself and wants the same frames. */
export declare function errorPayload(thrown: unknown, mechanism: string, origin?: string): ErrorPayload;
/**
 * Read the two stack formats browsers produce.
 *
 *     at checkout (https://shop.example/assets/app.js?v=3:10:5)
 *     checkout@https://shop.example/assets/app.js?v=3:10:5
 */
export declare function parseStack(stack: string, origin?: string): StackFrame[];
