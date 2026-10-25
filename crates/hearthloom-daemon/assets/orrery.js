// orrery.js — the Orrery, reborn: Jon's retired project lends its name
// to the visualization of what replaced it. Each active GOAL is a sun;
// every episode it has caught orbits it as an ember — recent catches
// orbit close and hot, old ones drift far and cool. Unattached goals sit
// as quiet, dim suns waiting for their first catch. One canvas, gentle
// clockwork motion (it IS an orrery), hover reads out the catch, click
// opens its day; clicking a sun opens the goal's matches in Search.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const css = (name, fallback) =>
  (getComputedStyle(document.documentElement).getPropertyValue(name) || fallback).trim();
const DAY_MS = 86400000;

const SUN_VARS = ["--chart-1", "--chart-3", "--chart-2", "--chart-4", "--chart-5", "--chart-6"];

async function fetchJSON(url) {
  return (await fetch(url)).json();
}

export async function mount(container) {
  container.innerHTML = `
    <div class="orrery-wrap">
      <div class="loom-head">
        <span class="eyebrow">the orrery</span>
        <h1 class="pagehead">Goals in motion</h1>
        <p class="subhead">Every active goal is a sun; everything its standing watch has
          caught orbits it — fresh catches close and bright, old ones far and cooling.
          Hover a body to read it, click to open its day; click a sun for its catches.</p>
      </div>
      <canvas id="orrery-canvas"></canvas>
      <div class="loom-tip" id="orrery-tip" hidden></div>
      <div class="orrery-empty" id="orrery-empty" hidden>
        <p class="meta">No active goals yet — set one on the
        <a href="#/desk">Desk</a> and the watch begins.</p>
      </div>
    </div>`;

  const canvas = container.querySelector("#orrery-canvas");
  const tip = container.querySelector("#orrery-tip");
  const ctx = canvas.getContext("2d");

  // — data: active goals + their matches (episodes, unseen state) —
  const goals = ((await fetchJSON("/v1/intents")).intents || [])
    .filter((g) => g.status === "active");
  if (!goals.length) {
    container.querySelector("#orrery-empty").hidden = false;
  }
  const systems = await Promise.all(goals.map(async (g, i) => {
    let matches = [];
    try {
      const data = await fetchJSON(
        `/v1/intents/${encodeURIComponent(g.name)}/matches?mark_seen=false`);
      matches = data.matches || [];
    } catch { /* goal with no matches yet */ }
    const now = Date.now();
    const bodies = matches.slice(0, 60).map((m, j) => {
      const when = Date.parse(m.episode.event_time || m.found_at) || now;
      const ageDays = Math.max((now - when) / DAY_MS, 0);
      return {
        episode: m.episode,
        seen: m.seen,
        // fresh = tight hot orbit; a year old = the cold rim
        orbit: 0.22 + 0.78 * Math.min(ageDays / 365, 1),
        phase: (j * 2.399963) % (Math.PI * 2), // golden angle: no clumps
        speed: (0.05 + 0.12 / (0.4 + ageDays / 90)) * (j % 2 ? 1 : -1),
        heat: Math.max(0, 1 - ageDays / 365),
      };
    });
    return { goal: g, bodies, color: css(SUN_VARS[i % SUN_VARS.length], "#e8833a") };
  }));

  // — layout: suns spread on a loose ring around the view center —
  let W = 0, H = 0, dpr = 1;
  function layout() {
    const n = systems.length || 1;
    systems.forEach((sys, i) => {
      const angle = (i / n) * Math.PI * 2 - Math.PI / 2;
      const ringR = n === 1 ? 0 : Math.min(W, H) * 0.28;
      sys.cx = W / 2 + Math.cos(angle) * ringR;
      sys.cy = H / 2 + Math.sin(angle) * ringR * 0.82;
      sys.maxR = n === 1 ? Math.min(W, H) * 0.4 : Math.min(W, H) * 0.21;
    });
  }
  function resize() {
    W = container.clientWidth;
    H = Math.max(480, window.innerHeight - 260);
    dpr = window.devicePixelRatio || 1;
    canvas.width = W * dpr;
    canvas.height = H * dpr;
    canvas.style.width = `${W}px`;
    canvas.style.height = `${H}px`;
    layout();
  }

  let hover = null; // {sys, body} | {sys}
  let t = 0;
  function draw() {
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, W, H);
    const night = document.documentElement.dataset.theme !== "daylight";
    const inkFaint = css("--ink-faint", "#6b6154");
    const bgGlow = night ? "rgba(232,131,58," : "rgba(192,95,29,";

    for (const sys of systems) {
      const focused = hover && hover.sys === sys;
      // orbit rings, whisper-faint
      ctx.strokeStyle = inkFaint;
      for (const ring of [0.33, 0.66, 1]) {
        ctx.globalAlpha = focused ? 0.28 : 0.14;
        ctx.beginPath();
        ctx.ellipse(sys.cx, sys.cy, sys.maxR * ring, sys.maxR * ring * 0.62, 0, 0, Math.PI * 2);
        ctx.stroke();
      }
      ctx.globalAlpha = 1;

      // the sun: bigger + warmer with more catches; a halo when fresh
      const catches = sys.bodies.length;
      const unseen = sys.bodies.filter((b) => !b.seen).length;
      const sunR = 7 + Math.min(Math.sqrt(catches) * 2.2, 13);
      const pulse = 1 + Math.sin(t * 1.6 + sys.cx) * 0.05;
      const glow = ctx.createRadialGradient(sys.cx, sys.cy, 1, sys.cx, sys.cy, sunR * 3.4 * pulse);
      glow.addColorStop(0, bgGlow + (unseen ? "0.5)" : "0.3)"));
      glow.addColorStop(1, bgGlow + "0)");
      ctx.fillStyle = glow;
      ctx.beginPath();
      ctx.arc(sys.cx, sys.cy, sunR * 3.4 * pulse, 0, Math.PI * 2);
      ctx.fill();
      ctx.fillStyle = sys.color;
      ctx.beginPath();
      ctx.arc(sys.cx, sys.cy, sunR * pulse, 0, Math.PI * 2);
      ctx.fill();

      // the name under the sun; catch count if any
      ctx.fillStyle = css("--ink", "#e8ddcb");
      ctx.font = `italic 600 13px ${css("--font-display", "serif")}`;
      ctx.textAlign = "center";
      ctx.fillText(sys.goal.name, sys.cx, sys.cy + sunR + 18);
      ctx.fillStyle = inkFaint;
      ctx.font = `10px ${css("--font-mono", "monospace")}`;
      ctx.fillText(
        catches ? `${catches} caught${unseen ? ` · ${unseen} new` : ""}` : "watching…",
        sys.cx, sys.cy + sunR + 32);

      // the bodies
      for (const body of sys.bodies) {
        const angle = body.phase + t * body.speed;
        const r = sys.maxR * body.orbit;
        body.x = sys.cx + Math.cos(angle) * r;
        body.y = sys.cy + Math.sin(angle) * r * 0.62;
        const isHover = hover && hover.body === body;
        const size = 1.6 + body.heat * 2.6 + (isHover ? 1.6 : 0);
        ctx.globalAlpha = 0.35 + body.heat * 0.6;
        ctx.fillStyle = isHover ? css("--ember-100", "#ffe8c8") : sys.color;
        ctx.beginPath();
        ctx.arc(body.x, body.y, size, 0, Math.PI * 2);
        ctx.fill();
        ctx.globalAlpha = 1;
      }
    }
  }

  let raf = 0, last = 0;
  function animate(now) {
    if (!last) last = now;
    t += (now - last) / 1000;
    last = now;
    draw();
    raf = requestAnimationFrame(animate);
  }

  canvas.addEventListener("mousemove", (e) => {
    const mx = e.offsetX, my = e.offsetY;
    hover = null;
    for (const sys of systems) {
      if (Math.hypot(mx - sys.cx, my - sys.cy) < 22) {
        hover = { sys };
        break;
      }
      for (const body of sys.bodies) {
        if (body.x != null && Math.hypot(mx - body.x, my - body.y) < 7) {
          hover = { sys, body };
          break;
        }
      }
      if (hover) break;
    }
    if (hover?.body) {
      const ep = hover.body.episode;
      tip.hidden = false;
      tip.innerHTML = `<b style="color:${hover.sys.color}">${esc(hover.sys.goal.name)}</b>
        caught · ${esc((ep.event_time || "").slice(0, 10))} ·
        ${esc(ep.title.slice(0, 70))} <span class="meta">— click to open</span>`;
      tip.style.left = `${Math.min(mx + 14, W - 300)}px`;
      tip.style.top = `${my - 30}px`;
      canvas.style.cursor = "pointer";
    } else if (hover?.sys) {
      tip.hidden = false;
      tip.innerHTML = `<b style="color:${hover.sys.color}">${esc(hover.sys.goal.name)}</b>
        <span class="meta">· ${esc(hover.sys.goal.query.slice(0, 60))} — click for its catches</span>`;
      tip.style.left = `${Math.min(mx + 14, W - 300)}px`;
      tip.style.top = `${my - 30}px`;
      canvas.style.cursor = "pointer";
    } else {
      tip.hidden = true;
      canvas.style.cursor = "";
    }
  });
  canvas.addEventListener("mouseleave", () => { hover = null; tip.hidden = true; });
  canvas.addEventListener("click", () => {
    if (hover?.body) {
      const day = (hover.body.episode.event_time || "").slice(0, 10);
      if (day) location.hash = `#/day/${day}`;
    } else if (hover?.sys) {
      location.hash = `#/search/${encodeURIComponent(hover.sys.goal.query)}`;
    }
  });

  resize();
  const ro = new ResizeObserver(resize);
  ro.observe(container);
  raf = requestAnimationFrame(animate);
  return () => {
    cancelAnimationFrame(raf);
    ro.disconnect();
  };
}
