// The TallyOwl dashboard.
//
// It shows ingest health and an event chart, which is the Phase 4 deliverable,
// and it owns the sign-in callback route, which is what `linkkeys.callbackUrl`
// needs a server behind.
//
// Everything it knows arrives over the same-origin browser carrier in
// `transport.ts`. It opens no TallyOwl connection of its own, reaches no
// TallyOwl domain, and holds no query text.

export { Control, ControlError, TransportFailure } from "./transport.ts";
export { eventBreakdown, eventTrend, metricNames, metricRate } from "./queries.ts";
export * as view from "./view.ts";
export * as signIn from "./sign-in.ts";

import {
  fromProjectListCbor,
  fromQueryResponseCbor,
  toListRequestCbor,
  toQueryRequestCbor,
} from "./control-api.ts";
import { eventBreakdown, eventTrend, metricNames, metricRate } from "./queries.ts";
import { begin, complete, forgetSession, heldSession, type Storage } from "./sign-in.ts";
import { Control, ControlError, TransportFailure } from "./transport.ts";
import * as view from "./view.ts";

/// How the dashboard is wired to its host page. Every dependency is injected,
/// so a test drives the whole flow without a browser and without a network.
export interface Options {
  readonly control: Control;
  readonly storage: Storage;
  readonly root: HTMLElement;
  readonly location: string;
  readonly callbackUrl: string;
  readonly projectId: Uint8Array;
  /// Injected so a test has a fixed clock. A chart whose range moved between
  /// runs would not be testable.
  readonly now: () => number;
  readonly readiness?: () => Promise<view.HealthReport>;
  /// Which metric the chart draws. The dashboard asks the head which names
  /// exist and draws the first one when the host names none.
  readonly metricName?: string;
}

const HOUR_MS = 3_600_000;

/// Draw the dashboard once.
export async function render(options: Options): Promise<void> {
  const { control, storage, root } = options;
  root.replaceChildren();

  try {
    const signedIn = await complete(control, storage, options.location);
    if (signedIn === undefined) {
      control.withSession(heldSession(storage));
    }
  } catch (cause) {
    // A failed completion is not a reason to show nothing. The rest of the
    // page still draws, and the person is told what to do next.
    root.append(view.failure(readable(cause)));
  }

  if (control.session() === undefined) {
    root.append(signInPrompt(options));
    return;
  }

  if (options.readiness !== undefined) {
    try {
      root.append(view.health(await options.readiness()));
    } catch (cause) {
      root.append(view.failure(readable(cause)));
    }
  }

  const rangeEnd = options.now();
  const range = {
    rangeStart: rangeEnd - 24 * HOUR_MS,
    rangeEnd,
    basis: "occurred_at" as const,
  };

  try {
    const trend = fromQueryResponseCbor(
      await control.call("run-query", toQueryRequestCbor(eventTrend(options.projectId, range, HOUR_MS))),
    );
    root.append(view.chart(view.pointsFrom(trend.columns, trend.rows)));

    const breakdown = fromQueryResponseCbor(
      await control.call(
        "run-query",
        toQueryRequestCbor(eventBreakdown(options.projectId, range, 10)),
      ),
    );
    root.append(view.breakdown(breakdown.columns, breakdown.rows));
  } catch (cause) {
    root.append(view.failure(readable(cause)));
  }

  // The metric chart. It is drawn after the event chart and it fails on its
  // own: a project with no metrics still gets its events.
  try {
    const names = fromQueryResponseCbor(
      await control.call(
        "run-query",
        toQueryRequestCbor(metricNames(options.projectId, range, 20)),
      ),
    );
    const available = view.metricNamesFrom(names.columns, names.rows);
    if (available.length > 0) {
      const chosen = options.metricName !== undefined && available.includes(options.metricName)
        ? options.metricName
        : available[0];
      root.append(
        view.metricChooser(available, chosen, (name) => {
          void render({ ...options, metricName: name });
        }),
      );
      const rate = fromQueryResponseCbor(
        await control.call(
          "run-query",
          toQueryRequestCbor(metricRate(options.projectId, chosen, range, HOUR_MS)),
        ),
      );
      root.append(view.metricChart(chosen, view.ratePointsFrom(rate.columns, rate.rows)));
    }
  } catch (cause) {
    root.append(view.failure(readable(cause)));
  }
}

function signInPrompt(options: Options): HTMLElement {
  const form = document.createElement("form");
  const label = document.createElement("label");
  label.textContent = "Your LinkKeys domain";
  const input = document.createElement("input");
  input.name = "domain";
  input.required = true;
  label.append(input);

  const submit = document.createElement("button");
  submit.type = "submit";
  submit.textContent = "Sign in";
  form.append(label, submit);

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    begin(options.control, options.storage, input.value, options.callbackUrl)
      .then((redirect) => {
        globalThis.location.assign(redirect);
      })
      .catch((cause) => {
        forgetSession(options.storage);
        form.append(view.failure(readable(cause)));
      });
  });
  return form;
}

/// One sentence a person can act on, whatever went wrong.
///
/// A `ControlError` already carries a message written for a person, per
/// CONVENTIONS.md. Everything else gets one here rather than reaching a page as
/// a stack trace.
export function readable(cause: unknown): string {
  if (cause instanceof ControlError) return cause.message;
  if (cause instanceof TransportFailure) {
    return `We could not reach TallyOwl. ${cause.message}`;
  }
  return "Something went wrong. Try again, and tell the person who runs TallyOwl if it keeps happening.";
}

/// The first project this session can read.
///
/// A dashboard that had to be told a project ID could not be opened by a person
/// who has one; the head already knows which projects the session may read, so
/// it is asked rather than configured.
export async function firstProject(control: Control): Promise<Uint8Array | undefined> {
  const payload = await control.call("list-projects", toListRequestCbor({}));
  return fromProjectListCbor(payload).projects[0]?.projectId;
}

/// Start the dashboard against the real page. `boot.ts` calls this.
export async function start(): Promise<void> {
  const root = document.getElementById("dashboard");
  if (root === null) return;

  const control = new Control().withSession(heldSession(globalThis.sessionStorage));
  const options = {
    control,
    storage: globalThis.sessionStorage,
    root,
    location: globalThis.location.href,
    callbackUrl: `${globalThis.location.origin}/sign-in/callback`,
    now: () => Date.now(),
    readiness: async () => {
      const response = await fetch("/api/health");
      return (await response.json()) as view.HealthReport;
    },
  };

  // The project is resolved after any sign-in, because listing projects needs
  // a session.
  await render({ ...options, projectId: new Uint8Array(16) });
  if (control.session() === undefined) return;
  try {
    const projectId = await firstProject(control);
    if (projectId === undefined) {
      root.append(view.failure("This account can read no project yet."));
      return;
    }
    await render({ ...options, projectId });
  } catch (cause) {
    root.append(view.failure(readable(cause)));
  }
}
