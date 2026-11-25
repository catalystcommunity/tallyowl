// How the dashboard reads and shows time.
//
// TallyOwl keeps and sends each instant in UTC. A person chooses only the
// clock that the page shows: the time of the browser, UTC, or one IANA zone.
// The choice is stored on their account (`get-user-preferences`), so that it
// follows them to each browser.
//
// This module has no DOM, so a test runs it without a browser.

import {
  addDays,
  dateFormat,
  formatDate,
  fromInstant,
  parseSelection,
  toInstant,
  type DateTimeParts,
} from "@catalystcommunity/date-model";
import type { UserPreferences } from "./control-api.ts";

export const HOUR_MS = 3_600_000;
export const DAY_MS = 24 * HOUR_MS;

/// The clock that the page shows. `timeZone` is the IANA name to give to
/// `Intl`, or nothing for the zone of the browser.
export interface Clock {
  readonly timeZone: string | undefined;
  /// The words for the clock, for the status bar: "UTC", "Europe/Berlin", or
  /// "browser time (America/Denver)".
  readonly label: string;
}

/// The default, before the head answers or when it cannot answer. A display
/// preference must never stop a page.
export const BROWSER_PREFERENCES: UserPreferences = { timeDisplay: "browser" };

/// The zone of this browser, by name.
export function browserZone(): string {
  return dateFormat().resolvedOptions().timeZone;
}

export function clockOf(preferences: UserPreferences): Clock {
  switch (preferences.timeDisplay) {
    case "utc":
      return { timeZone: "UTC", label: "UTC" };
    case "zone":
      if (preferences.timeZone !== undefined && preferences.timeZone.length > 0) {
        return { timeZone: preferences.timeZone, label: preferences.timeZone };
      }
      break;
  }
  return { timeZone: undefined, label: `browser time (${browserZone()})` };
}

/// The value of the time chooser: `browser`, `UTC`, or a zone name. One
/// editable list holds all three, so that a person types "Berl" and picks.
export const BROWSER_CHOICE = "browser";
export const UTC_CHOICE = "UTC";

export function choiceOf(preferences: UserPreferences): string {
  if (preferences.timeDisplay === "utc") return UTC_CHOICE;
  if (preferences.timeDisplay === "zone" && preferences.timeZone) return preferences.timeZone;
  return BROWSER_CHOICE;
}

/// The preferences a choice stores. A zone stays in the record after the
/// person goes back to UTC or to the browser, so the list can offer it again.
export function preferencesFor(choice: string, previous: UserPreferences): UserPreferences {
  const kept = previous.timeZone;
  if (choice === BROWSER_CHOICE) return withZone({ timeDisplay: "browser" }, kept);
  if (choice === UTC_CHOICE || choice === "Etc/UTC") return withZone({ timeDisplay: "utc" }, kept);
  return { timeDisplay: "zone", timeZone: choice };
}

function withZone(preferences: UserPreferences, zone: string | undefined): UserPreferences {
  return zone === undefined ? preferences : { ...preferences, timeZone: zone };
}

/// Each zone the browser knows, for the chooser. An old browser with no list
/// still gets the two choices that need no name.
export function knownZones(): string[] {
  const supported = (Intl as { supportedValuesOf?: (key: string) => string[] }).supportedValuesOf;
  return supported === undefined ? [] : supported("timeZone").filter((zone) => zone !== "UTC");
}

// ---------------------------------------------------------------------------
// The range the charts show
// ---------------------------------------------------------------------------

/// What the charts cover. `days` is the picker value (`2026-11-01/2026-11-07`)
/// when the person chose days, and nothing for the rolling last 24 hours.
export interface ChartRange {
  readonly start: number;
  readonly end: number;
  readonly bucketMs: number;
  /// True when the range ends now, so the last bucket is not over yet.
  readonly live: boolean;
  readonly days?: string;
}

/// The longest range that the charts draw by hour. A longer range draws by
/// day, so a chart never has more than about a hundred bars.
export const HOURLY_LIMIT_MS = 4 * DAY_MS;

