// What the dashboard draws.
//
// Every function here takes data and returns an element. None of them fetches,
// and none of them holds state, so a test drives a view without a network and
// without a browser.
//
// The chart is inline SVG. A charting library would be a dependency on the
// always-on dashboard path, and a count for each bucket needs a polyline.

import { type ResultRow, type TypedValue } from "./control-api.ts";

/// One point of the trend: the bucket's start, and how many events it holds.
export interface Point {
  readonly at: number;
  readonly count: number;
}

/// Read a typed value as a number, or nothing when it is not one.
///
/// A value that is not a number is not coerced. A chart that drew a zero for a
/// value it could not read would be inventing data.
export function asNumber(value: TypedValue | undefined): number | undefined {
  if (value === undefined) return undefined;
  if (typeof value.intValue === "number") return value.intValue;
  if (typeof value.uintValue === "number") return value.uintValue;
  if (typeof value.floatValue === "number") return value.floatValue;
  return undefined;
}

export function asText(value: TypedValue | undefined): string | undefined {
  return value?.textValue;
}

/// Read a trend result into points, in time order.
export function pointsFrom(columns: string[], rows: ResultRow[]): Point[] {
  const bucket = columns.indexOf("bucket");
  const count = columns.indexOf("events");
  const points: Point[] = [];
  for (const row of rows) {
    const at = asNumber(row.values[bucket === -1 ? 0 : bucket]);
    const value = asNumber(row.values[count === -1 ? row.values.length - 1 : count]);
    if (at === undefined || value === undefined) continue;
    points.push({ at, count: value });
  }
  return points.sort((left, right) => left.at - right.at);
}

/// The event chart.
///
/// An empty range draws the axis and says there is nothing in it, rather than
/// drawing nothing at all: "no data" and "the chart is broken" must not look
/// the same.
export function chart(points: Point[], width = 640, height = 180): SVGElement {
  const svg = svgElement("svg");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Events over time");

  const axis = svgElement("line");
  axis.setAttribute("x1", "0");
  axis.setAttribute("y1", String(height - 1));
  axis.setAttribute("x2", String(width));
  axis.setAttribute("y2", String(height - 1));
  axis.setAttribute("stroke", "currentColor");
  svg.append(axis);

  if (points.length === 0) {
    const label = svgElement("text");
    label.setAttribute("x", String(width / 2));
    label.setAttribute("y", String(height / 2));
    label.setAttribute("text-anchor", "middle");
    label.textContent = "No events in this range.";
    svg.append(label);
    return svg;
  }

  const highest = Math.max(...points.map((point) => point.count), 1);
  const step = points.length > 1 ? width / (points.length - 1) : 0;
  const line = svgElement("polyline");
  line.setAttribute("fill", "none");
  line.setAttribute("stroke", "currentColor");
  line.setAttribute("stroke-width", "2");
  line.setAttribute(
    "points",
    points
      .map((point, index) => {
        const x = points.length > 1 ? index * step : width / 2;
        const y = height - 1 - (point.count / highest) * (height - 12);
        return `${x.toFixed(1)},${y.toFixed(1)}`;
      })
      .join(" "),
  );
  svg.append(line);
  return svg;
}

/// The ingest health panel.
///
/// A check that fails names itself and says why, because "unhealthy" with no
/// reason costs an operator the whole diagnosis. See CONVENTIONS.md section 3.
export interface HealthReport {
  readonly ready: boolean;
  readonly checks: ReadonlyArray<{ readonly name: string; readonly detail: string }>;
}

export function health(report: HealthReport): HTMLElement {
  const panel = document.createElement("section");
  panel.className = "health";

  const heading = document.createElement("h2");
  heading.textContent = report.ready ? "Accepting telemetry" : "Not accepting telemetry";
  panel.append(heading);

  if (report.checks.length > 0) {
    const list = document.createElement("ul");
    for (const check of report.checks) {
      const item = document.createElement("li");
      item.textContent = `${check.name}: ${check.detail}`;
      list.append(item);
    }
    panel.append(list);
  }
  return panel;
}

