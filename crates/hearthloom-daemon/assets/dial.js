// dial.js — the Dial: a fisheye drum timeline, viewed edge-on. The date
// under the hairline faces you; time curves away left (past) and right
// (future), foreshortening as it goes — perspective is the decluttering
// mechanism. Episodes are ember lines whose glow scales with density;
// at most LABEL_BUDGET leader-line labels ride above the drum.
//
// Semantic zoom: the drum shows YEAR marks when the window is wide,
// then months, then days — one mark per bucket, never one per episode.
// Spin with drag/scroll (momentum + friction, like a jog wheel); pinch
// or ctrl+wheel changes the time scale around the hairline.
//
// Shared component: Browse binds it to the from/to filters; Search
// mounts it zoomed to the span of the hits. Colors come from CSS custom
// properties (embers dual theme) — never hardcode one here.

const DAY_MS = 86400000;
const LABEL_BUDGET = 8;
const MIN_SPAN = 7;          // days across the drum
const MAX_SPAN = 365 * 40;
const EDGE = 0.98;           // fraction of the half-drum used

const dayNum = (d) => Date.parse(`${d}T12:00:00Z`) / DAY_MS;
const numDay = (n) => new Date(n * DAY_MS).toISOString().slice(0, 10);
const clamp = (v, lo, hi) => Math.min(Math.max(v, lo), hi);