/// Today in UTC, as ISO text. The picker offers no day after it, because a day
/// that has not started has no data.
///
/// A chosen day is always a UTC day, whatever the clock of the page is. Two
/// people in different zones who choose the same days then see the same
/// instants and the same numbers. Only the text follows the clock.
export function utcToday(now: number): string {
  return formatDate(fromInstant(new Date(now), "UTC"));
}

/// The ranges a person picks most. Each one ends today. The picker has its own
/// "Today".
export function dayPresets(): { label: string; value: (now: DateTimeParts) => string }[] {
  const back = (days: number) => (now: DateTimeParts) =>
    `${formatDate(addDays(now, -(days - 1)))}/${formatDate(now)}`;
  return [
    { label: "Last 7 days", value: back(7) },
    { label: "Last 30 days", value: back(30) },
    { label: "Last 90 days", value: back(90) },
  ];
}

/// The rolling range that the page shows before a person chooses days.
export function lastDay(now: number): ChartRange {
  return { start: now - DAY_MS, end: now, bucketMs: HOUR_MS, live: true };
}

/// The instants of the days a person chose.
///
/// Each day is a UTC day: "1 November" starts at 00:00 UTC on 1 November, for
/// a person in Berlin and for a person in Denver. TallyOwl keeps each bucket in
/// UTC, and the viewer's clock never decides which instants a query covers.
/// See `docs/QUERY.md` section 8.
///
/// A short range draws by hour and a long range by UTC day
/// (`Math.floor(t / DAY_MS)`). The range ends at the start of the day after the
/// last day, or now when that is earlier.
export function chosenDays(days: string, now: number): ChartRange | undefined {
  const parts = parseSelection(days, "date", "range");
  if (parts === null || parts.length !== 2) return undefined;
  const [first, last] = parts;
  const dayCount = Math.round(
    (Date.UTC(last.year, last.month - 1, last.day) - Date.UTC(first.year, first.month - 1, first.day)) / DAY_MS,
  ) + 1;
  const byDay = dayCount * DAY_MS > HOURLY_LIMIT_MS;
  const start = toInstant({ ...first, hour: 0, minute: 0, second: 0 }, "UTC").getTime();
  const after = toInstant({ ...addDays(last, 1), hour: 0, minute: 0, second: 0 }, "UTC").getTime();
  if (!(start < after) || start >= now) return undefined;
  const end = Math.min(after, now);
  return {
    start,
    end,
    bucketMs: byDay ? DAY_MS : HOUR_MS,
    live: end === now,
    days,
  };
}

// ---------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------

/// The time of day, in the clock of the page.
export function timeOfDay(at: number, clock: Clock): string {
  return dateFormat(undefined, { hour: "numeric", minute: "2-digit", timeZone: clock.timeZone }).format(at);
}

/// A day and a time, for an axis that covers more than one day.
export function dayAndTime(at: number, clock: Clock): string {
  return dateFormat(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
    timeZone: clock.timeZone,
  }).format(at);
}

/// The UTC day a day bucket holds. It is a UTC day, so it is written in UTC
/// whatever the clock of the page is.
export function utcDay(at: number): string {
  return dateFormat(undefined, { month: "short", day: "numeric", timeZone: "UTC" }).format(at);
}

/// One instant in full, with the name of its zone, for a table cell.
export function instant(at: number, clock: Clock): string {
  return dateFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "medium",
    timeZone: clock.timeZone,
  }).format(at);
}

/// The days of a range, as a person reads them: "Nov 1 – 7, 2026 (UTC)".
/// Chosen days are UTC days, so the text says so on every clock.
export function dayRangeText(range: ChartRange): string {
  const format = dateFormat(undefined, { dateStyle: "medium", timeZone: "UTC" });
  // The end is the first instant after the range. A range that ends at
  // midnight shows the day before.
  return `${format.formatRange(range.start, Math.max(range.start, range.end - 1))} (UTC)`;
}
