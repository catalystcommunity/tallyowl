// What the dashboard draws.
//
// Every function here takes data and returns an element. None of them fetches,
// and none of them holds state, so a test drives a view without a network and
// without a browser.
//
// The controls, frames, table, and status bar are Zest components. A view
// makes each one by its tag name and sets attributes, so this module imports
// no component class and a test runs it without a browser; `zest.ts` gives
// each tag its class before the page draws.
//
// The charts are inline SVG. A charting library would be a dependency on the
// always-on dashboard path, and a count for each bucket needs a rectangle.
// How they look is in `static/dashboard.css`; these functions only give each
// part a class.

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

/// The time a chart covers, and the width of one of its buckets.
export interface Range {
  readonly start: number;
  readonly end: number;
  readonly bucketMs: number;
}

const WHOLE = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
const RATE = new Intl.NumberFormat(undefined, { maximumSignificantDigits: 3 });
const CLOCK = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });

/// A count as a person reads it, with the separators of their locale.
export function whole(value: number): string {
  return WHOLE.format(value);
}

/// The event chart: one bar for each bucket, and the axis under it.
///
/// An empty range draws the frame and says there is nothing in it, rather than
/// drawing nothing at all: "no data" and "the chart is broken" must not look
/// the same.
export function eventPanel(points: Point[], range: Range): HTMLElement {
  const section = panel("events", `Events in the last ${hoursIn(range)} hours`);
  const total = points.reduce((sum, point) => sum + point.count, 0);
  const peak = Math.max(0, ...points.map((point) => point.count));

  const summary = element("p", "summary");
  summary.append(element("strong", "", whole(total)), element("span", "", total === 1 ? " event" : " events"));
  section.append(summary);

  const frame = element("div", points.length === 0 ? "chart-frame empty" : "chart-frame");
  frame.append(barChart(points, range));
  if (points.length === 0) {
    frame.append(element("p", "chart-empty", "No events in this range. They show here after an application sends them."));
  }
  section.append(frame, timeAxis(range));
  if (peak > 0) {
    section.append(element("p", "chart-caption", `The busiest hour had ${whole(peak)} ${peak === 1 ? "event" : "events"}. The last bar is the current hour, which is not over yet.`));
  }
  return section;
}

/// A Zest group box: a frame with a title around one part of the page.
function panel(className: string, title: string): HTMLElement {
  const box = element("group-box", `panel ${className}`);
  box.setAttribute("label", title);
  return box;
}

/// The bars. Each bucket of the range has a slot, and a bucket the head did
/// not return has no bar: a count that is absent had no events in it.
export function barChart(points: Point[], range: Range): SVGElement {
  const first = Math.floor(range.start / range.bucketMs) * range.bucketMs;
  const slots = Math.max(1, Math.ceil((range.end - first) / range.bucketMs));
  const slotWidth = 10;
  const width = slots * slotWidth;
  const height = 100;

  const svg = svgElement("svg");
  svg.setAttribute("class", "bars");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Events for each hour");
  svg.append(gridLine(0, width), gridLine(height / 2, width), gridLine(height - 0.5, width, "baseline"));

  const highest = Math.max(1, ...points.map((point) => point.count));
  for (const point of points) {
    const slot = Math.floor((point.at - first) / range.bucketMs);
    if (slot < 0 || slot >= slots || point.count <= 0) continue;
    // A bucket with one event in it still gets a bar a person can see.
    const barHeight = Math.max(1.5, (point.count / highest) * (height - 4));
    const bar = svgElement("rect");
    bar.setAttribute("class", slot === slots - 1 ? "bar current" : "bar");
    bar.setAttribute("x", String(slot * slotWidth + 1.5));
    bar.setAttribute("width", String(slotWidth - 3));
    bar.setAttribute("y", (height - barHeight).toFixed(2));
    bar.setAttribute("height", barHeight.toFixed(2));
    const title = svgElement("title");
    title.textContent = `${CLOCK.format(point.at)}: ${whole(point.count)} ${point.count === 1 ? "event" : "events"}`;
    bar.append(title);
    svg.append(bar);
  }
  return svg;
}

