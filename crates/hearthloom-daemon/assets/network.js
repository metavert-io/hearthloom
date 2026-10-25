// network.js — the Network page (☰ → Network): connections to other
// brains and what this record shares with each. Two directions on one
// page: brains I can ask (outbound, their grant — with an abbreviated
// "they share: …" line per connection) and what I share (v46: a
// DEFAULT template every connection inherits + per-friend tri-state
// overrides + explicit item shares; v54: GROUPS — named sets of
// connections with their own additive sharing). Plus my calling card
// and the transparency ledger.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&#39;", "'": "&#39;" }[c]));

let api, apiPost, confirmDanger;
const ACTOR = "human:dashboard";

const CLASSES = ["episodes", "tasks", "notes", "relationships", "profile"];
const CLASS_LABEL = {
  episodes: "memories", tasks: "tasks", notes: "notes",
  relationships: "relationships", profile: "profile card",
};
const SPHERES = ["work", "personal", "family", "health"];
const BUILTIN = { classes: { episodes: "on", profile: "on" }, spheres: [], detail: "excerpts" };

// Mirror of the server resolver — for the "effectively:" preview lines.
function resolveEff(tpl, over) {
  const pick = (key, fallback) => {
    for (const src of [over || {}, tpl || {}, BUILTIN]) {
      if (src[key] !== undefined && src[key] !== null) return src[key];
    }
    return fallback;
  };
  const classes = CLASSES.filter((c) => {
    for (const src of [over || {}, tpl || {}, BUILTIN]) {
      const v = src.classes?.[c];
      if (v === "on") return true;
      if (v === "off") return false;
    }
    return false;
  });
  return {
    classes,
    spheres: pick("spheres", []),
    detail: pick("detail", "excerpts"),
    completed_tasks: pick("completed_tasks", false),
    evidence_relationships: pick("evidence_relationships", false),
  };
}

function effSummary(eff) {
  if (!eff.classes.length) return "nothing";
  const bits = eff.classes.map((c) => CLASS_LABEL[c]);
  const scope = eff.spheres.length ? eff.spheres.join(", ") : "no spheres yet";
  return `${bits.join(" · ")} — ${scope} · ${eff.detail}` +
    (eff.completed_tasks ? " · incl. completed tasks" : "") +
    (eff.evidence_relationships ? " · incl. evidence-tier people" : "");
}

/* ————— brains I can ask (outbound) ————— */

