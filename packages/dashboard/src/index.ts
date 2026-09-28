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
import {
  begin,
  complete,
  forgetSession,
  heldSession,
  holdSession,
  isSessionToken,
  type Storage,
} from "./sign-in.ts";
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
  /// The project to draw. When the host names none, the dashboard asks the head
  /// which projects this session can read and draws the first.
  readonly projectId?: Uint8Array;
  /// Said above the sign-in prompt. A session that ended says so here.
  readonly notice?: string;
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
    if (options.notice !== undefined) root.append(view.failure(options.notice));
    root.append(signInPrompt(options));
    return;
  }

  // The project comes first, and nothing is asked about a project until there
  // is one. The page used to draw once against a project ID of all zeros, and
  // every load put three failed queries in the head's log.
  let projectId = options.projectId;
  if (projectId === undefined) {
    try {
      projectId = await firstProject(control);
    } catch (cause) {
      if (sessionEnded(cause)) return signInAgain(options);
      root.append(view.failure(readable(cause)));
      return;
    }
    if (projectId === undefined) {
      root.append(
        view.failure(
          "This account can read no project yet. Ask the person who runs TallyOwl to give you a role in a workspace.",
        ),
      );
      return;
    }
  }
  const drawn = { ...options, projectId };

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
      await control.call("run-query", toQueryRequestCbor(eventTrend(projectId, range, HOUR_MS))),
    );
    root.append(view.chart(view.pointsFrom(trend.columns, trend.rows)));

    const breakdown = fromQueryResponseCbor(
      await control.call(
        "run-query",
        toQueryRequestCbor(eventBreakdown(projectId, range, 10)),
      ),
    );
    root.append(view.breakdown(breakdown.columns, breakdown.rows));
  } catch (cause) {
    if (sessionEnded(cause)) return signInAgain(options);
    root.append(view.failure(readable(cause)));
  }

  // The metric chart. It is drawn after the event chart and it fails on its
  // own: a project with no metrics still gets its events.
  try {
    const names = fromQueryResponseCbor(
      await control.call(
        "run-query",
        toQueryRequestCbor(metricNames(projectId, range, 20)),
      ),
    );
    const available = view.metricNamesFrom(names.columns, names.rows);
    if (available.length > 0) {
      const chosen = options.metricName !== undefined && available.includes(options.metricName)
        ? options.metricName
        : available[0];
      root.append(
        view.metricChooser(available, chosen, (name) => {
          void render({ ...drawn, metricName: name });
        }),
      );
      const rate = fromQueryResponseCbor(
        await control.call(
          "run-query",
          toQueryRequestCbor(metricRate(projectId, chosen, range, HOUR_MS)),
        ),
      );
      root.append(view.metricChart(chosen, view.ratePointsFrom(rate.columns, rate.rows)));
    }
  } catch (cause) {
    if (sessionEnded(cause)) return signInAgain(options);
    root.append(view.failure(readable(cause)));
  }
}

/// Whether the head refused the session this tab holds.
function sessionEnded(cause: unknown): boolean {
  return cause instanceof ControlError && cause.code === "unauthenticated";
}

/// Forget a session the head no longer accepts, and offer the sign-in again.
///
/// The session used to stay in the tab. Every panel then said "This credential
/// is not valid. Ask the person who runs TallyOwl for a new one", which is
/// written for an application key, and the sign-in form never came back until
/// the tab closed. The person only has to sign in again, so that is what the
/// page says and offers.
function signInAgain(options: Options): Promise<void> {
  forgetSession(options.storage);
  options.control.withSession(undefined);
  return render({
    ...options,
    projectId: undefined,
    notice: "Your session ended. Sign in again.",
  });
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

  const prompt = document.createElement("section");
  prompt.append(form, sessionTokenForm(options));
  return prompt;
}

/// The way in for an installation with no LinkKeys domain.
///
/// `linkkeys.enabled` is `false` by default, and `tallyowl-head session create`
/// then prints a session token. The page had no field that took one, so the
/// only way to use it was to write `sessionStorage` by hand in the browser's
/// developer tools.
function sessionTokenForm(options: Options): HTMLElement {
  const form = document.createElement("form");
  const label = document.createElement("label");
  label.textContent = "Or paste a session token from `tallyowl-head session create`";
  const input = document.createElement("input");
  input.name = "session-token";
  // A token is a credential, so it is not shown as it is typed and the browser
  // is asked not to remember it.
  input.type = "password";
  input.autocomplete = "off";
  input.required = true;
  label.append(input);

  const submit = document.createElement("button");
  submit.type = "submit";
  submit.textContent = "Use this token";
  form.append(label, submit);

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    if (!isSessionToken(input.value)) {
      form.append(
        view.failure(
          "That is not a session token. A session token starts with `tos_`. A key that starts with `tow_` belongs to an application and cannot sign a person in.",
        ),
      );
      return;
    }
    holdSession(options.control, options.storage, input.value);
    void render({ ...options, notice: undefined });
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

/// Where a sign-in returns to, as the head serves it.
///
/// The head writes `dashboard.callbackPath` into the document. The page used to
/// assume `/sign-in/callback`, and an installation that configured another
/// path had every sign-in refused with "That is not this installation's
/// sign-in address".
export function callbackPath(read: (name: string) => string | null | undefined): string {
  const configured = read("tallyowl-callback-path");
  return configured !== null && configured !== undefined && configured.startsWith("/")
    ? configured
    : "/sign-in/callback";
}

/// Start the dashboard against the real page. `boot.ts` calls this.
export async function start(): Promise<void> {
  const root = document.getElementById("dashboard");
  if (root === null) return;

  const control = new Control().withSession(heldSession(globalThis.sessionStorage));
  const path = callbackPath((name) =>
    document.querySelector(`meta[name="${name}"]`)?.getAttribute("content"),
  );
  await render({
    control,
    storage: globalThis.sessionStorage,
    root,
    location: globalThis.location.href,
    callbackUrl: `${globalThis.location.origin}${path}`,
    now: () => Date.now(),
    readiness: async () => {
      const response = await fetch("/api/health");
      return (await response.json()) as view.HealthReport;
    },
  });
}