/// A rate over time: a line, with the area under it lightly filled.
///
/// A point is placed at its time, not at its index. A series that starts
/// halfway through the range starts halfway across the chart.
export function lineChart(points: Point[], range: Range, label: string): SVGElement {
  const width = 600;
  const height = 100;
  const svg = svgElement("svg");
  svg.setAttribute("class", "line");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", label);
  svg.append(gridLine(0, width), gridLine(height / 2, width), gridLine(height - 0.5, width, "baseline"));
  if (points.length === 0) return svg;

  const highest = Math.max(...points.map((point) => point.count), Number.MIN_VALUE);
  const span = Math.max(1, range.end - range.start);
  const at = (point: Point) => {
    const x = Math.min(width, Math.max(0, ((point.at - range.start) / span) * width));
    const y = height - 1 - (point.count / highest) * (height - 6);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  };
  const line = points.map(at).join(" ");
  const firstX = at(points[0]).split(",")[0];
  const lastX = at(points[points.length - 1]).split(",")[0];

  const area = svgElement("polygon");
  area.setAttribute("class", "area");
  area.setAttribute("points", `${firstX},${height} ${line} ${lastX},${height}`);
  const stroke = svgElement("polyline");
  stroke.setAttribute("class", "stroke");
  stroke.setAttribute("points", line);
  stroke.setAttribute("vector-effect", "non-scaling-stroke");
  svg.append(area, stroke);
  return svg;
}

function gridLine(y: number, width: number, kind = "grid"): SVGElement {
  const line = svgElement("line");
  line.setAttribute("class", kind);
  line.setAttribute("x1", "0");
  line.setAttribute("x2", String(width));
  line.setAttribute("y1", String(y));
  line.setAttribute("y2", String(y));
  line.setAttribute("vector-effect", "non-scaling-stroke");
  return line;
}

/// The times under a chart. They are text in the page rather than in the
/// drawing, so they stay readable when the drawing is narrow.
export function timeAxis(range: Range): HTMLElement {
  const axis = element("div", "axis");
  axis.setAttribute("aria-hidden", "true");
  const marks = 4;
  for (let mark = 0; mark <= marks; mark += 1) {
    const time = range.start + ((range.end - range.start) * mark) / marks;
    axis.append(element("span", "", mark === marks ? "Now" : CLOCK.format(time)));
  }
  return axis;
}

function hoursIn(range: Range): number {
  return Math.round((range.end - range.start) / 3_600_000);
}

/// The ingest health, as a short status for the top of the page.
///
/// A check that fails names itself and says why, because "unhealthy" with no
/// reason costs an operator the whole diagnosis. See CONVENTIONS.md section 3.
/// The reasons are in `healthDetail`, under the bar, where they have room.
export interface HealthReport {
  readonly ready: boolean;
  readonly checks: ReadonlyArray<{ readonly name: string; readonly detail: string }>;
}

/// The health, as the first pane of the status bar. The pane is live, so a
/// screen reader says it when the page draws again with a different answer.
export function healthStatus(report: HealthReport | undefined): HTMLElement {
  const pane = element("status-pane", "health");
  pane.setAttribute("live", "");
  if (report === undefined) {
    pane.append(element("span", "", "Health not known"));
    return pane;
  }
  pane.setAttribute("data-ready", String(report.ready));
  pane.append(element("span", "dot"), element("span", "", report.ready ? "Accepting telemetry" : "Not accepting telemetry"));
  return pane;
}

/// The bar at the bottom of the page: the health, the range the charts show,
/// and when the page drew them. The time is there because the page does not
/// draw again by itself, and a person must know how old the numbers are.
export function statusBar(report: HealthReport | undefined, range: Range, drawnAt: number): HTMLElement {
  const bar = element("status-bar", "footer");
  bar.setAttribute("label", "Status");
  const span = element("status-pane", "", `Last ${hoursIn(range)} hours, by hour`);
  const drawn = element("status-pane", "", `Drawn at ${CLOCK.format(drawnAt)}`);
  drawn.setAttribute("stretch", "");
  bar.append(healthStatus(report), span, drawn);
  return bar;
}

