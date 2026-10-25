// relationships.js — the Relationships directory: the people and orgs
// of a life, tiered by lived evidence rather than list-keeping.
//   inner circle — declared importance or a confirmed close kind
//   active       — evidence in the last 90 days
//   drifting     — present this year, silent for 90+
// Predicted kinds ("collaborator?") confirm or reject inline — the
// prediction-then-confirmation flow, right where you read the row.
// Sparklines are twelve months of evidence; ⭑ chips are goals this
// person keeps appearing around; ✨ marks someone matching what you're
// seeking. Everyone else stays behind Search — zero gardening.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const ACTOR = "human:dashboard";

async function api(path) {
  const response = await fetch(path);
  if (!response.ok) throw new Error((await response.json()).error || response.statusText);
  return response.json();
}
async function apiPost(path, body) {
  const response = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error((await response.json()).error || response.statusText);
  return response.json();
}

function sparkline(months) {
  const max = Math.max(...months, 1);
  const bars = months.map((n, i) => {
    const h = n ? Math.max(2, (n / max) * 14) : 1;
    return `<rect x="${i * 5}" y="${15 - h}" width="3.4" height="${h}"
      fill="${n ? "var(--chart-1)" : "var(--line)"}" opacity="${n ? 0.45 + 0.55 * (n / max) : 0.5}"/>`;
  }).join("");
  return `<svg class="rel-spark" width="60" height="16" viewBox="0 0 60 16"
    aria-hidden="true">${bars}</svg>`;
}

function toast(msg) {
  document.querySelector(".desk-toast")?.remove();
  const el = document.createElement("div");
  el.className = "desk-toast";
  el.textContent = msg;
  document.body.appendChild(el);
  requestAnimationFrame(() => el.classList.add("in"));
  setTimeout(() => { el.classList.remove("in"); setTimeout(() => el.remove(), 350); }, 2600);
}

const SKELETON = Array.from({ length: 8 }, (_, i) =>
  `<div class="ghost-row" style="width:${88 - (i % 4) * 9}%"></div>`).join("");

function ago(day) {
  if (!day) return "";
  const days = Math.floor((Date.now() - Date.parse(`${day}T12:00:00Z`)) / 86400000);
  if (days <= 0) return "today";
  if (days === 1) return "yesterday";
  if (days < 30) return `${days}d ago`;
  if (days < 365) return `${Math.round(days / 30)}mo ago`;
  return `${Math.round(days / 365)}y ago`;
}

const TIER_HEAD = {
  self: ["you", "the record's owner — self-evident from the evidence"],
  inner: ["inner circle", "declared important, or confirmed close"],
  active: ["active", "evidence in the last 90 days"],
  drifting: ["drifting", "present this year, quiet lately — worth a hello?"],
};

