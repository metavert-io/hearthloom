// spend.js — ☰ → Spend: what the thinking costs. Every AI model call
// Hearthloom makes lands in the usage ledger (llm_usage, v50); this page
// prices it and breaks it down — headline figures, spend per day stacked
// by model / purpose / provider / tier, a model × purpose grid, what
// each question cost, and the full report table with CSV export.
//
// One request (/v1/llm/spend) returns day × provider × model × tier ×
// purpose cells; every regrouping below happens here, so switching the
// breakdown never refetches. Costs are computed server-side at read time
// from the price table — a model with no price on file shows tokens and
// "no price on file", never an invented figure.
//
// Vault: the endpoint 403s while locked (spend patterns can reveal
// vaulted activity); the page then shows only the unlock card.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

let api;
let apiPost;
let openTrace;

const DIMS = [
  { key: "model", label: "model" },
  { key: "purpose", label: "purpose" },
  { key: "provider", label: "provider" },
  { key: "tier", label: "tier" },
];
const PRESETS = [
  { days: 7, label: "7 days" },
  { days: 30, label: "30 days" },
  { days: 90, label: "90 days" },
  { days: 0, label: "all" },
];
const NODES = [
  { value: "", label: "both nodes" },
  { value: "local", label: "local-device" },
  { value: "cloud", label: "cloud-based" },
];
// What a purpose string means to a person (ledger values are pass names).
const PURPOSE_LABEL = {
  ask: "Ask", ask_map: "Ask · row reads", extraction: "entity extraction",
  classification: "scope classification", daily_digest: "daily digest",
  chapters: "chapters", cascades: "cascades", self_model: "self-model",
  intent_decomposition: "goal decomposition", relationship_predict: "relationship kinds",
  relationship_briefs: "relationship briefs", topic_pages: "topic pages",
  weekly_review: "weekly review", lint: "record lint",
  embed_backfill: "embeddings (on this machine)",
};
const SERIES_SLOTS = 7; // --chart-1..7 carry identity; --chart-8 is "other"
const OTHER = "other";

const state = { days: 30, from: "", to: "", node: "", dim: "model", sort: { col: "day", dir: -1 }, showAll: false, editBudget: false };
let data = null;

