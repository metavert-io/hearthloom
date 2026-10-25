// biography.js — the book your data wrote, woven by year: each spread
// carries that year's heatmap strip (every day an ember cell), the
// LLM's chapter headings (linked into Memories at that month), and a
// year-clock of rhythms — health rings when your body reported, and
// always the outer "record" ring (episodes/day), so no year is blank.
// Everything comes from existing endpoints — composition, not new data.

const DAY_MS = 86400000;
const dayNum = (d) => Date.parse(`${d}T12:00:00Z`) / DAY_MS;
const css = (name, fallback) =>
  (getComputedStyle(document.documentElement).getPropertyValue(name) || fallback).trim();
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

async function fetchJSON(url) {
  return (await fetch(url)).json();
}


/* — one floating tooltip for every canvas in the book (native title
   tooltips are slow and easy to miss; this follows the cursor) — */
let bioTipEl = null;
function bioTip(canvas, e, text) {
  if (!bioTipEl) {
    bioTipEl = document.createElement("div");
    bioTipEl.className = "bio-tip";
    bioTipEl.hidden = true;
    document.body.appendChild(bioTipEl);
  }
  if (!text || !e) { bioTipEl.hidden = true; return; }
  bioTipEl.textContent = text;
  bioTipEl.hidden = false;
  const pad = 14;
  const w = bioTipEl.offsetWidth || 120;
  const x = Math.min(e.clientX + pad, window.innerWidth - w - 8);
  bioTipEl.style.left = `${x}px`;
  bioTipEl.style.top = `${e.clientY - 34}px`;
}

/* — one year's heatmap strip — */
function drawYearStrip(canvas, year, days, maxN) {
  const ctx = canvas.getContext("2d");
  const CELL = 9, GAP = 2;
  const W = 54 * (CELL + GAP) + 8, H = 7 * (CELL + GAP);
  const dpr = window.devicePixelRatio || 1;
  canvas.width = W * dpr; canvas.height = H * dpr;
  canvas.style.width = `${W}px`; canvas.style.height = `${H}px`;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  const ember = css("--chart-1", "#e8833a");
  const cellBg = css("--bg-raise", "#1d1915");
  const jan1 = dayNum(`${year}-01-01`);
  const startDow = new Date(jan1 * DAY_MS).getUTCDay();
  const daysInYear = (dayNum(`${year}-12-31`) - jan1) + 1;
  const cells = [];
  for (let i = 0; i < daysInYear; i++) {
    const iso = new Date((jan1 + i) * DAY_MS).toISOString().slice(0, 10);
    const dow = (startDow + i) % 7;
    const week = Math.floor((startDow + i) / 7);
    const x = 4 + week * (CELL + GAP);
    const y = dow * (CELL + GAP);
    const n = days.get(iso) || 0;
    ctx.fillStyle = cellBg;
    ctx.fillRect(x, y, CELL, CELL);
    if (n > 0) {
      ctx.globalAlpha = 0.18 + 0.82 * Math.pow(n / maxN, 0.4);
      ctx.fillStyle = ember;
      ctx.fillRect(x, y, CELL, CELL);
      ctx.globalAlpha = 1;
    }
    cells.push({ x, y, iso, n });
  }
  const CELLSZ = CELL;
  const hit = (e) => cells.find((c) =>
    e.offsetX >= c.x && e.offsetX < c.x + CELLSZ && e.offsetY >= c.y && e.offsetY < c.y + CELLSZ);
  canvas.onmousemove = (e) => {
    const c = hit(e);
    canvas.style.cursor = c && c.n ? "pointer" : "";
    bioTip(canvas, e, c ? `${c.iso} · ${c.n.toLocaleString()} episode${c.n === 1 ? "" : "s"}${c.n ? " — click for the day" : ""}` : "");
  };
  canvas.onmouseleave = () => bioTip(canvas, null, "");
  canvas.onclick = (e) => {
    const c = hit(e);
    if (c && c.n > 0) location.hash = `#/day/${c.iso}`;
  };
}

/* — the year-clock: rhythm rings + hover + legend — */
const RINGS = [
  { id: "record", label: "record", color: () => css("--chart-3", "#d9a441"), unit: "episodes" },
  { id: "steps", label: "activity", color: () => css("--chart-1", "#e8833a"), unit: "steps" },
  { id: "sleep", label: "sleep", color: () => css("--chart-2", "#6fb3ab"), unit: "h" },
  { id: "rhr", label: "heart", color: () => css("--chart-4", "#c96a5e"), unit: "bpm" },
];