async function paintFriends(host) {
  const data = await api("/v1/friends").catch(() => ({ friends: [] }));
  const rows = (data.friends || []).map((f) => `
    <div class="card" data-friend="${esc(f.name)}">
      <div class="task-row" style="align-items:baseline">
        ${window.friendAvatar ? window.friendAvatar(f.name, { entity: f.entity }) : "🧠"}
        <b>${esc(f.name)}</b>
        <span class="meta">${esc(f.url)}</span>
        ${f.entity ? `<a class="task-tag plain-tag" href="#/entity/${encodeURIComponent(f.entity)}">their person card</a>` : ""}
        ${!f.key_present ? `<span class="statchip">⚠ key missing — reconnect</span>` : ""}
        <button class="btn btn-quiet fr-refresh" title="re-fetch their profile card">↻ card</button>
        <button class="btn btn-quiet fr-remove">disconnect…</button>
      </div>
      <div class="meta fr-offer" data-friend="${esc(f.name)}">checking what they share…</div>
    </div>`).join("");
  host.innerHTML = `
    ${rows || `<p class="meta">no connected brains yet.</p>`}
    <div class="card">
      <div class="meta" style="margin-bottom:var(--s2)">Connect a friend's brain — they send you
        a key from THEIR Network page ("share with a friend" below, on their side):</div>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="fr-name" placeholder="their name — e.g. ada" style="flex:0.5">
        <input id="fr-url" placeholder="https://their-brain…" style="flex:1">
        <input id="fr-key" placeholder="the key they sent (hl_…)" style="flex:1">
        <button class="btn" id="fr-connect">connect</button>
      </div>
      <p class="meta">connecting seeds a Person + relationship for them automatically,
        built from their profile card.</p>
      <p class="meta" id="fr-note" hidden></p>
    </div>`;
  // Abbreviated per-connection "they share:" line — best-effort, one
  // fetch per friend, quietly absent when their node is old or offline.
  host.querySelectorAll(".fr-offer").forEach(async (el) => {
    const name = el.dataset.friend;
    try {
      const offer = await api(`/v1/friends/${encodeURIComponent(name)}/offer`);
      const eff = {
        classes: (offer.classes || []).filter((c) => CLASSES.includes(c)),
        spheres: offer.spheres || [], detail: offer.detail || "excerpts",
        completed_tasks: !!offer.completed_tasks,
        evidence_relationships: !!offer.evidence_relationships,
      };
      el.textContent = `they share: ${effSummary(eff)}` +
        (offer.shared_cursor ? " · + items pinned to you" : "");
    } catch {
      el.textContent = "they share: (their node doesn't say — older version or offline)";
    }
  });
  const note = host.querySelector("#fr-note");
  host.querySelector("#fr-connect").onclick = async () => {
    note.hidden = false; note.textContent = "reaching their brain…";
    const res = await apiPost("/v1/friends", {
      actor: ACTOR,
      name: host.querySelector("#fr-name").value.trim(),
      url: host.querySelector("#fr-url").value.trim(),
      key: host.querySelector("#fr-key").value.trim(),
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    paintFriends(host);
  };
  host.querySelectorAll(".fr-refresh").forEach((btn) => {
    btn.onclick = async () => {
      const name = btn.closest(".card").dataset.friend;
      btn.textContent = "↻…";
      await apiPost(`/v1/friends/${encodeURIComponent(name)}/refresh-card`, {})
        .catch(() => {});
      paintFriends(host);
    };
  });
  host.querySelectorAll(".fr-remove").forEach((btn) => {
    const name = btn.closest(".card").dataset.friend;
    btn.onclick = async () => {
      // `action` is the button's label; the dialog resolves true on confirm
      const confirmed = await confirmDanger({
        title: `Disconnect from ${name}'s brain?`,
        body: "Your key to their brain is retired (best-effort) and Ask stops reaching " +
          "them. Anything they shared stays theirs — nothing was ever copied without asking.",
        action: "disconnect",
      });
      if (!confirmed) return;
      await fetch(`/v1/friends/${encodeURIComponent(name)}`, {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ actor: ACTOR }),
      });
      paintFriends(host);
    };
  });
}

/* ————— my calling card ————— */

async function paintMyCard(host) {
  // one shared editor (app.js) — the same card lives on Settings
  host.innerHTML = `<div class="card"><div id="net-mycard-inner"></div></div>`;
  window.mountProfileCard?.(host.querySelector("#net-mycard-inner"));
}

/* ————— the default sharing template ————— */

