// rewind.js — the self-playing demo reel: thirty seconds of a life.
// Stage 1: the Atlas time-lapse sweeps every recorded journey.
// Stage 2: the Dial spins through the years while the chapter headings
// your hearth wrote read out like a table of contents.
// Stage 3: the numbers, then home. Esc or click leaves at any time.

const DAY_MS = 86400000;
const iso = (n) => new Date(n).toISOString().slice(0, 10);

export async function play() {
  const overlay = document.createElement("div");
  overlay.className = "rewind";
  overlay.innerHTML = `
    <div class="rewind-stage" id="rewind-stage"></div>
    <div class="rewind-caption" id="rewind-caption"></div>
    <div class="rewind-skip">esc to leave</div>`;
  document.body.appendChild(overlay);
  const stage = overlay.querySelector("#rewind-stage");
  const caption = overlay.querySelector("#rewind-caption");
  let alive = true;
  const leave = () => {
    if (!alive) return;
    alive = false;
    overlay.classList.add("out");
    setTimeout(() => overlay.remove(), 600);
  };
  overlay.addEventListener("click", leave);
  const onKey = (e) => { if (e.key === "Escape") leave(); };
  window.addEventListener("keydown", onKey, { once: true });
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const say = (text) => { caption.textContent = text; caption.classList.remove("in"); void caption.offsetWidth; caption.classList.add("in"); };

  try {
    // — Stage 1: everywhere you have ever been —
    say("Everywhere the record places you.");
    const { mount } = await import("/assets/atlas.js");
    stage.innerHTML = `<div class="rewind-atlas"></div>`;
    await mount(stage.firstElementChild);
    if (!alive) return;
    const speed = stage.querySelector("#atlas-speed");
    if (speed) speed.value = "365";
    // The atlas loads its journey asynchronously after mount — clicking
    // play too early is a silent no-op (the old "flashes and moves on"
    // bug). Poke play until the lapse actually starts (button flips to
    // stop), up to ~8s, THEN wait for it to finish.
    const playBtn = () => stage.querySelector("#atlas-play");
    for (let i = 0; i < 20 && alive; i++) {
      const btn = playBtn();
      if (btn?.textContent.includes("stop")) break; // running
      btn?.click();
      await wait(400);
    }
    if (!alive) return;
    // run until the lapse stops itself (or ~30s cap)
    for (let i = 0; i < 120 && alive; i++) {
      await wait(250);
      if (playBtn()?.textContent.includes("play")) break;
    }
    if (!alive) return;

    // — Stage 2: the chapters of a life —
    say("The chapters your hearth wrote.");
    stage.innerHTML = `<div class="rewind-dial"><div id="rewind-dial-host"></div>
      <div class="rewind-chapter" id="rewind-chapter"></div></div>`;
    const { mountDial } = await import("/assets/dial.js");
    const today = Date.now();
    const hist = await (await fetch(`/v1/timeline/histogram?from=0000-01-01&to=9999-12-31&bucket=year`)).json();
    const years = hist.buckets.filter((b) => b.n > 15).map((b) => +b.bucket);
    const startYear = Math.min(...years);
    const dial = mountDial(document.getElementById("rewind-dial-host"), {
      from: `${startYear}-01-01`, to: `${startYear}-06-30`,
      filters: () => ({}),
      onWindow: () => {}, onPick: () => {},
    });
    const chapterEl = document.getElementById("rewind-chapter");
    let cursor = Date.parse(`${startYear}-03-01T12:00:00Z`);
    while (alive && cursor < today) {
      const mid = iso(cursor);
      dial.setWindow(iso(cursor - 60 * DAY_MS), iso(cursor + 60 * DAY_MS));
      // read the chapter straight from the notable endpoint (cached fast)
      fetch(`/v1/timeline/notable?from=${mid.slice(0, 7)}-01&to=${mid.slice(0, 7)}-28&bucket=month&k=3`)
        .then((r) => r.json())
        .then((d) => {
          const ch = (d.notable || []).find((n) => n.kind === "chapter");
          if (ch && alive) chapterEl.textContent = `“${ch.title}”`;
        }).catch(() => {});
      await wait(420);
      cursor += 120 * DAY_MS;
    }
    if (!alive) return;

    // — Stage 3: the numbers, then home —
    const stats = window.__stats || await (await fetch("/v1/stats")).json();
    say("");
    stage.innerHTML = `<div class="rewind-end">
      <div class="rewind-big">${(stats.episodes || 0).toLocaleString()}</div>
      <div class="rewind-line">episodes of a life · ${startYear} → ${new Date().getFullYear()}</div>
      <div class="rewind-line dim">one SQLite file · on this disk · yours</div>
    </div>`;
    await wait(4200);
  } finally {
    leave();
  }
}
