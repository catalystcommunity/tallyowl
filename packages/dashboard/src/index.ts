// The TallyOwl dashboard.
//
// It shows ingest health, an event chart, the top events, and a metric chart for
// one project at a time, and it owns the sign-in callback route, which is what `linkkeys.callbackUrl`
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
  rememberLoginName,
  rememberedLoginName,
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
  /// Storage that outlives the tab, for the login name a person asked to be
  /// remembered. Absent, nothing is remembered.
  readonly memory?: Storage;
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

/// The project this tab chose last. It is a choice, not a credential, and it
/// ends with the tab like the session does.
const PROJECT_KEY = "tallyowl.project";

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
    const { prompt, focus } = signInPrompt(options, options.notice);
    root.append(prompt);
    focusSoon(focus);
    return;
  }

  // The project comes first, and nothing is asked about a project until there
  // is one. The page used to draw once against a project ID of all zeros, and
  // every load put three failed queries in the head's log.
  let projects: view.ProjectChoice[];
  try {
    projects = await listProjects(control);
  } catch (cause) {
    if (sessionEnded(cause)) return signInAgain(options);
    root.append(view.failure(readable(cause)));
    return;
  }
  const projectId = chooseProject(projects, options.projectId, options.storage);
  if (projectId === undefined) {
    root.append(
      view.failure(
        "This account can read no project yet. Ask the person who runs TallyOwl to give you a role in a workspace.",
      ),
    );
    return;
  }
  const drawn = { ...options, projectId };

  let report: view.HealthReport | undefined;
  let healthFailure: unknown;
  if (options.readiness !== undefined) {
    try {
      report = await options.readiness();
    } catch (cause) {
      healthFailure = cause;
    }
  }

  root.append(
    view.topBar({
      projects,
      selected: projectId,
      onProject: (chosen) => {
        options.storage.setItem(PROJECT_KEY, view.projectKey(chosen));
        void render({ ...options, projectId: chosen, metricName: undefined });
      },
      onSignOut: () => {
        forgetSession(options.storage);
        control.withSession(undefined);
        void render({ ...options, projectId: undefined, notice: undefined });
      },
    }),
  );
  const content = document.createElement("div");
  content.className = "content";
  root.append(content);
  if (healthFailure !== undefined) content.append(view.failure(readable(healthFailure)));
  const detail = report === undefined ? undefined : view.healthDetail(report);
  if (detail !== undefined) content.append(detail);

  const rangeEnd = options.now();
  const range = {
    rangeStart: rangeEnd - 24 * HOUR_MS,
    rangeEnd,
    basis: "occurred_at" as const,
  };
  const drawnRange = { start: range.rangeStart, end: range.rangeEnd, bucketMs: HOUR_MS };
  // The status bar is the last thing on the page, whatever the panels do.
  root.append(view.statusBar(report, drawnRange, rangeEnd));

  try {
    const trend = fromQueryResponseCbor(
      await control.call("run-query", toQueryRequestCbor(eventTrend(projectId, range, HOUR_MS))),
    );
    content.append(view.eventPanel(view.pointsFrom(trend.columns, trend.rows), drawnRange));
  } catch (cause) {
    if (sessionEnded(cause)) return signInAgain(options);
    content.append(view.failure(readable(cause)));
  }

  const lower = document.createElement("div");
  lower.className = "columns";
  content.append(lower);

  try {
    const breakdown = fromQueryResponseCbor(
      await control.call(
        "run-query",
        toQueryRequestCbor(eventBreakdown(projectId, range, 10)),
      ),
    );
    lower.append(view.breakdown(breakdown.columns, breakdown.rows));
  } catch (cause) {
    if (sessionEnded(cause)) return signInAgain(options);
    lower.append(view.failure(readable(cause)));
  }

  // The metric chart. It is drawn after the events and it fails on its own: a
  // project with no metrics still gets its events.
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
      const rate = fromQueryResponseCbor(
        await control.call(
          "run-query",
          toQueryRequestCbor(metricRate(projectId, chosen, range, HOUR_MS)),
        ),
      );
      lower.append(
        view.metricPanel(available, chosen, view.ratePointsFrom(rate.columns, rate.rows), drawnRange, (name) => {
          void render({ ...drawn, metricName: name });
        }),
      );
    }
  } catch (cause) {
    if (sessionEnded(cause)) return signInAgain(options);
    lower.append(view.failure(readable(cause)));
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

function signInPrompt(options: Options, notice?: string): { prompt: HTMLElement; focus?: HTMLElement } {
  const card = document.createElement("div");
  card.className = "sign-in-card";

  const mark = document.createElement("img");
  mark.className = "mark";
  mark.setAttribute("src", view.OWL);
  mark.setAttribute("alt", "");
  const heading = document.createElement("h1");
  heading.textContent = "Sign in to TallyOwl";
  card.append(mark, heading);
  if (notice !== undefined) card.append(view.failure(notice));

  // A person signs in with their login, such as `you@example.com`. The head
  // takes the domain out of it and sends the name on to the domain's sign-in
  // page, so the name is typed once. A remembered name fills the field and
  // "Sign in" has the focus, so signing in again is one press.
  const remembered = rememberedLoginName(options.memory);
  const form = document.createElement("form");
  const login = zest("text-field", {
    label: "LinkKeys login name",
    name: "login",
    required: "",
    autocomplete: "username",
    placeholder: "you@example.com",
    value: remembered,
  });
  // The attribute is the value a form reset goes back to, and the property is
  // the value now. A defined Zest field reads both the same way.
  (login as HTMLElement & { value: string }).value = remembered;
  // On unless the person turned it off: remembering is what saves the typing.
  const keep = zest("check-box", { name: "remember" }, "Remember my login name");
  const keepOn = remembered.length > 0 || options.memory !== undefined;
  if (keepOn) keep.setAttribute("checked", "");
  (keep as HTMLElement & { checked: boolean }).checked = keepOn;
  const submit = zest("push-button", { type: "submit", variant: "primary" }, "Sign in");
  if (remembered.length > 0) submit.setAttribute("autofocus", "");
  form.append(login, keep, submit);

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const name = valueOf(login).trim();
    rememberLoginName(options.memory, name, (keep as HTMLElement & { checked?: boolean }).checked === true);
    begin(options.control, options.storage, name, options.callbackUrl)
      .then((redirect) => {
        globalThis.location.assign(redirect);
      })
      .catch((cause) => {
        forgetSession(options.storage);
        form.append(view.failure(readable(cause)));
      });
  });

  const other = zest("group-box", { label: "Use a session token instead", collapsible: "", collapsed: "" });
  other.className = "other-way";
  other.append(sessionTokenForm(options));
  card.append(form, other);

  const prompt = document.createElement("section");
  prompt.className = "sign-in";
  prompt.append(card);
  return { prompt, focus: remembered.length > 0 ? submit : login };
}