let __friends = [];
async function loadFriends() {
  try { __friends = (await api("/v1/friends")).friends || []; } catch { __friends = []; }
}
const brainChip = (r) => {
  const f = __friends.find((f) => f.entity && (f.entity === r.entity_id || f.entity === r.name));
  return f ? ` <a class="task-tag plain-tag no-modal" href="#/network" title="their brain is connected — Ask can reach what they share">🧠</a>` : "";
};
export async function mount(container) {
  loadFriends(); // fire-and-forget: chips appear on the next repaint
  container.innerHTML = `
    <span class="eyebrow">relationships</span>
    <h1 class="pagehead">The people of the record</h1>
    <p class="subhead">Tiered by lived evidence, not list-keeping. Hearthloom predicts
      how you relate from the record — <b>you</b> confirm or reject each guess.
      Everyone not shown is a Search away.</p>
    <div class="searchbar">
      <input id="rel-q" placeholder="filter by name or org…">
      <span id="rel-type" style="display:flex; gap:4px">
        <button class="loom-chip" data-type="">everyone</button>
        <button class="loom-chip resting" data-type="Person">people</button>
        <button class="loom-chip resting" data-type="Org">orgs</button>
      </span>
      <span class="meta" id="rel-pending"></span>
    </div>
    <div id="rel-body">${SKELETON}</div>`;

  const bodyEl = container.querySelector("#rel-body");
  const qEl = container.querySelector("#rel-q");
  let directory = [];
  let kinds = [];

  const qual = (rel) => rel.note
    ? ` · ${esc(rel.note.length > 24 ? rel.note.slice(0, 24) + "…" : rel.note)}` : "";
  const row = (r) => {
    const chips = r.relationships.map((rel) => {
      if (rel.status === "confirmed")
        return `<span class="rel-chip confirmed" data-rel="${esc(rel.id)}"
           title="${esc(rel.origin)}${rel.note ? ` · ${esc(rel.note)}` : ""} · since ${esc((rel.t_valid || "").slice(0, 10))} — × ends it">
           ${esc(rel.kind)}${qual(rel)} <button class="rel-end" title="no longer true">×</button></span>`;
      if (rel.status === "proposed")
        return `<span class="rel-chip proposed" data-rel="${esc(rel.id)}"
           title="predicted from ${rel.evidence.length} episode${rel.evidence.length === 1 ? "" : "s"}">
           ${esc(rel.kind)}?
           <button class="rel-yes" title="yes, that's right">✓</button>
           <button class="rel-no" title="no">✗</button></span>`;
      // ended: history stays visible — bi-temporal, not deleted
      return `<span class="rel-chip ended"
           title="ended ${esc((rel.t_invalid || "").slice(0, 10))}${rel.note ? ` · ${esc(rel.note)}` : ""}">was ${esc(rel.kind)}${qual(rel)}</span>`;
    }).join("");
    return `
      <div class="rel-row" data-entity="${esc(r.entity_id)}" data-name="${esc(r.name)}">
        <span class="rel-name">
          <a href="#/entity/${encodeURIComponent(r.name)}">${esc(r.name)}</a>${brainChip(r)}
          ${r.entity_type === "Org" ? `<span class="typebadge">Org</span>` : ""}
          ${r.declared_importance ? "★".repeat(r.declared_importance) : ""}
          ${r.org ? `<span class="meta"> · ${esc(r.org)}</span>` : ""}
          ${r.handle ? `<span class="rel-handle" title="the identity behind the name">${esc(r.handle)}</span>` : ""}
        </span>
        <span class="rel-chips">
          ${chips}
          <select class="rel-declare" title="declare how you relate — 'was' records a former relationship">
            <option value="">relate…</option>
            <optgroup label="is my…">
              ${kinds.filter((k) => k !== "me").map((k) => `<option value="${k}">${k}</option>`).join("")}
            </optgroup>
            <optgroup label="was my… (former)">
              ${kinds.filter((k) => k !== "me").map((k) => `<option value="former:${k}">${k}</option>`).join("")}
            </optgroup>
          </select>
        </span>
        <span class="rel-signals">
          ${r.goals.map((g) => `<span class="task-tag" title="keeps appearing around this goal">⭑ ${esc(g)}</span>`).join("")}
          ${r.seeking_match ? `<span class="task-tag" title="matches something you're seeking">✨</span>` : ""}
          <button class="task-tag plain-tag rel-notes" data-count="${r.notes}"
            title="${r.notes ? `${r.notes} note${r.notes === 1 ? "" : "s"} mentioning them — opens the panel` : "write a note about them"}">✎${r.notes ? ` ${r.notes}` : " +"}</button>
        </span>
        ${sparkline(r.months)}
        <span class="meta rel-last${r.tier === "drifting" || (r.tier === "inner" && r.last_evidence && ago(r.last_evidence).includes("mo")) ? " rel-quiet" : ""}">${ago(r.last_evidence)}</span>
        <button class="rel-more" title="more actions — merge identities">⋯</button>
      </div>`;
  };

  function paint() {
    const q = qEl.value.trim().toLowerCase();
    const filtered = q
      ? directory.filter((r) =>
          r.name.toLowerCase().includes(q) || (r.org || "").toLowerCase().includes(q))
      : directory;
    const tiers = ["self", "inner", "active", "drifting"];
    bodyEl.innerHTML = tiers.map((tier) => {
      const rows = filtered.filter((r) => r.tier === tier);
      if (!rows.length) return "";
      const [head, hint] = TIER_HEAD[tier];
      return `
        <div class="section-title" style="margin-top:var(--s5)">${head}
          <span class="meta" style="font-weight:400">— ${hint}</span>
          <span class="statchip">${rows.length}</span></div>
        ${rows.map(row).join("")}`;
    }).join("") || (q ? "" : `<p class="meta" style="color:var(--ink-faint)">nothing matches.</p>`);
    bind();
    paintBeyond(qEl.value.trim());
  }

  // The tiers only carry the evidence-ranked top — but the FILTER must
  // find anyone the record knows (a contact with 9 episodes included).
  // Server search fills in "beyond the tiers" rows on demand.
  let beyondSeq = 0;
  async function paintBeyond(q) {
    const seq = ++beyondSeq;
    if (q.length < 2) return;
    const [data, conns] = await Promise.all([
      api(`/v1/browse/entities?q=${encodeURIComponent(q)}&type=Person,Org&limit=12`).catch(() => null),
      api(`/v1/connections?q=${encodeURIComponent(q)}&limit=8`).catch(() => null),
    ]);
    if (seq !== beyondSeq || !data) return; // a newer keystroke won
    const shown = new Set(
      directory.filter((r) => r.name.toLowerCase().includes(q.toLowerCase())
        || (r.org || "").toLowerCase().includes(q.toLowerCase())).map((r) => r.name));
    const extra = (data.items || []).filter((e) => !shown.has(e.name));
    // registry-only names: the network says you know them, but they
    // have no entity yet — one click materializes
    const entityNames = new Set([...shown, ...extra.map((e) => e.name)]);
    const registryOnly = (conns?.connections || [])
      .filter((c) => !c.entity_id && !entityNames.has(c.name));
    if (!extra.length && !registryOnly.length) {
      if (!bodyEl.children.length) {
        bodyEl.innerHTML = `<p class="meta" style="color:var(--ink-faint)">nothing matches.</p>`;
      }
      return;
    }
    const host = document.createElement("div");
    host.innerHTML = `
      ${extra.length ? `
      <div class="section-title" style="margin-top:var(--s5)">beyond the tiers
        <span class="meta" style="font-weight:400">— everyone else the record knows</span>
        <span class="statchip">${extra.length}</span></div>
      ${extra.map((e) => row({
        entity_id: e.id, name: e.name, entity_type: e.entity_type,
        declared_importance: 0, org: null, handle: null,
        last_evidence: null, evidence_year: e.episode_count || 0,
        months: Array(12).fill(0), relationships: [], goals: [],
        seeking_match: false, notes: 0, tier: "beyond",
      })).join("")}` : ""}
      ${registryOnly.length ? `
      <div class="section-title" style="margin-top:var(--s5)">connections
        <span class="meta" style="font-weight:400">— a network says you know them; no interaction evidence in your history yet</span>
        <span class="statchip">${registryOnly.length}</span></div>
      ${registryOnly.map((c) => `
        <div class="rel-row rel-registry" data-network="${esc(c.network)}" data-name="${esc(c.name)}">
          <span class="rel-name">${esc(c.name)}
            ${c.handle ? `<span class="rel-handle">${esc(c.handle)}</span>` : ""}</span>
          <span class="meta">${esc(c.network)} ${c.status === "removed"
            ? `friend until ${esc((c.ended_at || "").slice(0, 4))}`
            : `friend${c.connected_at ? ` since ${esc(c.connected_at.slice(0, 4))}` : ""}`}${
            c.position || c.company
              ? ` · ${esc([c.position, c.company].filter(Boolean).join(", "))}` : ""}</span>
          <span></span><span></span><span></span>
          <button class="btn btn-quiet rel-materialize"
            title="create their entry in the record — then relate, note, star">add to the record</button>
        </div>`).join("")}` : ""}`;
    bodyEl.appendChild(host);
    bind();
    host.querySelectorAll(".rel-materialize").forEach((btn) => {
      btn.onclick = async () => {
        const rowEl = btn.closest(".rel-registry");
        btn.disabled = true;
        btn.textContent = "adding…";
        await apiPost("/v1/connections/materialize", {
          actor: ACTOR, network: rowEl.dataset.network, name: rowEl.dataset.name,
        });
        toast(`${rowEl.dataset.name} is in the record — relate, note, or star them`);
        window.__openPersonModal?.(rowEl.dataset.name);
        paint();
      };
    });
  }

  function bind() {
    bodyEl.querySelectorAll(".rel-chip.proposed").forEach((chip) => {
      chip.querySelector(".rel-yes").onclick = async () => {
        await apiPost(`/v1/relationships/${chip.dataset.rel}/resolve`,
          { actor: ACTOR, action: "confirm" });
        await load();
      };
      chip.querySelector(".rel-no").onclick = async () => {
        await apiPost(`/v1/relationships/${chip.dataset.rel}/resolve`,
          { actor: ACTOR, action: "reject" });
        chip.remove();
      };
    });
    bodyEl.querySelectorAll(".rel-chip.confirmed .rel-end").forEach((end) => {
      end.onclick = async () => {
        const chip = end.closest(".rel-chip");
        await apiPost(`/v1/relationships/${chip.dataset.rel}/resolve`,
          { actor: ACTOR, action: "end" });
        await load();
      };
    });
    // notes chip → the left panel filtered to them; with none yet, a
    // fresh note prefilled with their linked @mention
    bodyEl.querySelectorAll(".rel-notes").forEach((chip) => {
      chip.onclick = () => {
        const name = chip.closest(".rel-row").dataset.name;
        if (+chip.dataset.count > 0) window.__openNotesPanel?.(name);
        else window.__newNoteFor?.(name);
      };
    });
    // ⋯ menu: special actions — merge identities first
    bodyEl.querySelectorAll(".rel-more").forEach((btn) => {
      btn.onclick = (e) => {
        e.stopPropagation();
        openMergePanel(btn.closest(".rel-row").dataset.name);
      };
    });
    bodyEl.querySelectorAll(".rel-declare").forEach((sel) => {
      sel.onchange = () => {
        if (!sel.value) return;
        const name = sel.closest(".rel-row").dataset.name;
        const former = sel.value.startsWith("former:");
        // optional freeform qualifier ("accountant — since 2019")
        window.__askRelContext(sel, async (ctx) => {
          await apiPost("/v1/relationships", {
            actor: ACTOR, entity: name,
            kind: former ? sel.value.slice(7) : sel.value,
            ended: former,
            note: ctx || null,
          });
          await load();
        });
      };
    });
  }

  /* — the ⋯ merge panel: who's already merged in (with unmerge — the
     reversibility made visible), Tend's suspicions, live close matches,
     free typeahead. Every action answers on the spot: the row flips to
     "merging…", lands in the merged list, and a toast confirms — */
  async function openMergePanel(name) {
    const overlay = document.createElement("div");
    overlay.className = "note-overlay";
    overlay.innerHTML = `
      <div class="note-editor" style="width:min(560px,92vw)">
        <div class="searchbar" style="margin-bottom:var(--s3)">
          <b style="flex:1">merge identities — ${esc(name)}</b>
          <button class="btn" id="mp-close">✕</button>
        </div>
        <p class="meta">Pick who <b>${esc(name)}</b> is the same as. Merges are
          reversible — anything merged in appears below with an unmerge.</p>
        <div id="mp-merged"></div>
        <div id="mp-suggested">${SKELETON}</div>
        <div class="searchbar" style="margin-top:var(--s3)">
          <input id="mp-q" placeholder="or search anyone…">
        </div>
        <div id="mp-results"></div>
      </div>`;
    document.body.appendChild(overlay);
    const close = () => { overlay.remove(); load(); };
    overlay.querySelector("#mp-close").onclick = close;
    overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });

    const mergeWith = async (otherName, reviewId) => {
      if (reviewId) {
        await apiPost(`/v1/reviews/${reviewId}`, { actor: ACTOR, resolution: "same" });
      } else {
        await apiPost("/v1/identity/merge", {
          actor: ACTOR, winner: name, loser: otherName,
          reason: "merged from the Relationships directory",
        });
      }
    };

    /* A candidate row EXPANDS on click — full identities, evidence
       span, and explicit actions. Nothing merges until the button
       says so (the lesson: a click that mutates must look like one). */
    const candidate = (label, sub, reviewId) => {
      const el = document.createElement("div");
      el.className = "mp-cand";
      el.innerHTML = `
        <div class="note-row mp-cand-head">
          <span class="note-title">${esc(label)}</span>
          <span class="meta">${esc(sub)} · <b>details ▾</b></span>
        </div>
        <div class="mp-cand-detail" hidden></div>`;
      const head = el.querySelector(".mp-cand-head");
      const detail = el.querySelector(".mp-cand-detail");
      let loaded = false;
      head.onclick = async () => {
        detail.hidden = !detail.hidden;
        if (loaded || detail.hidden) return;
        loaded = true;
        detail.innerHTML = `<div class="ghost-row"></div>`;
        let d = null;
        try { d = await api(`/v1/entity/${encodeURIComponent(label)}`); } catch { /* thin */ }
        const attrs = (d?.attributes || [])
          .map(([k, v]) => `<div class="attr-row"><span class="k">${esc(k)}</span><span style="overflow-wrap:anywhere">${esc(v)}</span></div>`)
          .join("");
        const last = (d?.recent_episodes || [])[0]?.[3];
        detail.innerHTML = `
          ${attrs || `<p class="meta">no identifying attributes on file.</p>`}
          <p class="meta">${d ? `${d.episode_count} episodes${last ? ` · latest ${esc(last.slice(0, 10))}` : ""}` : ""}</p>
          <div class="tend-actions">
            <button class="btn mp-do-merge">merge into ${esc(name)}</button>
            <button class="btn btn-quiet mp-not-same">different person</button>
            <button class="btn btn-quiet mp-open-card">open their card</button>
          </div>
          <div class="searchbar" style="margin-top:var(--s2)">
            <input class="mp-other" placeholder="…or merge them into someone else — type a name">
          </div>
          <div class="mp-other-results"></div>`;
        detail.querySelector(".mp-do-merge").onclick = async (ev) => {
          ev.target.disabled = true;
          ev.target.textContent = "merging…";
          try {
            await mergeWith(label, reviewId);
            toast(`merged — ${label} is now part of ${name} (reversible below)`);
            el.remove();
            paintMerged();
          } catch (err) {
            ev.target.disabled = false;
            ev.target.textContent = `failed: ${err.message}`;
          }
        };
        detail.querySelector(".mp-not-same").onclick = async () => {
          if (reviewId) {
            await apiPost(`/v1/reviews/${reviewId}`, { actor: ACTOR, resolution: "distinct" });
            toast(`noted — ${label} stays a distinct person`);
          }
          el.remove();
        };
        // "tell Hearthloom about them": the person card — declare,
        // star, note — right from here.
        detail.querySelector(".mp-open-card").onclick = () =>
          window.__openPersonModal?.(label);
        // third-party merge: they're not YOU, but you spot who they are
        const otherEl = detail.querySelector(".mp-other");
        const otherResults = detail.querySelector(".mp-other-results");
        otherEl.addEventListener("input", async () => {
          const q = otherEl.value.trim();
          if (q.length < 2) { otherResults.innerHTML = ""; return; }
          const data = await api(`/v1/browse/entities?q=${encodeURIComponent(q)}&type=Person,Org&limit=6`).catch(() => null);
          otherResults.innerHTML = "";
          for (const item of (data?.items || [])) {
            if (item.name === label) continue;
            const opt = document.createElement("div");
            opt.className = "note-row";
            opt.innerHTML = `<span class="note-title">${esc(item.name)}</span>
              <span class="meta">${item.episode_count ?? "?"} episodes · <b>merge ${esc(label)} into them</b></span>`;
            opt.onclick = async () => {
              await apiPost("/v1/identity/merge", {
                actor: ACTOR, winner: item.name, loser: label,
                reason: "redirected from the Relationships merge panel",
              });
              if (reviewId) {
                await apiPost(`/v1/reviews/${reviewId}`, { actor: ACTOR, resolution: "distinct" })
                  .catch(() => {});
              }
              toast(`merged — ${label} is now part of ${item.name}`);
              el.remove();
            };
            otherResults.appendChild(opt);
          }
        });
      };
      return el;
    };

    // 0) already part of this identity — each undoable on the spot
    const mergedEl = overlay.querySelector("#mp-merged");
    async function paintMerged() {
      try {
        const data = await api("/v1/merges");
        const mine = (data.merges || []).filter((m) =>
          !m.reverted_at && (m.winner?.name === name));
        mergedEl.innerHTML = mine.length
          ? `<div class="section-title">already merged in</div>` : "";
        for (const m of mine) {
          const rowEl = document.createElement("div");
          rowEl.className = "note-row";
          rowEl.innerHTML = `<span class="note-title">${esc(m.loser_name)}</span>
            <span class="meta">${esc((m.merged_at || "").slice(0, 10))}
              · <button class="btn btn-quiet mp-unmerge">unmerge</button></span>`;
          rowEl.querySelector(".mp-unmerge").onclick = async (e) => {
            e.stopPropagation();
            await apiPost(`/v1/merges/${m.id}/revert`, { actor: ACTOR });
            toast(`unmerged — ${m.loser_name} stands alone again`);
            paintMerged();
          };
          mergedEl.appendChild(rowEl);
        }
      } catch { mergedEl.innerHTML = ""; }
    }
    paintMerged();

    // 1) Tend already suspects: pending + parked pairs involving them
    const suggested = overlay.querySelector("#mp-suggested");
    try {
      const [pending, parked, similar] = await Promise.all([
        api("/v1/reviews"), api("/v1/reviews?state=unknown"),
        api(`/v1/entities/similar?name=${encodeURIComponent(name)}`),
      ]);
      suggested.innerHTML = "";
      const mine = [...(pending.pending || []), ...(parked.pending || [])]
        .filter((r) => r.entity_a.name === name || r.entity_b.name === name);
      for (const r of mine) {
        const other = r.entity_a.name === name ? r.entity_b : r.entity_a;
        suggested.appendChild(candidate(other.name,
          `Tend suspects — ${r.reason.slice(0, 40)}`, r.id));
      }
      const seen = new Set(mine.map((r) =>
        (r.entity_a.name === name ? r.entity_b : r.entity_a).name));
      for (const m of (similar.matches || [])) {
        if (seen.has(m.name) || m.name === name) continue;
        suggested.appendChild(candidate(m.name,
          `${m.handle || "close name match"} · ${m.episodes ?? "?"} episodes`, null));
      }
      if (!suggested.children.length) {
        suggested.innerHTML = `<p class="meta" style="color:var(--ink-faint)">no likely matches — search below.</p>`;
      }
    } catch {
      suggested.innerHTML = `<p class="meta">suggestion lookup failed — search below.</p>`;
    }

    // 3) free typeahead over all entities
    const qEl = overlay.querySelector("#mp-q");
    const resultsEl = overlay.querySelector("#mp-results");
    qEl.addEventListener("input", async () => {
      const q = qEl.value.trim();
      if (q.length < 2) { resultsEl.innerHTML = ""; return; }
      const data = await api(`/v1/browse/entities?q=${encodeURIComponent(q)}&limit=8`);
      resultsEl.innerHTML = "";
      for (const item of (data.items || [])) {
        if (item.name === name) continue;
        resultsEl.appendChild(candidate(item.name,
          `${item.entity_type || ""} · ${item.episodes ?? item.episode_count ?? "?"} episodes`, null));
      }
    });
    qEl.focus();
  }

  let typeFilter = ""; // "" | Person | Org — orgs get their own tiers
  container.querySelectorAll("#rel-type .loom-chip").forEach((chip) => {
    chip.onclick = async () => {
      typeFilter = chip.dataset.type;
      container.querySelectorAll("#rel-type .loom-chip").forEach((c) =>
        c.classList.toggle("resting", c !== chip));
      bodyEl.innerHTML = SKELETON;
      await load();
    };
  });

  async function load() {
    const data = await api(`/v1/relationships${typeFilter ? `?type=${typeFilter}` : ""}`);
    // the view may have unmounted while we fetched (panel-close reload)
    if (!container.querySelector("#rel-pending")) return;
    directory = data.directory || [];
    kinds = data.kinds || [];
    container.querySelector("#rel-pending").textContent = data.pending
      ? `${data.pending} prediction${data.pending === 1 ? "" : "s"} await your yes/no (marked with ?)`
      : "";
    paint();
  }
  qEl.addEventListener("input", paint);
  await load();
  return () => {};
}
