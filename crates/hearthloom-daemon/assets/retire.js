/* Retirement (D.12): the preview, the one confirm, the one undo.
   Retiring takes a memory — or a whole source — out of the record
   everywhere (search, Ask, the timeline, the galaxy, what friends can
   see) without deleting anything: what it fed is switched off with the
   prior state saved, and Undo puts every part of it back. */

let H; // helpers from app.js: { api, apiPost, esc }

const SUMMARY = { summary: "daily digest", review: "weekly review", chapter: "chapter heading" };

const plural = (n, one, many = `${one}s`) => `${Number(n).toLocaleString()} ${n === 1 ? one : many}`;

function overlay() {
  let el = document.getElementById("retire-overlay");
  if (!el) {
    el = document.createElement("div");
    el.id = "retire-overlay";
    el.className = "confirm-overlay";
    el.hidden = true;
    el.innerHTML = `<div class="confirm-card retire-card" role="dialog" aria-modal="true" aria-labelledby="retire-title"></div>`;
    document.body.appendChild(el);
  }
  return el;
}

/* Open the preview for `{kind: "memory"|"source", target, label}`.
   Resolves with the POST result after the confirm, or null. */
export function openRetire({ kind, target, label }, helpers) {
  H = helpers;
  const el = overlay();
  const card = el.firstElementChild;
  const esc = H.esc;
  card.innerHTML = `<h3 id="retire-title">Retire ${kind === "source" ? `everything from ${esc(target)}` : `“${esc(label || "this memory")}”`}?</h3>
    <p class="meta">working out everything it touched…</p>`;
  el.hidden = false;
  return new Promise((resolve) => {
    const close = (result) => {
      el.hidden = true;
      document.removeEventListener("keydown", onKey);
      el.onclick = null;
      resolve(result);
    };
    const onKey = (e) => { if (e.key === "Escape") close(null); };
    document.addEventListener("keydown", onKey);
    el.onclick = (e) => { if (e.target === el) close(null); };
    preview(kind, target, card, close);
  });
}

