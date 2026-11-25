// The clock of the page and the days a person chooses. No browser, no head.

import assert from "node:assert/strict";
import { test } from "node:test";

import * as time from "../src/time.ts";

const UTC: time.Clock = { timeZone: "UTC", label: "UTC" };

test("each preference gives one clock, and a zone with no name is the browser", () => {
  assert.equal(time.clockOf({ timeDisplay: "utc" }).timeZone, "UTC");
  assert.equal(time.clockOf({ timeDisplay: "zone", timeZone: "Asia/Tokyo" }).timeZone, "Asia/Tokyo");
  assert.equal(time.clockOf({ timeDisplay: "zone" }).timeZone, undefined);
  assert.equal(time.clockOf({ timeDisplay: "browser", timeZone: "Asia/Tokyo" }).timeZone, undefined);
});

test("a choice keeps the zone a person used last", () => {
  const tokyo = time.preferencesFor("Asia/Tokyo", { timeDisplay: "browser" });
  assert.deepEqual(tokyo, { timeDisplay: "zone", timeZone: "Asia/Tokyo" });
  assert.deepEqual(time.preferencesFor(time.UTC_CHOICE, tokyo), { timeDisplay: "utc", timeZone: "Asia/Tokyo" });
  assert.deepEqual(time.preferencesFor(time.BROWSER_CHOICE, tokyo), { timeDisplay: "browser", timeZone: "Asia/Tokyo" });
  assert.equal(time.choiceOf(tokyo), "Asia/Tokyo");
  assert.equal(time.choiceOf({ timeDisplay: "utc", timeZone: "Asia/Tokyo" }), time.UTC_CHOICE);
});

test("chosen days are UTC days, also across a change of summer time", () => {
  const now = Date.UTC(2026, 11, 1);
  // Denver leaves summer time on 1 November 2026. The range does not move:
  // the clock of the viewer never decides which instants a query covers.
  const range = time.chosenDays("2026-10-31/2026-11-02", now)!;
  assert.equal(range.start, Date.UTC(2026, 9, 31));
  assert.equal(range.end, Date.UTC(2026, 10, 3));
  assert.equal(range.bucketMs, time.HOUR_MS, "three days draw by hour");
  assert.equal(range.live, false);
  assert.match(time.dayRangeText(range), /\(UTC\)$/, "a short range says its days are UTC days");
});

test("a long range draws by UTC day, so each bar is one of the chosen days", () => {
  const now = Date.UTC(2026, 11, 1);
  const range = time.chosenDays("2026-11-16/2026-11-22", now)!;
  assert.equal(range.bucketMs, time.DAY_MS);
  assert.equal(range.start, Date.UTC(2026, 10, 16));
  assert.equal(range.end, Date.UTC(2026, 10, 23));
  assert.match(time.dayRangeText(range), /\(UTC\)$/);
});

test("a range that reaches today ends now, and a long range draws by UTC day", () => {
  const now = Date.UTC(2026, 10, 22, 15, 30);
  const range = time.chosenDays("2026-10-24/2026-11-22", now)!;
  assert.equal(range.start, Date.UTC(2026, 9, 24));
  assert.equal(range.end, now);
  assert.equal(range.live, true);
  assert.equal(range.bucketMs, time.DAY_MS);
});

test("a range that cannot be read, or that starts after now, is no range", () => {
  const now = Date.UTC(2026, 10, 22);
  assert.equal(time.chosenDays("not days", now), undefined);
  assert.equal(time.chosenDays("2026-11-23/2026-11-24", now), undefined);
});

test("today for the picker is the UTC date, also where it is still yesterday", () => {
  // 03:00 UTC on 22 November is still 21 November in Denver. The picker offers
  // 22 November, because a chosen day is a UTC day.
  const now = Date.UTC(2026, 10, 22, 3);
  assert.equal(time.utcToday(now), "2026-11-22");
});

test("a day bucket is named by its UTC day, whatever the clock", () => {
  assert.match(time.utcDay(Date.UTC(2026, 10, 21)), /21/);
  assert.match(time.instant(Date.UTC(2026, 10, 21, 12), UTC), /12:00:00/);
});