/// The breakdown table beside the chart.
export function breakdown(columns: string[], rows: ResultRow[]): HTMLElement {
  const table = document.createElement("table");
  const head = document.createElement("tr");
  for (const column of columns) {
    const cell = document.createElement("th");
    cell.textContent = column;
    head.append(cell);
  }
  table.append(head);

  for (const row of rows) {
    const line = document.createElement("tr");
    for (const value of row.values) {
      const cell = document.createElement("td");
      cell.textContent = asText(value) ?? String(asNumber(value) ?? "");
      line.append(cell);
    }
    table.append(line);
  }
  return table;
}

/// Read a metric-rate result into points, in time order.
///
/// A bucket whose rate is absent is left out rather than drawn as a zero. A
/// rate needs two readings, so the first bucket of a series has none, and a
/// zero there would say the metric stopped when it had not started.
export function ratePointsFrom(columns: string[], rows: ResultRow[]): Point[] {
  const bucket = columns.indexOf("bucket");
  const rate = columns.indexOf("each_second");
  const points: Point[] = [];
  for (const row of rows) {
    const at = asNumber(row.values[bucket === -1 ? 0 : bucket]);
    const value = asNumber(row.values[rate === -1 ? row.values.length - 1 : rate]);
    if (at === undefined || value === undefined) continue;
    points.push({ at, count: value });
  }
  return points.sort((left, right) => left.at - right.at);
}

/// The metric chart, with the metric's name above it.
///
/// The name is shown because a rate with no name is a line nobody can act on,
/// and the unit is each second because that is what `rate` answers.
export function metricChart(name: string, points: Point[]): HTMLElement {
  const section = document.createElement("section");
  section.className = "metric";
  const heading = document.createElement("h2");
  heading.textContent = `${name}, each second`;
  section.append(heading, chart(points));
  if (points.length === 0) {
    section.append(note(`No points for ${name} in this range.`));
  }
  return section;
}

/// A chooser for the metric the chart draws.
///
/// The names come from the head rather than from configuration, because a
/// configured list goes stale the moment an application declares an instrument.
export function metricChooser(
  names: string[],
  selected: string | undefined,
  onSelect: (name: string) => void,
): HTMLElement {
  const label = document.createElement("label");
  label.textContent = "Metric";
  const select = document.createElement("select");
  select.name = "metric";
  for (const name of names) {
    const option = document.createElement("option");
    option.value = name;
    option.textContent = name;
    if (name === selected) option.selected = true;
    select.append(option);
  }
  select.addEventListener("change", () => onSelect(select.value));
  label.append(select);
  return label;
}

/// The metric names a project holds, read out of a breakdown result.
export function metricNamesFrom(columns: string[], rows: ResultRow[]): string[] {
  const column = columns.indexOf("metric_name");
  const names: string[] = [];
  for (const row of rows) {
    const name = asText(row.values[column === -1 ? 0 : column]);
    if (name !== undefined && name.length > 0) names.push(name);
  }
  return names;
}

/// A quiet note beside a chart. It is not a failure, so it does not shout.
function note(message: string): HTMLElement {
  const element = document.createElement("p");
  element.className = "note";
  element.textContent = message;
  return element;
}

/// A message a person can act on. Every failure the dashboard shows goes
/// through here, so none of them reaches a person as a stack trace.
export function failure(message: string): HTMLElement {
  const element = document.createElement("p");
  element.className = "failure";
  element.setAttribute("role", "alert");
  element.textContent = message;
  return element;
}

function svgElement(name: string): SVGElement {
  return document.createElementNS("http://www.w3.org/2000/svg", name) as SVGElement;
}

// ---------------------------------------------------------------------------
// Campaigns and business outcomes. Phase 9.
// ---------------------------------------------------------------------------