async function paintDefault(host, onChange) {
  const tpl = await api("/v1/grants/default").catch(() => ({}));
  const eff = resolveEff(tpl, {});
  host.innerHTML = `
    <div class="card" style="border-color:var(--line)">
      <div class="meta" style="margin-bottom:var(--s2)"><b>Default sharing</b> — every connection
        inherits this unless overridden below. Change it here and it changes for everyone
        who inherits. <b>Private is never shareable</b>, full stop.</div>
      <div class="searchbar" style="flex-wrap:wrap; align-items:center">
        ${CLASSES.map((c) => `<label class="meta" style="white-space:nowrap">
          <input type="checkbox" class="df-class" value="${c}"
            ${eff.classes.includes(c) ? "checked" : ""}> ${CLASS_LABEL[c]}</label>`).join("")}
      </div>
      <div class="searchbar" style="flex-wrap:wrap; align-items:center; margin-top:var(--s2)">
        <span class="meta">spheres:</span>
        ${SPHERES.map((s) => `<label class="meta" style="white-space:nowrap">
          <input type="checkbox" class="df-sphere" value="${s}"
            ${eff.spheres.includes(s) ? "checked" : ""}> ${s}</label>`).join("")}
        <select id="df-detail">
          <option value="excerpts" ${eff.detail === "excerpts" ? "selected" : ""}>excerpts only</option>
          <option value="full" ${eff.detail === "full" ? "selected" : ""}>full detail</option>
        </select>
        <label class="meta"><input type="checkbox" id="df-done" ${eff.completed_tasks ? "checked" : ""}>
          completed tasks too</label>
        <label class="meta"><input type="checkbox" id="df-ev" ${eff.evidence_relationships ? "checked" : ""}>
          evidence-tier people too</label>
        <button class="btn" id="df-save">save default</button>
      </div>
      <p class="meta" id="df-note" hidden></p>
    </div>`;
  host.querySelector("#df-save").onclick = async () => {
    const note = host.querySelector("#df-note");
    note.hidden = false; note.textContent = "saving…";
    const classes = {};
    host.querySelectorAll(".df-class").forEach((c) => { classes[c.value] = c.checked ? "on" : "off"; });
    const res = await apiPost("/v1/grants/default", {
      actor: ACTOR,
      sharing: {
        classes,
        spheres: [...host.querySelectorAll(".df-sphere:checked")].map((c) => c.value),
        detail: host.querySelector("#df-detail").value,
        completed_tasks: host.querySelector("#df-done").checked,
        evidence_relationships: host.querySelector("#df-ev").checked,
      },
    }).catch((e) => ({ error: e.message }));
    note.textContent = res?.error ? `✗ ${res.error}` : "✓ saved — inheriting connections updated instantly";
    onChange?.();
  };
}

/* ————— per-friend grants: matrix + explicit shares ————— */

const KIND_ICON = { task: "☑", note: "✎", relationship: "◉" };

function triLabel(v, inherited) {
  if (v === "on") return "on";
  if (v === "off") return "off";
  return `inherit (${inherited ? "on" : "off"})`;
}