/* ————— formatting ————— */
const usd = (n) => {
  if (n == null || Number.isNaN(n)) return "—";
  const a = Math.abs(n);
  if (a >= 100) return `$${n.toFixed(0)}`;
  if (a >= 1) return `$${n.toFixed(2)}`;
  if (a >= 0.01) return `$${n.toFixed(3)}`;
  return a === 0 ? "$0" : `$${n.toFixed(4)}`;
};
const tok = (n) => {
  n = Number(n) || 0;
  if (n >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(n >= 1e4 ? 0 : 1)}k`;
  return String(n);
};
const dimLabel = (dim, key) => (dim === "purpose" ? PURPOSE_LABEL[key] || key : key);
const isoDay = (d) => d.toISOString().slice(0, 10);
const addDays = (iso, n) => {
  const d = new Date(`${iso}T00:00:00Z`);
  d.setUTCDate(d.getUTCDate() + n);
  return isoDay(d);
};

/* ————— color follows the entity, never its rank —————
   A model keeps its hue when the window, node or filter changes: slots
   are handed out once per (dimension, key) and remembered. Past the
   seven identity hues a key folds into "other" — never a generated hue. */
const slots = (() => {
  let saved = {};
  try { saved = JSON.parse(localStorage.getItem("hl-spend-slots") || "{}"); } catch { /* private window */ }
  return saved;
})();
function slotFor(dim, key) {
  const table = (slots[dim] ||= {});
  if (key in table) return table[key];
  const used = new Set(Object.values(table));
  for (let i = 1; i <= SERIES_SLOTS; i++) {
    if (!used.has(i)) {
      table[key] = i;
      try { localStorage.setItem("hl-spend-slots", JSON.stringify(slots)); } catch { /* fine */ }
      return i;
    }
  }
  return 0; // no identity hue left
}
const slotColor = (slot) => `var(--chart-${slot || 8})`;

/* ————— rollups ————— */
function rollup(cells, dim) {
  const out = new Map();
  for (const c of cells) {
    const g = out.get(c[dim]) || { key: c[dim], calls: 0, input: 0, output: 0, cacheRead: 0,
      cacheWrite: 0, errors: 0, cost: 0, saved: 0, priced: true };
    g.calls += c.calls; g.input += c.input_tokens; g.output += c.output_tokens;
    g.cacheRead += c.cache_read_tokens; g.cacheWrite += c.cache_creation_tokens;
    g.errors += c.errors;
    if (c.cost_usd == null) g.priced = false; else g.cost += c.cost_usd;
    g.saved += c.cache_saved_usd || 0;
    out.set(c[dim], g);
  }
  return [...out.values()].sort((a, b) => b.cost - a.cost || b.calls - a.calls);
}

/// The series the chart shows: keys that hold an identity hue, biggest
/// first, and everything else folded into one "other" series.
function seriesFor(cells, dim) {
  const groups = rollup(cells, dim);
  const named = [];
  let other = null;
  for (const g of groups) {
    const slot = slotFor(dim, g.key);
    if (slot) named.push({ key: g.key, label: dimLabel(dim, g.key), slot, total: g.cost, priced: g.priced });
    else {
      other ||= { key: OTHER, label: "other", slot: 0, total: 0, priced: true };
      other.total += g.cost;
    }
  }
  return other ? [...named, other] : named;
}

/* ————— the stacked bar chart —————
   One axis (USD). Segments stack from the baseline with a 2px surface
   gap between fills; only the top segment gets rounded ends. A legend is
   always present for ≥2 series; values live in the hover tooltip and the
   table below, never as a number on every bar. */
function bucketize(cells, from, to, dim, series) {
  const span = Math.round((new Date(`${to}T00:00:00Z`) - new Date(`${from}T00:00:00Z`)) / 86400000) + 1;
  const weekly = span > 92;
  const named = new Set(series.filter((s) => s.key !== OTHER).map((s) => s.key));
  const buckets = [];
  const index = new Map();
  for (let d = from; d <= to; d = addDays(d, weekly ? 7 : 1)) {
    index.set(d, buckets.length);
    buckets.push({ start: d, end: weekly ? addDays(d, 6) : d, total: 0, parts: {} });
  }
  for (const c of cells) {
    let at = index.get(c.day);
    if (at == null && weekly) {
      const offset = Math.floor((new Date(`${c.day}T00:00:00Z`) - new Date(`${from}T00:00:00Z`)) / 86400000 / 7);
      at = offset >= 0 && offset < buckets.length ? offset : null;
    }
    if (at == null) continue;
    const key = named.has(c[dim]) ? c[dim] : OTHER;
    const cost = c.cost_usd || 0;
    buckets[at].parts[key] = (buckets[at].parts[key] || 0) + cost;
    buckets[at].total += cost;
  }
  return { buckets, weekly };
}

function niceMax(v) {
  if (v <= 0) return 1;
  const pow = 10 ** Math.floor(Math.log10(v));
  for (const m of [1, 2, 2.5, 5, 10]) if (m * pow >= v) return m * pow;
  return 10 * pow;
}

function paintChart(host, cells, from, to, dim) {
  const series = seriesFor(cells, dim);
  const { buckets, weekly } = bucketize(cells, from, to, dim, series);
  const max = niceMax(Math.max(...buckets.map((b) => b.total), 0));
  const W = Math.max(host.clientWidth || 720, 320), H = 280;
  const pad = { l: 54, r: 12, t: 12, b: 30 };
  const plotW = W - pad.l - pad.r, plotH = H - pad.t - pad.b;
  const band = plotW / buckets.length;
  const barW = Math.max(2, Math.min(band - 2, 34));
  const y = (v) => pad.t + plotH - (v / max) * plotH;

  // Ticks share ONE precision, chosen from the step — "$0.500" beside
  // "$1.00" reads as two different scales.
  const step = max / 4;
  const places = step >= 1 ? 0 : step >= 0.1 ? 2 : step >= 0.01 ? 3 : 4;
  const tick = (v) => `$${v.toFixed(places)}`;
  let grid = "";
  for (let i = 0; i <= 4; i++) {
    const v = (max / 4) * i;
    grid += `<line x1="${pad.l}" x2="${W - pad.r}" y1="${y(v)}" y2="${y(v)}" class="spend-grid"/>
      <text x="${pad.l - 8}" y="${y(v) + 4}" text-anchor="end" class="spend-axis">${esc(tick(v))}</text>`;
  }
  const every = Math.max(1, Math.ceil(buckets.length / Math.max(4, Math.floor(plotW / 64))));
  let bars = "", labels = "", hits = "";
  buckets.forEach((b, i) => {
    const x = pad.l + i * band + (band - barW) / 2;
    let base = b.total > 0 ? 0 : null;
    const live = series.filter((s) => (b.parts[s.key] || 0) > 0);
    live.forEach((s, si) => {
      const v = b.parts[s.key];
      const y0 = y(base), y1 = y(base + v);
      const top = si === live.length - 1;
      // 2px surface gap between stacked fills (dropped when a sliver
      // would vanish), rounded ends on the top segment only.
      const gap = !top && y0 - y1 > 4 ? 2 : 0;
      const h = Math.max(y0 - y1 - gap, 1);
      const r = top ? Math.min(3, barW / 2, h) : 0;
      const yy = y1 + gap;
      bars += r
        ? `<path d="M${x},${yy + h} V${yy + r} Q${x},${yy} ${x + r},${yy} H${x + barW - r} Q${x + barW},${yy} ${x + barW},${yy + r} V${yy + h} Z" fill="${slotColor(s.slot)}"/>`
        : `<rect x="${x}" y="${yy}" width="${barW}" height="${h}" fill="${slotColor(s.slot)}"/>`;
      base += v;
    });
    if (i % every === 0) {
      labels += `<text x="${pad.l + i * band + band / 2}" y="${H - 10}" text-anchor="middle" class="spend-axis">${esc(b.start.slice(5))}</text>`;
    }
    // Hit target = the whole column band, far bigger than the mark.
    hits += `<rect x="${pad.l + i * band}" y="${pad.t}" width="${band}" height="${plotH}" fill="transparent" data-i="${i}" class="spend-hit"/>`;
  });

  const legend = series.length >= 2
    ? `<div class="spend-legend">${series.map((s) => `
        <span class="spend-key"><i style="background:${slotColor(s.slot)}"></i>${esc(s.label)}
          <b>${s.priced ? esc(usd(s.total)) : "no price on file"}</b></span>`).join("")}</div>`
    : "";
  host.innerHTML = `
    <div class="spend-plot">
      <svg viewBox="0 0 ${W} ${H}" width="100%" height="${H}" role="img"
        aria-label="Spend per ${weekly ? "week" : "day"} in US dollars, stacked by ${esc(dim)}">
        ${grid}<line x1="${pad.l}" x2="${W - pad.r}" y1="${y(0)}" y2="${y(0)}" class="spend-base"/>
        ${bars}${labels}${hits}
      </svg>
      <div class="spend-tip" hidden></div>
    </div>${legend}`;

  const tip = host.querySelector(".spend-tip");
  const plot = host.querySelector(".spend-plot");
  host.querySelectorAll(".spend-hit").forEach((hit) => {
    const show = () => {
      const b = buckets[Number(hit.dataset.i)];
      const rows = series.filter((s) => (b.parts[s.key] || 0) > 0)
        .sort((a, z) => b.parts[z.key] - b.parts[a.key])
        .map((s) => `<div class="spend-tip-row"><i style="background:${slotColor(s.slot)}"></i>
          <span>${esc(s.label)}</span><b>${esc(usd(b.parts[s.key]))}</b></div>`).join("");
      tip.innerHTML = `<div class="spend-tip-head">${esc(weekly ? `week of ${b.start}` : b.start)}
          <b>${esc(usd(b.total))}</b></div>${rows || `<div class="spend-tip-row"><span>no calls</span></div>`}`;
      tip.hidden = false;
      const box = hit.getBoundingClientRect(), frame = plot.getBoundingClientRect();
      const left = box.left - frame.left + box.width / 2;
      tip.style.left = `${Math.max(90, Math.min(frame.width - 90, left))}px`;
    };
    hit.addEventListener("mouseenter", show);
    hit.addEventListener("focus", show);
    hit.addEventListener("mouseleave", () => { tip.hidden = true; });
  });
}

/* ————— sections ————— */
function tiles(d) {
  const groups = rollup(d.cells, "model");
  const windowCost = groups.reduce((a, g) => a + g.cost, 0);
  const saved = groups.reduce((a, g) => a + g.saved, 0);
  const calls = groups.reduce((a, g) => a + g.calls, 0);
  const tile = (label, value, note) => `
    <div class="spend-tile"><span class="eyebrow">${esc(label)}</span>
      <b>${esc(value)}</b><span class="meta">${note}</span></div>`;
  const m = d.month;
  return `<div class="spend-tiles">
    ${tile("today", usd(m.today_usd), "so far, UTC day")}
    ${tile("this month", usd(m.cost_usd), `since ${esc(m.start)}`)}
    ${tile("projected month", m.projected_usd == null ? "—" : usd(m.projected_usd),
      m.projected_usd == null ? "needs one full day" : "at the rate of completed days")}
    ${tile("per question", d.asks.mean_usd == null ? "—" : usd(d.asks.mean_usd),
      d.asks.count ? `mean of ${d.asks.count} · median ${esc(usd(d.asks.median_usd))}` : "no questions in this window")}
    ${tile("cache savings", usd(saved), saved < 0
      ? "cache writes outweighed reads here" : "versus full input price")}
    ${tile("in this window", usd(windowCost), `${calls.toLocaleString()} model calls`)}
    ${budgetTile(d)}
  </div>`;
}

/// The monthly budget: the figure, how much of it this month has used
/// (both nodes), and the one control that sets or clears it — in place.
function budgetTile(d) {
  const b = d.budget || {};
  if (state.editBudget) {
    return `<div class="spend-tile spend-budget"><span class="eyebrow">monthly budget</span>
      <form id="spend-budget-form" class="spend-budget-form">
        <span class="meta">$</span>
        <input id="spend-budget-input" type="number" min="0" step="any" inputmode="decimal"
          placeholder="none" value="${b.monthly_usd ?? ""}" aria-label="monthly budget in dollars">
        <button class="btn" type="submit">save</button>
        <button class="btn" type="button" id="spend-budget-cancel">cancel</button>
      </form>
      <span class="meta" id="spend-budget-note">You get a notification at 80% and at 100%. Nothing is switched off. Leave it empty for no budget.</span></div>`;
  }
  if (b.monthly_usd == null) {
    return `<div class="spend-tile spend-budget"><span class="eyebrow">monthly budget</span>
      <b>—</b><span class="meta"><button class="spend-link" id="spend-budget-edit">set a budget</button> to be told at 80% and 100%</span></div>`;
  }
  const pct = (b.month_spent_usd || 0) / b.monthly_usd * 100;
  const level = pct >= 100 ? "over" : pct >= 80 ? "near" : "ok";
  return `<div class="spend-tile spend-budget"><span class="eyebrow">monthly budget</span>
    <b>${esc(usd(b.monthly_usd))}</b>
    <div class="spend-meter" data-level="${level}" role="img"
      aria-label="${Math.round(pct)}% of the monthly budget used"><i style="width:${Math.min(100, pct).toFixed(1)}%"></i></div>
    <span class="meta">${level === "over" ? "<b class=\"spend-over\">over budget</b> — " : ""}${Math.round(pct)}% used this month, both nodes ·
      <button class="spend-link" id="spend-budget-edit">change</button></span></div>`;
}

function grid(d) {
  const models = rollup(d.cells, "model");
  const purposes = rollup(d.cells, "purpose");
  if (!models.length) return "";
  const cost = new Map();
  let max = 0;
  for (const c of d.cells) {
    const k = `${c.model}\u0000${c.purpose}`;
    const v = (cost.get(k) || 0) + (c.cost_usd || 0);
    cost.set(k, v);
    max = Math.max(max, v);
  }
  // Sequential = ONE hue, light → dark by magnitude; the number stays in ink.
  const cell = (v) => {
    if (!v) return `<td class="spend-cell"></td>`;
    const pct = Math.round(8 + 62 * Math.sqrt(v / max));
    return `<td class="spend-cell" style="background:color-mix(in srgb, var(--chart-heat) ${pct}%, transparent)">${esc(usd(v))}</td>`;
  };
  return `
    <div class="section-title">where each model's money goes</div>
    <div class="spend-scroll"><table class="spend-table spend-gridtable">
      <thead><tr><th>model</th>${purposes.map((p) => `<th>${esc(dimLabel("purpose", p.key))}</th>`).join("")}<th>total</th></tr></thead>
      <tbody>${models.map((m) => `<tr><td>${esc(m.key)}</td>
        ${purposes.map((p) => cell(cost.get(`${m.key}\u0000${p.key}`))).join("")}
        <td><b>${m.priced ? esc(usd(m.cost)) : "no price on file"}</b></td></tr>`).join("")}
        <tr class="spend-total"><td>total</td>${purposes.map((p) => `<td>${esc(usd(p.cost))}</td>`).join("")}
          <td><b>${esc(usd(models.reduce((a, m) => a + m.cost, 0)))}</b></td></tr>
      </tbody></table></div>`;
}

function tiers(d) {
  const rows = rollup(d.cells, "tier");
  const total = rows.reduce((a, r) => a + r.cost, 0) || 1;
  if (!rows.length) return "";
  return `
    <div class="section-title">by tier</div>
    <table class="spend-table"><thead><tr><th>tier</th><th>calls</th><th>tokens in</th>
      <th>tokens out</th><th>cost</th><th>share</th></tr></thead>
      <tbody>${rows.map((r) => `<tr><td>${esc(r.key)}</td><td>${r.calls.toLocaleString()}</td>
        <td>${tok(r.input + r.cacheRead + r.cacheWrite)}</td><td>${tok(r.output)}</td>
        <td>${esc(usd(r.cost))}</td><td>${Math.round((r.cost / total) * 100)}%</td></tr>`).join("")}
      </tbody></table>`;
}

function asks(d) {
  const a = d.asks;
  if (!a.count) return "";
  // Distribution: ten equal-width bins up to the 95th percentile, so one
  // runaway question doesn't flatten the rest into a single bar.
  const costs = a.costs;
  const cap = costs[Math.min(costs.length - 1, Math.floor(costs.length * 0.95))] || 0;
  const hi = niceMax(cap || costs[costs.length - 1] || 1);
  const bins = Array.from({ length: 10 }, (_, i) => ({ lo: (hi / 10) * i, n: 0 }));
  let over = 0;
  for (const c of costs) {
    if (c > hi) { over++; continue; }
    bins[Math.min(9, Math.floor((c / hi) * 10))].n++;
  }
  const peak = Math.max(...bins.map((b) => b.n), over, 1);
  // Every other band is labelled (the bars are narrow); the tooltip names all.
  const bar = (n, label, show = true) => `<div class="spend-bin" title="${n} question${n === 1 ? "" : "s"} · from ${esc(label)}">
      <i style="height:${Math.max(n ? 3 : 0, Math.round((n / peak) * 64))}px"></i><span>${show ? esc(label) : "&nbsp;"}</span></div>`;
  return `
    <div class="section-title">what a question costs</div>
    <div class="spend-two">
      <div>
        <p class="meta">${a.count} question${a.count === 1 ? "" : "s"} in this window — how many fell in each cost band
          (root loop plus row reads).</p>
        <div class="spend-bins">${bins.map((b, i) => bar(b.n, usd(b.lo), i % 2 === 0)).join("")}${over ? bar(over, `> ${usd(hi)}`) : ""}</div>
      </div>
      <div>
        <p class="meta">The dearest ones</p>
        <table class="spend-table"><tbody>${a.top.map((t) => `<tr>
          <td class="spend-q">${t.question ? esc(t.question) : `<span class="meta">(deleted from history)</span>`}
            <span class="meta">${esc(t.ts.slice(0, 10))} · ${t.calls} call${t.calls === 1 ? "" : "s"}</span></td>
          <td>${esc(usd(t.cost_usd))}</td>
          <td>${t.question && openTrace ? `<button class="btn spend-trace" data-ask="${esc(t.ask_id)}">trace</button>` : ""}</td>
        </tr>`).join("")}</tbody></table>
      </div>
    </div>`;
}

const REPORT_COLS = [
  { key: "day", label: "day (UTC)" }, { key: "provider", label: "provider" },
  { key: "model", label: "model" }, { key: "tier", label: "tier" },
  { key: "purpose", label: "purpose" }, { key: "calls", label: "calls", num: true },
  { key: "input_tokens", label: "in", num: true }, { key: "output_tokens", label: "out", num: true },
  { key: "cache_read_tokens", label: "cache read", num: true },
  { key: "cache_creation_tokens", label: "cache write", num: true },
  { key: "errors", label: "errors", num: true }, { key: "cost_usd", label: "cost", num: true },
];
function report(d) {
  if (!d.cells.length) return "";
  const { col, dir } = state.sort;
  const rows = [...d.cells].sort((a, b) => {
    const x = a[col] ?? -1, y = b[col] ?? -1;
    return (x < y ? -1 : x > y ? 1 : 0) * dir || (a.day < b.day ? 1 : -1);
  });
  const shown = state.showAll ? rows : rows.slice(0, 60);
  const cellOf = (r, c) => c.key === "cost_usd" ? (r.cost_usd == null ? "no price on file" : usd(r.cost_usd))
    : c.key === "purpose" ? dimLabel("purpose", r.purpose)
    : c.num ? (c.key === "calls" || c.key === "errors" ? r[c.key].toLocaleString() : tok(r[c.key])) : r[c.key];
  return `
    <div class="section-title">the report
      <button class="btn spend-csv" id="spend-csv">download CSV</button></div>
    <div class="spend-scroll"><table class="spend-table spend-report">
      <thead><tr>${REPORT_COLS.map((c) => `<th data-col="${c.key}" class="${c.num ? "num" : ""}"
        aria-sort="${col === c.key ? (dir > 0 ? "ascending" : "descending") : "none"}">${esc(c.label)}${
        col === c.key ? (dir > 0 ? " ↑" : " ↓") : ""}</th>`).join("")}</tr></thead>
      <tbody>${shown.map((r) => `<tr>${REPORT_COLS.map((c) =>
        `<td class="${c.num ? "num" : ""}">${esc(cellOf(r, c))}</td>`).join("")}</tr>`).join("")}</tbody>
    </table></div>
    ${rows.length > shown.length
      ? `<button class="btn" id="spend-more">show all ${rows.length} rows</button>` : ""}`;
}

function csv(d) {
  const cols = [...REPORT_COLS.map((c) => c.key), "cache_saved_usd"];
  const quote = (v) => (v == null ? "" : /[",\n]/.test(String(v)) ? `"${String(v).replace(/"/g, '""')}"` : String(v));
  return [cols.join(","), ...d.cells.map((r) => cols.map((c) => quote(r[c])).join(","))].join("\n");
}

