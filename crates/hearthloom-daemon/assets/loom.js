// loom.js — the Loom: the record as woven cloth. Every source is a
// thread; every month the weave thickens where that part of life ran
// hot — mail and calendars as the warp of decades, then git arrives,
// photos bloom, health starts reporting, the desk begins recording what
// you chose to do. It's the app's namesake made literal: a streamgraph
// (ThemeRiver wiggle layout, sqrt-scaled so no thread drowns the rest),
// hand-rolled on one canvas with catmull-rom smoothing and the embers
// palette. Hover follows a thread and reads out source · month · count;
// click drops you into Memories at that month; the legend can rest
// threads to compare the remainder.

const css = (name, fallback) =>
  (getComputedStyle(document.documentElement).getPropertyValue(name) || fallback).trim();
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

const PALETTE_VARS = [
  "--chart-1", "--chart-2", "--chart-3", "--chart-4",
  "--chart-5", "--chart-6", "--chart-7", "--chart-8",
];

async function fetchJSON(url) {
  return (await fetch(url)).json();
}

/* Month arithmetic on "YYYY-MM" keys. */
const monthIndex = (m) => (+m.slice(0, 4)) * 12 + (+m.slice(5, 7)) - 1;
const indexMonth = (i) => `${String(Math.floor(i / 12)).padStart(4, "0")}-${String((i % 12) + 1).padStart(2, "0")}`;

