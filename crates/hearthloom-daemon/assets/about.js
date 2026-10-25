// about.js — ☰ → About me: everything Hearthloom holds as true about its
// owner, where each belief came from, and the controls to correct it.
// One read (/v1/portrait), no model call. Locked, private items are
// simply absent — no count, no placeholder.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const ME = "human:dashboard";
let api, apiPost;

const KINDS = [
  { key: "identity", label: "Who you are" }, { key: "strength", label: "Strengths" },
  { key: "offering", label: "What you offer" }, { key: "seeking", label: "What you're seeking" },
  { key: "constraint", label: "Constraints" },
];
const SPHERES = ["work", "personal", "family", "health", "private"];
const day = (ts) => (ts || "").slice(0, 10);
const evidenceIds = (b) => { try { return JSON.parse(b.evidence || "[]"); } catch { return []; } };
const originChip = (o) => `<span class="statchip">${o === "declared" ? "you said" : "Hearthloom inferred"}</span>`;

const REMOVE_BTN = `<button class="btn btn-quiet about-act" data-act="remove" data-confirm="really remove?"
  title="it was never right — takes it off this page for good">remove</button>`;

function belief(b) {
  const ev = evidenceIds(b);
  return `<div class="card about-belief" data-id="${esc(b.id)}">
    <div class="about-text">${esc(b.text)}</div>
    <div class="meta about-meta">${originChip(b.origin)} <span>${esc(b.sphere)}</span>
      <span>since ${esc(day(b.created_at))}</span>
      ${ev.length ? `<details class="about-why"><summary>${ev.length} piece${ev.length === 1 ? "" : "s"} of evidence</summary>
        ${ev.slice(0, 12).map((id) => `<a href="#/episode/${esc(id)}">${esc(id)}</a>`).join(" ")}</details>` : ""}
      <span class="about-acts">
        ${b.status === "proposed" ? `<button class="btn about-act" data-act="confirm">yes, true</button>
          <button class="btn btn-quiet about-act" data-act="reject">no</button>` : ""}
        ${b.status !== "retired" ? `<button class="btn btn-quiet about-edit">correct</button>` : ""}
        ${b.status === "confirmed" ? `<button class="btn btn-quiet about-act" data-act="retire">no longer true</button>` : ""}
        ${b.sphere !== "private" ? `<button class="btn btn-quiet about-act" data-act="make_private"
          title="move it into the vault — hidden while locked, never in an agent's briefing">make private</button>` : ""}
        ${b.status !== "proposed" ? REMOVE_BTN : ""}
      </span></div>
  </div>`;
}

