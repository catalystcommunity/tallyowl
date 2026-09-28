// Opt-in capture of errors nobody handled.
//
// Nothing here runs until a host calls `captureGlobalErrors`. It records the
// error type, its message, and its stack frames, and nothing else: no Document
// Object Model, no request body, and no query string. A frame's file is the
// path of the script only, because a query string can carry a token. See the
// last rule under "Delivery and data" in AGENTS.md.
import { Capture } from "./capture.js";
const MAX_FRAMES = 50;
const MAX_MESSAGE = 1024;
/**
 * Record every `error` and `unhandledrejection` the page raises.
 *
 * Each one is an unhandled error, so it seals the buffer. This function sends
 * nothing itself: the host's flush, or `client.start()`, does that.
 *
 * The result detaches the listeners.
 */
export function captureGlobalErrors(client, options = {}) {
    const target = options.target ?? globalThis;
    if (target === undefined || typeof target.addEventListener !== "function")
        return () => { };
    const origin = options.origin ?? globalThis.location?.origin;
    const record = (thrown, mechanism) => {
        // A failure in here must not raise another `error` event, or one bad
        // report becomes a loop in the host page.
        try {
            const capture = Capture.error(errorPayload(thrown, mechanism, origin));
            const session = typeof options.sessionId === "function" ? options.sessionId() : options.sessionId;
            if (session !== undefined)
                capture.withSession(session);
            client.capture(capture);
        }
        catch {
            // Nothing to do: there is nowhere safer to report it.
        }
    };
    const onError = (event) => {
        const e = event;
        record(e.error ?? e.message, "onerror");
    };
    const onRejection = (event) => {
        record(event.reason, "unhandledrejection");
    };
    target.addEventListener("error", onError);
    target.addEventListener("unhandledrejection", onRejection);
    return () => {
        target.removeEventListener("error", onError);
        target.removeEventListener("unhandledrejection", onRejection);
    };
}
/** Build the payload for one thrown value. Exported for a host that catches an
 * error itself and wants the same frames. */
export function errorPayload(thrown, mechanism, origin) {
    const error = thrown;
    const isError = typeof error === "object" && error !== null;
    const errorType = isError && typeof error.name === "string" && error.name !== "" ? error.name : "Error";
    const message = isError && typeof error.message === "string"
        ? error.message
        : typeof thrown === "string"
            ? thrown
            : "A value that is not an error was thrown.";
    const payload = {
        errorType,
        message: message.slice(0, MAX_MESSAGE),
        handled: false,
        severity: "error",
        mechanism,
        runtime: "browser",
    };
    if (isError && typeof error.stack === "string") {
        const frames = parseStack(error.stack, origin);
        if (frames.length > 0)
            payload.frames = frames;
    }
    return payload;
}
/**
 * Read the two stack formats browsers produce.
 *
 *     at checkout (https://shop.example/assets/app.js?v=3:10:5)
 *     checkout@https://shop.example/assets/app.js?v=3:10:5
 */
export function parseStack(stack, origin) {
    const frames = [];
    for (const raw of stack.split("\n")) {
        const line = raw.trim();
        let name;
        let location;
        const chromium = /^at (?:(.+?) \((.+)\)|(.+))$/.exec(line);
        if (chromium !== null) {
            name = chromium[1];
            location = chromium[2] ?? chromium[3];
        }
        else if (line.includes("@")) {
            const at = line.lastIndexOf("@");
            name = line.slice(0, at) || undefined;
            location = line.slice(at + 1);
        }
        if (location === undefined)
            continue;
        const where = /^(.*?)(?::(\d+))?(?::\d+)?$/.exec(location);
        const url = where?.[1] ?? location;
        const frame = { inApp: true };
        if (name !== undefined)
            frame.function = name;
        const file = scriptPath(url);
        if (file !== undefined)
            frame.file = file;
        if (where?.[2] !== undefined)
            frame.line = Number(where[2]);
        const frameOrigin = /^[a-z][a-z0-9+.-]*:\/\/[^/]+/i.exec(url)?.[0];
        if (origin !== undefined && frameOrigin !== undefined)
            frame.inApp = frameOrigin === origin;
        frames.push(frame);
        if (frames.length >= MAX_FRAMES)
            break;
    }
    return frames;
}
/** The path of a script, with no origin, no query string, and no fragment. */
function scriptPath(url) {
    if (url === "" || url === "<anonymous>" || url === "native")
        return undefined;
    const withoutOrigin = url.replace(/^[a-z][a-z0-9+.-]*:\/\/[^/]+/i, "");
    const path = withoutOrigin.split(/[?#]/)[0];
    return path === "" ? undefined : path;
}