async function drawYearClock(canvas, legendEl, year, days) {
  const bands = { record: new Map(), steps: new Map(), sleep: new Map(), rhr: new Map() };
  for (const [d, n] of days) {
    if (d.startsWith(String(year))) bands.record.set(d, n);
  }
  try {
    const sql = `SELECT day, metric, avg, sum FROM daily WHERE metric IN ` +
      `('StepCount','RestingHeartRate') AND day LIKE '${year}-%'`;
    const sleepSql = `SELECT day, MAX(s) FROM (SELECT day, source, SUM(sum) s FROM daily ` +
      `WHERE metric LIKE 'Sleep/Asleep%' AND day LIKE '${year}-%' GROUP BY day, source) GROUP BY day`;
    const [a, b] = await Promise.all([
      fetchJSON(`/v1/sources/virtual/apple-health/query?sql=${encodeURIComponent(sql)}&limit=800`),
      fetchJSON(`/v1/sources/virtual/apple-health/query?sql=${encodeURIComponent(sleepSql)}&limit=400`),
    ]);
    for (const row of (a.rows || [])) {
      const [day, metric, avg, sum] = row;
      if (metric === "StepCount") bands.steps.set(day, sum ?? avg);
      else bands.rhr.set(day, avg);
    }
    for (const [day, s] of (b.rows || [])) bands.sleep.set(day, s);
  } catch { /* pre-health years: the record ring still draws */ }

  const active = RINGS.filter((r) => bands[r.id].size > 3);
  if (!active.length) return false;

  const S = 220, R = S / 2;
  const dpr = window.devicePixelRatio || 1;
  canvas.width = S * dpr; canvas.height = S * dpr;
  canvas.style.width = `${S}px`; canvas.style.height = `${S}px`;
  const ctx = canvas.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.translate(R, R);

  const jan1 = dayNum(`${year}-01-01`);
  const daysInYear = (dayNum(`${year}-12-31`) - jan1) + 1;
  const ringGeom = [];
  let rOuter = R - 8;
  const RING_W = Math.min(16, (R - 44) / active.length - 4);
  for (const ring of active) {
    const band = bands[ring.id];
    const r1 = rOuter, r0 = rOuter - RING_W;
    rOuter = r0 - 4;
    const max = Math.max(...band.values());
    for (const [day, v] of band) {
      const i = dayNum(day) - jan1;
      const a0 = (i / daysInYear) * Math.PI * 2 - Math.PI / 2;
      const a1 = ((i + 1.6) / daysInYear) * Math.PI * 2 - Math.PI / 2;
      ctx.strokeStyle = ring.color();
      ctx.globalAlpha = 0.25 + 0.75 * (v / max);
      ctx.lineWidth = RING_W;
      ctx.beginPath();
      ctx.arc(0, 0, (r0 + r1) / 2, a0, a1);
      ctx.stroke();
    }
    ctx.globalAlpha = 1;
    ringGeom.push({ id: ring.id, r0, r1, unit: ring.unit, label: ring.label });
  }
  ctx.fillStyle = css("--ink-mute", "#a89a86");
  ctx.font = `700 20px ${css("--font-display", "serif")}`;
  ctx.textAlign = "center";
  ctx.fillText(String(year), 0, 7);

  // hover: angle → day, radius → ring, value in the tooltip
  canvas.onmousemove = (e) => {
    const x = e.offsetX - R, y = e.offsetY - R;
    const r = Math.hypot(x, y);
    const geom = ringGeom.find((g) => r >= g.r0 - 2 && r <= g.r1 + 2);
    if (!geom) { bioTip(canvas, null, ""); return; }
    let angle = Math.atan2(y, x) + Math.PI / 2;
    if (angle < 0) angle += Math.PI * 2;
    const day = new Date((jan1 + Math.floor((angle / (Math.PI * 2)) * daysInYear)) * DAY_MS)
      .toISOString().slice(0, 10);
    const v = bands[geom.id].get(day);
    bioTip(canvas, e, v != null
      ? `${day} · ${geom.label}: ${Math.round(v * 10) / 10} ${geom.unit}`
      : `${day} · ${geom.label}: no reading`);
  };
  canvas.onmouseleave = () => bioTip(canvas, null, "");

  legendEl.innerHTML = active.map((r) =>
    `<span class="bio-legend-item"><span class="bio-dot" style="background:${r.color()}"></span>${r.label}</span>`
  ).join("");
  return true;
}

