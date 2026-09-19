export interface FrameCarrier {
    sendFrame(bytes: Uint8Array): void | Promise<void>;
    recvFrame(): Uint8Array | null | Promise<Uint8Array | null>;
}
export interface DatagramCarrier {
    sendDatagram(bytes: Uint8Array): void | Promise<void>;
    recvDatagram(): Uint8Array | null | Promise<Uint8Array | null>;
}
export declare function frameLengthPrefixed(bytes: Uint8Array, max?: number): Uint8Array;
export declare class LengthPrefixedDeframer {
    private buf;
    private readonly max;
    constructor(max?: number);
    get maxFrame(): number;
    push(chunk: Uint8Array): void;
    next(): Uint8Array | null;
}
export declare class LoopbackFrameCarrier implements FrameCarrier {
    readonly outbound: Uint8Array[];
    readonly inbound: Uint8Array[];
    pushInbound(bytes: Uint8Array): void;
    takeOutbound(): Uint8Array | undefined;
    sendFrame(bytes: Uint8Array): void;
    recvFrame(): Uint8Array | null;
}
export declare class LoopbackDatagramCarrier implements DatagramCarrier {
    readonly outbound: Uint8Array[];
    readonly inbound: Uint8Array[];
    pushInbound(bytes: Uint8Array): void;
    takeOutbound(): Uint8Array | undefined;
    sendDatagram(bytes: Uint8Array): void;
    recvDatagram(): Uint8Array | null;
}
