// The same-origin browser carrier.
//
// `docs/DESIGN.md` section 4.2 gives this hop: "Dashboard browser → head | RPC
// or Events | Same-origin browser carrier | TallyOwl session from LinkKeys
// login". A browser cannot open a TCP socket, so one CSIL-RPC frame travels in
// the body of a `POST` and one comes back.
//
// The envelopes, the codecs, and the correlation IDs are the ones every other
// hop uses. Nothing here parses a query string into a query, and nothing here
// invents a second protocol.

import { RpcRequest, RpcResponse } from "../../../.deps/csilgen/transports/typescript/src/rpc.ts";
import { fromServiceErrorCbor, type ServiceError } from "./control-api.ts";

export const CONTROL_SERVICE = "TallyOwlControl";
const SERVICE_ERROR_VARIANT = "ServiceError";

/// A typed rejection from the head. `retryable` is a fact a caller builds on,
/// so a wrong value produces either a retry storm or a lost action.
export class ControlError extends Error {
  readonly code: string;
  readonly retryable: boolean;

  constructor(error: ServiceError) {
    super(error.message);
    this.name = "ControlError";
    this.code = String(error.code);
    this.retryable = error.retryable;
  }
}

/// A transport failure: the head could not deliver a typed reply at all.
///
/// This is deliberately a different class from `ControlError`. Conflating the
/// two makes a caller retry a permanent rejection, or give up on a reset.
export class TransportFailure extends Error {
  readonly status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "TransportFailure";
    this.status = status;
  }
}

export interface CarrierOptions {
  /// Where the carrier is. Same origin, so a path rather than a URL.
  readonly endpoint?: string;
  /// Injected so a test drives the carrier without a network.
  readonly fetch?: typeof globalThis.fetch;
}

/// One connection to the head's control service.
///
/// The session token is held here rather than passed to each call, because the
/// credential belongs to the connection on every other hop too.
export class Control {
  private readonly endpoint: string;
  private readonly send: typeof globalThis.fetch;
  private nextId = 1;
  private sessionToken: string | undefined;

  constructor(options: CarrierOptions = {}) {
    this.endpoint = options.endpoint ?? "/api/rpc";
    this.send = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /// Present this session on every call.
  withSession(token: string | undefined): this {
    this.sessionToken = token && token.length > 0 ? token : undefined;
    return this;
  }

  session(): string | undefined {
    return this.sessionToken;
  }

  /// Invoke `op` and return the reply's payload bytes.
  ///
  /// A `ServiceError` becomes a thrown `ControlError` and a non-zero transport
  /// status becomes a `TransportFailure`, so a caller can tell a rejected
  /// request from an unreachable head.
  async call(op: string, payload: Uint8Array): Promise<Uint8Array> {
    const request = new RpcRequest(CONTROL_SERVICE, op, payload).withId(this.nextId++);
    const headers: Record<string, string> = { "content-type": "application/cbor" };
    if (this.sessionToken !== undefined) {
      headers["authorization"] = `Bearer ${this.sessionToken}`;
    }

    let http: Response;
    try {
      http = await this.send(this.endpoint, {
        method: "POST",
        headers,
        body: request.encode() as BodyInit,
      });
    } catch (cause) {
      throw new TransportFailure(7, `We could not reach TallyOwl. ${String(cause)}`);
    }

    const body = new Uint8Array(await http.arrayBuffer());
    let response: RpcResponse;
    try {
      response = RpcResponse.decode(body);
    } catch (cause) {
      throw new TransportFailure(
        http.status,
        `TallyOwl sent a reply we could not read. ${String(cause)}`,
      );
    }

    if (response.status !== 0) {
      throw new TransportFailure(response.status, response.error ?? "The request did not arrive.");
    }
    if (response.variant === SERVICE_ERROR_VARIANT) {
      throw new ControlError(fromServiceErrorCbor(response.payload));
    }
    return response.payload;
  }
}