/// The way in for an installation with no LinkKeys domain.
///
/// `linkkeys.enabled` is `false` by default, and `tallyowl-head session create`
/// then prints a session token. The page had no field that took one, so the
/// only way to use it was to write `sessionStorage` by hand in the browser's
/// developer tools.
function sessionTokenForm(options: Options): HTMLElement {
  const form = document.createElement("form");
  // A token is a credential, so it is not shown as it is typed and the browser
  // is asked not to remember it.
  const token = zest("text-field", {
    label: "Session token",
    name: "session-token",
    type: "password",
    autocomplete: "off",
    required: "",
    helper: "From `tallyowl-head session create`. It starts with `tos_`.",
  });
  const submit = zest("push-button", { type: "submit" }, "Use this token");
  form.append(token, submit);

  form.addEventListener("submit", (event) => {
    event.preventDefault();
    const typed = valueOf(token);
    if (!isSessionToken(typed)) {
      form.append(
        view.failure(
          "That is not a session token. A session token starts with `tos_`. A key that starts with `tow_` belongs to an application and cannot sign a person in.",
        ),
      );
      return;
    }
    holdSession(options.control, options.storage, typed);
    void render({ ...options, notice: undefined });
  });
  return form;
}

/// A Zest component with its attributes, and its text when it has some.
function zest(tag: string, attributes: Record<string, string>, text?: string): HTMLElement {
  const made = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) made.setAttribute(name, value);
  if (text !== undefined) made.textContent = text;
  return made;
}

/// The current value of a Zest field. It is a property, as on a native input.
function valueOf(field: HTMLElement): string {
  return (field as HTMLElement & { value?: string }).value ?? "";
}

/// Put the focus on a control after the page shows it. Zest builds a control's
/// inner input when it connects, so the focus waits for the next frame.
function focusSoon(target: HTMLElement | undefined): void {
  if (target === undefined || typeof requestAnimationFrame === "undefined") return;
  requestAnimationFrame(() => target.focus());
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

/// The projects this session can read.
///
/// A dashboard that had to be told a project ID could not be opened by a person
/// who has one; the head already knows which projects the session may read, so
/// it is asked rather than configured.
export async function listProjects(control: Control): Promise<view.ProjectChoice[]> {
  const payload = await control.call("list-projects", toListRequestCbor({}));
  return fromProjectListCbor(payload).projects.map((project) => ({
    projectId: project.projectId,
    name: project.name,
  }));
}

/// The first project this session can read.
export async function firstProject(control: Control): Promise<Uint8Array | undefined> {
  return (await listProjects(control))[0]?.projectId;
}

/// The project to draw: the one the host named, then the one this tab chose
/// last, then the first. A choice that names a project this session can no
/// longer read is passed over rather than queried.
function chooseProject(
  projects: readonly view.ProjectChoice[],
  named: Uint8Array | undefined,
  storage: Storage,
): Uint8Array | undefined {
  const keys = projects.map((project) => view.projectKey(project.projectId));
  for (const wanted of [named === undefined ? null : view.projectKey(named), storage.getItem(PROJECT_KEY)]) {
    const found = wanted === null ? -1 : keys.indexOf(wanted);
    if (found !== -1) return projects[found].projectId;
  }
  return projects[0]?.projectId;
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

/// The browser's `localStorage`, or nothing when the browser refuses it.
function longLived(): Storage | undefined {
  try {
    return globalThis.localStorage;
  } catch {
    return undefined;
  }
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
    memory: longLived(),
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
