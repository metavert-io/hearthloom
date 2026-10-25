// atlas.js — the Atlas of Embers: your presence on a vendored vector
// world (Natural Earth 110m, public domain — no tiles, no cloud; the
// doctrine allows a configured cloud renderer later, with this as the
// fallback). Canvas 2D: warm coastlines on the night background, days
// as glowing embers, movements as fading arcs when the time-lapse
// plays. All colors come from CSS custom properties so both themes
// hold (see app.css "atlas" section).

let world = null; // {polys: [[[lon,lat],...], ...]}

async function loadWorld() {
  if (!world) {
    world = await (await fetch("/assets/vendor/world-land-110m.json")).json();
  }
  return world;
}

export async function mount(container) {
  await loadWorld();
  container.innerHTML = `
    <div class="atlas-head">
      <div class="atlas-title">
        <h2>Atlas</h2>
        <span class="atlas-sub">where the evidence places you · <span id="atlas-count"></span></span>
      </div>
      <div class="atlas-controls">
        <input type="date" id="atlas-from" title="from">
        <span class="atlas-dash">–</span>
        <input type="date" id="atlas-to" title="to">
        <button class="btn" id="atlas-play" title="play the journey">▶ play</button>
        <button class="btn" id="atlas-globe" title="globe mode" hidden>🌐</button>
        <select id="atlas-speed" title="days per second">
          <option value="30">1 mo/s</option>
          <option value="90" selected>3 mo/s</option>
          <option value="365">1 yr/s</option>
        </select>
      </div>
    </div>
    <div id="atlas-dial"></div>
    <div class="atlas-stage">
      <canvas id="atlas-canvas"></canvas>
      <div class="atlas-date" id="atlas-date" hidden></div>
      <div class="atlas-tip" id="atlas-tip" hidden></div>
      <div class="atlas-visit" id="atlas-visit" hidden></div>
      <div class="atlas-empty" id="atlas-empty" hidden>
        no location points in this range — the location index builds during
        consolidation from photo GPS and workout routes
      </div>
    </div>`;

  // Globe toggle binds by delegation immediately — mount continues with
  // slow awaits below, and a rebuilt inner DOM must not orphan it.
  let globe = null;
  container.addEventListener("click", async (e) => {
    const btn = e.target.closest("#atlas-globe");
    if (!btn) return;
    if (globe) {
      globe.dispose();
      globe = null;
      canvas.style.display = "";
      btn.textContent = "🌐";
      return;
    }
    try {
      const { mountGlobe } = await import("/assets/globe.js");
      canvas.style.display = "none";
      globe = mountGlobe(canvas.parentElement, { points, world, colors });
      btn.textContent = "🗺";
    } catch (err) {
      console.error("globe failed:", err);
      canvas.style.display = "";
    }
  });

  const canvas = container.querySelector("#atlas-canvas");
  const ctx = canvas.getContext("2d");
  const tip = container.querySelector("#atlas-tip");
  const dateEl = container.querySelector("#atlas-date");
  const emptyEl = container.querySelector("#atlas-empty");
  const fromEl = container.querySelector("#atlas-from");
  const toEl = container.querySelector("#atlas-to");
  const playBtn = container.querySelector("#atlas-play");
  const speedEl = container.querySelector("#atlas-speed");

  /* — theme colors, re-read when the theme flips — */
  let colors = {};
  const readColors = () => {
    const cs = getComputedStyle(document.documentElement);
    const v = (name, fallback) => (cs.getPropertyValue(name) || fallback).trim();
    colors = {
      land: v("--atlas-land", "#211c16"),
      coast: v("--atlas-coast", "#57493a"),
      ember: v("--chart-1", "#e8833a"),
      emberHot: v("--ember-100", "#ffe8c8"),
      arc: v("--chart-3", "#d9a441"),
      ink: v("--ink-mute", "#a89a86"),
    };
  };
  readColors();
  const themeWatcher = new MutationObserver(() => { readColors(); draw(); });
  themeWatcher.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

  /* — viewport: equirectangular with zoom/pan — */
  let zoom = 1, panX = 0, panY = 0; // pan in canvas px
  let W = 0, H = 0, dpr = 1;
  const base = () => Math.min(W / 360, H / 180); // px per degree at zoom 1
  const px = (lon, lat) => {
    const s = base() * zoom;
    return [W / 2 + lon * s + panX, H / 2 - lat * s + panY];
  };

  function resize() {
    dpr = window.devicePixelRatio || 1;
    const rect = canvas.parentElement.getBoundingClientRect();
    W = rect.width;
    H = Math.max(rect.height, 420);
    canvas.width = W * dpr;
    canvas.height = H * dpr;
    canvas.style.width = `${W}px`;
    canvas.style.height = `${H}px`;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    draw();
  }

  /* — data — */
  let points = []; // sorted by day asc
  let journey = []; // one entry per day: {day, lat, lon, places, n} (dominant cluster)
  async function load() {
    const from = fromEl.value || "0000-01-01";
    const to = toEl.value || "9999-12-31";
    const data = await (await fetch(`/v1/atlas/points?from=${from}&to=${to}`)).json();
    points = data.points || [];
    container.querySelector("#atlas-count").textContent =
      `${points.length.toLocaleString()} points`;
    emptyEl.hidden = points.length > 0;
    // The journey: one dominant location per day, for arcs + the lapse.
    const byDay = new Map();
    for (const p of points) {
      const cur = byDay.get(p.day);
      if (!cur || p.n > cur.n) byDay.set(p.day, p);
    }
    journey = [...byDay.values()].sort((a, b) => (a.day < b.day ? -1 : 1));
    if (!fromEl.value && journey.length) fromEl.value = journey[0].day;
    if (!toEl.value && journey.length) toEl.value = journey[journey.length - 1].day;
    stop();
    draw();
  }

  /* — drawing — */
  let landPath = null;
  function buildLand() {
    landPath = new Path2D();
    for (const poly of world.polys) {
      landPath.moveTo(poly[0][0], -poly[0][1]);
      for (let i = 1; i < poly.length; i++) landPath.lineTo(poly[i][0], -poly[i][1]);
      landPath.closePath();
    }
  }
  buildLand();

  function drawWorld() {
    ctx.clearRect(0, 0, W, H);
    const s = base() * zoom;
    ctx.save();
    ctx.translate(W / 2 + panX, H / 2 + panY);
    ctx.scale(s, s);
    ctx.fillStyle = colors.land;
    ctx.fill(landPath);
    ctx.lineWidth = 0.8 / s;
    ctx.strokeStyle = colors.coast;
    ctx.stroke(landPath);
    ctx.restore();
  }

  function drawEmber(lon, lat, r, alpha) {
    const [x, y] = px(lon, lat);
    if (x < -20 || x > W + 20 || y < -20 || y > H + 20) return;
    ctx.save();
    ctx.globalAlpha = alpha;
    ctx.shadowColor = colors.ember;
    ctx.shadowBlur = r * 3;
    ctx.fillStyle = colors.ember;
    ctx.beginPath();
    ctx.arc(x, y, r, 0, Math.PI * 2);
    ctx.fill();
    ctx.shadowBlur = 0;
    ctx.fillStyle = colors.emberHot;
    ctx.beginPath();
    ctx.arc(x, y, Math.max(r * 0.35, 0.6), 0, Math.PI * 2);
    ctx.fill();
    ctx.restore();
  }

  function drawArc(a, b, alpha) {
    const [x1, y1] = px(a.lon, a.lat);
    const [x2, y2] = px(b.lon, b.lat);
    const dx = x2 - x1, dy = y2 - y1;
    const dist = Math.hypot(dx, dy);
    if (dist < 3) return;
    ctx.save();
    ctx.globalAlpha = alpha;
    ctx.strokeStyle = colors.arc;
    ctx.lineWidth = 1.1;
    ctx.beginPath();
    ctx.moveTo(x1, y1);
    // lift the midpoint perpendicular for a flight-map curve
    ctx.quadraticCurveTo(
      (x1 + x2) / 2 - dy * 0.18,
      (y1 + y2) / 2 + dx * 0.18,
      x2, y2,
    );
    ctx.stroke();
    ctx.restore();
  }

  let visitHighlight = null; // Set of points in the open visit
  function draw() {
    drawWorld();
    if (playing) return; // the lapse loop draws its own layer
    const dim = visitHighlight ? 0.3 : 0.75;
    for (const p of points) {
      if (visitHighlight && visitHighlight.has(p)) continue;
      drawEmber(p.lon, p.lat, 1.6 + Math.log2(1 + p.n), dim);
    }
    if (visitHighlight) {
      for (const p of visitHighlight) {
        drawEmber(p.lon, p.lat, 2.4 + Math.log2(1 + p.n), 1);
      }
    }
  }

  /* — the time-lapse — */
  let playing = false, raf = 0, cursor = 0, lastTick = 0;
  const TRAIL_DAYS = 45;
  const dayNum = (d) => Date.parse(`${d}T12:00:00Z`) / 86400000;

  function lapseFrame(now) {
    if (!playing) return;
    const speed = Number(speedEl.value); // days per second
    if (lastTick) cursor += ((now - lastTick) / 1000) * speed;
    lastTick = now;
    const start = dayNum(journey[0].day);
    const end = dayNum(journey[journey.length - 1].day);
    const at = start + cursor;
    if (at > end + TRAIL_DAYS) { stop(); draw(); return; }

    drawWorld();
    let prev = null;
    for (const p of journey) {
      const d = dayNum(p.day);
      if (d > at) break;
      const age = at - d;
      if (prev && age <= TRAIL_DAYS) {
        drawArc(prev, p, Math.max(0, 1 - age / TRAIL_DAYS) * 0.8);
      }
      if (age <= TRAIL_DAYS) {
        const heat = Math.max(0, 1 - age / TRAIL_DAYS);
        drawEmber(p.lon, p.lat, 1.5 + heat * 3.5, 0.25 + heat * 0.75);
      } else {
        drawEmber(p.lon, p.lat, 1.1, 0.16); // cooled ash of where you've been
      }
      prev = p;
    }
    const shown = new Date((at) * 86400000);
    dateEl.textContent = shown.toISOString().slice(0, 7);
    raf = requestAnimationFrame(lapseFrame);
  }

  function play() {
    if (!journey.length) return;
    playing = true;
    cursor = 0;
    lastTick = 0;
    dateEl.hidden = false;
    playBtn.textContent = "◼ stop";
    raf = requestAnimationFrame(lapseFrame);
  }
  function stop() {
    playing = false;
    cancelAnimationFrame(raf);
    dateEl.hidden = true;
    playBtn.textContent = "▶ play";
  }
  playBtn.onclick = () => (playing ? (stop(), draw()) : play());

  /* — pan / zoom / hover / click — */
  let dragging = false, dragFrom = null;
  canvas.addEventListener("mousedown", (e) => { dragging = true; dragFrom = [e.offsetX - panX, e.offsetY - panY]; });
  window.addEventListener("mouseup", () => (dragging = false));
  canvas.addEventListener("mousemove", (e) => {
    if (dragging) {
      panX = e.offsetX - dragFrom[0];
      panY = e.offsetY - dragFrom[1];
      if (!playing) draw();
      return;
    }
    const hit = nearest(e.offsetX, e.offsetY);
    if (hit) {
      tip.hidden = false;
      tip.style.left = `${e.offsetX + 14}px`;
      tip.style.top = `${e.offsetY + 6}px`;
      tip.innerHTML = `<b>${hit.day}</b>${hit.places.length ? " · " + hit.places.slice(0, 3).join(", ") : ""}<span class="atlas-tip-n">${hit.n} episode${hit.n === 1 ? "" : "s"} · click for the day</span>`;
      canvas.style.cursor = "pointer";
    } else {
      tip.hidden = true;
      canvas.style.cursor = dragging ? "grabbing" : "grab";
    }
  });
  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    const factor = e.deltaY < 0 ? 1.15 : 1 / 1.15;
    const nz = Math.min(Math.max(zoom * factor, 0.8), 40);
    // zoom toward the pointer
    const k = nz / zoom;
    panX = (panX - (e.offsetX - W / 2)) * k + (e.offsetX - W / 2);
    panY = (panY - (e.offsetY - H / 2)) * k + (e.offsetY - H / 2);
    zoom = nz;
    if (!playing) draw();
  }, { passive: false });
  canvas.addEventListener("click", (e) => {
    if (dragFrom && Math.hypot(e.offsetX - panX - dragFrom[0], e.offsetY - panY - dragFrom[1]) > 4) return;
    const hit = nearest(e.offsetX, e.offsetY);
    if (hit) openVisit(hit);
    else closeVisit();
  });

  /* — visits: a click asks "what did I do here", not "jump to a day" —
     Stay-point segmentation over the loaded points: cluster within
     VISIT_RADIUS_KM of the click, split day-runs at gaps > VISIT_GAP
     days, pick the run containing the clicked day. Long runs / places
     holding a big share of all located days are ANCHORS (home bases):
     stats + a small window instead of a monster list. — */
  const VISIT_RADIUS_KM = 25, VISIT_GAP = 2, TRIP_MAX_DAYS = 14, ANCHOR_SHARE = 0.2, ANCHOR_WINDOW = 3;
  const visitEl = container.querySelector("#atlas-visit");

  function kmBetween(a, b) {
    const dLat = (a.lat - b.lat) * 111.32;
    const dLon = (a.lon - b.lon) * 111.32 * Math.cos(((a.lat + b.lat) / 2) * Math.PI / 180);
    return Math.hypot(dLat, dLon);
  }

  /* Trust ranking inside segmentation: a photo is a sensor observing
     you somewhere; a facebook check-in is a declaration. When the same
     day disagrees (>VISIT_RADIUS_KM apart), the declared point stays
     on the map as an ember but does NOT seed or extend a visit — the
     sensor wins the "where was I" question. No sensor that day →
     the declaration stands. */
  const isSensor = (p) => (p.sources || []).includes("photos");
  function trustFiltered(list) {
    const sensorByDay = new Map();
    for (const p of list) {
      if (!isSensor(p)) continue;
      if (!sensorByDay.has(p.day)) sensorByDay.set(p.day, []);
      sensorByDay.get(p.day).push(p);
    }
    return list.filter((p) => {
      if (isSensor(p)) return true;
      const sensors = sensorByDay.get(p.day);
      if (!sensors) return true;
      return sensors.some((s) => kmBetween(s, p) <= VISIT_RADIUS_KM);
    });
  }

  function segmentVisits(hit) {
    const near = trustFiltered(points).filter((p) => kmBetween(p, hit) <= VISIT_RADIUS_KM);
    const days = [...new Set(near.map((p) => p.day))].sort();
    const runs = [];
    let run = [];
    for (const d of days) {
      if (run.length && dayNum(d) - dayNum(run[run.length - 1]) > VISIT_GAP + 1) {
        runs.push(run);
        run = [];
      }
      run.push(d);
    }
    if (run.length) runs.push(run);
    const current = runs.find((r) => r.includes(hit.day)) || runs[0];
    const placeCounts = new Map();
    for (const p of near) for (const name of p.places) placeCounts.set(name, (placeCounts.get(name) || 0) + 1);
    const place = [...placeCounts.entries()].sort((a, b) => b[1] - a[1])[0]?.[0]
      || `${hit.lat.toFixed(2)}, ${hit.lon.toFixed(2)}`;
    const totalDays = new Set(points.map((p) => p.day)).size;
    const anchor = current.length > TRIP_MAX_DAYS || days.length / Math.max(totalDays, 1) > ANCHOR_SHARE;
    return { near, days, runs, current, place, anchor };
  }

  const fmtRange = (run) => run.length === 1 ? run[0] : `${run[0]} → ${run[run.length - 1]}`;
  const escapeHtml = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

  async function openVisit(hit) {
    const v = segmentVisits(hit);
    visitHighlight = new Set(v.near.filter((p) => v.current.includes(p.day)));
    draw();
    const run = v.anchor
      ? v.days.filter((d) => Math.abs(dayNum(d) - dayNum(hit.day)) <= ANCHOR_WINDOW)
      : v.current;
    const others = v.runs.filter((r) => r !== v.current).slice(-4).reverse();

    // Day rows land lazily; remember the visit so Day view can show a
    // "part of this visit" chip pointing back here.
    sessionStorage.setItem("hl-visit", JSON.stringify({
      label: v.place, from: run[0], to: run[run.length - 1],
    }));

    visitEl.hidden = false;
    visitEl.innerHTML = `
      <div class="visit-head">
        <div>
          <div class="visit-place">${escapeHtml(v.place)}</div>
          <div class="visit-when">${v.anchor
            ? `home base · ${v.days.length.toLocaleString()} located days · ${v.days[0].slice(0, 4)}–${v.days[v.days.length - 1].slice(0, 4)}`
            : `${fmtRange(v.current)} · ${v.current.length} day${v.current.length === 1 ? "" : "s"}`}</div>
        </div>
        <button class="visit-close" title="close">×</button>
      </div>
      ${v.anchor ? `<div class="visit-anchor-note">showing ±${ANCHOR_WINDOW} days around ${escapeHtml(hit.day)}</div>` : ""}
      ${others.length ? `<div class="visit-stays">also here: ${others.map((r) =>
        `<button class="visit-stay" data-day="${r[0]}">${r[0].slice(0, 7)} (${r.length}d)</button>`).join(" ")}</div>` : ""}
      <div class="visit-days">${run.map((d) =>
        `<a class="visit-day" href="#/day/${d}" data-day="${d}">
           <span class="visit-day-date">${d}</span>
           <span class="visit-day-info">…</span>
         </a>`).join("")}</div>
      <div class="visit-foot">
        <a href="#/browse/episodes" class="visit-browse" data-from="${run[0]}" data-to="${run[run.length - 1]}"
           data-place="${escapeHtml(v.place)}">open in Browse →</a>
      </div>`;

    visitEl.querySelector(".visit-close").onclick = closeVisit;
    visitEl.querySelectorAll(".visit-stay").forEach((b) => {
      b.onclick = () => {
        const p = v.near.find((pt) => pt.day === b.dataset.day) || hit;
        openVisit(p);
      };
    });
    const browseLink = visitEl.querySelector(".visit-browse");
    browseLink.onclick = () => {
      if (window.__browsePrefill) {
        window.__browsePrefill({
          place: v.anchor ? browseLink.dataset.place : browseLink.dataset.place,
          from: v.anchor ? browseLink.dataset.from : run[0],
          to: v.anchor ? browseLink.dataset.to : run[run.length - 1],
        });
      }
    };

    // Fill day rows with counts + a couple of concrete titles.
    const INTERESTING = ["photos", "calendar_event", "workout", "commit", "article"];
    await Promise.all(run.map(async (d) => {
      try {
        const day = await (await fetch(`/v1/recall?on=${d}&detail=summary`)).json();
        const row = visitEl.querySelector(`.visit-day[data-day="${d}"] .visit-day-info`);
        if (!row) return;
        const eps = day.episodes || [];
        const picks = eps
          .filter((ep) => INTERESTING.includes(ep.kind))
          .concat(eps)
          .slice(0, 2)
          .map((ep) => escapeHtml(ep.title.slice(0, 52)));
        row.innerHTML = `<b>${day.episode_count ?? eps.length}</b> episodes${
          picks.length ? ` · ${picks.join(" · ")}` : ""}`;
      } catch { /* daemon busy */ }
    }));
  }

  function closeVisit() {
    visitEl.hidden = true;
    visitHighlight = null;
    sessionStorage.removeItem("hl-visit");
    draw();
  }
  function nearest(mx, my) {
    let best = null, bestD = 12;
    for (const p of points) {
      const [x, y] = px(p.lon, p.lat);
      const d = Math.hypot(x - mx, y - my);
      if (d < bestD) { bestD = d; best = p; }
    }
    return best;
  }

  fromEl.onchange = toEl.onchange = load;
  window.addEventListener("resize", resize, { passive: true });
  resize();
  await load();

  /* — dial labels: departures from the norm. Home is wherever most of
     your located days cluster; everything ELSE that held you for a few
     days is a story — so trips get labels, weighted by how long you
     stayed and how far from home you went. Chapters ride the same
     ticks (the Memories treatment), places in ember with a ⌖. — */
  let homeCache = null;
  async function ensureHome() {
    if (homeCache) return homeCache;
    const data = await (await fetch(`/v1/atlas/points?from=0000-01-01&to=9999-12-31`)).json();
    const cellDays = new Map();
    for (const p of data.points || []) {
      const key = `${Math.round(p.lat * 2) / 2},${Math.round(p.lon * 2) / 2}`;
      if (!cellDays.has(key)) cellDays.set(key, new Set());
      cellDays.get(key).add(p.day);
    }
    let best = null;
    for (const [key, days] of cellDays) {
      if (!best || days.size > best.days) {
        const [lat, lon] = key.split(",").map(Number);
        best = { lat, lon, days: days.size };
      }
    }
    homeCache = best || { lat: 0, lon: 0, days: 0 };
    return homeCache;
  }

  const TRIP_LABEL_MAX_DAYS = 21, HOME_KM = 40;
  const stayLabelCache = new Map();
  async function departureLabels(from, to) {
    const key = `${from}|${to}`;
    if (stayLabelCache.has(key)) return stayLabelCache.get(key);
    const home = await ensureHome();
    const data = await (await fetch(
      `/v1/atlas/points?from=${from}&to=${to}`)).json();
    // one dominant point per day, chronological
    const byDay = new Map();
    for (const p of data.points || []) {
      const cur = byDay.get(p.day);
      if (!cur || p.n > cur.n) byDay.set(p.day, p);
    }
    const seq = [...byDay.values()].sort((a, b) => (a.day < b.day ? -1 : 1));
    // group consecutive days into stays (gap ≤ VISIT_GAP+1, ≤ radius)
    const stays = [];
    let cur = null;
    for (const p of seq) {
      if (cur && kmBetween(cur, p) <= VISIT_RADIUS_KM
          && dayNum(p.day) - dayNum(cur.lastDay) <= VISIT_GAP + 1) {
        cur.pts.push(p);
        cur.lastDay = p.day;
      } else {
        if (cur) stays.push(cur);
        cur = { lat: p.lat, lon: p.lon, pts: [p], firstDay: p.day, lastDay: p.day };
      }
    }
    if (cur) stays.push(cur);

    const labels = [];
    for (const s of stays) {
      const kmHome = kmBetween(s, home);
      const len = dayNum(s.lastDay) - dayNum(s.firstDay) + 1;
      // the norm is home and month-long encampments; skip both
      if (kmHome <= HOME_KM || len > TRIP_LABEL_MAX_DAYS) continue;
      const names = new Map();
      for (const p of s.pts) {
        for (const name of p.places || []) names.set(name, (names.get(name) || 0) + 1);
      }
      const title = [...names.entries()].sort((a, b) => b[1] - a[1])[0]?.[0];
      if (!title) continue; // a nameless dot is not a label
      const mid = new Date((dayNum(s.firstDay) + Math.floor(len / 2)) * 86400000)
        .toISOString().slice(0, 10);
      labels.push({
        day: mid,
        title,
        kind: "place",
        // farther + longer = more of a departure = louder label
        score: 26 + Math.min(len * 3, 18) + Math.min(kmHome / 400, 12),
      });
    }
    if (stayLabelCache.size > 40) stayLabelCache.clear();
    stayLabelCache.set(key, labels);
    return labels;
  }

  // The same clock, everywhere: a compact dial above the map — now with
  // the label tier: chapter headings + place departures on the ticks.
  let atlasDial = null;
  try {
    const { mountDial } = await import("/assets/dial.js");
    atlasDial = mountDial(container.querySelector("#atlas-dial"), {
      from: fromEl.value || undefined,
      to: toEl.value || undefined,
      compact: true,
      labels: true,
      labelFilter: (ev) => ev.kind === "chapter" || ev.kind === "place",
      extraLabels: departureLabels,
      filters: () => ({}),
      onWindow: (from, to) => {
        if (fromEl.value === from && toEl.value === to) return;
        fromEl.value = from;
        toEl.value = to;
        load();
      },
      onPick: () => {},
    });
    const syncDial = () => atlasDial?.setWindow(fromEl.value, toEl.value);
    fromEl.addEventListener("change", syncDial);
    toEl.addEventListener("change", syncDial);
  } catch { /* dial optional */ }

  return () => { stop(); globe?.dispose(); themeWatcher.disconnect(); window.removeEventListener("resize", resize); };
}