function paint(view, d) {
  const by = (status) => d.beliefs.filter((b) => b.status === status);
  const confirmed = by("confirmed"), proposed = by("proposed"), retired = by("retired");
  const rels = d.relationships || [];
  const relRow = (r) => `<tr><td style="text-align:left"><a href="#/entity/${encodeURIComponent(r.entity_id)}">${esc(r.name)}</a></td>
    <td style="text-align:left">${esc(r.kind)}${r.note ? ` · ${esc(r.note)}` : ""}</td>
    <td style="text-align:left">${r.origin === "declared" ? "you said" : "inferred"}</td>
    <td>${r.t_invalid ? `until ${esc(day(r.t_invalid))}` : r.t_valid ? `since ${esc(day(r.t_valid))}` : ""}</td></tr>`;
  const relTable = (list) => list.length
    ? `<table class="spend-table"><tbody>${list.map(relRow).join("")}</tbody></table>` : `<p class="meta">none yet.</p>`;
  const ended = rels.filter((r) => r.status === "ended");
  view.innerHTML = `
    <span class="eyebrow">the self-model</span>
    <h1 class="pagehead">About me</h1>
    <p class="subhead">What Hearthloom holds as true about you, where each belief came from, and the controls to set it right.
      ${d.vault_locked ? "The vault is locked, so anything private is left out." : "The vault is open: private items are included."}</p>

    <div class="section-title">you</div>
    <div class="card">${d.identities.length
      ? d.identities.map((i) => `<span class="statchip" title="${esc(i.kind)}">${esc(i.value)}</span>`).join(" ")
      : `<span class="meta">No email or handle is claimed as you yet — claim one under Relationships.</span>`}</div>

    ${proposed.length ? `<div class="section-title">waiting for your answer <span class="statchip">${proposed.length}</span></div>
      ${proposed.map(belief).join("")}` : ""}

    <div class="section-title">what Hearthloom believes</div>
    ${KINDS.map((k) => {
      const list = confirmed.filter((b) => b.kind === k.key);
      return list.length ? `<h3 class="about-kind">${k.label}</h3>${list.map(belief).join("")}` : "";
    }).join("") || `<p class="meta">Nothing confirmed yet. Add something below, or answer what's waiting above.</p>`}

    <div class="card about-add">
      <b>Tell it something yourself</b>
      <div class="searchbar" style="flex-wrap:wrap;margin-top:var(--s2)">
        <select id="about-kind">${KINDS.map((k) => `<option value="${k.key}">${k.label}</option>`).join("")}</select>
        <select id="about-sphere">${SPHERES.map((s) => `<option>${s}</option>`).join("")}</select>
        <input id="about-new" placeholder="one true sentence about you" style="flex:1;min-width:220px">
        <button class="btn" id="about-add">add</button>
      </div><p class="meta" id="about-note" hidden></p>
    </div>

    <div class="section-title">people and roles</div>
    ${relTable(rels.filter((r) => r.status === "confirmed"))}
    ${rels.some((r) => r.status !== "confirmed" && r.status !== "ended") ? `<h3 class="about-kind">Guesses, not yet confirmed</h3>
      ${relTable(rels.filter((r) => r.status !== "confirmed" && r.status !== "ended"))}
      <p class="meta">Confirm or dismiss these under <a href="#/relationships">Relationships</a>.</p>` : ""}

    <div class="section-title">what you're pursuing</div>
    ${d.goals.length ? `<table class="spend-table"><tbody>${d.goals.map((g) => `<tr>
      <td style="text-align:left"><b>${esc(g.name)}</b></td><td style="text-align:left">${esc(g.description || g.query || "")}</td>
      <td style="text-align:left">${esc(g.sphere || "")}</td><td>${esc(g.status || "")}</td></tr>`).join("")}</tbody></table>`
      : `<p class="meta">No goals yet — add one on the <a href="#/desk">Desk</a>.</p>`}

    ${retired.length || ended.length ? `<div class="section-title">used to be true</div>
      ${retired.map((b) => `<div class="card about-belief about-past" data-id="${esc(b.id)}"><div class="about-text">${esc(b.text)}</div>
        <div class="meta about-meta">${originChip(b.origin)} <span>${esc(day(b.created_at))} → ${esc(day(b.updated_at))}</span>
          <span class="about-acts">${REMOVE_BTN}</span></div></div>`).join("")}
      ${ended.length ? relTable(ended) : ""}` : ""}

    <div class="section-title">what an agent sees</div>
    <p class="meta">This is the briefing every agent session loads, word for word. Private material is never in it,
      whether or not the vault is open.</p>
    <details><summary class="meta">show the briefing (${d.brain_md.split("\n").length} lines)</summary>
      <pre class="trace-result" style="white-space:pre-wrap">${esc(d.brain_md) || "(not generated yet)"}</pre></details>`;

  const reload = () => mount(view, { api, apiPost });
  const resolve = async (id, action, text) => {
    await apiPost(`/v1/selfmodel/${id}/resolve`, { actor: ME, action, text });
    reload();
  };
  view.querySelectorAll(".about-act").forEach((b) => {
    b.onclick = () => {
      // A destructive act asks once more, in place: the first click
      // re-labels the button, the second one acts.
      if (b.dataset.confirm && !b.dataset.armed) {
        b.dataset.armed = "1"; b.textContent = b.dataset.confirm;
        return;
      }
      resolve(b.closest(".about-belief").dataset.id, b.dataset.act).catch((e) => { b.textContent = `✗ ${e.message}`; });
    };
  });
  // "correct": your wording replaces it. A proposed belief is confirmed as
  // edited; a confirmed one is retired and re-declared, so the old wording
  // stays on the record under "used to be true".
  view.querySelectorAll(".about-edit").forEach((b) => {
    b.onclick = () => {
      const card = b.closest(".about-belief");
      const item = d.beliefs.find((x) => x.id === card.dataset.id);
      const textEl = card.querySelector(".about-text");
      textEl.innerHTML = `<div class="searchbar"><input class="about-input" value="${esc(item.text)}" style="flex:1">
        <button class="btn about-save">save</button><button class="btn btn-quiet about-cancel">cancel</button></div>`;
      const input = textEl.querySelector("input"); input.focus();
      textEl.querySelector(".about-cancel").onclick = reload;
      textEl.querySelector(".about-save").onclick = async () => {
        const text = input.value.trim();
        if (!text || text === item.text) return reload();
        try {
          if (item.status === "proposed") {
            await apiPost(`/v1/selfmodel/${item.id}/resolve`, { actor: ME, action: "confirm", text });
          } else {
            await apiPost("/v1/selfmodel", { actor: ME, sphere: item.sphere, kind: item.kind, text });
            await apiPost(`/v1/selfmodel/${item.id}/resolve`, { actor: ME, action: "retire" });
          }
          reload();
        } catch (e) { input.value = `✗ ${e.message}`; }
      };
    };
  });
  view.querySelector("#about-add").onclick = async () => {
    const text = view.querySelector("#about-new").value.trim();
    const note = view.querySelector("#about-note");
    if (!text) return;
    try {
      await apiPost("/v1/selfmodel", { actor: ME, kind: view.querySelector("#about-kind").value,
        sphere: view.querySelector("#about-sphere").value, text });
      reload();
    } catch (e) { note.hidden = false; note.textContent = `✗ ${e.message}`; }
  };
}

export async function mount(view, helpers) {
  api = helpers.api; apiPost = helpers.apiPost;
  if (!view.querySelector(".about-belief, .about-add")) {
    view.innerHTML = `<div class="loading"><span class="spinner"></span> gathering what it knows…</div>`;
  }
  let d;
  try { d = await api("/v1/portrait"); }
  catch (e) { view.innerHTML = `<div class="empty">couldn't read the self-model: ${esc(e.message)}</div>`; return; }
  paint(view, d);
}
