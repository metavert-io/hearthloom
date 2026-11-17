// Pulse's series cache, in Node: the real assets/pulse.js with a stubbed
// fetch. A request that fails — the health sidecar answers an error while
// an upgrade brings it up to date, or the daemon is busy — must not be
// kept for the life of the page: the next view of the metric asks again.
// Node built-ins only. `cargo test` runs it (dashboard.rs,
// test_pulse_asks_again_after_a_failed_request); by hand:
// node tests/pulse/cache.mjs
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const assets = process.env.ASSETS || path.resolve(here, "../../crates/hearthloom-daemon/assets");

// Each request is answered by the first handler whose prefix it starts
// with; the handlers can change between views.
let answers = {};
const asked = [];
globalThis.fetch = async (url) => {
  asked.push(url);
  const prefix = Object.keys(answers).find((p) => url.startsWith(p));
  if (!prefix) return new Response("{}", { status: 404 });
  const [status, body] = answers[prefix]();
  return new Response(JSON.stringify(body), { status });
};
const count = (prefix) => asked.filter((url) => url.startsWith(prefix)).length;

const { loadSeries } = await import(pathToFileURL(path.join(assets, "pulse.js")).href);

const HISTORY = "/v1/sources/virtual/apple-health/query";
const TODAY = "/v1/health/today";
const upgrading = () => [500, { error: "being brought up to date; try again in a minute" }];
const today = () => [200, { day: "2026-03-03", rows: [{ metric: "RestingHeartRate", avg: 55 }] }];
const history = () => [200, { rows: [["2026-03-01", 54], ["2026-03-02", 53]] }];
const metric = (id) => ({
  id,
  src: "health",
  sql: "SELECT day, MIN(avg) v FROM daily WHERE metric='RestingHeartRate' GROUP BY day",
  today: (rows) => rows.find((r) => r.metric === "RestingHeartRate")?.avg ?? null,
});

// The history fails while the sidecar is brought up to date: shown
// empty, and not kept.
answers = { [HISTORY]: upgrading, [TODAY]: today };
let series = await loadSeries(metric("rhr"));
assert.deepEqual(series.points, []);
// Up to date now: the next view asks again and gets the history.
answers = { [HISTORY]: history, [TODAY]: today };
series = await loadSeries(metric("rhr"));
assert.deepEqual(series.points.map((p) => p[1]), [54, 53, 55]);
assert.equal(count(HISTORY), 2);
// Answered in full: kept, and not asked for again.
await loadSeries(metric("rhr"));
assert.equal(count(HISTORY), 2);

// Today's live point (asked for in the history's unit) fails once: the
// series isn't kept, and neither is the failed answer.
const glucose = {
  id: "glucose",
  src: "health",
  hk: "BloodGlucose",
  sql: "SELECT day, AVG(avg) v, MAX(unit) u FROM daily WHERE metric='BloodGlucose' GROUP BY day",
  today: (rows) => rows.find((r) => r.metric === "BloodGlucose")?.avg ?? null,
};
const glucoseHistory = () => [200, { rows: [["2026-03-01", 100, "mg/dL"], ["2026-03-02", 110, "mg/dL"]] }];
const glucoseToday = () => [200, { day: "2026-03-03", rows: [{ metric: "BloodGlucose", avg: 105, unit: "mg/dL" }] }];
answers = { [HISTORY]: glucoseHistory, [TODAY]: () => [503, { error: "busy" }] };
series = await loadSeries(glucose);
assert.deepEqual(series.points.map((p) => p[1]), [100, 110]);
answers = { [HISTORY]: glucoseHistory, [TODAY]: glucoseToday };
series = await loadSeries(glucose);
assert.deepEqual(series.points.map((p) => p[1]), [100, 110, 105]);
assert.equal(series.unit, "mg/dL");

// A request that never reached the daemon is asked again too.
answers = {};
const offline = { id: "episodes", src: "histogram" };
globalThis.fetch = async () => {
  throw new TypeError("network down");
};
series = await loadSeries(offline);
assert.deepEqual(series.points, []);
let histogramAsks = 0;
globalThis.fetch = async (url) => {
  histogramAsks += 1;
  assert.ok(url.startsWith("/v1/timeline/histogram"), url);
  return new Response(JSON.stringify({ buckets: [{ bucket: "2026-03-01", n: 4 }] }));
};
series = await loadSeries(offline);
assert.deepEqual(series.points.map((p) => p[1]), [4]);
assert.equal(histogramAsks, 1);

console.log("pulse cache: ok");
