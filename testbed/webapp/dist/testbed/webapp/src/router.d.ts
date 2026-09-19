import type { Router } from "../../../packages/browser/src/index.ts";
export interface HostRoutes {
    readonly telemetry: string;
    readonly unload: string;
}
export declare const defaultRoutes: HostRoutes;
export declare class HostRouter implements Router {
    private readonly routes;
    private readonly send;
    private nextId;
    constructor(routes?: HostRoutes, send?: typeof globalThis.fetch);
    call(service: string, op: string, payload: Uint8Array): Promise<{
        variant?: string;
        payload: Uint8Array;
    }>;
}
export declare function unloadSender(routes?: HostRoutes, beacon?: (url: string, payload: BlobPart) => boolean): (payload: Uint8Array) => void;
