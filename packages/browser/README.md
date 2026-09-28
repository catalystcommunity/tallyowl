# TallyOwl browser package

This package records application events in a browser. The events are page
views, product events, sessions, conversions, and errors.

The package does not record the Document Object Model. It does not do session
replay.

## The integration rule

The browser package never contacts TallyOwl. It has no address setting, no
socket, and no request to a TallyOwl domain.

Your application gives the package one `Router`. The `Router` sends encoded
bytes on the same-origin CSIL connection that your application already has.
Your backend holds the TallyOwl credential. Your backend sends the items to a
collector with an app driver.

The browser does not know a TallyOwl address, a workspace, a project, or a
credential.

## Install

```sh
npm install @catalystcommunity/tallyowl-browser
```

The package is on npmjs. It has no runtime dependencies. It needs an
ECMAScript 2022 browser.

## Send the first event

```ts
import { BrowserClient, Capture, Session, text } from "@catalystcommunity/tallyowl-browser";
import type { Router } from "@catalystcommunity/tallyowl-browser";

// Your application's own CSIL connection. Replace the body with your carrier.
const router: Router = {
  async call(service, op, payload) {
    const reply = await myCarrier.call(service, op, payload);
    return { variant: reply.variant, payload: reply.payload };
  },
};

const client = new BrowserClient(router, { release: "shop-1.4.0" });
client.start();
client.attachUnloadFlush(
  (payload) => navigator.sendBeacon("/telemetry/unload", new Blob([payload])),
  document,
);

const session = Session.start();
client.capture(Capture.sessionStart(session.id, "/"));
client.capture(Capture.pageView("/pricing").withSession(session.id));
client.capture(
  Capture.event("seed-added").withSession(session.id).withProperty("surface", text("web")),
);
```

`testbed/webapp/src/router.ts` in the TallyOwl repository is a complete
`Router` over an HTTP route.

## What your backend must supply

1. Supply the `TallyOwlIngest` service on your CSIL connection. The
   operations are `capture` and `capture-critical`. The backend gives each
   item to its app driver.
2. Supply one same-origin route for the unload flush. This is a requirement
   of decision D34: "The host application must expose one same-origin route
   for the flush." The route receives one encoded `CaptureRequest` as the
   request body. The route is a route of your application. It is not a
   TallyOwl address.

The unload flush is best effort. The package does not claim durable delivery
after a tab closes.

## When the package sends

`capture` puts one item in a buffer. `capture` sends nothing.

The buffer reaches a seal condition when one of these is true:

- The buffer holds `maxItems` items.
- The oldest item is older than `lingerMs`.
- The buffer holds a critical item. A conversion and an unhandled error are
  critical. `asCritical()` makes any item critical.

Select one of these procedures to send the buffer:

- Call `client.start()` one time. A timer then calls `flushSafely()` when a
  seal condition is true. `client.stop()` stops the timer. The default period
  is half of `lingerMs`.
- Call `client.flushSafely()` from your own scheduler or event handler.
  `flushSafely()` never rejects.
- Call `await client.flush()` when you want the result. `flush()` rejects
  when the host does not take the items.
- Call `await client.sendCritical()` when you must know that the collector
  accepted the items durably. Read `durable` in the result.

Each request holds a maximum of `maxItems` items.

## What the package drops, and how you see it

The package never drops an item without a count.

| Condition | Result |
| --- | --- |
| The host connection fails | The items stay in the buffer. The next flush sends them again. |
| The host returns a rejection with `retryable: true` | The items stay in the buffer. |
| The host returns a rejection with `retryable: false` | The package drops the items and counts them. |
| The buffer holds `maxBuffered` items | The package drops the oldest ordinary item and counts it. It drops a critical item only when no ordinary item remains. |
| `sendBeacon` returns `false` during the unload flush | The package counts the items of that request as dropped. |

Read the counts with `client.stats()`:

| Field | Meaning |
| --- | --- |
| `captured` | Items that `capture` accepted |
| `sent` | Items that the host took |
| `dropped` | Items that the package dropped |
| `buffered` | Items in the buffer now |
| `lastError` | The most recent failure. A successful send clears it. |

Set `onError` to receive each failure from `flushSafely()`, from the timer,
and from the unload flush.

Set `debug: true` to write each captured item to the console. Set `debug` to
a function to receive each item. The `debug` setting only logs. It sends
nothing.

## The unload flush

`attachUnloadFlush(send, document)` sends the buffer on `pagehide` and when
`visibilitychange` reports `hidden`. It returns a function that removes the
listeners.

`send` must return the result of `sendBeacon`. A browser refuses a beacon
above its quota. Thus the package sends the buffer as several requests of
`maxBeaconBytes` or less. It sends critical items first.

## Errors that the application did not handle

```ts
import { captureGlobalErrors } from "@catalystcommunity/tallyowl-browser";

const detach = captureGlobalErrors(client, { sessionId: () => session.id });
```

This function is optional. It records the `error` event and the
`unhandledrejection` event. It records the error type, the message, and the
stack frames. A frame holds the path of the script. A frame does not hold the
origin, the query string, or the fragment. The function records no element, no
request, and no URL of the page.

## Join a browser event to a backend trace

```ts
Capture.event("checkout-started").withTrace(response.headers.get("traceparent") ?? "");
```

`withTrace` accepts a `traceparent` value, 32 hexadecimal characters, or 16
bytes. If it cannot read the value, the item has no trace.

## Defaults

| Setting | Default | Meaning |
| --- | --- | --- |
| `maxItems` | 64 | Items in one request, and a seal condition |
| `lingerMs` | 2000 | Age of the oldest item that seals the buffer |
| `maxBuffered` | 512 | Maximum items in the buffer |
| `maxBeaconBytes` | 32768 | Maximum encoded bytes in one unload request |
| `release` | none | Release name on each item |
| `onError` | none | Receives each failure that does not reject |
| `debug` | off | Logs each captured item |

## Privacy

The package records only what your code gives it. Do not put a secret, a
credential, a request body, or raw personal data in an event or a property.