export function mountDial(container, opts) {
  const {
    onWindow, onPick, filters = () => ({}), compact = false,
    // labels: leader-line event labels above the drum. Full dials show
    // them by default; compact hosts (Atlas, Pulse) opt in and get a
    // taller drum to make room. labelFilter narrows which notable kinds
    // ride the ticks; extraLabels(fromISO, toISO) can inject a host's
    // own events (the Atlas feeds place departures through it).
    labels = !compact,
    labelFilter = null,
    extraLabels = null,
    // Clicking a label opens its day unless the host overrides (the
    // Notes dial opens the note itself).
    onLabel = null,
  } = opts;
  container.innerHTML = `
    <div class="dial">
      <div class="dial-readout">
        <span class="dial-center" id="dial-center"></span>
        <span class="dial-span" id="dial-span"></span>
        <span class="dial-chapter" id="dial-chapter"></span>
      </div>
      <canvas class="dial-canvas"></canvas>
    </div>`;
  const canvas = container.querySelector(".dial-canvas");
  const ctx = canvas.getContext("2d");
  const centerEl = container.querySelector("#dial-center");
  const spanEl = container.querySelector("#dial-span");
  const chapterEl = container.querySelector("#dial-chapter");

  /* — theme — */
  let colors = {};
  const readColors = () => {
    const cs = getComputedStyle(document.documentElement);
    const v = (n, f) => (cs.getPropertyValue(n) || f).trim();
    colors = {
      ember: v("--chart-1", "#e8833a"),
      hot: v("--ember-100", "#ffe8c8"),
      tick: v("--ink-faint", "#6f6455"),
      label: v("--ink-mute", "#a89a86"),
      hair: v("--accent", "#e8833a"),
      surface: v("--bg-raise", "#1d1915"),
      line: v("--line", "#37302a"),
      mono: v("--font-mono", "ui-monospace"),
    };
  };
  readColors();
  const themeWatcher = new MutationObserver(() => { readColors(); draw(); });
  themeWatcher.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

  /* — state — */
  let center = dayNum(opts.to || numDay(Date.now() / DAY_MS)) - 0;
  let span = MIN_SPAN;
  if (opts.from && opts.to) {
    const a = dayNum(opts.from), b = dayNum(opts.to);
    center = (a + b) / 2;
    span = clamp(b - a + 1, MIN_SPAN, MAX_SPAN);
  } else {
    span = 365;
    center -= span / 2 - span / 2; // today at hairline, year behind it
  }
  let W = 0, H = 0, dpr = 1;
  const DRUM_TOP = compact ? (labels ? 88 : 26) : 100;

  const tier = () => (span > 365 * 4 ? "year" : span > 190 ? "month" : "day");
  const bucketStart = (b) =>
    dayNum(b.length === 4 ? `${b}-01-01` : b.length === 7 ? `${b}-01` : b);
  const bucketDays = (b) =>
    b.length === 4 ? 365 : b.length === 7 ? 30 : 1;

  /* — projection: day → x on the drum — */
  const theta = (d) => clamp((d - center) / (span / 2), -1, 1) * (Math.PI / 2) * EDGE;
  const xOf = (d) => W / 2 + Math.sin(theta(d)) * (W / 2) * EDGE;
  const scaleOf = (d) => Math.cos(theta(d));
  // inverse for hit-testing / drag (near center it's ~linear)
  const dayAtX = (x) => {
    const s = clamp((x - W / 2) / ((W / 2) * EDGE), -1, 1);
    return center + (Math.asin(s) / ((Math.PI / 2) * EDGE)) * (span / 2);
  };

  /* — data: cached histogram per (tier, filters), fetched wide — */
  let buckets = new Map(); // bucket string -> n
  let fetchedRange = [0, 0];
  let fetchedKey = "";
  let fetchTimer = null;
  async function ensureData(force) {
    const f = filters();
    const key = `${tier()}|${f.q || ""}|${f.source || ""}|${f.place || ""}`;
    const lo = center - span * 1.6, hi = center + span * 1.6;
    if (!force && key === fetchedKey && lo >= fetchedRange[0] && hi <= fetchedRange[1]) return;
    clearTimeout(fetchTimer);
    fetchTimer = setTimeout(async () => {
      const from = numDay(center - span * 3), to = numDay(center + span * 3);
      const p = new URLSearchParams({ from, to, bucket: tier() });
      if (f.q) p.set("q", f.q);
      if (f.source) p.set("source", f.source);
      if (f.place) p.set("place", f.place);
      try {
        const data = await (await fetch(`/v1/timeline/histogram?${p}`)).json();
        buckets = new Map(data.buckets.map((b) => [b.bucket, b.n]));
        fetchedRange = [dayNum(from), dayNum(to)];
        fetchedKey = key;
        draw();
      } catch { /* daemon busy */ }
    }, force ? 0 : 150);
  }

  /* — notable events: the label layer. Fetched on settle (top-K per
     viewport, importance-ranked server-side — the map-label pattern);
     hidden while the drum is in motion, fading in at rest. — */
  let notable = [];
  let notableKey = "";
  let labelAlpha = 0;
  let fadeRaf = 0;
  async function fetchNotable() {
    const f = filters();
    // ±16d margin: month-anchored chapters (the 15th) must be caught
    // even when a narrow window misses their anchor day.
    const from = numDay(center - span / 2 - 16), to = numDay(center + span / 2 + 16);
    const key = `${from}|${to}|${tier()}|${f.q || ""}|${f.source || ""}|${f.place || ""}`;
    if (key === notableKey) { fadeLabelsIn(); return; }
    const p = new URLSearchParams({ from, to, bucket: tier(), k: "16" });
    if (f.q) p.set("q", f.q);
    if (f.source) p.set("source", f.source);
    if (f.place) p.set("place", f.place);
    try {
      const data = await (await fetch(`/v1/timeline/notable?${p}`)).json();
      let events = data.notable || [];
      if (extraLabels) {
        try {
          events = events.concat((await extraLabels(from, to)) || []);
        } catch { /* host provider hiccup — notable alone still shows */ }
      }
      notable = events;
      notableKey = key;
      fadeLabelsIn();
    } catch { /* daemon busy */ }
  }
  function fadeLabelsIn() {
    cancelAnimationFrame(fadeRaf);
    const step = () => {
      labelAlpha = Math.min(labelAlpha + 0.12, 1);
      draw();
      if (labelAlpha < 1) fadeRaf = requestAnimationFrame(step);
    };
    fadeRaf = requestAnimationFrame(step);
  }

  /* — drawing — */
  function resize() {
    dpr = window.devicePixelRatio || 1;
    const rect = container.querySelector(".dial").getBoundingClientRect();
    W = rect.width;
    H = compact ? (labels ? 182 : 120) : 236;
    canvas.width = W * dpr;
    canvas.height = H * dpr;
    canvas.style.width = `${W}px`;
    canvas.style.height = `${H}px`;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }

  let visible = []; // [{b, n, x, scale, h}] for hit-testing + labels
  let labelHits = []; // [{ev, box}] notable labels for click/hover
  function draw() {
    ctx.clearRect(0, 0, W, H);
    const drumH = H - DRUM_TOP;
    const baseY = H - 14;

    // drum surface: lit at the center, falling away to the edges
    const grad = ctx.createLinearGradient(0, 0, W, 0);
    grad.addColorStop(0, "rgba(0,0,0,0)");
    grad.addColorStop(0.5, colors.surface);
    grad.addColorStop(1, "rgba(0,0,0,0)");
    ctx.fillStyle = grad;
    ctx.fillRect(0, DRUM_TOP, W, drumH);
    ctx.strokeStyle = colors.line;
    ctx.beginPath();
    ctx.moveTo(0, baseY + 8);
    ctx.lineTo(W, baseY + 8);
    ctx.stroke();

    // calendar ticks one tier up (years under month view, months under day view)
    const t = tier();
    const tickTier = t === "day" ? "month" : "year";
    const from = center - span, to = center + span;
    for (let d = Math.floor(from); d <= to; d++) {
      const iso = numDay(d);
      const isTick = tickTier === "month" ? iso.endsWith("-01") : iso.endsWith("-01-01");
      if (!isTick) continue;
      const x = xOf(d), sc = scaleOf(d);
      if (sc < 0.05) continue;
      ctx.globalAlpha = 0.5 * sc;
      ctx.strokeStyle = colors.tick;
      ctx.beginPath();
      ctx.moveTo(x, baseY + 2);
      ctx.lineTo(x, baseY + 8);
      ctx.stroke();
      if (sc > 0.35 && t !== "year") {
        ctx.font = `500 9px ${colors.mono}`;
        ctx.fillStyle = colors.tick;
        ctx.textAlign = "center";
        ctx.fillText(tickTier === "month" ? iso.slice(0, 7) : iso.slice(0, 4), x, baseY + 18 > H ? H - 2 : baseY + 18);
      }
      ctx.globalAlpha = 1;
    }

    // ember marks per bucket
    const maxN = Math.max(1, ...buckets.values());
    visible = [];
    for (const [b, n] of buckets) {
      const d = bucketStart(b) + bucketDays(b) / 2;
      if (d < center - span || d > center + span) continue;
      const sc = scaleOf(d);
      if (sc < 0.04) continue;
      const x = xOf(d);
      const h = (10 + (drumH - 40) * Math.pow(n / maxN, 0.5)) * sc;
      const alpha = 0.25 + 0.72 * sc;
      ctx.save();
      ctx.globalAlpha = alpha;
      ctx.strokeStyle = colors.ember;
      ctx.lineWidth = Math.max(1, 2.4 * sc);
      ctx.shadowColor = colors.ember;
      ctx.shadowBlur = 6 * sc * (n / maxN);
      ctx.beginPath();
      ctx.moveTo(x, baseY);
      ctx.lineTo(x, baseY - h);
      ctx.stroke();
      if (sc > 0.5 && n / maxN > 0.4) {
        ctx.shadowBlur = 0;
        ctx.fillStyle = colors.hot;
        ctx.globalAlpha = alpha;
        ctx.fillRect(x - 0.7, baseY - h, 1.4, Math.min(4, h));
      }
      ctx.restore();
      visible.push({ b, n, x, scale: sc, h, d });
    }

    // leader-line labels: notable EVENTS (importance-ranked server-side),
    // visible only at rest — motion shows shape, rest shows meaning.
    // Layout rules that keep it honest: dedupe repeated titles, place
    // each label in the first of three rows where it fits NEAR its
    // anchor, and if none fits, draw NOTHING — a leader line without
    // readable text is worse than glow alone.
    labelHits = [];
    if (labels && labelAlpha > 0.02) {
      const seen = new Map();
      const pool = labelFilter ? notable.filter(labelFilter) : notable;
      for (const ev of [...pool].sort((a, b) => b.score - a.score)) {
        if (!seen.has(ev.title)) seen.set(ev.title, ev);
      }
      const ranked = [...seen.values()]
        .map((ev) => ({ ...ev, d: dayNum(ev.day) }))
        .filter((ev) => Number.isFinite(ev.d) && scaleOf(ev.d) > 0.3);
      // Kind-fair budget: host-injected places (the Atlas departures)
      // get up to 3 reserved seats — importance-ranked chapters were
      // starving them out of the slice, so they flashed in only while
      // the full notable set was still in flight, then vanished.
      const placeSeats = ranked.filter((ev) => ev.kind === "place").slice(0, 3);
      const restSeats = ranked
        .filter((ev) => ev.kind !== "place")
        .slice(0, Math.max(LABEL_BUDGET - placeSeats.length, 3));
      const cands = [...restSeats, ...placeSeats]
        .map((ev) => ({ ...ev, x: xOf(ev.d), scale: scaleOf(ev.d) }))
        .sort((a, b) => a.x - b.x);
      ctx.font = `500 10px ${colors.mono}`;
      // Staggered rows (each label first tries the row after its left
      // neighbor's); compact hosts get three rows, full dials four.
      const rowYs = compact
        ? [DRUM_TOP - 16, DRUM_TOP - 38, DRUM_TOP - 60]
        : [DRUM_TOP - 16, DRUM_TOP - 38, DRUM_TOP - 60, DRUM_TOP - 82];
      const rows = rowYs.map((y) => ({ y, right: -Infinity }));
      const MAX_SHIFT = 70; // a label may drift this far from its anchor, no further
      const GAP = 22;
      let nextRow = 0;
      for (const ev of cands) {
        const isChapter = ev.kind === "chapter";
        const isPlace = ev.kind === "place";
        ctx.font = isChapter
          ? `italic 600 11.5px ${(getComputedStyle(document.documentElement).getPropertyValue("--font-display") || "serif")}`
          : `500 10px ${colors.mono}`;
        const text = (isPlace ? "⌖ " : "") +
          (ev.title.length > 34 ? ev.title.slice(0, 33) + "…" : ev.title);
        const w = ctx.measureText(text).width;
        const ideal = clamp(ev.x - w / 2, 4, W - w - 4);
        let placed = null;
        for (let attempt = 0; attempt < rows.length; attempt++) {
          const row = rows[(nextRow + attempt) % rows.length];
          const lx = Math.max(ideal, row.right + GAP);
          if (lx + w <= W - 4 && lx - ideal <= MAX_SHIFT) {
            placed = { row, lx };
            break;
          }
        }
        if (!placed) continue; // no room near the anchor — glow only
        nextRow = (nextRow + 1) % rows.length;
        const { row, lx } = placed;
        const ly = row.y;
        ctx.globalAlpha = labelAlpha * (0.55 + 0.4 * ev.scale);
        ctx.strokeStyle = colors.tick;
        ctx.beginPath();
        ctx.moveTo(ev.x, baseY - 6);
        ctx.lineTo(ev.x, ly + 12);
        ctx.lineTo(clamp(ev.x, lx, lx + w), ly + 8);
        ctx.stroke();
        // places read like ordinary event labels (gray mono, ⌖-prefixed);
        // only chapters get the accent italic — same hierarchy as Memories
        ctx.fillStyle = isChapter ? colors.hair : colors.label;
        ctx.textAlign = "left";
        ctx.fillText(text, lx, ly + 4);
        ctx.font = `500 10px ${colors.mono}`;
        // an ember dot where the leader meets the drum
        ctx.fillStyle = colors.ember;
        ctx.beginPath();
        ctx.arc(ev.x, baseY - 4, 2.2, 0, Math.PI * 2);
        ctx.fill();
        ctx.globalAlpha = 1;
        row.right = lx + w;
        labelHits.push({ ev, box: [lx - 2, ly - 8, w + 4, 18] });
      }
    }

    // the hairline
    ctx.strokeStyle = colors.hair;
    ctx.globalAlpha = 0.9;
    ctx.beginPath();
    ctx.moveTo(W / 2, DRUM_TOP - 6);
    ctx.lineTo(W / 2, baseY + 8);
    ctx.stroke();
    ctx.globalAlpha = 1;

    centerEl.textContent = numDay(center);
    const month = numDay(center).slice(0, 7);
    const chapter = notable.find((ev) => ev.kind === "chapter" && ev.day.startsWith(month));
    chapterEl.textContent = chapter ? `— “${chapter.title}”` : "";
    spanEl.textContent = span >= 365
      ? `${(span / 365).toFixed(span > 1000 ? 0 : 1)} years across the drum`
      : `${Math.round(span)} days across the drum`;
    ensureData(false);
  }

  /* — physics: momentum spin — */
  const FUTURE_LIMIT = Date.now() / DAY_MS + 120;
  const clampCenter = () => { center = Math.min(center, FUTURE_LIMIT); };
  let vel = 0, raf = 0, settleTimer = null;
  function kick() {
    cancelAnimationFrame(raf);
    const step = () => {
      center += vel;
      clampCenter();
      vel *= 0.93;
      draw();
      if (Math.abs(vel) > span / 4000) raf = requestAnimationFrame(step);
      else settled();
    };
    raf = requestAnimationFrame(step);
  }
  function settled() {
    clearTimeout(settleTimer);
    settleTimer = setTimeout(() => {
      if (onWindow) onWindow(numDay(center - span / 2), numDay(center + span / 2));
      fetchNotable();
    }, 250);
  }

  let dragging = false, lastX = 0, lastT = 0, moved = 0;
  canvas.addEventListener("pointerdown", (e) => {
    dragging = true; moved = 0;
    lastX = e.clientX; lastT = performance.now();
    vel = 0; cancelAnimationFrame(raf);
    canvas.setPointerCapture(e.pointerId);
  });
  canvas.addEventListener("pointermove", (e) => {
    if (!dragging) return;
    const dx = e.clientX - lastX;
    moved += Math.abs(dx);
    const daysPerPx = span / (W * 0.8);
    center -= dx * daysPerPx;
    clampCenter();
    const dt = Math.max(performance.now() - lastT, 1);
    vel = -dx * daysPerPx * Math.min(16 / dt, 2);
    lastX = e.clientX; lastT = performance.now();
    draw();
  });
  canvas.addEventListener("pointerup", () => {
    dragging = false;
    if (Math.abs(vel) > span / 2000) kick();
    else settled();
  });
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    if (e.ctrlKey || e.metaKey) {
      // pinch (mac trackpad sends ctrl+wheel) / explicit zoom
      const at = dayAtX(e.offsetX);
      span = clamp(span * (1 + e.deltaY * 0.012), MIN_SPAN, MAX_SPAN);
      center = at + (center - at) * 1; // keep pointer date roughly fixed
      clampCenter();
      draw();
      settled();
    } else {
      const d = (Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY);
      vel = d * (span / (W * 0.8)) * 0.6;
      kick();
    }
  }, { passive: false });
  canvas.addEventListener("dblclick", (e) => {
    center = dayAtX(e.offsetX);
    span = clamp(span / 2.2, MIN_SPAN, MAX_SPAN);
    draw();
    settled();
  });
  canvas.addEventListener("click", (e) => {
    if (moved > 5) return; // it was a drag
    const label = labelHits.find(({ box }) =>
      e.offsetX >= box[0] && e.offsetX <= box[0] + box[2] &&
      e.offsetY >= box[1] && e.offsetY <= box[1] + box[3]);
    if (label) {
      // an event label opens its day unless the host says otherwise
      if (onLabel) onLabel(label.ev);
      else location.hash = `#/day/${label.ev.day}`;
      return;
    }
    const hit = visible.find((v) => Math.abs(v.x - e.offsetX) < 6 && e.offsetY > DRUM_TOP);
    if (hit && onPick) onPick(hit.b);
  });
  canvas.addEventListener("mousemove", (e) => {
    const label = labelHits.find(({ box }) =>
      e.offsetX >= box[0] && e.offsetX <= box[0] + box[2] &&
      e.offsetY >= box[1] && e.offsetY <= box[1] + box[3]);
    const nearBar = visible.some((v) => Math.abs(v.x - e.offsetX) < 6 && e.offsetY > DRUM_TOP);
    canvas.style.cursor = label || nearBar ? "pointer" : "grab";
    canvas.title = label ? `${label.ev.day} · ${label.ev.source} — ${label.ev.title}` : "";
  });

  window.addEventListener("resize", resize, { passive: true });
  resize();
  ensureData(true);
  fetchNotable();

  return {
    setWindow(from, to) {
      if (!from || !to) return;
      const a = dayNum(from), b = dayNum(to);
      if (!Number.isFinite(a) || !Number.isFinite(b)) return;
      center = (a + b) / 2;
      span = clamp(Math.max(b - a + 1, MIN_SPAN), MIN_SPAN, MAX_SPAN);
      draw();
      fetchNotable();
    },
    refresh() { ensureData(true); },
    destroy() {
      cancelAnimationFrame(raf);
      clearTimeout(settleTimer);
      themeWatcher.disconnect();
      window.removeEventListener("resize", resize);
    },
  };
}
