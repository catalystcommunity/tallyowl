// seedstore's web application.
//
// It is one of the reference application's client surfaces. Its job in the test
// bed is to produce the browser half of the telemetry the ledger predicts:
// page views, a funnel that some sessions complete, a conversion, and an
// unhandled error.
//
// Every event goes to seedstore's own backend, which holds the one TallyOwl
// credential. This surface never learns a TallyOwl address, never learns a
// workspace or a project, and holds no credential.
import { BrowserClient, Capture, Session, defaultSettings, decimal, text, } from "../../../packages/browser/src/index.js";
import { HostRouter, defaultRoutes, unloadSender } from "./router.js";
export { HostRouter, defaultRoutes, unloadSender } from "./router.js";
/// One step of the seedstore funnel. The ledger names these exactly, so the
/// names live in one place and both sides read them.
export const FUNNEL = ["catalogue-viewed", "seed-added", "checkout-started", "purchase"];
/// Open a seedstore session and record the entry page view.
export function open(router, release = "seedstore-0.0.0") {
    const client = new BrowserClient(router, { ...defaultSettings, release });
    // The client library issues the session ID; a person cannot select it. D11.
    const session = Session.start();
    client.capture(Capture.sessionStart(session.id, "/"));
    client.capture(Capture.pageView("/").withSession(session.id));
    return { client, session };
}
/// Walk `steps` of the funnel, in order.
///
/// The caller decides how far a session gets, because the ledger is what
/// decides how many sessions complete and this surface must not invent one.
export async function walk(storefront, steps) {
    for (const step of steps) {
        storefront.client.capture(Capture.event(step)
            .withSession(storefront.session.id)
            .withProperty("surface", text("web")));
    }
    await storefront.client.flush();
}
/// Record a purchase. Money travels as an exact decimal and never as a float.
export async function purchase(storefront, orderId, value, currency) {
    // A conversion is critical, so it seals the buffer and `sendCritical` waits
    // for the collector's durability boundary rather than riding the ordinary
    // best-effort path.
    storefront.client.capture(Capture.conversion("purchase", { value: decimalOf(value), currency, orderId })
        .withSession(storefront.session.id)
        .asCritical());
    await storefront.client.sendCritical();
}
/// Record an unhandled browser error.
///
/// Phase 5's exit criterion needs a browser error that correlates to a session
/// and a release. The producer never supplies a group; the projector computes
/// the fingerprint. See D39.
export async function fail(storefront, errorType, message) {
    storefront.client.capture(Capture.error({ errorType, message, handled: false, severity: "fatal" })
        .withSession(storefront.session.id)
        .asCritical());
    await storefront.client.sendCritical();
}
/// The exact decimal a conversion carries.
///
/// Money travels as an exact decimal and never as a float, so the value arrives
/// as its canonical text and is refused here if it is not a number.
function decimalOf(value) {
    const parsed = decimal(value);
    if (parsed.kind !== "decimal") {
        throw new Error(`\`${value}\` is not a number.`);
    }
    return parsed.value;
}
/// Close the session and flush.
export async function close(storefront) {
    storefront.client.capture(Capture.sessionEnd(storefront.session.id, "explicit"));
    await storefront.client.flush();
}
/// Wire seedstore to the real page.
export function start(routes = defaultRoutes) {
    const root = document.getElementById("seedstore");
    if (root === null)
        return;
    const storefront = open(new HostRouter(routes));
    // The unload flush reaches seedstore's own route, never a TallyOwl address.
    storefront.client.attachUnloadFlush(unloadSender(routes), globalThis.document);
    root.replaceChildren();
    for (const step of FUNNEL) {
        const button = document.createElement("button");
        button.textContent = step;
        button.addEventListener("click", () => {
            void walk(storefront, [step]);
        });
        root.append(button);
    }
}