/// One row of a campaign report.
export interface CampaignRow {
  readonly dimension: string;
  readonly touches: number;
  readonly people: number;
  readonly sessions: number;
  readonly conversions: number;
  /// Exact decimal text. It is never a number: a credited value read as a
  /// float would stop agreeing with the revenue it was divided from.
  readonly value: string;
  readonly cost: string;
  /// Absent when nothing was spent. A zero would read as "this campaign earned
  /// nothing for its spend", and a campaign with no spend recorded earned
  /// everything for nothing.
  readonly returnOnSpend: number | undefined;
}

/// Read an exact decimal out of a typed value.
///
/// A decimal stays text all the way to the screen. `asNumber` deliberately does
/// not read one, so a credited value cannot become a float by accident on the
/// way to a table cell.
export function asDecimal(value: TypedValue | undefined): string | undefined {
  if (value === undefined) return undefined;
  if (value.decimalValue !== undefined) return trimZeros(value.decimalValue.toString());
  return value.textValue;
}

/// Drop the trailing zeros of a fraction.
///
/// A credited value is divided at six guard digits, so `28` arrives as
/// `28.000000`. Two amounts that are the same number must read the same, or a
/// person comparing a campaign report against a revenue report finds two
/// numbers that look different and are not.
function trimZeros(text: string): string {
  if (!text.includes(".")) return text;
  return text.replace(/0+$/, "").replace(/\.$/, "");
}

/// Read a campaign report into rows.
export function campaignRowsFrom(columns: string[], rows: ResultRow[]): CampaignRow[] {
  const at = (name: string, fallback: number) => {
    const found = columns.indexOf(name);
    return found === -1 ? fallback : found;
  };
  return rows.map((row) => ({
    dimension: asText(row.values[0]) ?? "",
    touches: asNumber(row.values[at("touches", 1)]) ?? 0,
    people: asNumber(row.values[at("people", 2)]) ?? 0,
    sessions: asNumber(row.values[at("sessions", 3)]) ?? 0,
    conversions: asNumber(row.values[at("conversions", 4)]) ?? 0,
    value: asDecimal(row.values[at("value", 5)]) ?? "0",
    cost: asDecimal(row.values[at("cost", 6)]) ?? "0",
    returnOnSpend: asNumber(row.values[at("return", 7)]),
  }));
}

/// The campaign report table: sessions, people, conversions, value, cost, and
/// return, for each campaign. `docs/DATA_MODEL.md` section 6.
export function campaignReport(rows: CampaignRow[], dimension = "campaign"): HTMLElement {
  const section = document.createElement("section");
  section.className = "campaigns";

  const heading = document.createElement("h2");
  heading.textContent = `Campaigns, by ${dimension}`;
  section.append(heading);

  if (rows.length === 0) {
    section.append(note("No campaign traffic in this range."));
    return section;
  }

  const table = document.createElement("table");
  const head = document.createElement("tr");
  for (const column of [dimension, "touches", "people", "sessions", "conversions", "value", "cost", "return"]) {
    const cell = document.createElement("th");
    cell.textContent = column;
    head.append(cell);
  }
  table.append(head);

  for (const row of rows) {
    const line = document.createElement("tr");
    for (const value of [
      row.dimension,
      String(row.touches),
      String(row.people),
      String(row.sessions),
      // A share of a conversion is a fraction under a multi-touch model, and
      // rounding it to a whole would make three tenth-shares read as nothing.
      row.conversions.toFixed(2),
      row.value,
      row.cost,
      // No spend, no return. It is a dash rather than a zero, because a zero
      // is a claim and this is the absence of one.
      row.returnOnSpend === undefined ? "—" : `${row.returnOnSpend.toFixed(2)}×`,
    ]) {
      const cell = document.createElement("td");
      cell.textContent = value;
      line.append(cell);
    }
    table.append(line);
  }
  section.append(table);
  return section;
}

