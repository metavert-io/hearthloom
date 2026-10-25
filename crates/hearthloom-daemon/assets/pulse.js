// pulse.js — Pulse: an oscilloscope for a life. Pick any quantitative
// series the hearth knows — your body's numbers from the health
// sidecar, or the hearth's own introspection (episodes per day) — and
// PLAY it: the trace streams past a fixed now-line like a live ticker,
// the current value ticks over as a glowing odometer, and when the
// line crosses its all-time high or low an ember flares with a
// caption. Pause and it's an ordinary explorable chart. Same compact
// dial as everywhere; overlay a second series to watch two numbers
// argue through the years.

const DAY_MS = 86400000;
const dayNum = (d) => Date.parse(`${d}T12:00:00Z`) / DAY_MS;
const numDay = (n) => new Date(n * DAY_MS).toISOString().slice(0, 10);
const css = (name, fallback) =>
  (getComputedStyle(document.documentElement).getPropertyValue(name) || fallback).trim();

// Health metrics carry a `today` reducer over /v1/health/today rows
// ({metric, source, n, sum, min, max, avg}) — the LIVE intra-day point
// the export sidecar can't have (v44 device stream). Each mirrors its
// history SQL's aggregate so today's point is comparable to the trace.
const METRICS = [
  { id: "rhr", label: "Resting heart rate", unit: "bpm", src: "health",
    sql: "SELECT day, MIN(avg) v FROM daily WHERE metric='RestingHeartRate' GROUP BY day",
    today: (rows) => aggToday(rows, (r) => r.metric === "RestingHeartRate", "avg", Math.min) },
  { id: "steps", label: "Steps", unit: "steps/day", src: "health",
    sql: "SELECT day, MAX(sum) v FROM daily WHERE metric='StepCount' GROUP BY day",
    today: (rows) => aggToday(rows, (r) => r.metric === "StepCount", "sum", Math.max) },
  { id: "sleep", label: "Sleep", unit: "h/night", src: "health",
    sql: "SELECT day, MAX(s) v FROM (SELECT day, source, SUM(sum) s FROM daily WHERE metric LIKE 'Sleep/Asleep%' GROUP BY day, source) GROUP BY day",
    today: (rows) => {
      const bySource = new Map();
      for (const r of rows) {
        if (!r.metric.startsWith("Sleep/Asleep") || r.sum == null) continue;
        bySource.set(r.source, (bySource.get(r.source) || 0) + r.sum);
      }
      return bySource.size ? Math.max(...bySource.values()) : null;
    } },
  { id: "weight", label: "Body mass", unit: "lb", src: "health",
    sql: "SELECT day, AVG(avg) v FROM daily WHERE metric='BodyMass' GROUP BY day",
    today: (rows) => aggToday(rows, (r) => r.metric === "BodyMass", "avg", "mean") },
  { id: "hrv", label: "Heart-rate variability", unit: "ms", src: "health",
    sql: "SELECT day, MAX(avg) v FROM daily WHERE metric='HeartRateVariabilitySDNN' GROUP BY day",
    today: (rows) => aggToday(rows, (r) => r.metric === "HeartRateVariabilitySDNN", "avg", Math.max) },
  { id: "glucose", label: "Blood glucose", unit: "mg/dL", src: "health",
    sql: "SELECT day, AVG(avg) v FROM daily WHERE metric='BloodGlucose' GROUP BY day",
    today: (rows) => aggToday(rows, (r) => r.metric === "BloodGlucose", "avg", "mean") },
  { id: "episodes", label: "Episodes recorded", unit: "/day", src: "histogram" },
  { id: "mail", label: "Mail", unit: "/day", src: "histogram", source: "mbox" },
  { id: "commits", label: "Commits", unit: "/day", src: "histogram", source: "git" },
  { id: "photos", label: "Photos", unit: "/day", src: "histogram", source: "photos" },
  { id: "tasks", label: "Tasks finished", unit: "/day", src: "histogram", source: "desk" },
  // Native workout numerics (the Desk's derived metric_points).
  { id: "strength", label: "Strength volume (logged)", unit: "lb·reps/day", src: "metrics",
    metric: "strength/%/volume", agg: "sum" },
  { id: "cardio-min", label: "Cardio minutes (logged)", unit: "min/day", src: "metrics",
    metric: "cardio/%/minutes", agg: "sum" },
];