/// The checks that need saying, or nothing when every check passed. A panel
/// that listed every passing check would bury the one that failed.
export function healthDetail(report: HealthReport): HTMLElement | undefined {
  if (report.checks.length === 0) return undefined;
  const banner = element("section", "banner");
  banner.setAttribute("data-ready", String(report.ready));
  banner.setAttribute("role", "status");
  banner.append(element("h2", "", report.ready ? "Accepting telemetry, with warnings" : "Not accepting telemetry"));
  const list = element("ul");
  for (const check of report.checks) {
    const item = element("li");
    item.append(element("strong", "", check.name), element("span", "", ` ${check.detail}`));
    list.append(item);
  }
  banner.append(list);
  return banner;
}

/// One project a person can choose.
export interface ProjectChoice {
  readonly projectId: Uint8Array;
  readonly name: string;
}

/// A project ID as text, for a form value and for storage.
export function projectKey(projectId: Uint8Array): string {
  return [...projectId].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

/// The bar across the top: the name, the project, and sign-out.
export function topBar(options: {
  readonly projects: readonly ProjectChoice[];
  readonly selected: Uint8Array;
  readonly onProject: (projectId: Uint8Array) => void;
  readonly onSignOut: () => void;
}): HTMLElement {
  const bar = element("header", "topbar");

  const brand = element("div", "brand");
  const mark = element("img", "mark");
  mark.setAttribute("src", OWL);
  mark.setAttribute("alt", "");
  brand.append(mark, element("span", "wordmark", "TallyOwl"));
  bar.append(brand);

  const selectedKey = projectKey(options.selected);
  if (options.projects.length > 1) {
    const chooser = chooserOf(
      "project",
      "Project",
      options.projects.map((project) => ({ value: projectKey(project.projectId), label: project.name })),
      selectedKey,
    );
    chooser.addEventListener("change", () => {
      const chosen = options.projects.find((project) => projectKey(project.projectId) === valueOf(chooser));
      if (chosen !== undefined) options.onProject(chosen.projectId);
    });
    bar.append(chooser);
  } else {
    const only = options.projects.find((project) => projectKey(project.projectId) === selectedKey);
    if (only !== undefined) bar.append(element("p", "project", only.name));
  }

  const signOut = element("push-button", "sign-out", "Sign out");
  signOut.setAttribute("variant", "flat");
  signOut.addEventListener("click", () => options.onSignOut());
  bar.append(signOut);
  return bar;
}

/// Where the owl mark is. The build puts it beside the bundle.
export const OWL = "/assets/app/owl.svg";

/// A Zest combo box that chooses one of a few values.
///
/// The options and the value are attributes, not properties: an attribute
/// reaches the component however late its tag is defined.
function chooserOf(
  name: string,
  label: string,
  options: readonly { readonly value: string; readonly label: string }[],
  selected: string | undefined,
): HTMLElement {
  const chooser = element("combo-box", `chooser ${name}`);
  chooser.setAttribute("name", name);
  chooser.setAttribute("aria-label", label);
  chooser.setAttribute("options", JSON.stringify(options));
  if (selected !== undefined) chooser.setAttribute("value", selected);
  return chooser;
}

/// The current value of a Zest control.
function valueOf(control: HTMLElement): string {
  return (control as HTMLElement & { value?: string }).value ?? "";
}

/// The most frequent events, each with a bar for its share of the busiest.
///
/// It is a Zest table view, so a person can sort it and move through it with
/// the keyboard. The columns of a table view have fixed widths, so the event
/// column takes the width that the count leaves.
export function breakdown(columns: string[], rows: ResultRow[]): HTMLElement {
  const section = panel("top-events", "Top events");

  const name = columns.indexOf("name");
  const count = columns.indexOf("events");
  const entries = rows
    .map((row) => ({
      name: asText(row.values[name === -1 ? 0 : name]) ?? "",
      count: asNumber(row.values[count === -1 ? row.values.length - 1 : count]) ?? 0,
    }))
    .filter((entry) => entry.name.length > 0);

  if (entries.length === 0) {
    section.append(note("No events in this range."));
    return section;
  }

  const highest = Math.max(1, ...entries.map((entry) => entry.count));
  const table = element("table-view", "ranked") as TableView;
  table.setAttribute("label", "Top events");
  table.keyOf = (entry) => (entry as { name: string }).name;
  table.columns = [
    {
      key: "name",
      label: "Event",
      width: 240,
      sortValue: (entry: { name: string }) => entry.name,
      // The view reuses a cell for another row, so this replaces what the
      // cell held and adds nothing that stays.
      render: (cell: HTMLElement, value: unknown, entry: { count: number }) => {
        const line = element("span", "event");
        line.append(element("span", "event-name", String(value)), share(entry.count / highest));
        cell.replaceChildren(line);
      },
    },
    { key: "count", label: "Count", type: "number", width: COUNT_WIDTH, format: (value: number) => whole(value) },
  ];
  table.items = entries;
  fitFirstColumn(table);
  section.append(table);
  return section;
}

const COUNT_WIDTH = 96;

/// What the dashboard sets on a Zest table view.
interface TableView extends HTMLElement {
  keyOf: (item: unknown) => unknown;
  columns: unknown[];
  items: unknown[];
  setColumnWidth?: (index: number, width: number) => void;
}

/// Give the event column the width of the table that the count does not use.
function fitFirstColumn(table: TableView): void {
  if (typeof ResizeObserver === "undefined") return;
  new ResizeObserver(([entry]) => {
    // The rows add a 0.25em selection bar at their start, about 4px.
    const width = Math.floor(entry.contentRect.width) - COUNT_WIDTH - 5;
    table.setColumnWidth?.(0, Math.max(160, width));
  }).observe(table);
}

/// A thin bar for one share. It is a drawing rather than a styled box, because
/// the content security policy refuses a width set in a `style` attribute.
function share(fraction: number): SVGElement {
  const svg = svgElement("svg");
  svg.setAttribute("class", "share");
  svg.setAttribute("viewBox", "0 0 100 4");
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("aria-hidden", "true");
  const track = svgElement("rect");
  track.setAttribute("class", "track");
  track.setAttribute("width", "100");
  track.setAttribute("height", "4");
  const fill = svgElement("rect");
  fill.setAttribute("class", "fill");
  fill.setAttribute("width", Math.max(1, fraction * 100).toFixed(1));
  fill.setAttribute("height", "4");
  svg.append(track, fill);
  return svg;
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

/// The metric panel: a chooser for the metric, and its rate.
///
/// The name is shown because a rate with no name is a line nobody can act on,
/// and the unit is each second because that is what `rate` answers. The names
/// come from the head rather than from configuration, because a configured
/// list goes stale the moment an application declares an instrument.
export function metricPanel(
  names: string[],
  chosen: string,
  points: Point[],
  range: Range,
  onSelect: (name: string) => void,
): HTMLElement {
  const section = panel("metric", "Metrics");
  const head = element("div", "panel-head");
  const summary = element("p", "summary");
  const latest = points.at(-1);
  if (latest !== undefined) {
    summary.append(element("strong", "", RATE.format(latest.count)), element("span", "", " each second, latest hour"));
  } else {
    summary.append(element("span", "", "Each second"));
  }
  head.append(summary, metricChooser(names, chosen, onSelect));
  section.append(head);

  const frame = element("div", points.length === 0 ? "chart-frame short empty" : "chart-frame short");
  frame.append(lineChart(points, range, `${chosen}, each second`));
  if (points.length === 0) frame.append(element("p", "chart-empty", `No points for ${chosen} in this range.`));
  section.append(frame, timeAxis(range));
  return section;
}

/// A chooser for the metric the chart draws.
export function metricChooser(
  names: string[],
  selected: string | undefined,
  onSelect: (name: string) => void,
): HTMLElement {
  const chooser = chooserOf(
    "metric",
    "Metric",
    names.map((name) => ({ value: name, label: name })),
    selected,
  );
  chooser.addEventListener("change", () => onSelect(valueOf(chooser)));
  return chooser;
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
  return element("p", "note", message);
}

/// One element, with its class and its text. Text and children are not mixed
/// on one element: a caller that needs both appends a child for the text.
function element(tag: string, className = "", text?: string): HTMLElement {
  const made = document.createElement(tag);
  if (className.length > 0) made.className = className;
  if (text !== undefined) made.textContent = text;
  return made;
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