/// The chooser for the attribution model a report uses.
///
/// **The model is a choice a person makes and the report says which one it
/// used.** Two models disagree about the same traffic on purpose, and a report
/// that did not name its model would be a number nobody could argue with.
export function attributionChooser(
  models: readonly string[],
  selected: string,
  onSelect: (model: string) => void,
): HTMLElement {
  const label = document.createElement("label");
  label.textContent = "Attribution model";
  const select = document.createElement("select");
  select.name = "attribution-model";
  for (const model of models) {
    const option = document.createElement("option");
    option.value = model;
    option.textContent = model;
    if (model === selected) option.selected = true;
    select.append(option);
  }
  select.addEventListener("change", () => onSelect(select.value));
  label.append(select);
  return label;
}

/// What a result said about itself, shown beside the numbers.
///
/// Every warning the head returns reaches a person. An attribution answer says
/// which model and which settings version produced it, how much revenue nothing
/// earned, and how many people a consent policy left out; a report that hid
/// those would be a number without its footnotes.
export function resultNotes(warnings: readonly string[] | undefined): HTMLElement | undefined {
  if (warnings === undefined || warnings.length === 0) return undefined;
  const list = document.createElement("ul");
  list.className = "notes";
  for (const warning of warnings) {
    const item = document.createElement("li");
    item.textContent = warning;
    list.append(item);
  }
  return list;
}

// ---------------------------------------------------------------------------
// The operator surface: alerts and workflows. Phase 10.
// ---------------------------------------------------------------------------

/// One alert, as a person reads it.
export interface AlertRow {
  readonly ruleId: string;
  readonly name: string;
  readonly state: string;
  readonly since: number;
  readonly observedValue?: number;
  readonly reason?: string;
  readonly notificationsSent?: number;
}

/// One workflow's state.
export interface WorkflowRow {
  readonly kind: string;
  readonly pending: number;
  readonly inFlight: number;
  readonly quarantined: number;
  readonly lagMs: number;
  readonly failures: number;
  readonly lastFailure?: string;
}

/// The states an alert can be in, and what each one means to a reader.
///
/// **`unknown` is not a quiet `ok`.** It means TallyOwl could not answer, which
/// is a different fact from a value that did not cross a threshold, and the
/// interface has to keep them apart or an operator will read a broken query as
/// a healthy system.
const ALERT_MEANING: Readonly<Record<string, string>> = {
  ok: "The condition is not satisfied.",
  firing: "The condition is satisfied.",
  "no-data": "The query returned no rows.",
  unknown: "TallyOwl could not answer, so this alert is not reporting.",
  silenced: "Notification is suppressed. The evaluation is still running.",
};

/// The alert list.
export function alertList(alerts: readonly AlertRow[]): HTMLElement {
  const section = document.createElement("section");
  section.className = "alerts";
  const heading = document.createElement("h2");
  heading.textContent = "Alerts";
  section.append(heading);

  if (alerts.length === 0) {
    const empty = document.createElement("p");
    empty.textContent = "No alert rules. Nothing is being watched.";
    section.append(empty);
    return section;
  }

  const table = document.createElement("table");
  table.append(headerRow(["Rule", "State", "Since", "Value", "Notifications", "What it means"]));
  const body = document.createElement("tbody");
  for (const alert of alerts) {
    const row = document.createElement("tr");
    row.setAttribute("data-state", alert.state);
    row.append(
      cell(alert.name),
      cell(alert.state),
      cell(new Date(alert.since).toISOString()),
      // An absent value is not a zero. A `no-data` alert has no value at all,
      // and drawing a zero would say the measure was zero.
      cell(alert.observedValue === undefined ? "—" : String(alert.observedValue)),
      cell(String(alert.notificationsSent ?? 0)),
      cell(alert.reason && alert.reason.length > 0 ? alert.reason : ALERT_MEANING[alert.state] ?? ""),
    );
    body.append(row);
  }
  table.append(body);
  section.append(table);
  return section;
}

