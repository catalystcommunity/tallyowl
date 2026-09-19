export declare const MAX_DATAGRAM_DEFAULT = 1200;
export declare class Datagram {
    op_ord: number;
    seq: number;
    payload: Uint8Array;
    constructor(op_ord: number, seq: number, payload: Uint8Array);
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): Datagram;
}
export declare class CompactDatagram {
    op_ord: number;
    seq: number;
    epoch?: number;
    body: Uint8Array;
    constructor(op_ord: number, seq: number, body: Uint8Array);
    withEpoch(epoch: number): this;
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): CompactDatagram;
}
export type SeqEvent = {
    kind: "first";
} | {
    kind: "advanced";
    gap: number;
} | {
    kind: "late-or-duplicate";
} | {
    kind: "restart";
};
export declare const SeqEvent: {
    readonly first: () => SeqEvent;
    readonly advanced: (gap: number) => SeqEvent;
    readonly lateOrDuplicate: () => SeqEvent;
    readonly restart: () => SeqEvent;
};
export declare class SeqTracker {
    private lastSeq?;
    private lastEpoch?;
    observe(seq: number, epoch?: number): SeqEvent;
}
