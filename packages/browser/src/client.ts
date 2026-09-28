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

import {
  fromCaptureCriticalResponseCbor,
  fromCaptureResponseCbor,
  fromServiceErrorCbor,
  toCaptureCriticalRequestCbor,
  toCaptureRequestCbor,
} from "./ingest-api.ts";
import type { TelemetryItem } from "./ingest-api.ts";

import { Capture, nowMs, payloadName } from "./capture.ts";

const SERVICE = "TallyOwlIngest";

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
  call(
    service: string,
    op: string,
    payload: Uint8Array,
  ): Promise<{ variant?: string; payload: Uint8Array }>;
}

/** A typed rejection from the host or the collector behind it. */
export class ServiceError extends Error {
  constructor(
    readonly code: string,
    message: string,
    readonly retryable: boolean,
  ) {
    super(message);
    this.name = "ServiceError";
  }
}

/** What one send produced. */
export interface CaptureResult {
  accepted: number;
  /** True only for a critical send that reached the collector's durability
   * boundary. An ordinary capture is best effort and reports false. */
  durable: boolean;
  rejected: { eventId: Uint8Array; code: string; message: string }[];
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

export const defaultSettings: Settings = {
  maxItems: 64,
  lingerMs: 2_000,
  maxBuffered: 512,
  maxBeaconBytes: 32 * 1024,
};

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

/** One buffered item, and whether it sealed the buffer when it arrived. */
interface Held {
  item: TelemetryItem;
  critical: boolean;
}

/**
 * The session lifecycle, which works on every surface including one with no
 * browser storage.
 *
 * The library issues the identifier; a person cannot select it. See D11.
 */
export class Session {
  private constructor(readonly id: string) {}