export async function mount(container) {
  container.innerHTML = `
    <span class="eyebrow">biography</span>
    <h1 class="pagehead">The book your data wrote</h1>
    <p class="subhead">The years as chapters — every day an ember, headings by the
      hearth's own hand, rhythms from your body and the record itself.</p>
    <div id="bio-years"><div class="loading">binding the chapters…</div></div>`;

  const hist = await fetchJSON(`/v1/timeline/histogram?from=0000-01-01&to=9999-12-31&bucket=day`);
  const days = new Map(hist.buckets.map((b) => [b.bucket, b.n]));
  const isoDays = [...days.keys()].sort();
  const wrap = container.querySelector("#bio-years");
  if (!isoDays.length) {
    wrap.innerHTML = `<div class="empty">nothing recorded yet</div>`;
    return () => {};
  }
  // clamp to substantial years (a 1971 artifact must not add empty decades)
  const perYear = new Map();
  for (const d of isoDays) {
    const y = d.slice(0, 4);
    perYear.set(y, (perYear.get(y) || 0) + days.get(d));
  }
  const realYears = [...perYear.entries()].filter(([, n]) => n >= 15).map(([y]) => +y);
  const thisYear = new Date().getFullYear();
  const firstYear = Math.min(...realYears);
  const lastYear = Math.min(Math.max(...realYears), thisYear);
  const maxN = Math.max(1, ...days.values());

  wrap.innerHTML = "";
  // Newest year first — you open the book at the present and read back.
  for (let y = lastYear; y >= firstYear; y--) {
    const total = perYear.get(String(y)) || 0;
    if (!total) continue;
    const spread = document.createElement("section");
    spread.className = "bio-year";
    spread.innerHTML = `
      <div class="bio-year-head">
        <h2>${y}</h2>
        <span class="meta">${total.toLocaleString()} episodes</span>
      </div>
      <canvas class="bio-strip" data-year="${y}"></canvas>
      <div class="bio-year-body">
        <div class="bio-chapters" data-year="${y}"><div class="meta">…</div></div>
        <div class="bio-clockwrap">
          <canvas class="bio-clock" data-year="${y}" hidden></canvas>
          <div class="bio-legend"></div>
        </div>
      </div>`;
    wrap.appendChild(spread);
  }

  const seen = new Set();
  const io = new IntersectionObserver((entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      const el = entry.target;
      const y = el.querySelector(".bio-strip")?.dataset.year;
      if (!y || seen.has(y)) continue;
      seen.add(y);
      drawYearStrip(el.querySelector(".bio-strip"), +y, days, maxN);
      // chapters, linked into Memories at their month
      fetchJSON(`/v1/browse/episodes?kind=chapter&from=${y}-01-01&to=${y}-12-31&sort=time&dir=asc&limit=12`)
        .then((data) => {
          const chapters = (data.items || []).map((r) => ({
            title: r.title,
            month: (r.event_time || "").slice(0, 7),
          }));
          const host = el.querySelector(".bio-chapters");
          const monthName = (m) => new Date(Date.UTC(+m.slice(0, 4), +m.slice(5, 7) - 1, 15))
            .toLocaleString(undefined, { month: "long" });
          host.innerHTML = chapters.length
            ? chapters.map((c) => `<a class="bio-chapter" href="#/browse/episodes"
                data-month="${esc(c.month)}"><span class="bio-chapter-month">${esc(monthName(c.month))}</span> — ${esc(c.title)}</a>`).join("")
            : `<div class="meta">no chapter headings yet — the hearth writes them at idle</div>`;
          host.querySelectorAll(".bio-chapter").forEach((a) => {
            a.addEventListener("click", () => {
              const m = a.dataset.month;
              const last = new Date(Date.UTC(+m.slice(0, 4), +m.slice(5, 7), 0))
                .toISOString().slice(0, 10);
              window.__browsePrefill?.({ from: `${m}-01`, to: last, q: "", place: "" });
            });
          });
        }).catch(() => {});
      const clock = el.querySelector(".bio-clock");
      drawYearClock(clock, el.querySelector(".bio-legend"), +y, days)
        .then((drew) => { clock.hidden = !drew; }).catch(() => {});
    }
  }, { rootMargin: "400px" });
  wrap.querySelectorAll(".bio-year").forEach((el) => io.observe(el));

  return () => io.disconnect();
}