async function preview(kind, target, card, close) {
  const esc = H.esc;
  let p;
  try {
    p = await H.api(`/v1/retire/preview?kind=${kind}&target=${encodeURIComponent(target)}`);
  } catch (err) {
    card.innerHTML = `<h3 id="retire-title">Can't retire this</h3><p>${esc(err.message)}</p>
      <div class="confirm-actions"><button class="btn" data-close>Close</button></div>`;
    card.querySelector("[data-close]").onclick = () => close(null);
    return;
  }
  const c = p.counts;
  const changes = [
    [c.entity_links, "link to a person, place or thing", "links to people, places and things"],
    [c.edges, "connection loses its support", "connections lose their support"],
    [c.edges_retracted, "connection left with no support is withdrawn", "connections left with no support are withdrawn"],
    [c.intent_matches, "goal match", "goal matches"],
    [c.opportunities, "opportunity", "opportunities"],
    [c.map_points, "map point", "map points"],
    [c.semantic_pairs, "galaxy pairing", "galaxy pairings"],
  ].filter(([n]) => n > 0).map(([n, one, many]) => `<li>${plural(n, one, many)}</li>`);
  if (p.derived > 0) {
    changes.push(`<li>${plural(p.derived, "brief or opportunity written from it", "briefs and opportunities written from it")} retire${p.derived === 1 ? "s" : ""} with it</li>`);
  }
  const subject = kind === "source"
    ? `${plural(p.episodes, "memory", "memories")} from <b>${esc(p.label)}</b>, and anything that arrives from it later`
    : `“${esc(p.label)}”`;
  const hiddenMore = c.entities_hidden - p.entities_hidden.length;
  const hidden = c.entities_hidden ? `
    <section class="rt-sec">
      <h4>Hidden, not deleted</h4>
      <p>${plural(c.entities_hidden, "entity", "entities")} existed only because of it and will be hidden:</p>
      <div class="rt-chips">${p.entities_hidden.map((e) => `<span class="rt-chip">${esc(e.name)}<small>${esc(e.type)}</small></span>`).join("")}${
        hiddenMore > 0 ? `<span class="rt-chip faint">+${hiddenMore.toLocaleString()} more</span>` : ""}</div>
    </section>` : "";
  const summaries = p.summaries.length ? `
    <section class="rt-sec">
      <h4>Summaries to rewrite without it</h4>
      <p>These read it. Ticked ones are written again without it at the next idle cycle; untick any you'd rather keep as they are.</p>
      <div class="rt-sums">${p.summaries.map((s) => `
        <label class="rt-sum"><input type="checkbox" data-sum="${esc(s.id)}" checked>
          <span class="mono">${esc(s.day)}</span>
          <span>${esc(SUMMARY[s.kind] || s.kind)}${s.exact ? "" : ` <span class="rt-approx" title="${esc(s.why)} — matched by when it arrived, not a recorded read">≈</span>`}</span>
          <span class="faint rt-sumtitle">${esc(s.title)}</span>
        </label>`).join("")}</div>
    </section>` : "";
  const beliefs = p.beliefs.length ? `
    <section class="rt-sec">
      <h4>About-me beliefs back to Tend</h4>
      <p>They cited it, or a summary being rewritten — you'll be asked to confirm them again.</p>
      <ul class="rt-list" id="rt-beliefs"></ul>
    </section>` : "";
  const untraceable = p.untraceable.length ? `
    <section class="rt-sec">
      <h4>What can't be traced</h4>
      <ul class="rt-list faint">${p.untraceable.map((u) => `<li>${esc(u)}</li>`).join("")}</ul>
    </section>` : "";
  card.innerHTML = `
    <h3 id="retire-title">Retire ${kind === "source" ? `everything from ${esc(p.label)}` : "this memory"}?</h3>
    <p>${subject} leaves your record everywhere — search, Ask, the timeline, the galaxy and what friends can see.
      Nothing is deleted, and one undo puts all of it back.</p>
    ${changes.length ? `<section class="rt-sec"><h4>What it fed, switched off</h4><ul class="rt-list">${changes.join("")}</ul></section>` : ""}
    ${hidden}${summaries}${beliefs}${untraceable}
    <input class="confirm-input" id="rt-reason" placeholder="why (optional) — e.g. a phishing mail">
    <div class="confirm-actions">
      <button class="btn" data-cancel>Cancel</button>
      <button class="btn confirm-danger" data-go>Retire</button>
    </div>`;
  const ticked = () => [...card.querySelectorAll("input[data-sum]")].filter((b) => b.checked).map((b) => b.dataset.sum);
  const paintBeliefs = () => {
    const list = card.querySelector("#rt-beliefs");
    if (!list) return;
    const rewrite = new Set(ticked());
    const back = p.beliefs.filter((b) => b.always || b.via.some((id) => rewrite.has(id)));
    list.innerHTML = back.length
      ? back.map((b) => `<li>${esc(b.text)}</li>`).join("")
      : `<li class="faint">none, with the summaries you've kept</li>`;
  };
  paintBeliefs();
  card.querySelectorAll("input[data-sum]").forEach((box) => { box.onchange = paintBeliefs; });
  card.querySelector("[data-cancel]").onclick = () => close(null);
  card.querySelector("[data-cancel]").focus();
  const go = card.querySelector("[data-go]");
  go.onclick = async () => {
    go.disabled = true;
    go.textContent = "retiring…";
    try {
      const result = await H.apiPost("/v1/retire", {
        kind, target, regenerate: ticked(), reason: card.querySelector("#rt-reason").value.trim(),
      });
      close(result);
      undoToast(result.retirement, result.state === "applying"
        ? `Retiring ${result.retirement.label} — it carries on in the background.`
        : `Retired “${result.retirement.label}”.`);
    } catch (err) {
      go.disabled = false;
      go.textContent = "Retire";
      card.querySelector("#rt-reason").insertAdjacentHTML("beforebegin", `<p class="rt-err">${esc(err.message)}</p>`);
    }
  };
}

/* Undo one retirement; resolves true when it was undone. */
export async function undoRetirement(id, helpers) {
  H = helpers || H;
  await H.apiPost(`/v1/retirements/${encodeURIComponent(id)}/undo`, {});
  return true;
}

let toastTimer;
function undoToast(retirement, message) {
  let el = document.getElementById("retire-toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "retire-toast";
    el.className = "retire-toast";
    el.setAttribute("role", "status");
    document.body.appendChild(el);
  }
  el.innerHTML = `<span>${H.esc(message)}</span><button class="btn" data-undo>Undo</button>
    <a href="#/browse/retired">see retired</a>`;
  el.hidden = false;
  el.querySelector("[data-undo]").onclick = async () => {
    el.querySelector("[data-undo]").disabled = true;
    try {
      await undoRetirement(retirement.id);
      el.innerHTML = `<span>Put back — everything it touched is restored.</span>`;
      window.dispatchEvent(new CustomEvent("hl-retirement-changed"));
    } catch (err) {
      el.innerHTML = `<span>${H.esc(err.message)}</span>`;
    }
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { el.hidden = true; }, 4000);
  };
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { el.hidden = true; }, 12000);
  window.dispatchEvent(new CustomEvent("hl-retirement-changed"));
}