/* ————— the page ————— */
function controls() {
  const seg = (items, current, attr) => items.map((it) =>
    `<button class="spend-seg${String(it.value ?? it.key ?? it.days) === String(current) ? " on" : ""}"
      data-${attr}="${esc(it.value ?? it.key ?? it.days)}">${esc(it.label)}</button>`).join("");
  return `<div class="spend-controls">
    <div class="spend-segs" role="group" aria-label="period">${seg(PRESETS, state.from ? "custom" : state.days, "days")}</div>
    <label class="meta">from <input type="date" id="spend-from" value="${esc(state.from || (data?.from ?? ""))}"></label>
    <label class="meta">to <input type="date" id="spend-to" value="${esc(state.to || (data?.to ?? ""))}"></label>
    <select id="spend-node" aria-label="node">${NODES.map((n) =>
      `<option value="${n.value}" ${n.value === state.node ? "selected" : ""}>${n.label}</option>`).join("")}</select>
  </div>`;
}

async function load(view) {
  const body = view.querySelector("#spend-body");
  body.innerHTML = `<div class="loading"><span class="spinner"></span> tallying the ledger…</div>`;
  const params = new URLSearchParams();
  if (state.from && state.to) { params.set("from", state.from); params.set("to", state.to); }
  else if (state.days) params.set("from", addDays(isoDay(new Date()), -(state.days - 1)));
  else params.set("from", "2000-01-01"); // "all": the server report starts at the first row anyway
  if (state.node) params.set("node", state.node);
  try {
    data = await api(`/v1/llm/spend?${params}`);
  } catch (err) {
    const locked = String(err.message || "").includes("vault locked");
    view.querySelector("#spend-controls").innerHTML = "";
    body.innerHTML = `<div class="card">${locked
      ? `<b>🔒 unlock the vault to see spending</b>
         <p class="meta">What was spent, and when, can reveal private activity — so the ledger
         only opens while the vault is unlocked (the 🔒 in the top bar).</p>`
      : `<p class="meta">spending unavailable: ${esc(err.message)}</p>`}</div>`;
    return;
  }
  // "all" asks from year 2000; draw from where the ledger actually begins.
  if (!state.days && !state.from && data.ledger_starts) {
    const first = data.ledger_starts.slice(0, 10);
    if (first > data.from) data.from = first;
  }
  paint(view);
}

