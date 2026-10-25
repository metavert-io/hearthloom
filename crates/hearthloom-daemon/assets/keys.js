// keys.js — ☰ → API Keys. Two kinds of key:
//  · ordinary — coarse scopes, blanket reach (trusted integrations);
//  · SCOPED — per-class grants on a default-deny route table, optional
//    expiry and rate caps, never reads while the vault is open (v52).
// Plus the ask-then-approve queue (an agent requests grants; you decide)
// and, per scoped key, what it wrote — with Undo.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const ME = "human:dashboard";
let api, apiPost;

const CLASSES = [
  { key: "tasks", label: "Tasks", levels: ["none", "read", "write"] },
  { key: "notes", label: "Notes", levels: ["none", "read", "write"] },
  { key: "memory", label: "Memory", levels: ["none", "read", "write"],
    hint: "read = search, recall, traverse · write = propose only (you review it)" },
];
const LIFETIMES = [
  { secs: 3600, label: "1 hour" }, { secs: 86400, label: "1 day" },
  { secs: 30 * 86400, label: "30 days" }, { secs: 90 * 86400, label: "90 days" },
  { secs: 0, label: "never expires" },
];

const AREAS = ["work", "personal", "family", "health"];

function describeGrants(g) {
  const parts = CLASSES.filter((c) => g[c.key] && g[c.key] !== "none")
    .map((c) => `${c.label.toLowerCase()}: ${g[c.key]}`);
  if (g.ask) parts.push("ask");
  if (g.capture) parts.push("capture (write-only)");
  if (g.own_device) parts.push("your own device: writes as you");
  if (g.writes_per_hour) parts.push(`≤${g.writes_per_hour} writes/hour`);
  if (g.asks_per_day) parts.push(`≤${g.asks_per_day} questions/day`);
  if (g.areas) parts.push(`${g.areas.join(" + ")} only`);
  return parts.join(" · ") || "nothing";
}

function expiryText(k) {
  if (k.revoked_at) return `revoked ${k.revoked_at.slice(0, 10)}`;
  if (!k.expires_at) return "no expiry";
  const ms = new Date(k.expires_at) - Date.now();
  if (ms <= 0) return `expired ${k.expires_at.slice(0, 10)}`;
  const h = ms / 3600000;
  return `expires in ${h < 1 ? `${Math.max(1, Math.round(h * 60))} min` : h < 48 ? `${Math.round(h)} h` : `${Math.round(h / 24)} days`}`;
}

function keyCard(k) {
  const dead = k.revoked_at || (k.expires_at && new Date(k.expires_at) <= Date.now());
  return `
    <div class="card key-card${dead ? " key-dead" : ""}" data-key="${esc(k.id)}">
      <div class="task-row" style="align-items:baseline;gap:var(--s3);flex-wrap:wrap">
        <b>${esc(k.name)}</b>
        <span class="statchip">${k.grants ? "scoped" : "ordinary"}</span>
        <span class="meta" style="flex:1">${k.grants ? esc(describeGrants(k.grants)) : esc((k.scopes || []).join(", "))}</span>
        <span class="meta">${esc(expiryText(k))}</span>
        ${k.grants ? `<button class="btn btn-quiet key-activity">activity</button>` : ""}
        ${k.revoked_at ? "" : `<button class="btn btn-quiet key-revoke">revoke</button>`}
      </div>
      <div class="meta">created ${esc(k.created_at.slice(0, 10))} · ${k.last_used_at
        ? `last used ${esc(k.last_used_at.slice(0, 16).replace("T", " "))}` : "never used"}</div>
      <div class="key-activity-host"></div>
    </div>`;
}

async function paintActivity(card) {
  const host = card.querySelector(".key-activity-host");
  if (host.dataset.open) { host.innerHTML = ""; delete host.dataset.open; return; }
  host.dataset.open = "1";
  host.innerHTML = `<p class="meta"><span class="spinner"></span> reading what it wrote…</p>`;
  const id = card.dataset.key;
  let res;
  try { res = await api(`/v1/keys/${id}/activity`); } catch (e) { host.innerHTML = `<p class="meta">✗ ${esc(e.message)}</p>`; return; }
  const items = res.items || [];
  host.innerHTML = items.length ? `<table class="spend-table" style="margin-top:var(--s2)"><tbody>${items.map((i) => `
      <tr><td style="text-align:left">${esc(i.ts.slice(0, 16).replace("T", " "))}</td>
        <td style="text-align:left">${esc(i.op)} ${esc(i.kind.replace(/s$/, ""))}</td>
        <td style="text-align:left">${esc(i.label)}</td>
        <td>${i.op !== "create" ? "" : i.live
          ? `<button class="btn btn-quiet key-undo" data-kind="${esc(i.kind)}" data-id="${esc(i.entity_id)}">undo</button>`
          : `<span class="meta">undone</span>`}</td>
      </tr>`).join("")}</tbody></table>`
    : `<p class="meta">This key hasn't written anything.</p>`;
  host.querySelectorAll(".key-undo").forEach((b) => {
    b.onclick = async () => {
      b.disabled = true;
      try {
        await apiPost(`/v1/keys/${id}/undo`, { actor: ME, kind: b.dataset.kind, entity_id: b.dataset.id });
        delete host.dataset.open;
        paintActivity(card);
      } catch (e) { b.disabled = false; b.textContent = `✗ ${e.message}`; }
    };
  });
}