  static start(): Session {
    const bytes = new Uint8Array(16);
    if (typeof globalThis.crypto?.getRandomValues === "function") {
      globalThis.crypto.getRandomValues(bytes);
    } else {
      for (let i = 0; i < bytes.length; i++) bytes[i] = Math.floor(Math.random() * 256);
    }
    return new Session(
      Array.from(bytes)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join(""),
    );
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
  private readonly settings: Settings;
  private buffer: Held[] = [];
  private openedAt: number | undefined;
  private captured = 0;
  private sent = 0;
  private dropped = 0;
  private lastError: unknown;
  private timer: ReturnType<typeof setInterval> | undefined;
  private flushing = false;

  constructor(
    private readonly router: Router,
    settings: Partial<Settings> = {},
  ) {
    this.settings = { ...defaultSettings, ...settings };
  }

  /** How many items are waiting. */
  get buffered(): number {
    return this.buffer.length;
  }

  /** How many items this client dropped. A rising count is the signal that a
   * browser is producing faster than the host can take, or that the host is
   * refusing what it is given. */
  get droppedCount(): number {
    return this.dropped;
  }

  /** What this client has captured, sent, and dropped. */
  stats(): Stats {
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
  capture(capture: Capture): void {
    const item = capture.item;
    payloadName(item);
    if (this.settings.release !== undefined && item.envelope.release === undefined) {
      item.envelope.release = this.settings.release;
    }

    this.captured += 1;
    this.log(item);
    if (this.openedAt === undefined) this.openedAt = nowMs();
    this.buffer.push({ item, critical: capture.critical });
    this.evictToFit();
  }

  /** Drop until the buffer fits, oldest ordinary item first. */
  private evictToFit(): void {
    const limit = Math.max(1, this.settings.maxBuffered);
    while (this.buffer.length > limit) {
      const ordinary = this.buffer.findIndex((held) => !held.critical);
      this.buffer.splice(ordinary === -1 ? 0 : ordinary, 1);
      this.dropped += 1;
    }
    if (this.buffer.length === 0) this.openedAt = undefined;
  }

  private log(item: TelemetryItem): void {
    const debug = this.settings.debug;
    if (debug === undefined || debug === false) return;
    try {
      if (typeof debug === "function") debug(item);
      else globalThis.console?.debug("tallyowl captured", item);
    } catch {
      // A logger that throws must not cost the host its event.
    }
  }

  /** Whether the buffer has reached a seal condition. */
  get shouldFlush(): boolean {
    if (this.buffer.length === 0) return false;
    if (this.buffer.length >= this.settings.maxItems) return true;
    if (this.buffer.some((held) => held.critical)) return true;
    return this.openedAt !== undefined && nowMs() - this.openedAt >= this.settings.lingerMs;
  }

  /** Take the oldest items, at most one sealed batch of them. */
  private take(limit = Math.max(1, this.settings.maxItems)): Held[] {
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
  private restore(taken: Held[]): void {
    this.buffer = taken.concat(this.buffer);
    if (this.openedAt === undefined && this.buffer.length > 0) this.openedAt = nowMs();
    this.evictToFit();
  }

  /**
   * Send every sealed batch with `send`, and keep what the host did not take.
   *
   * A failure on the carrier and a retryable rejection keep the items for the
   * next flush. A permanent rejection drops them and counts them, because
   * sending the same items again gets the same answer.
   */
  private async drain(
    send: (items: TelemetryItem[]) => Promise<CaptureResult>,
  ): Promise<CaptureResult | undefined> {
    let total: CaptureResult | undefined;
    while (this.buffer.length > 0) {
      const taken = this.take();
      let result: CaptureResult;
      try {
        result = await send(taken.map((held) => held.item));
      } catch (error) {
        if (error instanceof ServiceError && !error.retryable) this.dropped += taken.length;
        else this.restore(taken);
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
  async flush(): Promise<CaptureResult | undefined> {
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
  async flushSafely(): Promise<CaptureResult | undefined> {
    try {
      return await this.flush();
    } catch (error) {
      this.report(error);
      return undefined;
    }
  }

  private report(error: unknown): void {
    this.lastError = error;
    try {
      this.settings.onError?.(error);
    } catch {
      // A failure handler that throws must not reach the host page either.
    }
  }

  /**
   * Flush on a timer whenever a seal condition is reached.
   *
   * This is opt-in. A host that already has a scheduler calls `flushSafely`
   * from it and never calls this. The default period is half of `lingerMs`.
   */
  start(intervalMs?: number): void {
    if (this.timer !== undefined) return;
    const period = Math.max(50, intervalMs ?? this.settings.lingerMs / 2);
    this.timer = setInterval(() => {
      if (this.flushing || !this.shouldFlush) return;
      this.flushing = true;
      void this.flushSafely().finally(() => {
        this.flushing = false;
      });
    }, period);
    // A timer must not keep a process that is not a browser alive.
    (this.timer as { unref?: () => void }).unref?.();
  }

  /** Stop the flush timer. What is buffered stays buffered. */
  stop(): void {
    if (this.timer === undefined) return;
    clearInterval(this.timer);
    this.timer = undefined;
  }

  /**
   * Send everything buffered and wait for the collector's durability boundary.
   *
   * A host chooses whether to expose this. The result reports the durability it
   * actually reached, so a caller never has to assume. See D5.
   */
  async sendCritical(): Promise<CaptureResult | undefined> {
    return this.drain(async (items) => {
      const reply = await this.router.call(
        SERVICE,
        "capture-critical",
        toCaptureCriticalRequestCbor({ items }),
      );
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
  attachUnloadFlush(
    send: UnloadSend,
    target: {
      addEventListener(type: string, listener: () => void): void;
      removeEventListener?(type: string, listener: () => void): void;
      visibilityState?: string;
    },
  ): () => void {
    const onPageHide = () => {
      this.flushOnUnload(send);
    };
    const onVisibility = () => {
      if (target.visibilityState === "hidden") this.flushOnUnload(send);
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
  flushOnUnload(send: UnloadSend): number {
    if (this.buffer.length === 0) return 0;
    const taken = this.take(this.buffer.length);
    const ordered = taken
      .filter((held) => held.critical)
      .concat(taken.filter((held) => !held.critical))
      .map((held) => held.item);

    let handed = 0;
    for (const chunk of this.beaconChunks(ordered)) {
      let took: unknown;
      try {
        took = send(toCaptureRequestCbor({ items: chunk }));
      } catch (error) {
        this.report(error);
        took = false;
      }
      if (took === false) {
        this.dropped += chunk.length;
      } else {
        this.sent += chunk.length;
        handed += chunk.length;
      }
    }
    return handed;
  }

  /** Split items into requests that each stay under `maxBeaconBytes`. An item
   * that is larger than the bound goes alone. */
  private beaconChunks(items: TelemetryItem[]): TelemetryItem[][] {
    const limit = Math.max(1, this.settings.maxBeaconBytes);
    const chunks: TelemetryItem[][] = [];
    let open: TelemetryItem[] = [];
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
    if (open.length > 0) chunks.push(open);
    return chunks;
  }

  private throwIfServiceError(reply: { variant?: string; payload: Uint8Array }): void {
    if (reply.variant !== "ServiceError") return;
    const wire = fromServiceErrorCbor(reply.payload);
    throw new ServiceError(wire.code, wire.message, wire.retryable);
  }
}