function paint(view) {
  const d = data;
  view.querySelector("#spend-controls").innerHTML = controls();
  const body = view.querySelector("#spend-body");
  if (!d.cells.length) {
    body.innerHTML = `${tiles(d)}<div class="empty">No model calls were recorded between ${esc(d.from)} and ${esc(d.to)}.</div>${foot(d)}`;
    wire(view);
    return;
  }
  body.innerHTML = `
    ${tiles(d)}
    <div class="section-title">spend per day
      <span class="spend-segs" role="group" aria-label="break down by">
        ${DIMS.map((x) => `<button class="spend-seg${x.key === state.dim ? " on" : ""}" data-dim="${x.key}">by ${x.label}</button>`).join("")}
      </span></div>
    <div id="spend-chart"></div>
    ${grid(d)}${tiers(d)}${asks(d)}${report(d)}${foot(d)}`;
  paintChart(body.querySelector("#spend-chart"), d.cells, d.from, d.to, state.dim);
  wire(view);
}

function foot(d) {
  const starts = d.ledger_starts ? d.ledger_starts.slice(0, 10) : null;
  return `<p class="meta spend-foot">
    Costs are estimates, computed from a price table when you look — the provider's invoice is the bill.
    ${d.unpriced_models.length ? `No price on file for ${d.unpriced_models.map(esc).join(", ")}; those rows count tokens only. ` : ""}
    ${starts ? `The ledger starts ${esc(starts)} — earlier spending was never recorded. ` : ""}
    Days are UTC. Models that run on this machine (embeddings) are listed and cost nothing; they count runs, not tokens.</p>`;
}