const isDead = (k) => !!k.revoked_at || (k.expires_at && new Date(k.expires_at) <= Date.now());
// Live keys first; expired and revoked ones (short-lived plugin-page
// tokens pile up by the dozen) fold away instead of burying them.
function keyListHtml(keys) {
  const live = keys.filter((k) => !isDead(k)), dead = keys.filter(isDead);
  return (live.map(keyCard).join("") || `<p class="meta">no active keys.</p>`)
    + (dead.length ? `<details style="margin-top:var(--s3)"><summary class="meta">${dead.length} expired or revoked</summary>
        ${dead.slice(0, 200).map(keyCard).join("")}</details>` : "");
}

export async function mount(view, helpers) {
  api = helpers.api; apiPost = helpers.apiPost;
  const paint = async () => {
    const [{ keys, scopes }, pending] = await Promise.all([
      api("/v1/keys"), api("/v1/keys/requests").catch(() => ({ requests: [] })),
    ]);
    const requests = pending.requests || [];
    view.innerHTML = `
      <span class="eyebrow">api keys</span>
      <h1 class="pagehead">Keys to the record</h1>
      <p class="subhead">Tokens for integrations and agents. A secret is shown <b>once</b>, at creation —
        the record keeps only its fingerprint.</p>

      ${requests.length ? `<div class="section-title">waiting for your answer</div>
        ${requests.map((r) => `<div class="card" style="border-color:var(--accent)" data-req="${esc(r.id)}">
          <div class="task-row" style="align-items:baseline;gap:var(--s3);flex-wrap:wrap">
            <b>${esc(r.name)}</b> <span class="meta" style="flex:1">is asking for: ${esc(describeGrants(r.grants))}${
              r.ttl_secs ? ` · for ${Math.round(r.ttl_secs / 3600) || 1} h` : " · with no expiry"}</span>
            <button class="btn req-yes">approve</button>
            <button class="btn btn-quiet req-no">deny</button>
          </div>
          <div class="meta">asked ${esc(r.created_at.slice(11, 16))} UTC — lapses 10 minutes after asking</div>
        </div>`).join("")}` : ""}

      <div class="section-title">mint a scoped key</div>
      <div class="card">
        <p class="meta">Reaches only what you grant, and nothing else. It never reads while the vault is open.</p>
        <div class="searchbar" style="flex-wrap:wrap;gap:var(--s3)">
          <input id="sk-name" placeholder="who will hold it? — e.g. claude-code" style="flex:1.5;min-width:220px">
          <select id="sk-life">${LIFETIMES.map((l) => `<option value="${l.secs}" ${l.secs === 30 * 86400 ? "selected" : ""}>${l.label}</option>`).join("")}</select>
        </div>
        <div style="display:flex;flex-wrap:wrap;gap:var(--s4);margin:var(--s3) 0">
          ${CLASSES.map((c) => `<label class="meta" title="${esc(c.hint || "")}">${c.label}
            <select class="sk-class" data-class="${c.key}">${c.levels.map((l) => `<option>${l}</option>`).join("")}</select></label>`).join("")}
          <label class="meta" title="may run Ask — each question spends model tokens"><input type="checkbox" id="sk-ask"> Ask</label>
          <label class="meta" title="write-only: may add a captured thought, can read nothing"><input type="checkbox" id="sk-capture"> Capture</label>
          <label class="meta" title="only for a key that stays on one of YOUR devices (your phone's capture, a Shortcut you run): what it writes counts as your own words, fully trusted. Never for a key an app or service holds."><input type="checkbox" id="sk-own"> My own device</label>
          <label class="meta">writes/hour <input id="sk-wph" type="number" min="1" placeholder="no cap" style="width:80px"></label>
          <label class="meta">questions/day <input id="sk-apd" type="number" min="1" placeholder="no cap" style="width:80px"></label>
        </div>
        <div style="display:flex;flex-wrap:wrap;gap:var(--s4);margin:var(--s3) 0" title="what it may read; private is never included">
          <span class="meta">reads from</span>
          ${AREAS.map((a) => `<label class="meta"><input type="checkbox" class="sk-area" value="${a}" checked> ${a}</label>`).join("")}
          <span class="meta" id="sk-area-note" hidden>Ask isn't available to an area-limited key</span>
        </div>
        <button class="btn" id="sk-create">mint scoped key</button>
      </div>

      <details style="margin:var(--s3) 0"><summary class="meta">Mint an ordinary key (blanket access by scope)</summary>
        <div class="searchbar" style="flex-wrap:wrap;margin-top:var(--s2)">
          <input id="key-name" placeholder="what will hold this key? — e.g. hearthfs" style="flex:1.5">
          ${scopes.map((s) => `<label class="meta" style="white-space:nowrap">
            <input type="checkbox" class="key-scope" value="${esc(s)}" ${s === "read" ? "checked" : ""}> ${esc(s)}</label>`).join("")}
          <button class="btn" id="key-create">mint key</button>
        </div></details>

      <div id="key-fresh" hidden></div>
      <div class="section-title">keys</div>
      <div id="key-list">${keyListHtml(keys)}</div>`;

    const showSecret = (res) => {
      const fresh = view.querySelector("#key-fresh");
      fresh.hidden = false;
      if (res.error) { fresh.innerHTML = `<p class="meta">✗ ${esc(res.error)}</p>`; return false; }
      fresh.innerHTML = `
        <div class="card" style="border-color:var(--accent)">
          <b>copy it now — it will not be shown again</b>
          <div class="searchbar" style="margin-top:var(--s2)">
            <input id="key-secret" value="${esc(res.secret)}" readonly>
            <button class="btn" id="key-copy">copy</button>
            ${res.key?.grants ? "" : `<button class="btn btn-quiet" id="key-use-here"
              title="store in this browser and send it with every dashboard request">use in this browser</button>`}
          </div>
        </div>`;
      // click selects the whole key, ready to copy
      view.querySelector("#key-secret").addEventListener("click", (e) => e.currentTarget.select());
      view.querySelector("#key-copy").onclick = () => navigator.clipboard?.writeText(res.secret);
      const here = view.querySelector("#key-use-here");
      if (here) here.onclick = () => { localStorage.setItem("hl-api-key", res.secret); paint(); };
      return true;
    };
    const refreshList = async () => {
      const { keys: fresh } = await api("/v1/keys");
      view.querySelector("#key-list").innerHTML = keyListHtml(fresh);
      wireCards();
    };
    const pickedAreas = () => [...view.querySelectorAll(".sk-area:checked")].map((c) => c.value);
    const syncAreas = () => {
      const limited = pickedAreas().length < AREAS.length;
      const ask = view.querySelector("#sk-ask");
      if (limited) ask.checked = false;
      ask.disabled = limited;
      view.querySelector("#sk-area-note").hidden = !limited;
    };
    view.querySelectorAll(".sk-area").forEach((c) => { c.onchange = syncAreas; });
    view.querySelector("#sk-create").onclick = async () => {
      const name = view.querySelector("#sk-name").value.trim();
      if (!name) { view.querySelector("#sk-name").focus(); return; }
      const grants = {
        ask: view.querySelector("#sk-ask").checked,
        capture: view.querySelector("#sk-capture").checked,
        own_device: view.querySelector("#sk-own").checked,
      };
      view.querySelectorAll(".sk-class").forEach((sel) => { grants[sel.dataset.class] = sel.value; });
      const wph = Number(view.querySelector("#sk-wph").value), apd = Number(view.querySelector("#sk-apd").value);
      if (wph > 0) grants.writes_per_hour = wph;
      if (apd > 0) grants.asks_per_day = apd;
      const areas = pickedAreas();
      if (areas.length < AREAS.length) grants.areas = areas;
      const secs = Number(view.querySelector("#sk-life").value);
      const res = await apiPost("/v1/keys", { actor: ME, name, grants, expires_in_secs: secs || undefined })
        .catch((e) => ({ error: e.message }));
      if (showSecret(res)) refreshList();
    };
    view.querySelector("#key-create").onclick = async () => {
      const name = view.querySelector("#key-name").value.trim();
      const picked = [...view.querySelectorAll(".key-scope:checked")].map((c) => c.value);
      if (!name) return;
      const res = await apiPost("/v1/keys", { actor: ME, name, scopes: picked }).catch((e) => ({ error: e.message }));
      if (showSecret(res)) refreshList();
    };
    view.querySelectorAll("[data-req]").forEach((card) => {
      const decide = (approve) => async () => {
        await apiPost(`/v1/keys/requests/${card.dataset.req}`, { actor: ME, approve }).catch(() => {});
        paint();
      };
      card.querySelector(".req-yes").onclick = decide(true);
      card.querySelector(".req-no").onclick = decide(false);
    });
    const wireCards = () => {
      view.querySelectorAll(".key-revoke").forEach((btn) => {
        btn.onclick = async () => {
          await apiPost(`/v1/keys/${btn.closest(".card").dataset.key}/revoke`, { actor: ME });
          refreshList();
        };
      });
      view.querySelectorAll(".key-activity").forEach((btn) => {
        btn.onclick = () => paintActivity(btn.closest(".card"));
      });
    };
    wireCards();
  };
  await paint();
}
