export type Profile = "verbose" | "compact";
export declare function parseProfile(s: string): Profile | undefined;
export declare const control: {
    readonly HELLO: 0;
    readonly HELLO_ACK: 1;
    readonly PING: 2;
    readonly PONG: 3;
    readonly CLOSE: 4;
    readonly ERROR: 5;
    readonly HELLO_NAME: "$hello";
    readonly HELLO_ACK_NAME: "$hello-ack";
    readonly PING_NAME: "$ping";
    readonly PONG_NAME: "$pong";
    readonly CLOSE_NAME: "$close";
    readonly ERROR_NAME: "$error";
};
export declare class Event {
    service?: string;
    serviceOrd?: number;
    event?: string;
    opOrd?: number;
    id?: number;
    payload: Uint8Array;
    private constructor();
    static verbose(service: string | undefined, event: string, payload: Uint8Array): Event;
    static compact(serviceOrd: number, opOrd: number, payload: Uint8Array): Event;
    withId(id: number): this;
    encode(profile: Profile): Uint8Array;
    private encodeVerbose;
    private encodeCompact;
    static decode(bytes: Uint8Array, profile: Profile): Event;
    private static decodeVerbose;
    private static decodeCompact;
}
export declare class Hello {
    versions: number[];
    profiles: string[];
    service?: string;
    auth?: string;
    constructor(versions: number[], profiles: string[], service?: string, auth?: string);
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): Hello;
    negotiate(supported: Profile[]): [number, Profile] | undefined;
}
export declare class HelloAck {
    v: number;
    profile: string;
    session?: string;
    constructor(v: number, profile: string, session?: string);
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): HelloAck;
}
export declare class Heartbeat {
    nonce: number;
    at?: number;
    constructor(nonce: number, at?: number);
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): Heartbeat;
}
export declare class Close {
    status: number;
    reason?: string;
    constructor(status: number, reason?: string);
    encode(): Uint8Array;
    static decode(bytes: Uint8Array): Close;
}