export async function mount(container) {
  container.innerHTML = `
    <div class="loom-wrap">
      <div class="loom-head">
        <span class="eyebrow">the loom</span>
        <h1 class="pagehead">The weave of the record</h1>
        <p class="subhead">Every source a thread, every month a pass of the shuttle —
          watch parts of your life join the weave. Thickness is √-scaled so the mail
          decades don't drown the rest; hover follows a thread, click opens the month.</p>
      </div>
      <canvas id="loom-canvas"></canvas>
      <div class="loom-tip" id="loom-tip" hidden></div>
      <div class="loom-legend" id="loom-legend"></div>
      <div id="loom-dial"></div>
    </div>`;

  const canvas = container.querySelector("#loom-canvas");
  const tip = container.querySelector("#loom-tip");
  const legendEl = container.querySelector("#loom-legend");

  // Start the dial module fetch NOW — the data waterfall below saturates
  // the connection pool, and a serial import made the dial arrive seconds
  // after the cloth.
  const dialImport = import("/assets/dial.js");

  // — data: monthly histogram per source (small: one row per month);
  //   the masthead already fetched /v1/stats once — reuse it —
  const stats = window.__stats || await fetchJSON("/v1/stats");
  const sources = (stats.sources || []).slice(0, PALETTE_VARS.length + 2);
  const series = await Promise.all(sources.map(async ([name]) => {
    const data = await fetchJSON(
      `/v1/timeline/histogram?from=0000-01-01&to=9999-12-31&bucket=month&source=${encodeURIComponent(name)}`);
    return { name, months: new Map((data.buckets || []).map((b) => [b.bucket, b.n])) };
  }));
  const threads = series
    .filter((s) => s.months.size > 0)
    .map((s, i) => ({
      ...s,
      color: css(PALETTE_VARS[i % PALETTE_VARS.length], "#e8833a"),
      total: [...s.months.values()].reduce((a, b) => a + b, 0),
      resting: false,
    }));
  if (!threads.length) {
    container.querySelector(".loom-wrap").innerHTML = `<div class="empty">nothing woven yet</div>`;
    return () => {};
  }

  // Substantial months only (a 1971 ICS artifact must not stretch the
  // warp across empty decades — the Biography clamp, reused).
  const monthTotals = new Map();
  for (const t of threads) {
    for (const [m, n] of t.months) monthTotals.set(m, (monthTotals.get(m) || 0) + n);
  }
  const substantial = [...monthTotals.entries()].filter(([, n]) => n >= 5).map(([m]) => m).sort();
  // full data extent; the dial narrows the view within it
  const dataM0 = monthIndex(substantial[0]);
  const dataM1 = monthIndex(substantial[substantial.length - 1]);
  let m0 = dataM0, m1 = dataM1;
  let monthCount = m1 - m0 + 1;
  let months = Array.from({ length: monthCount }, (_, i) => indexMonth(m0 + i));
  function setRange(fromISO, toISO) {
    const a = Math.max(dataM0, Math.min(dataM1, monthIndex(fromISO.slice(0, 7))));
    const b = Math.max(a + 1, Math.min(dataM1, monthIndex(toISO.slice(0, 7))));
    if (a === m0 && b === m1) return;
    m0 = a;
    m1 = b;
    monthCount = m1 - m0 + 1;
    months = Array.from({ length: monthCount }, (_, i) => indexMonth(m0 + i));
    hover = null;
    weave();
    draw();
  }

  let layout = null; // {stack: thread → Float64Array tops/bottoms, maxSpan}
  function weave() {
    const active = threads.filter((t) => !t.resting);
    // √-scaled values, lightly smoothed along time so threads flow.
    const vals = active.map((t) => {
      const raw = months.map((m) => Math.sqrt(t.months.get(m) || 0));
      const smooth = raw.map((v, i) =>
        (raw[i - 1] ?? v) * 0.25 + v * 0.5 + (raw[i + 1] ?? v) * 0.25);
      return smooth;
    });
    // ThemeRiver wiggle: center the stack on a moving baseline.
    const totals = months.map((_, i) => vals.reduce((a, v) => a + v[i], 0));
    const base = totals.map((t) => -t / 2);
    const bands = active.map(() => ({ top: new Float64Array(monthCount), bot: new Float64Array(monthCount) }));
    for (let i = 0; i < monthCount; i++) {
      let y = base[i];
      for (let k = 0; k < active.length; k++) {
        bands[k].bot[i] = y;
        y += vals[k][i];
        bands[k].top[i] = y;
      }
    }
    layout = { active, bands, maxSpan: Math.max(...totals, 1) };
  }

  // — canvas plumbing (explicit CSS size: a canvas is a replaced
  //   element and will NOT stretch from inset/percentages of buffer) —
  let W = 0, H = 0, dpr = 1;
  function resize() {
    W = container.clientWidth;
    H = Math.max(420, Math.min(640, window.innerHeight - 360));
    dpr = window.devicePixelRatio || 1;
    canvas.width = W * dpr;
    canvas.height = H * dpr;
    canvas.style.width = `${W}px`;
    canvas.style.height = `${H}px`;
    draw();
  }

  const PAD = { left: 18, right: 18, top: 26, bottom: 34 };
  const x = (i) => PAD.left + (i / (monthCount - 1)) * (W - PAD.left - PAD.right);
  let yScale = 1, yMid = 0;

  let hover = null; // {threadIdx (into layout.active), monthIdx}
  function draw() {
    const ctx = canvas.getContext("2d");
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    if (!layout) weave();
    const { active, bands, maxSpan } = layout;
    yMid = PAD.top + (H - PAD.top - PAD.bottom) / 2;
    yScale = (H - PAD.top - PAD.bottom) / maxSpan;

    // year gridlines behind the cloth
    ctx.strokeStyle = css("--line", "#37302a");
    ctx.fillStyle = css("--ink-faint", "#6b6154");
    ctx.font = `10px ${css("--font-mono", "monospace")}`;
    ctx.textAlign = "center";
    ctx.lineWidth = 1;
    const firstYear = Math.ceil((m0 + 11) / 12);
    for (let y = firstYear; y * 12 <= m1; y++) {
      const i = y * 12 - m0;
      if (i < 0 || i >= monthCount) continue;
      const px = x(i);
      ctx.globalAlpha = 0.5;
      ctx.beginPath();
      ctx.moveTo(px, PAD.top);
      ctx.lineTo(px, H - PAD.bottom);
      ctx.stroke();
      ctx.globalAlpha = 1;
      if ((W / (m1 / 12 - m0 / 12)) > 28 || y % 5 === 0) {
        ctx.fillText(String(y), px, H - PAD.bottom + 16);
      }
    }

    // threads, smoothed with midpoint-quadratic curves
    const trace = (arr, backward) => {
      const pts = [];
      for (let i = 0; i < monthCount; i++) pts.push([x(i), yMid + arr[i] * yScale]);
      if (backward) pts.reverse();
      ctx.lineTo(pts[0][0], pts[0][1]);
      for (let i = 1; i < pts.length - 1; i++) {
        const mx = (pts[i][0] + pts[i + 1][0]) / 2;
        const my = (pts[i][1] + pts[i + 1][1]) / 2;
        ctx.quadraticCurveTo(pts[i][0], pts[i][1], mx, my);
      }
      ctx.lineTo(pts[pts.length - 1][0], pts[pts.length - 1][1]);
    };
    active.forEach((t, k) => {
      const focused = hover && hover.threadIdx === k;
      const dimmed = hover && !focused;
      ctx.beginPath();
      ctx.moveTo(x(0), yMid + bands[k].bot[0] * yScale);
      trace(bands[k].bot, false);
      trace(bands[k].top, true);
      ctx.closePath();
      ctx.globalAlpha = dimmed ? 0.18 : focused ? 0.95 : 0.78;
      ctx.fillStyle = t.color;
      if (focused) {
        ctx.shadowColor = t.color;
        ctx.shadowBlur = 18;
      }
      ctx.fill();
      ctx.shadowBlur = 0;
      ctx.globalAlpha = 1;
    });

    // hover crosshair month
    if (hover) {
      const px = x(hover.monthIdx);
      ctx.strokeStyle = css("--ink-mute", "#a89a86");
      ctx.globalAlpha = 0.6;
      ctx.setLineDash([3, 4]);
      ctx.beginPath();
      ctx.moveTo(px, PAD.top);
      ctx.lineTo(px, H - PAD.bottom);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.globalAlpha = 1;
    }
  }

  function hitTest(e) {
    const rect = canvas.getBoundingClientRect();
    const mx = e.clientX - rect.left;
    const my = e.clientY - rect.top;
    if (!layout) return null;
    const i = Math.round(((mx - PAD.left) / (W - PAD.left - PAD.right)) * (monthCount - 1));
    if (i < 0 || i >= monthCount) return null;
    const yVal = (my - yMid) / yScale;
    for (let k = 0; k < layout.active.length; k++) {
      if (yVal >= layout.bands[k].bot[i] && yVal <= layout.bands[k].top[i]) {
        return { threadIdx: k, monthIdx: i };
      }
    }
    return null;
  }

  canvas.addEventListener("mousemove", (e) => {
    const hit = hitTest(e);
    const changed = JSON.stringify(hit) !== JSON.stringify(hover);
    hover = hit;
    if (hit) {
      const t = layout.active[hit.threadIdx];
      const month = months[hit.monthIdx];
      const n = t.months.get(month) || 0;
      tip.hidden = false;
      tip.innerHTML = `<b style="color:${t.color}">${esc(t.name)}</b> · ${esc(month)} ·
        ${n.toLocaleString()} episode${n === 1 ? "" : "s"} <span class="meta">— click to open the month</span>`;
      const rect = canvas.getBoundingClientRect();
      tip.style.left = `${Math.min(e.clientX - rect.left + 14, W - 260)}px`;
      tip.style.top = `${e.clientY - rect.top - 34}px`;
      canvas.style.cursor = "pointer";
    } else {
      tip.hidden = true;
      canvas.style.cursor = "";
    }
    if (changed) draw();
  });
  canvas.addEventListener("mouseleave", () => {
    hover = null;
    tip.hidden = true;
    draw();
  });
  canvas.addEventListener("click", (e) => {
    const hit = hitTest(e);
    if (!hit) return;
    const month = months[hit.monthIdx];
    const last = new Date(Date.UTC(+month.slice(0, 4), +month.slice(5, 7), 0))
      .toISOString().slice(0, 10);
    window.__browsePrefill?.({ from: `${month}-01`, to: last, q: "", place: "" });
    location.hash = "#/browse/episodes";
  });

  // legend: rest/wake threads
  function paintLegend() {
    legendEl.innerHTML = threads.map((t, i) => `
      <button class="loom-chip${t.resting ? " resting" : ""}" data-i="${i}">
        <span class="loom-dot" style="background:${t.color}"></span>
        ${esc(t.name)} <span class="meta">${t.total.toLocaleString()}</span>
      </button>`).join("");
    legendEl.querySelectorAll(".loom-chip").forEach((chip) => {
      chip.onclick = () => {
        const t = threads[+chip.dataset.i];
        // never rest the last thread standing
        if (!t.resting && threads.filter((x) => !x.resting).length === 1) return;
        t.resting = !t.resting;
        hover = null;
        weave();
        draw();
        paintLegend();
      };
    });
  }

  weave();
  paintLegend();
  resize();
  const ro = new ResizeObserver(resize);
  ro.observe(container);

  // The dial: pinch and spin the visible span of the weave; chapter
  // headings ride its ticks. Defaults to everything → present.
  try {
    const { mountDial } = await dialImport;
    mountDial(container.querySelector("#loom-dial"), {
      from: `${indexMonth(dataM0)}-01`,
      to: new Date().toISOString().slice(0, 10),
      labelFilter: (ev) => ev.kind === "chapter",
      filters: () => ({}),
      onWindow: (from, to) => setRange(from, to),
      onPick: () => {},
    });
  } catch (err) {
    console.warn("loom dial failed to mount:", err); // degraded, not silent
  }
  return () => ro.disconnect();
}
