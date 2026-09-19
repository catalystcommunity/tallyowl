import type { FrameCarrier } from "./carrier.ts";
export declare class RpcRequest {
    service: string;
    op: string;
    id?: number;
    payload: Uint8Array;
    auth?: string;
    constructor(service: string, op: string, payload: Uint8Array);
    withId(id: number): this;
    withAuth(auth: string): this;
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): RpcRequest;
}
export declare class RpcResponse {
    id?: number;
    status: number;
    variant?: string;
    error?: string;
    payload: Uint8Array;
    constructor(status: number, payload: Uint8Array);
    static ok(variant: string, payload: Uint8Array): RpcResponse;
    static transportError(status: number, message: string): RpcResponse;
    withId(id: number | undefined): this;
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): RpcResponse;
    intoTransportError(): RpcResponse;
}
export declare class RpcPush {
    service: string;
    event: string;
    payload: Uint8Array;
    constructor(service: string, event: string, payload: Uint8Array);
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): RpcPush;
}
export type HandlerOutcome = {
    kind: "reply";
    variant: string;
    payload: Uint8Array;
} | {
    kind: "transport";
    status: number;
    message: string;
};
export declare const reply: (variant: string, payload: Uint8Array) => HandlerOutcome;
export declare const transport: (status: number, message: string) => HandlerOutcome;
export declare class RpcClient {
    private nextId;
    private readonly carrier;
    private readonly multiplexed;
    constructor(carrier: FrameCarrier, multiplexed: boolean);
    call(service: string, op: string, payload: Uint8Array, auth?: string): Promise<RpcResponse>;
}
export declare class RpcServer {
    private readonly carrier;
    constructor(carrier: FrameCarrier);
    serveOne(handler: (req: RpcRequest) => HandlerOutcome | Promise<HandlerOutcome>): Promise<boolean>;
}
