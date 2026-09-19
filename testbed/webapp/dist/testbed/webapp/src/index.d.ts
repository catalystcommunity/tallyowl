import { BrowserClient, Session, type Router } from "../../../packages/browser/src/index.ts";
import { type HostRoutes } from "./router.ts";
export { HostRouter, defaultRoutes, unloadSender } from "./router.ts";
export declare const FUNNEL: readonly ["catalogue-viewed", "seed-added", "checkout-started", "purchase"];
export type FunnelStep = (typeof FUNNEL)[number];
export interface Storefront {
    readonly client: BrowserClient;
    readonly session: Session;
}
export declare function open(router: Router, release?: string): Storefront;
export declare function walk(storefront: Storefront, steps: readonly FunnelStep[]): Promise<void>;
export declare function purchase(storefront: Storefront, orderId: string, value: string, currency: string): Promise<void>;
export declare function fail(storefront: Storefront, errorType: string, message: string): Promise<void>;
export declare function close(storefront: Storefront): Promise<void>;
export declare function start(routes?: HostRoutes): void;