/// The workflow table: lag, failure, and quarantine.
///
/// `docs/PLAN.md` Phase 10 asks for exactly those three, and each one answers a
/// different question an operator has. **Lag** says whether a workflow is
/// keeping up. **Failures** say whether it is retrying. **Quarantine** says
/// whether something has stopped being retried and is waiting for a person —
/// which is the one that never resolves itself.
export function workflowTable(workflows: readonly WorkflowRow[]): HTMLElement {
  const section = document.createElement("section");
  section.className = "workflows";
  const heading = document.createElement("h2");
  heading.textContent = "Workflows";
  section.append(heading);

  const table = document.createElement("table");
  table.append(headerRow(["Workflow", "Waiting", "Running", "Lag", "Failures", "Quarantined"]));
  const body = document.createElement("tbody");
  for (const workflow of workflows) {
    const row = document.createElement("tr");
    row.setAttribute("data-workflow", workflow.kind);
    // Quarantine is the one that needs a person, so it is marked rather than
    // left as one number among six.
    if (workflow.quarantined > 0) row.setAttribute("data-needs-attention", "quarantine");
    row.append(
      cell(workflow.kind),
      cell(String(workflow.pending)),
      cell(String(workflow.inFlight)),
      cell(duration(workflow.lagMs)),
      cell(String(workflow.failures)),
      cell(String(workflow.quarantined)),
    );
    body.append(row);
    if (workflow.lastFailure && workflow.lastFailure.length > 0) {
      const detail = document.createElement("tr");
      detail.className = "failure";
      const held = document.createElement("td");
      held.setAttribute("colspan", "6");
      held.textContent = workflow.lastFailure;
      detail.append(held);
      body.append(detail);
    }
  }
  table.append(body);
  section.append(table);

  const stuck = workflows.filter((workflow) => workflow.quarantined > 0);
  if (stuck.length > 0) {
    const note = document.createElement("p");
    note.className = "needs-attention";
    note.textContent =
      `${stuck.reduce((total, workflow) => total + workflow.quarantined, 0)} pieces of work will not be retried again and are waiting for a person. Nothing was dropped.`;
    section.append(note);
  }
  return section;
}

/// A duration a person reads, rather than a number of milliseconds.
export function duration(ms: number): string {
  if (ms <= 0) return "none";
  if (ms < 1_000) return `${ms} ms`;
  if (ms < 60_000) return `${Math.round(ms / 100) / 10} s`;
  if (ms < 3_600_000) return `${Math.round(ms / 6_000) / 10} min`;
  return `${Math.round(ms / 360_000) / 10} h`;
}

/// The notification attempts, newest first.
export interface DeliveryRow {
  readonly ruleId: string;
  readonly target: string;
  readonly attempts: number;
  readonly delivered: boolean;
  readonly lastFailure?: string;
  readonly at: number;
}

export function deliveryList(deliveries: readonly DeliveryRow[]): HTMLElement {
  const section = document.createElement("section");
  section.className = "deliveries";
  const heading = document.createElement("h2");
  heading.textContent = "Notification attempts";
  section.append(heading);

  if (deliveries.length === 0) {
    const empty = document.createElement("p");
    empty.textContent = "No notification has been attempted yet.";
    section.append(empty);
    return section;
  }

  const table = document.createElement("table");
  table.append(headerRow(["When", "Rule", "Target", "Attempts", "Outcome"]));
  const body = document.createElement("tbody");
  for (const delivery of deliveries) {
    const row = document.createElement("tr");
    row.setAttribute("data-delivered", String(delivery.delivered));
    row.append(
      cell(new Date(delivery.at).toISOString()),
      cell(delivery.ruleId),
      cell(delivery.target),
      cell(String(delivery.attempts)),
      // A failure says why. "Failed" on its own sends a person to a log file.
      cell(delivery.delivered ? "delivered" : (delivery.lastFailure ?? "failed")),
    );
    body.append(row);
  }
  table.append(body);
  section.append(table);
  return section;
}

function headerRow(names: readonly string[]): HTMLElement {
  const head = document.createElement("thead");
  const row = document.createElement("tr");
  for (const name of names) {
    const held = document.createElement("th");
    held.textContent = name;
    row.append(held);
  }
  head.append(row);
  return head;
}

function cell(text: string): HTMLElement {
  const held = document.createElement("td");
  held.textContent = text;
  return held;
}