async function paintGrants(host) {
  const [data, tpl, allShares] = await Promise.all([
    api("/v1/grants").catch(() => ({ grants: [] })),
    api("/v1/grants/default").catch(() => ({})),
    api("/v1/sharing").catch(() => ({ shares: [] })),
  ]);
  const grants = (data.grants || []).filter((g) => g.friend !== "__default__");
  const rows = grants.map((g) => {
    const over = g.sharing || {};
    const eff = resolveEff(tpl, over);
    const shares = (allShares.shares || []).filter((s) => s.friend === g.friend);
    return `
    <div class="card${g.revoked_at ? " pm-task-done" : ""}" data-grant="${esc(g.friend)}">
      <div class="task-row" style="align-items:baseline">
        ${window.friendAvatar ? window.friendAvatar(g.friend, {}) : ""}
        <b>${esc(g.friend)}</b>
        <span class="meta">effectively: ${esc(effSummary(eff))}</span>
        <span class="meta">since ${esc(g.created_at.slice(0, 10))}</span>
        ${g.revoked_at
          ? `<span class="kind">revoked ${esc(g.revoked_at.slice(0, 10))}</span>
             <button class="btn btn-quiet gr-delete" title="remove this revoked grant for good">remove</button>`
          : `<button class="btn btn-quiet gr-edit">sharing…</button>
             <button class="btn btn-quiet gr-revoke">revoke…</button>`}
      </div>
      ${shares.length ? `<div class="meta" style="margin-top:var(--s1)">pinned to them:
        ${shares.map((s) => `<span class="task-tag plain-tag" title="shared ${esc(s.shared_at.slice(0, 10))}${s.collaborate ? " · collaborative" : ""}">
          ${KIND_ICON[s.kind] || ""} ${esc(s.kind)} ${esc(s.item_id.slice(0, 12))}…
          <button class="linkish gr-unshare" data-kind="${esc(s.kind)}" data-item="${esc(s.item_id)}" title="un-share">×</button></span>`).join(" ")}</div>` : ""}
      <div class="gr-matrix" hidden></div>
    </div>`;
  }).join("");
  host.innerHTML = `
    ${rows || `<p class="meta">you're not sharing with anyone yet.</p>`}
    <div class="card">
      <div class="meta" style="margin-bottom:var(--s2)">Share with a NEW friend — mints their key.
        They start on the default above; refine with "sharing…" after.</div>
      <div class="searchbar" style="flex-wrap:wrap; align-items:center">
        <input id="gr-friend" placeholder="their name — e.g. ada" style="flex:0.6">
        ${SPHERES.map((s) => `<label class="meta" style="white-space:nowrap">
          <input type="checkbox" class="gr-sphere" value="${esc(s)}"> ${esc(s)}</label>`).join("")}
        <select id="gr-detail"><option value="excerpts">excerpts only</option>
          <option value="full">full memories</option></select>
        <button class="btn" id="gr-invite">share &amp; mint their key</button>
      </div>
      <div id="gr-fresh" hidden></div>
      <p class="meta" id="gr-note" hidden></p>
    </div>`;
  const note = host.querySelector("#gr-note");
  host.querySelector("#gr-invite").onclick = async () => {
    const friend = host.querySelector("#gr-friend").value.trim();
    const picked = [...host.querySelectorAll(".gr-sphere:checked")].map((c) => c.value);
    if (!friend) return;
    const res = await apiPost("/v1/grants", {
      actor: ACTOR, friend, spheres: picked,
      detail: host.querySelector("#gr-detail").value, mint_key: true,
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { note.hidden = false; note.textContent = `✗ ${res.error}`; return; }
    const fresh = host.querySelector("#gr-fresh");
    fresh.hidden = false;
    fresh.innerHTML = `
      <div class="card" style="border-color:var(--accent)">
        <b>send ${esc(friend)} this key + your address — shown once</b>
        <div class="searchbar" style="margin-top:var(--s2)">
          <input id="gr-secret" value="${esc(res.secret || "")}" readonly>
          <button class="btn" id="gr-copy">copy</button>
        </div>
        <p class="meta">they paste it on THEIR Network page, under "connect a friend's brain".</p>
      </div>`;
    // click selects the whole key, ready to copy
    fresh.querySelector("#gr-secret").addEventListener("click", (e) => e.currentTarget.select());
    fresh.querySelector("#gr-copy").onclick = () => navigator.clipboard?.writeText(res.secret);
  };
  host.querySelectorAll(".gr-unshare").forEach((btn) => {
    btn.onclick = async () => {
      const friend = btn.closest(".card").dataset.grant;
      await apiPost("/v1/sharing", {
        actor: ACTOR, action: "unshare",
        kind: btn.dataset.kind, item_id: btn.dataset.item, friend,
      }).catch(() => {});
      paintGrants(host);
    };
  });
  host.querySelectorAll(".gr-edit").forEach((btn) => {
    btn.onclick = () => {
      const card = btn.closest(".card");
      const friend = card.dataset.grant;
      const g = grants.find((x) => x.friend === friend);
      const panel = card.querySelector(".gr-matrix");
      if (!panel.hidden) { panel.hidden = true; return; }
      panel.hidden = false;
      paintMatrix(panel, friend, g.sharing || {}, tpl, () => paintGrants(host));
    };
  });
  host.querySelectorAll(".gr-delete").forEach((btn) => {
    const friend = btn.closest(".card").dataset.grant;
    btn.onclick = async () => {
      await fetch(`/v1/grants/${encodeURIComponent(friend)}`, {
        method: "DELETE",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ actor: ACTOR }),
      }).catch(() => {});
      paintGrants(host);
    };
  });
  host.querySelectorAll(".gr-revoke").forEach((btn) => {
    const friend = btn.closest(".card").dataset.grant;
    btn.onclick = async () => {
      const confirmed = await confirmDanger({
        title: `Stop sharing with ${friend}?`,
        body: "Their brain's key is revoked and every request they make is refused from " +
          "this moment. Anything their brain already read stays read — revoking is not recall.",
        action: "stop sharing",
      });
      if (!confirmed) return;
      await apiPost(`/v1/grants/${encodeURIComponent(friend)}/revoke`, { actor: ACTOR });
      paintGrants(host);
    };
  });
}

/// The tri-state override editor: every control shows what it inherits;
/// touched controls become explicit; "reset" returns a control to
/// inherit. What you see is what she gets — the effective line updates
/// live.
function paintMatrix(panel, friend, over, tpl, onSaved) {
  const state = JSON.parse(JSON.stringify(over));
  const tplEff = resolveEff(tpl, {});
  const render = () => {
    const eff = resolveEff(tpl, state);
    panel.innerHTML = `
      <div style="margin-top:var(--s2); padding-top:var(--s2); border-top:1px solid var(--line)">
        <div class="meta" style="margin-bottom:var(--s1)">Overrides for <b>${esc(friend)}</b> —
          muted = inherited from the default; solid = set here for them alone.</div>
        <div class="searchbar" style="flex-wrap:wrap">
          ${CLASSES.map((c) => {
            const v = state.classes?.[c];
            const inherited = tplEff.classes.includes(c);
            return `<button class="btn btn-quiet mx-class ${v ? "mx-set" : "mx-inherit"}${(v === "on" || (!v && inherited)) ? " mx-on" : ""}"
              data-class="${c}" title="click to cycle: inherit → on → off">
              ${CLASS_LABEL[c]}: ${triLabel(v, inherited)}</button>`;
          }).join("")}
        </div>
        <div class="searchbar" style="flex-wrap:wrap; margin-top:var(--s2); align-items:center">
          <span class="meta">spheres:</span>
          ${state.spheres
            ? SPHERES.map((s) => `<label class="meta"><input type="checkbox" class="mx-sphere"
                value="${s}" ${state.spheres.includes(s) ? "checked" : ""}> ${s}</label>`).join("") +
              `<button class="btn btn-quiet mx-spheres-reset">reset to inherit</button>`
            : `<span class="meta mx-inherit-note">inherit (${tplEff.spheres.join(", ") || "none"})</span>
               <button class="btn btn-quiet mx-spheres-custom">customize</button>`}
          <select class="mx-detail">
            <option value="" ${!state.detail ? "selected" : ""}>detail: inherit (${tplEff.detail})</option>
            <option value="excerpts" ${state.detail === "excerpts" ? "selected" : ""}>excerpts only</option>
            <option value="full" ${state.detail === "full" ? "selected" : ""}>full detail</option>
          </select>
          <select class="mx-done">
            <option value="" ${state.completed_tasks == null ? "selected" : ""}>completed tasks: inherit (${tplEff.completed_tasks ? "yes" : "no"})</option>
            <option value="true" ${state.completed_tasks === true ? "selected" : ""}>yes</option>
            <option value="false" ${state.completed_tasks === false ? "selected" : ""}>no</option>
          </select>
          <select class="mx-ev">
            <option value="" ${state.evidence_relationships == null ? "selected" : ""}>evidence people: inherit (${tplEff.evidence_relationships ? "yes" : "no"})</option>
            <option value="true" ${state.evidence_relationships === true ? "selected" : ""}>yes</option>
            <option value="false" ${state.evidence_relationships === false ? "selected" : ""}>no</option>
          </select>
        </div>
        <div class="meta" style="margin-top:var(--s1)">→ effectively: <b>${esc(effSummary(eff))}</b></div>
        <div class="tend-actions" style="margin-top:var(--s2)">
          <button class="btn mx-save">save for ${esc(friend)}</button>
          <button class="btn btn-quiet mx-clear">reset ALL to inherit</button>
        </div>
        <p class="meta mx-note" hidden></p>
      </div>`;
    panel.querySelectorAll(".mx-class").forEach((btn) => {
      btn.onclick = () => {
        const c = btn.dataset.class;
        state.classes = state.classes || {};
        const cur = state.classes[c];
        if (!cur) state.classes[c] = "on";
        else if (cur === "on") state.classes[c] = "off";
        else delete state.classes[c];
        render();
      };
    });
    panel.querySelector(".mx-spheres-custom")?.addEventListener("click", () => {
      state.spheres = [...tplEff.spheres]; render();
    });
    panel.querySelector(".mx-spheres-reset")?.addEventListener("click", () => {
      delete state.spheres; render();
    });
    panel.querySelectorAll(".mx-sphere").forEach((c) => {
      c.onchange = () => {
        state.spheres = [...panel.querySelectorAll(".mx-sphere:checked")].map((x) => x.value);
        render();
      };
    });
    panel.querySelector(".mx-detail").onchange = (e) => {
      if (e.target.value) state.detail = e.target.value; else delete state.detail;
      render();
    };
    panel.querySelector(".mx-done").onchange = (e) => {
      if (e.target.value) state.completed_tasks = e.target.value === "true";
      else delete state.completed_tasks;
      render();
    };
    panel.querySelector(".mx-ev").onchange = (e) => {
      if (e.target.value) state.evidence_relationships = e.target.value === "true";
      else delete state.evidence_relationships;
      render();
    };
    panel.querySelector(".mx-clear").onclick = () => {
      for (const k of Object.keys(state)) delete state[k];
      render();
    };
    panel.querySelector(".mx-save").onclick = async () => {
      const note = panel.querySelector(".mx-note");
      note.hidden = false; note.textContent = "saving…";
      const res = await apiPost("/v1/grants/sharing", {
        actor: ACTOR, friend, sharing: state,
      }).catch((e) => ({ error: e.message }));
      if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
      onSaved?.();
    };
  };
  render();
}

/* ————— the transparency ledger ————— */

/* ————— groups (v54): a named set of connections —————
   A group is a second way to share: what it grants is ADDED to what each
   member already gets, and it inherits nothing — an empty group grants
   nothing. Only this record edits its groups; members' brains are told
   the name and the roster. */
function groupEff(sharing) {
  const s = sharing || {};
  return {
    classes: CLASSES.filter((c) => s.classes?.[c] === "on"),
    spheres: s.spheres || [],
    detail: s.detail || "excerpts",
    completed_tasks: !!s.completed_tasks,
    evidence_relationships: !!s.evidence_relationships,
  };
}

async function paintGroups(host) {
  const data = await api("/v1/groups").catch(() => null);
  if (!data) { host.innerHTML = `<p class="meta">groups are unavailable right now.</p>`; return; }
  const groups = data.groups || [];
  const connections = data.connections || [];
  const repaint = () => paintGroups(host);
  const act = async (body, note) => {
    const res = await apiPost("/v1/groups", { actor: ACTOR, ...body }).catch((e) => ({ error: e.message }));
    if (res?.error) {
      if (note) { note.hidden = false; note.textContent = `✗ ${res.error}`; }
      return null;
    }
    return res;
  };
  const cards = groups.map((g) => {
    const eff = groupEff(g.sharing);
    const grants = eff.classes.length && eff.spheres.length ? effSummary(eff)
      : "nothing by category";
    return `
    <div class="card" data-group="${esc(g.id)}">
      <div class="task-row" style="align-items:baseline; flex-wrap:wrap">
        <b>◍ ${esc(g.name)}</b>
        <span class="meta">${g.members.length
          ? esc(g.members.join(", ")) : "no members yet"}</span>
        <span class="meta">adds: ${esc(grants)}${g.shared_items
          ? ` · ${g.shared_items} pinned item${g.shared_items === 1 ? "" : "s"}` : ""}</span>
        <button class="btn btn-quiet grp-edit">edit…</button>
        <button class="btn btn-quiet grp-delete">delete…</button>
      </div>
      <div class="grp-panel" hidden></div>
    </div>`;
  }).join("");
  const memberOf = (data.member_of || []).map((m) => `
    <div class="meta" style="padding:2px 0">◍ <b>${esc(m.name)}</b> — ${esc(m.owner)}'s group
      · ${esc((m.members || []).join(", "))}</div>`).join("");
  host.innerHTML = `
    ${cards || `<p class="meta">no groups yet. A group lets you share with several connections at once —
      what it grants is added to what each of them already gets.</p>`}
    <div class="card">
      <div class="searchbar" style="flex-wrap:wrap; align-items:center">
        <input id="grp-new" placeholder="a new group — e.g. Family" maxlength="60" style="flex:0.6">
        <button class="btn" id="grp-create">create group</button>
      </div>
      <p class="meta" id="grp-note" hidden></p>
    </div>
    ${memberOf ? `<div class="meta" style="margin-top:var(--s3)">groups you're in, on other people's records:</div>${memberOf}` : ""}`;
  const note = host.querySelector("#grp-note");
  const create = async () => {
    const name = host.querySelector("#grp-new").value.trim();
    if (!name) return;
    if (await act({ action: "create", name }, note)) repaint();
  };
  host.querySelector("#grp-create").onclick = create;
  host.querySelector("#grp-new").onkeydown = (e) => { if (e.key === "Enter") create(); };
  host.querySelectorAll(".grp-delete").forEach((btn) => {
    const g = groups.find((x) => x.id === btn.closest(".card").dataset.group);
    btn.onclick = async () => {
      const confirmed = await confirmDanger({
        title: `Delete the group “${g.name}”?`,
        body: "Its members stop getting what the group added from this moment" +
          (g.shared_items ? `, and the ${g.shared_items} item${g.shared_items === 1 ? "" : "s"} pinned to it are un-shared` : "") +
          ". What you share with each of them directly is unchanged.",
        action: "delete group",
      });
      if (!confirmed) return;
      await act({ action: "delete", id: g.id });
      repaint();
    };
  });
  host.querySelectorAll(".grp-edit").forEach((btn) => {
    btn.onclick = () => {
      const card = btn.closest(".card");
      const panel = card.querySelector(".grp-panel");
      if (!panel.hidden) { panel.hidden = true; return; }
      panel.hidden = false;
      paintGroupPanel(panel, groups.find((x) => x.id === card.dataset.group), connections, act, repaint);
    };
  });
}

function paintGroupPanel(panel, group, connections, act, repaint) {
  const eff = groupEff(group.sharing);
  // someone still on the roster whose connection was revoked stays listed
  // so they can be unticked
  const people = [...new Set([...connections, ...group.members])];
  const box = (cls, value, checked, label) => `<label class="meta" style="white-space:nowrap">
    <input type="checkbox" class="${cls}" value="${esc(value)}" ${checked ? "checked" : ""}> ${esc(label)}</label>`;
  panel.innerHTML = `
    <div style="margin-top:var(--s2); padding-top:var(--s2); border-top:1px solid var(--line)">
      <div class="searchbar" style="flex-wrap:wrap; align-items:center">
        <span class="meta">name</span>
        <input class="grp-name" value="${esc(group.name)}" maxlength="60" style="flex:0.5">
      </div>
      <div class="meta" style="margin:var(--s2) 0 var(--s1)">members — your connections</div>
      <div class="searchbar" style="flex-wrap:wrap">
        ${people.length ? people.map((p) => box("grp-member", p, group.members.includes(p), p)).join("")
          : `<span class="meta">you have no connections to add yet.</span>`}
      </div>
      <div class="meta" style="margin:var(--s2) 0 var(--s1)">what members get, on top of their own sharing
        — needs at least one kind AND one life area; leave empty to only pin items to the group</div>
      <div class="searchbar" style="flex-wrap:wrap">
        ${CLASSES.map((c) => box("grp-class", c, eff.classes.includes(c), CLASS_LABEL[c])).join("")}
      </div>
      <div class="searchbar" style="flex-wrap:wrap; margin-top:var(--s1)">
        ${SPHERES.map((s) => box("grp-sphere", s, eff.spheres.includes(s), s)).join("")}
        <select class="grp-detail">
          <option value="excerpts" ${eff.detail === "excerpts" ? "selected" : ""}>excerpts only</option>
          <option value="full" ${eff.detail === "full" ? "selected" : ""}>full detail</option>
        </select>
        ${box("grp-flag", "completed_tasks", eff.completed_tasks, "incl. completed tasks")}
        ${box("grp-flag", "evidence_relationships", eff.evidence_relationships, "incl. evidence-tier people")}
      </div>
      <div class="tend-actions" style="margin-top:var(--s2)">
        <button class="btn grp-save">save</button>
        <span class="meta grp-status" hidden></span>
      </div>
    </div>`;
  const status = panel.querySelector(".grp-status");
  panel.querySelector(".grp-save").onclick = async () => {
    const picked = (cls) => [...panel.querySelectorAll(`.${cls}:checked`)].map((c) => c.value);
    const name = panel.querySelector(".grp-name").value.trim();
    const flags = picked("grp-flag");
    const sharing = {
      classes: Object.fromEntries(picked("grp-class").map((c) => [c, "on"])),
      spheres: picked("grp-sphere"),
      detail: panel.querySelector(".grp-detail").value,
      completed_tasks: flags.includes("completed_tasks"),
      evidence_relationships: flags.includes("evidence_relationships"),
    };
    // all three, stopping at the first refusal so the message is the real one
    if (name !== group.name && !(await act({ action: "rename", id: group.id, name }, status))) return;
    if (!(await act({ action: "members", id: group.id, members: picked("grp-member") }, status))) return;
    if (!(await act({ action: "sharing", id: group.id, sharing }, status))) return;
    repaint();
  };
}

async function paintAccesses(host) {
  const data = await api("/v1/network/accesses").catch(() => ({ accesses: [] }));
  const rows = (data.accesses || []).slice(0, 20).map((a) => `
    <div class="meta" style="padding:2px 0">${esc(a.ts.slice(0, 16).replace("T", " "))} —
      <b>${esc(a.payload?.friend || "?")}</b> ${esc(a.payload?.op || a.payload?.surface || "")}
      ${a.payload?.query ? `“${esc(a.payload.query)}”` : esc(a.payload?.episode || "")}
      ${a.payload?.hits !== undefined ? `→ ${a.payload.hits} results` : ""}</div>`).join("");
  host.innerHTML = rows || `<p class="meta">no one has asked your brain anything yet.</p>`;
}

export async function mount(view, helpers) {
  ({ api, apiPost, confirmDanger } = helpers);
  view.innerHTML = `
    <span class="eyebrow">network</span>
    <h1 class="pagehead">Brains that know each other</h1>
    <p class="subhead">Connections are consent-governed in both directions: a default you
      set once, per-friend overrides when someone's different, and per-item shares that
      punch through. Every access is on the record — literally.</p>
    <div class="section-title">brains I can ask</div>
    <div id="net-friends"><div class="loading">…</div></div>
    <div class="section-title" style="margin-top:var(--s5)">my calling card</div>
    <div id="net-mycard"><div class="loading">…</div></div>
    <div class="section-title" style="margin-top:var(--s5)">what I share</div>
    <div id="net-default"><div class="loading">…</div></div>
    <div id="net-grants"><div class="loading">…</div></div>
    <div class="section-title" style="margin-top:var(--s5)">groups
      <span class="meta" style="font-weight:400">— share with several connections at once</span></div>
    <div id="net-groups"><div class="loading">…</div></div>
    <div class="section-title" style="margin-top:var(--s5)">who asked my brain
      <span class="meta" style="font-weight:400">— the transparency ledger</span></div>
    <div id="net-accesses"><div class="loading">…</div></div>`;
  await paintFriends(view.querySelector("#net-friends"));
  await paintMyCard(view.querySelector("#net-mycard"));
  const grantsHost = view.querySelector("#net-grants");
  await paintDefault(view.querySelector("#net-default"), () => paintGrants(grantsHost));
  await paintGrants(grantsHost);
  await paintGroups(view.querySelector("#net-groups"));
  await paintAccesses(view.querySelector("#net-accesses"));
  return () => {};
}