/// Fold /v1/health/today rows: filter → pick a field per source →
/// combine (Math.min / Math.max / "mean"). Null when nothing matches.
function aggToday(rows, keep, field, combine) {
  const vals = rows.filter(keep).map((r) => r[field]).filter((v) => v != null);
  if (!vals.length) return null;
  if (combine === "mean") return vals.reduce((a, b) => a + b, 0) / vals.length;
  return vals.reduce((a, b) => combine(a, b));
}

// One fetch of today's live rollup per page view, shared across metrics.
let todayPromise = null;
function loadToday() {
  todayPromise ??= fetch("/v1/health/today").then((r) => r.json()).catch(() => null);
  return todayPromise;
}

const seriesCache = new Map();
async function loadSeries(metric) {
  if (seriesCache.has(metric.id)) return seriesCache.get(metric.id);
  let points = [];
  try {
    if (metric.src === "health") {
      const data = await (await fetch(
        `/v1/sources/virtual/apple-health/query?sql=${encodeURIComponent(metric.sql)}&limit=6000`)).json();
      points = (data.rows || []).map(([day, v]) => [dayNum(day), v]);
      // "Today so far": the live device stream's intra-day point — the
      // export sidecar only knows finished, exported days.
      if (metric.today) {
        const today = await loadToday();
        const v = today?.rows?.length ? metric.today(today.rows) : null;
        if (v != null) {
          const n = dayNum(today.day);
          const i = points.findIndex((p) => p[0] === n);
          if (i >= 0) points[i][1] = Math.max(points[i][1], v);
          else points.push([n, v]);
        }
      }
    } else if (metric.src === "metrics") {
      // Native derived series: rows are [day, metric, value, unit];
      // fold to one point per day (sum across exercises, or max).
      const data = await (await fetch(
        `/v1/metrics?metric=${encodeURIComponent(metric.metric)}`)).json();
      const byDay = new Map();
      for (const [day, , value] of (data.rows || [])) {
        const n = dayNum(day);
        if (metric.agg === "max") byDay.set(n, Math.max(byDay.get(n) || 0, value));
        else byDay.set(n, (byDay.get(n) || 0) + value);
      }
      points = [...byDay.entries()];
    } else {
      const p = new URLSearchParams({ from: "0000-01-01", to: "9999-12-31", bucket: "day" });
      if (metric.source) p.set("source", metric.source);
      const data = await (await fetch(`/v1/timeline/histogram?${p}`)).json();
      points = (data.buckets || []).map((b) => [dayNum(b.bucket), b.n]);
    }
  } catch { /* daemon busy */ }
  points.sort((a, b) => a[0] - b[0]);
  // milestones: all-time extremes
  let hi = null, lo = null;
  for (const p of points) {
    if (!hi || p[1] > hi[1]) hi = p;
    if (!lo || p[1] < lo[1]) lo = p;
  }
  const series = { points, hi, lo };
  seriesCache.set(metric.id, series);
  return series;
}

