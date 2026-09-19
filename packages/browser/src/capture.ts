// Building one telemetry item.
//
// `envelope.kind` selects the payload field, and exactly one payload field is
// present. Every constructor here sets both together, so the two cannot
// disagree. An earlier draft made the payload a choice of the fifteen record
// types, and a TypeScript caller encoded a page view, an error, and a span all
// as the first arm. See docs/IMPLEMENTATION_LOG.md L017.

import type {
  ConversionPayload,
  Envelope,
  ErrorPayload,
  EventPayload,
  InteractionPayload,
  PageViewPayload,
  TelemetryItem,
  TelemetryKind,
} from "./collector-api.ts";

import { property, write, type Value } from "./value.ts";

export const SDK_NAME = "tallyowl-browser";
export const SDK_VERSION = "0.2.0";

/** Milliseconds since the Unix epoch. Every time TallyOwl stores is this. */
export const nowMs = (): number => Date.now();

/**
 * A UUIDv7: a millisecond timestamp followed by random bytes, so an identifier
 * sorts by time and does not repeat inside one millisecond.
 *
 * A browser-supplied identifier is untrusted. The app driver namespaces or
 * replaces it before it seals a batch, and a server-generated one is
 * authoritative when both exist. See DELIVERY.md section 2.
 */
export function newEventId(): Uint8Array {
  const out = new Uint8Array(16);
  const ms = nowMs();
  for (let i = 0; i < 6; i++) {
    out[i] = Math.floor(ms / 2 ** (8 * (5 - i))) & 0xff;
  }
  const random = out.subarray(6);
  if (typeof globalThis.crypto?.getRandomValues === "function") {
    globalThis.crypto.getRandomValues(random);
  } else {
    for (let i = 0; i < random.length; i++) random[i] = Math.floor(Math.random() * 256);
  }
  out[6] = (out[6] & 0x0f) | 0x70;
  out[8] = (out[8] & 0x3f) | 0x80;
  return out;
}

function newEnvelope(kind: TelemetryKind): Envelope {
  return {
    eventId: newEventId(),
    kind,
    schemaVersion: 1,
    occurredAt: nowMs(),
    // The collector stamps the receive time and the tenancy. A client that set
    // them would be claiming something it cannot know, and the collector
    // discards a payload value anyway.
    sdkName: SDK_NAME,
    sdkVersion: SDK_VERSION,
    properties: [],
  };
}

/** One item, before the host sends it. */
export class Capture {
  readonly item: TelemetryItem;
  critical = false;

  private constructor(item: TelemetryItem) {
    this.item = item;
  }

  /** A named product or behavior event. */
  static event(name: string, payload: Partial<EventPayload> = {}): Capture {
    return new Capture({ envelope: newEnvelope("event"), event: { ...payload, name } });
  }

  /** A page view or a screen view. */
  static pageView(route: string, payload: Partial<PageViewPayload> = {}): Capture {
    return new Capture({ envelope: newEnvelope("page-view"), pageView: { ...payload, route } });
  }

  /** A semantic interaction. TallyOwl records the name of what happened, never
   * a Document Object Model selector, and it does not do session replay. */
  static interaction(target: string, action: string): Capture {
    const payload: InteractionPayload = { target, action };
    return new Capture({ envelope: newEnvelope("interaction"), interaction: payload });
  }

  /** The start of a session. */
  static sessionStart(sessionId: string, entryRoute?: string): Capture {
    const capture = new Capture({
      envelope: newEnvelope("session-start"),
      sessionStart: entryRoute === undefined ? {} : { entryRoute },
    });
    return capture.withSession(sessionId);
  }

  /** The end of a session, and why it ended. */
  static sessionEnd(
    sessionId: string,
    reason: "explicit" | "timeout" | "maximum-lifetime",
  ): Capture {
    const capture = new Capture({
      envelope: newEnvelope("session-end"),
      sessionEnd: { reason },
    });
    return capture.withSession(sessionId);
  }

  /** A heartbeat carries no payload beyond its envelope. */
  static sessionHeartbeat(sessionId: string): Capture {
    return new Capture({ envelope: newEnvelope("session-heartbeat") }).withSession(sessionId);
  }

  /** A conversion. Money travels as an exact decimal and never as a float. */
  static conversion(goal: string, payload: Partial<ConversionPayload> = {}): Capture {
    const capture = new Capture({
      envelope: newEnvelope("conversion"),
      conversion: { ...payload, goal },
    });
    // A conversion is the first priority class in DELIVERY.md section 8.
    capture.critical = true;
    return capture;
  }

  /** An error occurrence. The producer never supplies a group; the projector
   * computes the fingerprint. See D39. */
  static error(payload: ErrorPayload): Capture {
    const capture = new Capture({ envelope: newEnvelope("error"), error: payload });
    capture.critical = !payload.handled;
    return capture;
  }

  /** Seal the current batch as soon as this item enters it. */
  asCritical(): this {
    this.critical = true;
    return this;
  }

  at(occurredAt: number): this {
    this.item.envelope.occurredAt = occurredAt;
    return this;
  }

  withEventId(eventId: Uint8Array): this {
    this.item.envelope.eventId = eventId;
    return this;
  }

  withSession(sessionId: string): this {
    this.item.envelope.sessionId = sessionId;
    return this;
  }

  withRequest(requestId: string): this {
    this.item.envelope.requestId = requestId;
    return this;
  }

  withRelease(release: string): this {
    this.item.envelope.release = release;
    return this;
  }

  /** A typed property from the calling code, at the event site. */
  withProperty(key: string, value: Value): this {
    this.item.envelope.properties.push(property(key, value, "client"));
    return this;
  }

  /** The anonymous identifier the host gave this browser. It is opaque and has
   * project scope. */
  withAnonymousId(anonymousId: string): this {
    this.item.envelope.anonymousId = anonymousId;
    return this;
  }

  get eventId(): Uint8Array {
    return this.item.envelope.eventId;
  }
}

/** Which payload field an item carries. */
export function payloadName(item: TelemetryItem): string {
  const present: string[] = [];
  const check = (name: string, value: unknown) => {
    if (value !== undefined) present.push(name);
  };
  check("event", item.event);
  check("page-view", item.pageView);
  check("session-start", item.sessionStart);
  check("session-end", item.sessionEnd);
  check("interaction", item.interaction);
  check("feature-exposure", item.featureExposure);
  check("identify", item.identify);
  check("alias", item.alias);
  check("group", item.group);
  check("conversion", item.conversion);
  check("error", item.error);
  check("span", item.span);
  check("metric-point", item.metricPoint);
  check("campaign-touch", item.campaignTouch);
  check("campaign-cost", item.campaignCost);

  const declared = item.envelope.kind;
  if (present.length > 1) {
    throw new Error(
      `One item described itself as ${present.join(" and ")} at the same time. An item holds one of them.`,
    );
  }
  if (present.length === 0) {
    if (declared === "session-heartbeat") return declared;
    throw new Error(`One item says it is ${declared} and carries no ${declared} details.`);
  }
  if (present[0] !== declared) {
    throw new Error(
      `One item says it is ${declared} and carries ${present[0]} details instead.`,
    );
  }
  return present[0];
}

export { write };
