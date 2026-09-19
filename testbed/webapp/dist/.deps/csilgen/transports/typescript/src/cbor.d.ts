export type Cbor = {
    readonly t: "int";
    readonly v: number;
} | {
    readonly t: "bytes";
    readonly v: Uint8Array;
} | {
    readonly t: "text";
    readonly v: string;
} | {
    readonly t: "array";
    readonly v: Cbor[];
} | {
    readonly t: "map";
    readonly v: ReadonlyArray<readonly [Cbor, Cbor]>;
} | {
    readonly t: "tag";
    readonly tag: number;
    readonly v: Cbor;
};
export declare const int: (v: number) => Cbor;
export declare const bytes: (v: Uint8Array) => Cbor;
export declare const text: (v: string) => Cbor;
export declare const array: (v: Cbor[]) => Cbor;
export declare const map: (v: ReadonlyArray<readonly [Cbor, Cbor]>) => Cbor;
export declare const tag: (tagNum: number, v: Cbor) => Cbor;
export declare function encode(value: Cbor): Uint8Array;
export declare function compareBytes(a: Uint8Array, b: Uint8Array): number;
export declare function decode(data: Uint8Array): Cbor;