function wire(view) {
  const reload = () => load(view);
  view.querySelectorAll("[data-days]").forEach((b) => {
    b.onclick = () => { state.days = Number(b.dataset.days); state.from = state.to = ""; reload(); };
  });
  const from = view.querySelector("#spend-from"), to = view.querySelector("#spend-to");
  const custom = () => {
    if (from.value && to.value && from.value <= to.value) { state.from = from.value; state.to = to.value; reload(); }
  };
  if (from) from.onchange = custom;
  if (to) to.onchange = custom;
  const node = view.querySelector("#spend-node");
  if (node) node.onchange = () => { state.node = node.value; reload(); };
  view.querySelectorAll("[data-dim]").forEach((b) => {
    b.onclick = () => { state.dim = b.dataset.dim; paint(view); };
  });
  view.querySelectorAll(".spend-report th").forEach((th) => {
    th.onclick = () => {
      const col = th.dataset.col;
      state.sort = { col, dir: state.sort.col === col ? -state.sort.dir : -1 };
      paint(view);
    };
  });
  const more = view.querySelector("#spend-more");
  if (more) more.onclick = () => { state.showAll = true; paint(view); };
  const dl = view.querySelector("#spend-csv");
  if (dl) dl.onclick = () => {
    const url = URL.createObjectURL(new Blob([csv(data)], { type: "text/csv" }));
    const a = Object.assign(document.createElement("a"), {
      href: url, download: `hearthloom-spend-${data.from}-to-${data.to}.csv` });
    document.body.append(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  view.querySelectorAll(".spend-trace").forEach((b) => { b.onclick = () => openTrace(b.dataset.ask); });
  const editBudget = view.querySelector("#spend-budget-edit");
  if (editBudget) editBudget.onclick = () => {
    state.editBudget = true;
    paint(view);
    view.querySelector("#spend-budget-input")?.focus();
  };
  const cancelBudget = view.querySelector("#spend-budget-cancel");
  if (cancelBudget) cancelBudget.onclick = () => { state.editBudget = false; paint(view); };
  const budgetForm = view.querySelector("#spend-budget-form");
  if (budgetForm) budgetForm.onsubmit = async (event) => {
    event.preventDefault();
    const raw = view.querySelector("#spend-budget-input").value.trim();
    const amount = raw === "" ? null : Number(raw);
    const note = view.querySelector("#spend-budget-note");
    if (amount != null && !(amount >= 0)) { note.textContent = "Enter a dollar amount, or leave it empty for no budget."; return; }
    try {
      await apiPost("/v1/llm/budget", { monthly_usd: amount });
    } catch (err) {
      note.textContent = `Could not save the budget: ${err.message}`;
      return;
    }
    state.editBudget = false;
    reload();
  };
}

export async function mount(view, helpers) {
  api = helpers.api;
  apiPost = helpers.apiPost;
  openTrace = helpers.openTrace;
  view.innerHTML = `
    <span class="eyebrow">the ledger</span>
    <h1 class="pagehead">Spend</h1>
    <p class="subhead">What the thinking costs: every call Hearthloom makes to an AI model, priced and broken down.</p>
    <div id="spend-controls"></div>
    <div id="spend-body"></div>`;
  // Redraw the chart at the new width (the SVG is laid out in pixels).
  let resizeTimer;
  const onResize = () => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      const chart = view.querySelector("#spend-chart");
      if (chart && data) paintChart(chart, data.cells, data.from, data.to, state.dim);
    }, 150);
  };
  window.addEventListener("resize", onResize);
  window.addEventListener("hashchange", () => window.removeEventListener("resize", onResize), { once: true });
  await load(view);
}