export async function mount(container) {
  container.innerHTML = `
    <span class="eyebrow">pulse</span>
    <h1 class="pagehead">The oscilloscope</h1>
    <p class="subhead">Any number the hearth knows, played over time — your body's
      signals or the record's own rhythm.</p>
    <div class="pulse-head">
      <select id="pulse-metric" class="pulse-pick"></select>
      <select id="pulse-compare" class="pulse-pick"><option value="">compare with…</option></select>
      <button class="btn" id="pulse-play">▶ play</button>
      <select id="pulse-speed" class="pulse-pick">
        <option value="30">1 mo/s</option>
        <option value="120" selected>4 mo/s</option>
        <option value="365">1 yr/s</option>
      </select>
      <div class="pulse-values">
        <div class="pulse-value" id="pulse-block1">
          <span class="pulse-name" id="pulse-name1"></span>
          <span id="pulse-now">—</span><span class="pulse-unit" id="pulse-unit"></span>
        </div>
        <div class="pulse-value compare" id="pulse-block2" hidden>
          <span class="pulse-name" id="pulse-name2"></span>
          <span id="pulse-now2">—</span><span class="pulse-unit" id="pulse-unit2"></span>
        </div>
      </div>
    </div>
    <div class="pulse-stage">
      <canvas id="pulse-canvas"></canvas>
      <div class="pulse-flash" id="pulse-flash" hidden></div>
    </div>
    <div id="pulse-dial"></div>`;

  const canvas = container.querySelector("#pulse-canvas");
  const ctx = canvas.getContext("2d");
  const nowEl = container.querySelector("#pulse-now");
  const unitEl = container.querySelector("#pulse-unit");
  const now2El = container.querySelector("#pulse-now2");
  const unit2El = container.querySelector("#pulse-unit2");
  const name1El = container.querySelector("#pulse-name1");
  const name2El = container.querySelector("#pulse-name2");
  const block2El = container.querySelector("#pulse-block2");
  const flashEl = container.querySelector("#pulse-flash");
  const playBtn = container.querySelector("#pulse-play");
  const speedEl = container.querySelector("#pulse-speed");
  const pickEl = container.querySelector("#pulse-metric");
  const cmpEl = container.querySelector("#pulse-compare");
  for (const m of METRICS) {
    pickEl.insertAdjacentHTML("beforeend", `<option value="${m.id}">${m.label}</option>`);
    cmpEl.insertAdjacentHTML("beforeend", `<option value="${m.id}">${m.label}</option>`);
  }

  let metric = METRICS[0];
  let compareMetric = null;
  let series = { points: [], hi: null, lo: null };
  let series2 = null;
  let shownValue2 = null;
  let cursor = 0;          // day number at the now-line
  let span = 365 * 2;      // days visible
  let playing = false;
  let raf = 0, lastT = 0;
  let shownValue = null;   // odometer easing
  let milestonesFired = new Set();
  let W = 0, H = 0;

  function resize() {
    const dpr = window.devicePixelRatio || 1;
    W = canvas.parentElement.clientWidth;
    H = 320;
    canvas.width = W * dpr;
    canvas.height = H * dpr;
    canvas.style.width = `${W}px`;
    canvas.style.height = `${H}px`;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }

  const NOW_X = () => W * 0.82; // the fixed now-line during playback

  function valueAt(pts, day) {
    // nearest-or-interpolated value at a fractional day
    if (!pts.length) return null;
    let lo = 0, hi = pts.length - 1;
    if (day <= pts[0][0]) return null;
    if (day >= pts[hi][0]) return pts[hi][1];
    while (hi - lo > 1) {
      const mid = (lo + hi) >> 1;
      (pts[mid][0] <= day ? (lo = mid) : (hi = mid));
    }
    const [d0, v0] = pts[lo], [d1, v1] = pts[hi];
    if (d1 - d0 > 14) return pts[lo][0] === day ? v0 : null; // gap: no line
    return v0 + (v1 - v0) * ((day - d0) / (d1 - d0));
  }

  function drawSeries(pts, color, yMin, yMax, fill) {
    const x0 = cursor - span, x1 = cursor + (playing ? span * 0.22 : 0);
    const X = (d) => ((d - x0) / (cursor - x0)) * NOW_X();
    const Y = (v) => 24 + (H - 60) * (1 - (v - yMin) / Math.max(yMax - yMin, 1e-9));
    ctx.beginPath();
    let pen = false;
    let prevD = null;
    for (const [d, v] of pts) {
      if (d < x0 - 2 || d > (playing ? cursor : x1) + 2) continue;
      if (d > cursor && playing) break;
      const x = X(d), y = Y(v);
      if (!pen || (prevD !== null && d - prevD > 14)) { ctx.moveTo(x, y); pen = true; }
      else ctx.lineTo(x, y);
      prevD = d;
    }
    ctx.strokeStyle = color;
    ctx.lineWidth = 2;
    ctx.shadowColor = color;
    ctx.shadowBlur = 6;
    ctx.stroke();
    ctx.shadowBlur = 0;
    if (fill) {
      ctx.lineTo(NOW_X(), H - 36);
      ctx.lineTo(0, H - 36);
      ctx.globalAlpha = 0.08;
      ctx.fillStyle = color;
      ctx.fill();
      ctx.globalAlpha = 1;
    }
    // the live head
    const v = valueAt(pts, cursor);
    if (v != null) {
      ctx.fillStyle = css("--ember-100", "#ffe8c8");
      ctx.beginPath();
      ctx.arc(NOW_X(), Y(v), 3.4, 0, Math.PI * 2);
      ctx.fill();
    }
    return v;
  }

  function windowExtent(pts) {
    const x0 = cursor - span;
    let mn = Infinity, mx = -Infinity;
    for (const [d, v] of pts) {
      if (d < x0 || d > cursor) continue;
      if (v < mn) mn = v;
      if (v > mx) mx = v;
    }
    if (mn === Infinity) return null;
    const pad = (mx - mn) * 0.15 || 1;
    return [mn - pad, mx + pad];
  }

  function draw() {
    ctx.clearRect(0, 0, W, H);
    const line = css("--line", "#37302a");
    const faint = css("--ink-faint", "#6f6455");
    // baseline + year gridlines
    ctx.strokeStyle = line;
    ctx.beginPath();
    ctx.moveTo(0, H - 36);
    ctx.lineTo(W, H - 36);
    ctx.stroke();
    const x0 = cursor - span;
    for (let d = Math.ceil(x0); d <= cursor; d++) {
      const iso = numDay(d);
      if (!iso.endsWith("-01-01")) continue;
      const x = ((d - x0) / span) * NOW_X();
      ctx.globalAlpha = 0.35;
      ctx.strokeStyle = line;
      ctx.beginPath(); ctx.moveTo(x, 24); ctx.lineTo(x, H - 36); ctx.stroke();
      ctx.globalAlpha = 1;
      ctx.fillStyle = faint;
      ctx.font = `500 9.5px ${css("--font-mono", "monospace")}`;
      ctx.textAlign = "center";
      ctx.fillText(iso.slice(0, 4), x, H - 22);
    }
    // now-line
    ctx.strokeStyle = css("--accent", "#e8833a");
    ctx.globalAlpha = 0.8;
    ctx.beginPath();
    ctx.moveTo(NOW_X(), 16);
    ctx.lineTo(NOW_X(), H - 30);
    ctx.stroke();
    ctx.globalAlpha = 1;
    ctx.fillStyle = faint;
    ctx.font = `500 9.5px ${css("--font-mono", "monospace")}`;
    ctx.textAlign = "center";
    ctx.fillText(numDay(cursor), NOW_X(), 12);

    const c1 = css("--chart-1", "#e8833a");
    const c2 = css("--chart-2", "#6fb3ab");
    const fmtAxis = (x) => Math.abs(x) >= 1000
      ? Math.round(x).toLocaleString() : (Math.round(x * 10) / 10).toLocaleString();
    const ext1 = windowExtent(series.points);
    let v = null, v2 = null;
    if (ext1) {
      v = drawSeries(series.points, c1, ext1[0], ext1[1], true);
      // primary scale lives on the LEFT edge, in its own color
      ctx.fillStyle = c1;
      ctx.font = `600 9.5px ${css("--font-mono", "monospace")}`;
      ctx.textAlign = "left";
      ctx.fillText(fmtAxis(ext1[1]), 6, 22);
      ctx.fillText(fmtAxis(ext1[0]), 6, H - 42);
      ctx.fillText(metric.label.toLowerCase(), 6, 34);
    }
    if (series2 && compareMetric) {
      const ext2 = windowExtent(series2.points);
      if (ext2) {
        v2 = drawSeries(series2.points, c2, ext2[0], ext2[1], false);
        // compare scale lives past the now-line on the RIGHT, in ITS color
        ctx.fillStyle = c2;
        ctx.font = `600 9.5px ${css("--font-mono", "monospace")}`;
        ctx.textAlign = "right";
        ctx.fillText(fmtAxis(ext2[1]), W - 6, 22);
        ctx.fillText(fmtAxis(ext2[0]), W - 6, H - 42);
        ctx.fillText(compareMetric.label.toLowerCase(), W - 6, 34);
      }
    }

    // odometers easing toward the live values, one per series
    const tick = (el, shown, val) => {
      if (val == null) { el.textContent = "—"; return null; }
      const next = shown == null ? val : shown + (val - shown) * 0.25;
      el.textContent = Math.abs(next) >= 1000
        ? Math.round(next).toLocaleString()
        : (Math.round(next * 10) / 10).toLocaleString();
      return next;
    };
    shownValue = tick(nowEl, shownValue, v);
    if (series2) shownValue2 = tick(now2El, shownValue2, v2);

    // milestone embers
    for (const [tag, pt] of [["high", series.hi], ["low", series.lo]]) {
      if (!pt || milestonesFired.has(tag) || !playing) continue;
      if (Math.abs(pt[0] - cursor) < 1.2) {
        milestonesFired.add(tag);
        flashEl.hidden = false;
        flashEl.textContent = `all-time ${tag}: ${pt[1].toLocaleString()} ${metric.unit} · ${numDay(pt[0])}`;
        flashEl.classList.remove("in");
        void flashEl.offsetWidth;
        flashEl.classList.add("in");
        setTimeout(() => { flashEl.hidden = true; }, 2600);
      }
    }
  }

  /* — playback — */
  function frame(now) {
    if (!playing) return;
    if (lastT) cursor += ((now - lastT) / 1000) * Number(speedEl.value);
    lastT = now;
    const end = series.points.length ? series.points[series.points.length - 1][0] + 8 : cursor;
    if (cursor >= end) { pause(); cursor = end; }
    dial?.setWindow(numDay(cursor - span), numDay(cursor));
    draw();
    if (playing) raf = requestAnimationFrame(frame);
  }
  function play() {
    if (!series.points.length) return;
    playing = true;
    lastT = 0;
    milestonesFired = new Set();
    if (cursor >= series.points[series.points.length - 1][0]) {
      cursor = series.points[0][0] + span * 0.4; // rewind to the start
    }
    playBtn.textContent = "◼ pause";
    raf = requestAnimationFrame(frame);
  }
  function pause() {
    playing = false;
    cancelAnimationFrame(raf);
    playBtn.textContent = "▶ play";
    draw();
  }
  playBtn.onclick = () => (playing ? pause() : play());

  /* — selection + dial — */
  async function setMetric() {
    metric = METRICS.find((m) => m.id === pickEl.value) || METRICS[0];
    unitEl.textContent = metric.unit;
    name1El.textContent = metric.label;
    series = await loadSeries(metric);
    if (series.points.length) {
      cursor = series.points[series.points.length - 1][0];
      const first = series.points[0][0];
      span = Math.min(Math.max(cursor - first, 90), 365 * 4);
    }
    shownValue = null;
    pause();
  }
  pickEl.onchange = setMetric;
  cmpEl.onchange = async () => {
    compareMetric = METRICS.find((x) => x.id === cmpEl.value) || null;
    series2 = compareMetric ? await loadSeries(compareMetric) : null;
    shownValue2 = null;
    block2El.hidden = !compareMetric;
    if (compareMetric) {
      name2El.textContent = compareMetric.label;
      unit2El.textContent = compareMetric.unit;
    }
    draw();
  };

  let dial = null;
  {
    const { mountDial } = await import("/assets/dial.js");
    dial = mountDial(container.querySelector("#pulse-dial"), {
      compact: true,
      // chapter headings ride the ticks — the era you're hearing,
      // named while the trace plays through it
      labels: true,
      labelFilter: (ev) => ev.kind === "chapter",
      filters: () => ({}),
      onWindow: (from, to) => {
        pause();
        cursor = dayNum(to);
        span = Math.max(dayNum(to) - dayNum(from), 30);
        draw();
      },
      onPick: () => {},
    });
  }

  window.addEventListener("resize", resize, { passive: true });
  await setMetric();
  resize();
  dial?.setWindow(numDay(cursor - span), numDay(cursor));

  return () => {
    pause();
    window.removeEventListener("resize", resize);
  };
}
