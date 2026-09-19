import type { ConversionPayload, ErrorPayload, EventPayload, PageViewPayload, TelemetryItem } from "./collector-api.ts";
import { write, type Value } from "./value.ts";
export declare const SDK_NAME = "tallyowl-browser";
export declare const SDK_VERSION = "0.2.0";
/** Milliseconds since the Unix epoch. Every time TallyOwl stores is this. */
export declare const nowMs: () => number;
/**
 * A UUIDv7: a millisecond timestamp followed by random bytes, so an identifier
 * sorts by time and does not repeat inside one millisecond.
 *
 * A browser-supplied identifier is untrusted. The app driver namespaces or
 * replaces it before it seals a batch, and a server-generated one is
 * authoritative when both exist. See DELIVERY.md section 2.
 */
export declare function newEventId(): Uint8Array;
/** One item, before the host sends it. */
export declare class Capture {
    readonly item: TelemetryItem;
    critical: boolean;
    private constructor();
    /** A named product or behavior event. */
    static event(name: string, payload?: Partial<EventPayload>): Capture;
    /** A page view or a screen view. */
    static pageView(route: string, payload?: Partial<PageViewPayload>): Capture;
    /** A semantic interaction. TallyOwl records the name of what happened, never
     * a Document Object Model selector, and it does not do session replay. */
    static interaction(target: string, action: string): Capture;
    /** The start of a session. */
    static sessionStart(sessionId: string, entryRoute?: string): Capture;
    /** The end of a session, and why it ended. */
    static sessionEnd(sessionId: string, reason: "explicit" | "timeout" | "maximum-lifetime"): Capture;
    /** A heartbeat carries no payload beyond its envelope. */
    static sessionHeartbeat(sessionId: string): Capture;
    /** A conversion. Money travels as an exact decimal and never as a float. */
    static conversion(goal: string, payload?: Partial<ConversionPayload>): Capture;
    /** An error occurrence. The producer never supplies a group; the projector
     * computes the fingerprint. See D39. */
    static error(payload: ErrorPayload): Capture;
    /** Seal the current batch as soon as this item enters it. */
    asCritical(): this;
    at(occurredAt: number): this;
    withEventId(eventId: Uint8Array): this;
    withSession(sessionId: string): this;
    withRequest(requestId: string): this;
    withRelease(release: string): this;
    /** A typed property from the calling code, at the event site. */
    withProperty(key: string, value: Value): this;
    /** The anonymous identifier the host gave this browser. It is opaque and has
     * project scope. */
    withAnonymousId(anonymousId: string): this;
    get eventId(): Uint8Array;
}
/** Which payload field an item carries. */
export declare function payloadName(item: TelemetryItem): string;
export { write };
