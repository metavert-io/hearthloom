// Hearthloom dashboard — hash-routed, dependency-free (three.js is the
// one vendored library, loaded only on the constellation view).

const API = "";
const SOURCE_HUES = {
  git: "var(--hue-git)",
  photos: "var(--hue-photos)",
  imessage: "var(--hue-imessage)",
  "chatlog-claude": "var(--hue-chat)",
  "chatlog-chatgpt": "var(--hue-chat)",
  "claude-code": "var(--hue-chat)",
  ics: "var(--hue-ics)",
  chrome: "var(--hue-chrome)",
  consolidation: "var(--hue-memory)",
  memory: "var(--hue-memory)",
};
const KIND_LABEL = {
  commit: "commit",
  photos: "photos",
  messages: "messages",
  calendar_event: "meeting",
  conversation: "ai chat",
  agent_session: "claude code",
  browsing: "browsing",
  summary: "digest",
  email: "email",
};

const view = document.getElementById("view");

/* If a key is stored (☰ → API Keys → "use in this browser"), send it —
   required when the daemon runs with HEARTHLOOM_REQUIRE_KEYS=1. */
const authHeaders = () => {
  const key = localStorage.getItem("hl-api-key");
  return key ? { Authorization: `Bearer ${key}` } : {};
};
async function api(path) {
  const response = await fetch(API + path, { headers: authHeaders() });
  if (!response.ok) throw new Error((await response.json()).error || response.statusText);
  return response.json();
}
const esc = (s) =>
  String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
// Both mention forms (item 13): typed @[Type:Name] and legacy @[Name] —
// $1 is the display name either way (twin of the notes.js export).
const MENTION_RE =
  /@\[(?:(?:person|org|group|project|place|artifact|event|topic|device):\s*)?([^\]]{1,80})\]/gi;
// note titles may carry @[Name] mention tokens → show plain @Name
const noteTitle = (t) => esc(t).replace(MENTION_RE, "@$1");

function localToday() {
  const now = new Date();
  now.setMinutes(now.getMinutes() - now.getTimezoneOffset());
  return now.toISOString().slice(0, 10);
}
function hhmm(iso, offsetMinutes) {
  if (!iso) return "";
  const at = new Date(iso);
  if (offsetMinutes != null) at.setMinutes(at.getMinutes() + offsetMinutes + at.getTimezoneOffset());
  return at.toTimeString().slice(0, 5);
}
function prettyDate(ymd) {
  return new Date(ymd + "T12:00:00").toLocaleDateString(undefined, {
    weekday: "long", year: "numeric", month: "long", day: "numeric",
  });
}

/* ————— tiny safe markdown (escape first, then decorate) ————— */
function md(text, charts) {
  let raw = String(text ?? "");
  // Lift ```chart fences out BEFORE escaping so their JSON survives;
  // each becomes a placeholder div the caller renders via bindCharts().
  if (charts) {
    raw = raw.replace(/```chart\s*\n([\s\S]*?)```/g, (whole, json) => {
      try {
        charts.push(JSON.parse(json));
        return `\u0000CHART${charts.length - 1}\u0000`;
      } catch {
        return whole; // malformed spec falls through as a code block
      }
    });
    // ```action fences: Ask may SUGGEST a write (task/goal) — rendered
    // as a button that PREFILLS the Desk; the human completes the save.
    // Two-phase by law: the engine never writes.
    raw = raw.replace(/```action\s*\n([\s\S]*?)```/g, (whole, json) => {
      try {
        charts.push({ type: "__action", spec: JSON.parse(json) });
        return `\u0000CHART${charts.length - 1}\u0000`;
      } catch {
        return whole;
      }
    });
  }
  let src = esc(raw);
  // fenced code
  src = src.replace(/```([\s\S]*?)```/g, (_, code) => `<pre><code>${code.trim()}</code></pre>`);
  const lines = src.split("\n");
  const out = [];
  let list = false;
  for (const raw of lines) {
    const line = raw;
    const heading = line.match(/^(#{1,4})\s+(.*)/);
    const bullet = line.match(/^\s*[-*]\s+(.*)/);
    if (bullet) {
      if (!list) { out.push("<ul>"); list = true; }
      out.push(`<li>${inline(bullet[1])}</li>`);
      continue;
    }
    if (list) { out.push("</ul>"); list = false; }
    if (heading) out.push(`<h${heading[1].length + 2}>${inline(heading[2])}</h${heading[1].length + 2}>`);
    else if (line.trim() === "") out.push("");
    else out.push(`<p>${inline(line)}</p>`);
  }
  if (list) out.push("</ul>");
  return out
    .join("\n")
    .replace(/(?:<p>)?\u0000CHART(\d+)\u0000(?:<\/p>)?/g,
      '<div class="ask-chart" data-chart="$1"></div>');
  function inline(s) {
    return s
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/\*([^*]+)\*/g, "<em>$1</em>")
      .replace(/`([^`]+)`/g, "<code>$1</code>");
  }
}

/* Render lifted chart specs into their placeholders (Ask answers).
   Map charts need the vendored world geometry — fetched once, lazily. */
async function bindCharts(root, charts, askId) {
  if (!charts || !charts.length || typeof buildChartSVG !== "function") return;
  if (charts.some((c) => c && c.type === "map") && !window.__worldLand) {
    try {
      window.__worldLand = await (await fetch("/assets/vendor/world-land-110m.json")).json();
    } catch { /* map renders its own placeholder */ }
  }
  root.querySelectorAll(".ask-chart[data-chart]").forEach((el) => {
    const item = charts[+el.dataset.chart];
    if (item && item.type === "__action") {
      const spec = item.spec || {};
      // "promote": keep a copy of an outside file (a mounted folder,
      // a mail archive) in the record. The button IS the human gate —
      // it performs the save right here, clearly labeled; task/goal
      // actions prefill the Desk instead because they have a form.
      if (spec.type === "promote" && spec.source && spec.path) {
        el.innerHTML = `<button class="btn ask-action-btn">🔖 keep a copy of “${esc(spec.path)}” in my record</button>
          <span class="meta"> — nothing is saved unless you click</span>`;
        el.querySelector(".ask-action-btn").onclick = async () => {
          el.innerHTML = `<span class="meta">saving…</span>`;
          try {
            const res = await apiPost(
              `/v1/sources/virtual/${encodeURIComponent(spec.source)}/promote`,
              { actor: "human:dashboard", path: spec.path });
            el.innerHTML = res.quarantined
              ? `<span class="meta">→ held for your review first (it came from an untrusted place) — see Tend</span>`
              : `<span class="meta">✓ saved to your record${res.existing ? " (it was already there)" : ""}</span>`;
          } catch (err) {
            el.innerHTML = `<span class="meta">✗ couldn't save: ${esc(err.message)}</span>`;
          }
        };
        return;
      }
      // "keep": file this whole answer back into the record as a
      // finding (citations preserved). The click is the human gate.
      if (spec.type === "keep") {
        if (!askId) {
          el.innerHTML = `<span class="meta">— reopen this answer from history to keep it</span>`;
          return;
        }
        el.innerHTML = `<button class="btn ask-action-btn">🔖 keep this answer in my record</button>
          <span class="meta"> — nothing is saved unless you click</span>`;
        el.querySelector(".ask-action-btn").onclick = async () => {
          el.innerHTML = `<span class="meta">saving…</span>`;
          try {
            const res = await apiPost(`/v1/ask/${encodeURIComponent(askId)}/keep`,
              { actor: "human:dashboard", title: spec.title || null });
            el.innerHTML = `<span class="meta">✓ kept as a finding${res.scope === "private" ? " (vaulted — this answer used private material)" : ""}</span>`;
          } catch (err) {
            el.innerHTML = `<span class="meta">✗ couldn't keep it: ${esc(err.message)}</span>`;
          }
        };
        return;
      }
      const label = spec.type === "goal"
        ? `⭑ create goal: ${spec.name || "…"}`
        : `＋ create task: ${spec.description || "…"}`;
      el.innerHTML = `<button class="btn ask-action-btn">${esc(label)}</button>
        <span class="meta"> — opens the Desk prefilled; nothing saves until you do</span>`;
      el.querySelector(".ask-action-btn").onclick = () => {
        sessionStorage.setItem("hl-desk-prefill", JSON.stringify(spec));
        document.getElementById("ask-close")?.click();
        location.hash = "#/desk";
        if (location.hash === "#/desk") route(); // already there: remount
      };
      return;
    }
    el.innerHTML = buildChartSVG(item) || "";
  });
}

/* ————— expandable episode cards ————— */
const bodyCache = new Map();
async function toggleEpisode(card, id) {
  const open = card.querySelector(".episode-body");
  if (open) { open.remove(); card.classList.remove("open"); return; }
  card.classList.add("open");
  const panel = document.createElement("div");
  panel.className = "episode-body";
  panel.innerHTML = `<div class="meta">opening episode…</div>`;
  card.appendChild(panel);
  try {
    if (!bodyCache.has(id)) bodyCache.set(id, await api(`/v1/episode/${id}`));
    const episode = bodyCache.get(id);
    panel.innerHTML = `
      <div class="episode-provenance">
        <span>${esc(episode.source)}/${esc(episode.kind)}</span>
        <span>scope ${esc(episode.scope)} · trust ${esc(episode.trust)}</span>
        <span class="mono">${esc(id)}</span>
        ${lifecycleControls(episode, id)}
      </div>
      <div class="md">${md(episode.body || "(no content)")}</div>`;
    wireLifecycle(panel, episode, id, () => { bodyCache.delete(id); card.classList.add("retired-card"); });
  } catch (err) {
    panel.innerHTML = `<div class="meta">${esc(err.message)}</div>`;
  }
}
window.__toggleEpisode = toggleEpisode;

/* ————— retirement (D.12): retire a memory or a whole source ————— */
const retireHelpers = { api, apiPost, esc };
async function retireFlow(kind, target, label) {
  const { openRetire } = await import("/assets/retire.js");
  return openRetire({ kind, target, label }, retireHelpers);
}
async function undoRetireFlow(id) {
  const { undoRetirement } = await import("/assets/retire.js");
  await undoRetirement(id, retireHelpers);
  window.dispatchEvent(new CustomEvent("hl-retirement-changed"));
}
/* The expanded memory's lifecycle line: "retire…", or what retired /
   replaced it (the detail view is forensic — it opens either way). */
function lifecycleControls(episode, id) {
  if (episode.retired_by) {
    return `<span class="lifebadge retired">retired</span>
      <button class="btn tiny" data-unretire="${esc(episode.retired_by)}">undo</button>`;
  }
  if (episode.superseded_by) return `<span class="lifebadge">replaced by a newer version</span>`;
  return `<button class="btn tiny ghost" data-retire="${esc(id)}" title="take it out of the record — undoable">retire…</button>`;
}
function wireLifecycle(root, episode, id, after) {
  root.querySelector("[data-retire]")?.addEventListener("click", async (event) => {
    event.stopPropagation();
    if (await retireFlow("memory", id, episode.title)) after?.();
  });
  root.querySelector("[data-unretire]")?.addEventListener("click", async (event) => {
    event.stopPropagation();
    const button = event.currentTarget;
    button.disabled = true;
    try {
      await undoRetireFlow(button.dataset.unretire);
      bodyCache.delete(id);
      button.replaceWith(Object.assign(document.createElement("span"), { className: "meta", textContent: "put back" }));
    } catch (err) {
      button.disabled = false;
      button.title = err.message;
    }
  });
}
function bindCards(root) {
  root.querySelectorAll(".card[data-episode]").forEach((card) => {
    card.addEventListener("click", (event) => {
      if (event.target.closest("a")) return; // chips stay links
      toggleEpisode(card, card.dataset.episode);
    });
  });
}

/* ————— header chips + theme ————— */
async function refreshChips() {
  try {
    const stats = await api("/v1/stats");
    if (!window.__version) {
      try { window.__version = (await api("/health")).version; } catch { /* soon */ }
    }
    document.getElementById("chip-episodes").textContent =
      stats.episodes.toLocaleString() + " episodes" +
      (window.__version ? ` · v${window.__version}` : "");
    const tendCount = stats.review_pending + stats.quarantine_pending +
      (stats.propositions_pending || 0) + (stats.opportunities_pending || 0) +
      (stats.lint_pending || 0) + (stats.trips_pending || 0);
    const tendBadge = document.getElementById("tend-badge");
    tendBadge.textContent = tendCount > 99 ? "99+" : (tendCount || "");
    tendBadge.hidden = !tendCount;
    const deskBadge = document.getElementById("desk-badge");
    deskBadge.textContent = stats.tasks_due || "";
    deskBadge.hidden = !stats.tasks_due;
    window.__stats = stats;
  } catch { /* daemon warming up */ }
  await refreshVersion();
}

/* ————— host version + upgrade badge (hamburger) ————— */
window.__hamFlags = window.__hamFlags || { update: false, llm: false };
function paintHamBadge() {
  const badge = document.getElementById("ham-badge");
  if (!badge) return;
  const on = window.__hamFlags.update || window.__hamFlags.llm;
  badge.hidden = !on;
  badge.textContent = on ? "!" : "";
}
async function refreshVersion() {
  const line = document.getElementById("ham-version-line");
  try {
    const v = await api("/v1/version");
    window.__version = v.current;
    window.__update = v;
    if (line) {
      line.textContent = v.update_available && v.latest
        ? `New version (${v.latest}) available →`
        : "✓ Running most recent";
      line.style.color = v.update_available ? "var(--accent)" : "var(--ink-faint)";
    }
    window.__hamFlags.update = !!v.update_available;
    paintHamBadge();
  } catch {
    if (line) line.textContent = `v${window.__version || "?"}`;
  }
}

/* generic POST helper for curation actions */
async function apiPost(path, body) {
  const response = await fetch(API + path, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error((await response.json()).error || response.statusText);
  return response.json();
}
async function apiDelete(path) {
  const response = await fetch(API + path, { method: "DELETE", headers: authHeaders() });
  if (!response.ok) throw new Error((await response.json()).error || response.statusText);
  return response.json();
}

/* — confirm modal: the gate in front of the dashboard's destructive acts.
   Never a browser confirm(); focus lands on Cancel (the safe default). — */
const confirmOverlay = document.getElementById("confirm-overlay");
function confirmDanger({ title, body, action, input }) {
  return new Promise((resolve) => {
    confirmOverlay.querySelector("#confirm-title").textContent = title;
    confirmOverlay.querySelector("#confirm-body").textContent = body;
    const field = confirmOverlay.querySelector("#confirm-input");
    field.hidden = !input;
    field.value = "";
    if (input) field.placeholder = input;
    const ok = confirmOverlay.querySelector("#confirm-ok");
    const cancel = confirmOverlay.querySelector("#confirm-cancel");
    ok.textContent = action;
    confirmOverlay.hidden = false;
    (input ? field : cancel).focus();
    const done = (raw) => {
      const answer = raw && input ? (field.value.trim() || null) : raw;
      confirmOverlay.hidden = true;
      ok.removeEventListener("click", onOk);
      cancel.removeEventListener("click", onCancel);
      document.removeEventListener("keydown", onKey);
      confirmOverlay.removeEventListener("click", onBackdrop);
      resolve(answer);
    };
    const onOk = () => done(true);
    const onCancel = () => done(false);
    const onKey = (e) => { if (e.key === "Escape") done(false); };
    const onBackdrop = (e) => { if (e.target === confirmOverlay) done(false); };
    ok.addEventListener("click", onOk);
    cancel.addEventListener("click", onCancel);
    document.addEventListener("keydown", onKey);
    confirmOverlay.addEventListener("click", onBackdrop);
  });
}
document.getElementById("rewind-btn")?.addEventListener("click", async () => {
  const { play } = await import("/assets/rewind.js");
  play();
});
document.getElementById("theme-toggle").addEventListener("click", () => {
  const root = document.documentElement;
  const next = root.dataset.theme === "night" ? "daylight" : "night";
  root.dataset.theme = next;
  localStorage.setItem("hearthloom-theme", next);
});
const savedTheme = localStorage.getItem("hearthloom-theme");
if (savedTheme) document.documentElement.dataset.theme = savedTheme;

/* ————— dropdown menus (Visualize + hamburger) ————— */
const VIZ_PAGES = ["loom", "orrery", "constellation", "atlas", "pulse", "biography", "ribbons"];
function bindMenu(btnId, dropId) {
  const btn = document.getElementById(btnId);
  const drop = document.getElementById(dropId);
  const wrap = btn.parentElement;
  const set = (open) => {
    wrap.classList.toggle("open", open);
    btn.setAttribute("aria-expanded", String(open));
  };
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    set(!wrap.classList.contains("open"));
  });
  document.addEventListener("click", (e) => {
    if (!wrap.contains(e.target)) set(false);
  });
  drop.addEventListener("click", () => set(false)); // navigating closes
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") set(false);
  });
}
bindMenu("viz-btn", "viz-drop");
bindMenu("ham-btn", "ham-drop");

// Mobile: the left ☰ collapses the primary nav into a drawer (CSS-gated to
// narrow widths; the button is display:none on desktop). Navigating, an
// outside click, or Escape closes it.
(function () {
  const btn = document.getElementById("nav-toggle");
  const nav = document.querySelector(".mainnav");
  if (!btn || !nav) return;
  const set = (open) => {
    nav.classList.toggle("open", open);
    btn.setAttribute("aria-expanded", String(open));
  };
  btn.addEventListener("click", (e) => { e.stopPropagation(); set(!nav.classList.contains("open")); });
  document.addEventListener("click", (e) => { if (!nav.contains(e.target) && e.target !== btn) set(false); });
  nav.addEventListener("click", (e) => { if (e.target.closest("a")) set(false); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape") set(false); });
})();

/* ————— the Notes panel: a slide-out from the left (Ask's mirror) ————— */
{
  const panel = document.getElementById("notes-panel");
  let ready = false;
  let refresh = () => {};
  let openEditor = null;
  let mentionFilter = null;

  async function ensure() {
    if (ready) return;
    ready = true;
    const { buildNoteEditor, paintNoteList } = await import("/assets/notes.js");
    openEditor = buildNoteEditor(panel, () => refresh(), confirmDanger);
    const listEl = document.getElementById("np-list");
    const qEl = document.getElementById("np-q");
    refresh = async () => {
      const filterEl = document.getElementById("np-filter");
      filterEl.hidden = !mentionFilter;
      if (mentionFilter) {
        filterEl.innerHTML = `notes mentioning <b>${esc(mentionFilter)}</b>
          — <a href="#" id="np-clear">all notes</a>`;
        document.getElementById("np-clear").onclick = (e) => {
          e.preventDefault();
          mentionFilter = null;
          refresh();
        };
      }
      const extra = mentionFilter ? `&mentions=${encodeURIComponent(mentionFilter)}` : "";
      const q = qEl.value.trim();
      // paintNoteList builds its own URL; do it manually for the filter
      const data = await api(`/v1/notes?limit=100${q ? `&q=${encodeURIComponent(q)}` : ""}${extra}`);
      const notes = data.notes || [];
      listEl.innerHTML = notes.length ? notes.map((n) => `
        <div class="note-row" data-note="${esc(n.id)}">
          <span class="note-title">${noteTitle(n.title)}
            ${n.kind === "template" ? `<span class="task-tag">template</span>` : ""}</span>
          <span class="meta">${n.updated_at ? esc(n.updated_at.slice(0, 10)) : "—"}${n.scope === "private" ? " · 🔒" : ""}</span>
        </div>`).join("")
        : `<p class="meta" style="color:var(--ink-faint); padding:0 var(--s4)">no notes${q || mentionFilter ? " match" : " yet"}.</p>`;
      listEl.querySelectorAll(".note-row").forEach((row) => {
        row.onclick = () => openEditor(row.dataset.note, null);
      });
    };
    qEl.addEventListener("input", refresh);
    document.getElementById("np-new").onclick = () => openEditor(null, null);
    await refresh();
  }

  let openedAt = 0; // the click that OPENED the panel must not close it
  // The panel is CROSS-CONTEXT only (Jon 2026-07-10): nav "Notes" goes
  // to the full page; this slide-out serves "notes mentioning X" lists
  // from Relationships/dossiers, keeping your place.
  const open = async (filter) => {
    mentionFilter = filter || null;
    openedAt = Date.now();
    panel.classList.add("open");
    await ensure();
    refresh();
  };
  const close = () => panel.classList.remove("open");
  // click-outside closes, same as the × (overlays exempt)
  document.addEventListener("click", (e) => {
    if (!panel.classList.contains("open") || Date.now() - openedAt < 350) return;
    if (e.target.closest("#notes-panel, .note-overlay, .confirm-overlay")) return;
    close();
  });
  document.getElementById("notes-panel-close").addEventListener("click", close);
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && panel.classList.contains("open")
        && !document.querySelector(".note-overlay:not([hidden])")) close();
  });
  // other views (Relationships rows, dossiers) open it filtered
  window.__openNotesPanel = open;
  // "+ note about them": the editor pulls out on the LEFT, prefilled
  // with the linked mention — context preserved behind it.
  window.__newNoteFor = async (name) => {
    await ensure();
    openEditor(null, { body: `@[${name}] ` });
  };
  // a note clicked from ANY other context: the left pull-out editor,
  // never a navigation — you don't lose your place.
  window.__openNoteById = async (id) => {
    await ensure();
    openEditor(id, null);
  };
  // a [[wiki-link]] to a title that doesn't exist yet: a fresh note,
  // pre-titled — the red-link flow.
  window.__openNotePrefilled = async (prefill) => {
    await ensure();
    openEditor(null, prefill);
  };
}

/* ————— plugin chrome (docs/plugins.md) —————
   Enabled plugins contribute nav entries, Visualize items, hamburger
   items, and Desk widgets via their manifest. Their PAGES run inside a
   sandboxed iframe — plugin JS never executes in this origin, so its
   PAT scopes (not your session) are the ceiling. */
window.__pluginUi = { nav: [], visualize: [], hamburger: [], desk: [] };
async function loadPluginUi() {
  try {
    window.__pluginUi = await api("/v1/plugins/ui");
  } catch { return; } // daemon warming up / older daemon: no chrome
  const ui = window.__pluginUi;
  const link = (item, hint) => {
    const a = document.createElement("a");
    a.href = `#/plugin/${item.plugin}/${encodeURIComponent(item.page)}`;
    a.dataset.nav = `plugin-${item.plugin}`;
    a.textContent = item.label;
    if (hint) {
      const s = document.createElement("span");
      s.className = "navmenu-hint";
      s.textContent = hint;
      a.append(" ", s);
    }
    return a;
  };
  // top-nav: after Search, before the Visualize menu
  const vizMenu = document.getElementById("viz-menu");
  (ui.nav || []).forEach((item) => {
    if (document.querySelector(`.mainnav a[data-nav="plugin-${CSS.escape(item.plugin)}"]`)) return;
    vizMenu.parentNode.insertBefore(link(item), vizMenu);
  });
  const vizDrop = document.getElementById("viz-drop");
  (ui.visualize || []).forEach((item) => vizDrop.append(link(item, `from ${item.plugin}`)));
  const hamDrop = document.querySelector(".hammenu-drop");
  const logout = document.getElementById("nav-logout");
  (ui.hamburger || []).forEach((item) =>
    hamDrop.insertBefore(link(item, `from ${item.plugin}`), logout));
}

/// A plugin page: sandboxed iframe + a scoped token handed via
/// postMessage (with the CSS palette so it can match the theme).
async function renderUpgrade() {
  setNav("upgrade");
  view.innerHTML = `<div class="loading">checking the release channel…</div>`;
  let v;
  try { v = await api("/v1/version"); }
  catch { view.innerHTML = `<div class="empty">couldn't reach the release channel.</div>`; return; }
  const notes = v.notes_url
    ? `<p class="meta"><a href="${esc(v.notes_url)}" target="_blank" rel="noopener">what's new →</a></p>` : "";
  if (!v.update_available) {
    view.innerHTML = `
      <span class="eyebrow">software</span>
      <h1 class="pagehead">Hearthloom is up to date</h1>
      <p class="subhead">You're running <b>v${esc(v.current)}</b>${v.latest ? ` — the latest release (v${esc(v.latest)})` : ""}.</p>
      ${notes}`;
    return;
  }
  view.innerHTML = `
    <span class="eyebrow">software</span>
    <h1 class="pagehead">New version available</h1>
    <p class="subhead">You're on <b>v${esc(v.current)}</b>; <b>v${esc(v.latest)}</b> is available.</p>
    ${notes}
    <div class="card" style="margin-top:var(--s3)">
      ${v.can_self_update
        ? `<p class="imp-what">This node can update itself: it downloads the published build,
             ${v.signature_pinned ? "verifies its release signature and hash" : "checks it against the release's published hash"},
             replaces the binary, and restarts.</p>
           <div class="tend-actions"><button class="btn" id="up-go">Upgrade to v${esc(v.latest)}</button>
             <span class="meta" id="up-note"></span></div>`
        : `<p class="imp-what">${v.target && v.target !== "unknown"
             ? "This node builds from source or runs as a hosted instance, so it doesn't self-update. "
             : "No official build is published for this platform. "}
             ${v.hosted
               ? "Hosted instances upgrade when their hosting provider updates their image."
               : "Pull the repository and rebuild."}</p>`}
    </div>`;
  const go = document.getElementById("up-go");
  if (go) {
    go.onclick = async () => {
      const note = document.getElementById("up-note");
      go.disabled = true; note.textContent = "downloading and verifying…";
      try {
        const res = await apiPost("/v1/upgrade", {});
        note.textContent = `upgrading to v${esc(res.to)} — the daemon is restarting. This page will refresh shortly.`;
        setTimeout(async () => {
          for (let i = 0; i < 40; i++) {
            await new Promise((r) => setTimeout(r, 1500));
            try {
              const h = await (await fetch("/health")).json();
              if (h.version === res.to) { window.__version = h.version; renderUpgrade(); return; }
            } catch { /* still restarting */ }
          }
        }, 1500);
      } catch (e) {
        go.disabled = false; note.textContent = e.message;
      }
    };
  }
}

async function renderPluginPage(arg) {
  const slash = arg.indexOf("/");
  const name = slash < 0 ? arg : arg.slice(0, slash);
  const page = slash < 0 ? "index.html" : arg.slice(slash + 1);
  setNav(`plugin-${name}`);
  const ui = window.__pluginUi || {};
  const entry = ["nav", "visualize", "hamburger", "card"]
    .flatMap((k) => ui[k] || [])
    .find((e) => e.plugin === name && e.page === page);
  const wantsDial = !!entry?.dial;
  const dISO = (n) => new Date(n).toISOString().slice(0, 10);
  let span = { from: dISO(Date.now() - 30 * 86400000), to: dISO(Date.now()) };
  const src = `/plugins/${encodeURIComponent(name)}/ui/${page}`;
  const label = entry?.label || name;
  // The frame stays in layout (so the page measures real widths) but
  // invisible until the page shows it is alive; the spinner holds the
  // screen meanwhile. Nothing here may leave a blank or a browser error
  // page in the frame — that is what "failed to connect" was.
  view.innerHTML = `${wantsDial ? `<div id="plugin-dial" style="margin-bottom:var(--s2)"></div>` : ""}
    <div class="loading" id="plugin-wait"><span class="spinner"></span>
      <span id="plugin-wait-text">opening ${esc(label)}…</span></div>
    <iframe class="plugin-frame" sandbox="allow-scripts"
    style="width:100%;height:420px;border:0;display:block;opacity:0;transition:opacity .2s"></iframe>`;
  const frame = view.querySelector("iframe");
  const waitText = document.getElementById("plugin-wait-text");
  const here = location.hash;
  const left = () => location.hash !== here || !frame.isConnected;
  let shown = false;
  const show = () => {
    if (shown) return;
    shown = true;
    document.getElementById("plugin-wait")?.remove();
    frame.style.opacity = "1";
  };
  // The page flows in the dashboard's own scroll — the ember reports
  // its content height and we size the iframe to it (no inner scrollbar).
  // Its first report doubles as the "I'm alive" signal.
  const onHeight = (ev) => {
    if (ev.source !== frame.contentWindow || ev.data?.type !== "hearthloom-height") return;
    frame.style.height = Math.max(160, Math.min(20000, Number(ev.data.px) || 420)) + "px";
    show();
  };
  window.addEventListener("message", onHeight);
  window.addEventListener("hashchange", () => window.removeEventListener("message", onHeight), { once: true });
  // OAuth sign-in: the sandbox has no popups, so the page asks the shell
  // to open the provider's sign-in. The shell opens ONLY the address the
  // daemon holds for this ember (what its pending sign-in registered) —
  // never a URL out of the message.
  const onOAuth = async (ev) => {
    if (ev.source !== frame.contentWindow || ev.data?.type !== "hearthloom-oauth") return;
    let url = null;
    try { url = (await api(`/v1/plugins/${encodeURIComponent(name)}/oauth`)).authorize_url; } catch { /* none pending */ }
    if (!url || !/^https:\/\//.test(url)) return;
    const win = window.open(url, "_blank", "noopener");
    if (!win) {
      // blocked: offer the same address as a plain link above the frame
      let bar = document.getElementById("plugin-oauth-bar");
      if (!bar) {
        bar = document.createElement("p");
        bar.id = "plugin-oauth-bar";
        bar.className = "meta";
        frame.before(bar);
      }
      bar.innerHTML = `Your browser blocked the sign-in window. <a href="${esc(url)}" target="_blank" rel="noopener">Open the sign-in page →</a>`;
    }
  };
  window.addEventListener("message", onOAuth);
  window.addEventListener("hashchange", () => window.removeEventListener("message", onOAuth), { once: true });

  // 1 · Wait until the page can actually be served. Right after a daemon
  // restart (or while it is busy) the request fails; loading the frame
  // then would park the browser's own error page in it until a manual
  // reload. Probe from the shell and retry with backoff instead.
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  let reachable = false, missing = false;
  for (let attempt = 0; attempt < 14 && !left(); attempt++) {
    try {
      const probe = await fetch(src, { cache: "no-store" });
      if (probe.ok) { reachable = true; break; }
      if (probe.status === 404) { missing = true; break; }
    } catch { /* daemon not answering yet */ }
    if (attempt >= 1) waitText.textContent = "waiting for Hearthloom to answer…";
    await sleep(Math.min(400 * (attempt + 1), 3000));
  }
  if (left()) return;
  if (!reachable) {
    view.innerHTML = `<div class="empty">${missing
      ? `${esc(label)} isn't available — the ember may be disabled or mid-update.`
      : `Hearthloom didn't answer in time.`}
      <div style="margin-top:var(--s3)"><button class="btn" id="plugin-retry">try again</button></div></div>`;
    document.getElementById("plugin-retry").onclick = () => renderPluginPage(arg);
    return;
  }

  // 2 · The scoped token. A disabled plugin or older daemon answers with
  // an error — the page then simply gets no token (unchanged behavior).
  let token = null;
  try {
    token = (await apiPost(`/v1/plugins/${encodeURIComponent(name)}/ui-token`, {})).token;
  } catch { /* no token */ }
  if (left()) return;

  // 3 · Only NOW point the frame at the page, with the load listener
  // already attached. (It used to be attached after the token await — a
  // page that finished loading first never got its handshake and sat on
  // "loading…" forever.)
  frame.addEventListener("load", () => {
    const palette = {};
    const styles = getComputedStyle(document.documentElement);
    for (const v of ["--bg", "--ink", "--ink-faint", "--accent", "--card", "--line",
      "--chart-1", "--chart-2", "--chart-3", "--chart-4",
      "--chart-5", "--chart-6", "--chart-7", "--chart-8", "--chart-heat"]) {
      palette[v] = styles.getPropertyValue(v).trim();
    }
    frame.contentWindow.postMessage(
      { type: "hearthloom", token, base: location.origin, palette },
      "*" // sandboxed frames have an opaque origin; "*" is the only address
    );
    if (wantsDial) {
      frame.contentWindow.postMessage({ type: "hearthloom-timespan", ...span }, "*");
    }
    // Older embers never report a height — don't hold the spinner on them.
    setTimeout(show, 1500);
  });
  frame.src = src;
  // The ember asked for time scoping: mount the REAL Dial in the shell
  // (never inside the sandbox) and stream the window in.
  if (wantsDial) {
    const { mountDial } = await import("/assets/dial.js");
    mountDial(document.getElementById("plugin-dial"), {
      from: span.from,
      to: span.to,
      compact: true,
      filters: () => ({}),
      onWindow: (from, to) => {
        span = { from, to };
        frame.contentWindow.postMessage({ type: "hearthloom-timespan", from, to }, "*");
      },
    });
  }
}

/* ————— router ————— */
function setNav(name) {
  document.querySelectorAll(".mainnav a, .hammenu-drop a").forEach((a) =>
    a.classList.toggle("active", a.dataset.nav === name));
  // The Visualize button lights up when any of its pages is active.
  document.getElementById("viz-btn").classList.toggle("active", VIZ_PAGES.includes(name));
}
async function route() {
  // The Desk is home — the dominant surface, Jon's first go-to.
  const hash = location.hash || "#/desk";
  const [, page, ...rest] = hash.split("/");
  const arg = decodeURIComponent(rest.join("/") || "");
  view.className = "view";
  try {
    if (page === "desk") return renderDesk();
    if (page === "today") return renderDay(localToday());
    // The date comes from the URL: only YYYY-MM-DD reaches the Day view.
    if (page === "day") return renderDay(/^\d{4}-\d{2}-\d{2}$/.test(arg) ? arg : localToday());
    if (page === "search") return renderSearch(arg);
    if (page === "browse") return renderBrowse(arg || "episodes");
    if (page === "privacy") return renderPrivacy();
    if (page === "entity") return renderEntity(arg);
    if (page === "constellation") return renderConstellation(arg);
    if (page === "atlas") return renderAtlas();
    if (page === "biography") return renderBiography();
    if (page === "pulse") return renderPulse();
    if (page === "loom") return renderLoom();
    if (page === "notes") return renderNotes();
    if (page === "relationships") return renderRelationships();
    if (page === "orrery") return renderOrrery();
    if (page === "analytics") return renderAnalytics();
    if (page === "integrations") return renderIntegrations();
    if (page === "apikeys") return renderApiKeys();
    if (page === "peering") return renderPeering();
    if (page === "network") return renderNetwork();
    if (page === "monitor") return renderMonitor();
    if (page === "spend") return renderSpend();
    if (page === "agents") return renderAgents();
    if (page === "about") return renderAbout();
    if (page === "settings") return renderSettings();
    if (page === "tend") return renderTend();
    if (page === "ribbons") return renderRibbons();
    if (page === "welcome") return renderWelcome();
    if (page === "upgrade") return renderUpgrade();
    if (page === "plugin") return renderPluginPage(arg);
    return renderDesk();
  } catch (err) {
    view.innerHTML = `<div class="empty">${esc(err.message)}</div>`;
  }
}
window.addEventListener("hashchange", route);

/* ————— onboarding completion nudges: a leave-warning when abandoning
   #/welcome mid-setup, and a dismissable hero on every page until the
   wizard is finished (or the user says stop). State is SERVER-side
   ("", done, dismissed) so it follows the record. ————— */
let onboardingState = "done"; // assume done until the server says otherwise
async function loadOnboarding() {
  try {
    onboardingState = (await api("/v1/onboarding")).state ?? "";
  } catch { /* daemon warming up */ }
}
async function setOnboarding(state) {
  onboardingState = state;
  paintOnboardingHero();
  try { await apiPost("/v1/onboarding", { state }); } catch { /* best-effort */ }
}
window.setOnboarding = setOnboarding; // settings.js drives New User Mode

/* ————— friend avatars (v46 sharing): a deterministic ember-hued
   initials disc for every connected brain — profile emoji/color
   override when their card carries one. Click opens their person
   modal (the connection always has one — seeded on connect). ————— */
/// "Jon Hale" -> "JH"; a single word -> its first two letters.
const nameInitials = (name) => {
  const words = String(name).trim().split(/\s+/).filter(Boolean);
  if (!words.length) return "?";
  return words.length === 1
    ? words[0].slice(0, 2)
    : (words[0][0] + words[words.length - 1][0]);
};
window.friendAvatar = (name, opts = {}) => {
  // A share addressed to a group is a labelled chip, not a person.
  if (String(name).startsWith("group:")) {
    return `<span class="task-tag plain-tag fravatar-group" title="${esc(opts.title || "shared with a group")}">◍ ${esc(opts.label || "group")}</span>`;
  }
  const hue = [...String(name)].reduce((h, c) => ((h * 31 + c.charCodeAt(0)) >>> 0), 7) % 360;
  const bg = opts.color || `hsl(${hue} 42% 34%)`;
  const av = opts.avatar || "";
  // image > emoji > INITIALS (no avatar supplied means their initials)
  const face = av && (av.startsWith("data:image/") || av.startsWith("https://"))
    ? `<img src="${esc(av)}" alt="">`
    : (opts.emoji || esc(nameInitials(opts.displayName || name)));
  const tip = esc(opts.title || `${name} — connected brain · click for their card`);
  return `<span class="fravatar${opts.self ? " fravatar-self" : ""}" style="background:${bg}" data-friend="${esc(name)}"
    ${opts.entity ? `data-entity="${esc(opts.entity)}"` : ""} title="${tip}">${face}</span>`;
};
// My own avatar beside Settings in the hamburger — painted at boot and
// after every card save.
window.paintHamAvatar = async () => {
  const btn = document.getElementById("ham-btn");
  if (!btn) return;
  const [card, me] = await Promise.all([
    api("/v1/network/mycard").catch(() => ({})),
    api("/v1/auth/me").catch(() => ({})),
  ]);
  // your initials, never "ME": card name first, else the sign-in email's
  // localpart ("jon.hale" → "Jon Hale" → JH)
  const fallback = (me.user || "").split("@")[0].replace(/[._-]+/g, " ").trim();
  const display = (card.name || "").trim() || fallback || "me";
  const badge = btn.querySelector(".navbadge")?.outerHTML || "";
  btn.innerHTML = window.friendAvatar(display, {
    self: true, avatar: card.avatar, emoji: card.emoji, color: card.color,
    displayName: display, title: "menu — your card lives in Settings",
  }) + badge;
};
window.paintHamAvatar();

// The profile-card editor — mounted on BOTH Settings ("who you are")
// and Network ("my calling card"): one implementation, one endpoint.
// The card peer-syncs (LWW) and is what connected friends see.
window.mountProfileCard = async (host) => {
  const card = await api("/v1/network/mycard").catch(() => ({}));
  host.innerHTML = `
    <div class="meta" style="margin-bottom:var(--s2)">Your card — saved here, synced to your
      other node, and shown to connected friends' brains (it seeds your Person entry in
      their record):</div>
    <div class="searchbar" style="flex-wrap:wrap; align-items:center">
      ${window.friendAvatar(card.name || "me", { avatar: card.avatar, emoji: card.emoji,
        color: card.color, title: "your avatar" })}
      <input id="pc-name" placeholder="your name" value="${esc(card.name || "")}" style="flex:0.8">
      <input id="pc-emails" placeholder="emails, comma-separated"
        value="${esc((card.emails || []).join(", "))}" style="flex:1">
      <input id="pc-emoji" placeholder="emoji" value="${esc(card.emoji || "")}" style="width:64px">
    </div>
    <div class="searchbar" style="flex-wrap:wrap; align-items:center; margin-top:var(--s2)">
      <input id="pc-bio" placeholder="one-line bio" value="${esc(card.bio || "")}" style="flex:1">
      <input id="pc-avatar" placeholder="avatar https:// URL (or upload →)"
        value="${esc((card.avatar || "").startsWith("data:") ? "" : card.avatar || "")}" style="flex:1">
      <button class="btn btn-quiet" id="pc-upload">${(card.avatar || "").startsWith("data:") ? "replace upload" : "upload…"}</button>
      <button class="btn" id="pc-save">save</button>
    </div>
    <p class="meta" id="pc-note" hidden></p>`;
  let uploadedAvatar = (card.avatar || "").startsWith("data:") ? card.avatar : "";
  host.querySelector("#pc-upload").onclick = () => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = "image/png,image/jpeg,image/webp,image/gif";
    input.onchange = () => {
      const file = input.files?.[0];
      const note = host.querySelector("#pc-note");
      if (!file) return;
      if (file.size > 1_000_000) {
        note.hidden = false;
        note.textContent = "✗ keep the avatar under 1MB (a small square works best)";
        return;
      }
      const reader = new FileReader();
      reader.onload = () => {
        uploadedAvatar = reader.result;
        host.querySelector("#pc-avatar").value = "";
        note.hidden = false;
        note.textContent = "image ready — click save";
      };
      reader.readAsDataURL(file);
    };
    input.click();
  };
  host.querySelector("#pc-save").onclick = async () => {
    const note = host.querySelector("#pc-note");
    note.hidden = false; note.textContent = "saving…";
    const url = host.querySelector("#pc-avatar").value.trim();
    const res = await apiPost("/v1/network/mycard", {
      actor: "human:dashboard",
      name: host.querySelector("#pc-name").value,
      emails: host.querySelector("#pc-emails").value.split(",").map((e) => e.trim()).filter(Boolean),
      emoji: host.querySelector("#pc-emoji").value,
      bio: host.querySelector("#pc-bio").value,
      avatar: url || uploadedAvatar || "",
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    note.textContent = "✓ saved — it syncs to your other node and shows to connected friends";
    window.paintHamAvatar?.();
    window.mountProfileCard(host);
  };
};
document.addEventListener("click", (e) => {
  const av = e.target.closest?.(".fravatar");
  if (!av || av.classList.contains("fravatar-self")) return; // self = the parent link wins
  e.preventDefault();
  e.stopPropagation();
  openPersonModal(av.dataset.entity || av.dataset.friend);
});
function paintOnboardingHero() {
  const existing = document.getElementById("onboard-hero");
  // "" = fresh record (grandfather big established ones); "active" =
  // New User Mode, which nags regardless of record size
  // Only judge "fresh" once stats has actually LOADED — right after a
  // daemon restart the cold /v1/stats can lag this paint by seconds,
  // and an undefined count must never read as an empty record (the
  // 91k-episode "isn't set up yet" banner, 2026-07-21).
  const fresh = onboardingState === "" && window.__stats
    && (window.__stats.episodes ?? 0) < 1000;
  const show = (fresh || onboardingState === "active")
    && location.hash !== "#/welcome"
    && !location.hash.startsWith("#/welcome?");
  if (!show) { existing?.remove(); return; }
  if (existing) return;
  const hero = document.createElement("div");
  hero.id = "onboard-hero";
  hero.innerHTML = `
    <span>🔥 Your second brain isn't fully set up yet — a few minutes of
      kindling makes everything else better.</span>
    <a class="btn" href="#/welcome">finish setting up →</a>
    <button class="btn btn-quiet" id="onboard-stop">stop reminding me</button>`;
  view.parentNode.insertBefore(hero, view);
  hero.querySelector("#onboard-stop").onclick = () => setOnboarding("dismissed");
}
let prevHash = location.hash;
let leaveWarned = false;
window.addEventListener("hashchange", () => {
  const from = prevHash;
  prevHash = location.hash;
  // abandoning the wizard mid-setup gets ONE gentle intercept
  if (from === "#/welcome" && (onboardingState === "" || onboardingState === "active")
      && !leaveWarned && location.hash !== "#/welcome") {
    leaveWarned = true;
    const overlay = document.createElement("div");
    overlay.className = "note-overlay";
    overlay.style.zIndex = "290";
    overlay.innerHTML = `
      <div class="person-card" style="max-width:420px">
        <h2 class="pm-name">Leave setup?</h2>
        <p class="meta">It's usually easiest to finish setting up now — the record
          sorts itself best when it knows who you are and has its first sources.
          You can always come back from the reminder banner or Settings.</p>
        <div class="tend-actions">
          <button class="btn" id="ob-stay">go back to setup</button>
          <button class="btn btn-quiet" id="ob-leave">continue anyway</button>
        </div>
      </div>`;
    document.body.appendChild(overlay);
    overlay.querySelector("#ob-stay").onclick = () => {
      overlay.remove();
      location.hash = "#/welcome";
    };
    overlay.querySelector("#ob-leave").onclick = () => overlay.remove();
  }
  paintOnboardingHero();
});

/* ————— atlas (lazy-loaded like the constellation) ————— */
let atlasUnmount = null;
async function renderAtlas() {
  setNav("atlas");
  if (atlasUnmount) { atlasUnmount(); atlasUnmount = null; }
  view.innerHTML = `<div class="loading">unrolling the atlas…</div>`;
  const { mount } = await import("/assets/atlas.js");
  view.innerHTML = "";
  atlasUnmount = await mount(view);
}

let notesUnmount = null;
async function renderNotes() {
  setNav("notes");
  if (notesUnmount) { notesUnmount(); notesUnmount = null; }
  view.innerHTML = `<div class="loading">opening the drawer…</div>`;
  const { mount } = await import("/assets/notes.js");
  view.innerHTML = "";
  notesUnmount = await mount(view, { confirmDanger });
}

let relationshipsUnmount = null;
async function renderRelationships() {
  setNav("relationships");
  if (relationshipsUnmount) { relationshipsUnmount(); relationshipsUnmount = null; }
  view.innerHTML = `<div class="loading">gathering the circle…</div>`;
  const { mount } = await import("/assets/relationships.js");
  view.innerHTML = "";
  relationshipsUnmount = await mount(view);
}

let orreryUnmount = null;
async function renderOrrery() {
  setNav("orrery");
  view.className = "view wide";
  if (orreryUnmount) { orreryUnmount(); orreryUnmount = null; }
  view.innerHTML = `<div class="loading">setting the spheres in motion…</div>`;
  const { mount } = await import("/assets/orrery.js");
  view.innerHTML = "";
  orreryUnmount = await mount(view);
}

let loomUnmount = null;
async function renderLoom() {
  setNav("loom");
  view.className = "view wide";
  if (loomUnmount) { loomUnmount(); loomUnmount = null; }
  view.innerHTML = `<div class="loading">threading the loom…</div>`;
  const { mount } = await import("/assets/loom.js");
  view.innerHTML = "";
  loomUnmount = await mount(view);
}

/* ————— analytics: what the record is made of ————— */
async function renderAnalytics() {
  setNav("");
  view.innerHTML = `
    <span class="eyebrow">the record</span>
    <h1 class="pagehead">What the record is made of</h1>
    <p class="subhead">Every number below is the substrate itself — one SQLite file,
      counted honestly (a locked vault counts nothing it hides).</p>
    <div id="an-tiles" class="an-tiles"></div>
    <div class="an-grid" id="an-grid"><div class="loading">counting…</div></div>`;

  const [stats, years] = await Promise.all([
    api("/v1/stats"),
    api("/v1/timeline/histogram?from=0000-01-01&to=9999-12-31&bucket=year"),
  ]);

  // Epistemics tiles: observed / declared / inferred + the graph.
  const derivations = Object.fromEntries(stats.derivations || []);
  const tile = (n, label, hint) => `
    <div class="an-tile" title="${esc(hint)}">
      <b>${(n || 0).toLocaleString()}</b><span>${esc(label)}</span>
    </div>`;
  document.getElementById("an-tiles").innerHTML =
    tile(stats.episodes, "episodes", "the append-only record") +
    tile(derivations.observed, "observed", "absorbed from data flows — mail, git, health, photos…") +
    tile(derivations.declared, "declared", "you stated it: tasks, notes") +
    tile(derivations.inferred, "inferred", "Hearthloom's own conclusions — digests, chapters, cascades; always citing evidence") +
    tile(stats.entities, "entities", "people, places, projects, orgs") +
    tile(stats.edges, "edges", "current relations in the graph") +
    tile(stats.mutations, "mutations", "every write ever made, attributed");

  const chart = (spec) => `<div class="an-chart">${window.buildChartSVG(spec)}</div>`;
  const top = (pairs, n) => {
    const list = (pairs || []).slice(0, n);
    const rest = (pairs || []).slice(n).reduce((a, [, v]) => a + v, 0);
    if (rest > 0) list.push(["everything else", rest]);
    return list;
  };
  document.getElementById("an-grid").innerHTML =
    chart({
      type: "bar",
      title: "By source — where the record comes from",
      xKey: "source",
      series: [{ key: "episodes", label: "episodes" }],
      data: top(stats.sources, 11).map(([source, n]) => ({ source, episodes: n })),
    }) +
    chart({
      type: "donut",
      title: "By kind — what the moments are",
      nameKey: "kind",
      valueKey: "n",
      data: top(stats.kinds, 9).map(([kind, n]) => ({ kind, n })),
    }) +
    chart({
      type: "area",
      title: "By year — the record deepens",
      xKey: "year",
      series: [{ key: "episodes", label: "episodes" }],
      // clamp to the current year — a few future-dated datapoints
      // (calendar invites) aren't worth a 2027 column
      data: (years.buckets || [])
        .filter((b) => b.n >= 15 && b.bucket <= String(new Date().getFullYear()))
        .map((b) => ({ year: b.bucket, episodes: b.n })),
    }) +
    chart({
      type: "bar",
      title: "By scope — which life it belongs to",
      xKey: "scope",
      series: [{ key: "episodes", label: "episodes" }],
      data: (stats.scopes || []).map(([scope, n]) => ({ scope, episodes: n })),
    });
}

let deskUnmount = null;
async function renderDesk() {
  setNav("desk");
  if (deskUnmount) { deskUnmount(); deskUnmount = null; }
  view.innerHTML = `<div class="loading">clearing the desk…</div>`;
  const { mount } = await import("/assets/desk.js");
  view.innerHTML = "";
  deskUnmount = await mount(view, { api, apiPost, confirmDanger, md });
}

async function renderMonitor() {
  setNav("monitor");
  view.innerHTML = `<div class="loading">reading the gauges…</div>`;
  const { mount } = await import("/assets/monitor.js");
  view.innerHTML = "";
  await mount(view, { api });
}

async function renderAgents() {
  setNav("agents");
  view.innerHTML = `<div class="loading"><span class="spinner"></span> finding your agents…</div>`;
  const { mount } = await import("/assets/agents.js");
  await mount(view, { api, apiPost, confirmDanger });
}

async function renderSpend() {
  setNav("spend");
  view.innerHTML = `<div class="loading"><span class="spinner"></span> opening the ledger…</div>`;
  const { mount } = await import("/assets/spend.js");
  await mount(view, { api, apiPost, openTrace: openAskTrace });
}

async function renderSettings() {
  setNav("settings");
  view.innerHTML = `<div class="loading">finding the dials…</div>`;
  const { mount } = await import("/assets/settings.js");
  view.innerHTML = "";
  await mount(view, { api, apiPost });
}

async function renderNetwork() {
  setNav("network");
  view.innerHTML = `<div class="loading">listening for other hearths…</div>`;
  const { mount } = await import("/assets/network.js");
  view.innerHTML = "";
  await mount(view, { api, apiPost, confirmDanger });
}

let peeringUnmount = null;
async function renderPeering() {
  setNav("peering");
  if (peeringUnmount) { peeringUnmount(); peeringUnmount = null; }
  view.innerHTML = `<div class="loading">reaching for the other hearth…</div>`;
  const { mount } = await import("/assets/peering.js");
  view.innerHTML = "";
  peeringUnmount = await mount(view, { api, apiPost, confirmDanger });
}

async function renderIntegrations() {
  setNav("integrations");
  view.innerHTML = `<div class="loading">opening the connections drawer…</div>`;
  const { mount } = await import("/assets/integrations.js");
  view.innerHTML = "";
  await mount(view, { api, apiPost, confirmDanger });
}

/* ————— API keys: scoped tokens for third-party integrations ————— */
async function renderApiKeys() {
  setNav("apikeys");
  view.innerHTML = `<div class="loading"><span class="spinner"></span> counting the keys…</div>`;
  const { mount } = await import("/assets/keys.js");
  await mount(view, { api, apiPost });
}

async function renderAbout() {
  setNav("about");
  view.innerHTML = `<div class="loading"><span class="spinner"></span> gathering what it knows…</div>`;
  const { mount } = await import("/assets/about.js");
  await mount(view, { api, apiPost });
}

let pulseUnmount = null;
async function renderPulse() {
  setNav("pulse");
  if (pulseUnmount) { pulseUnmount(); pulseUnmount = null; }
  view.innerHTML = `<div class="loading">warming the trace…</div>`;
  const { mount } = await import("/assets/pulse.js");
  view.innerHTML = "";
  pulseUnmount = await mount(view);
}

let bioUnmount = null;
async function renderBiography() {
  setNav("biography");
  if (bioUnmount) { bioUnmount(); bioUnmount = null; }
  view.innerHTML = `<div class="loading">opening the book…</div>`;
  const { mount } = await import("/assets/biography.js");
  view.innerHTML = "";
  bioUnmount = await mount(view);
}

/* ————— day view ————— */
/* If this day was opened from an Atlas visit, offer the way back. */
function visitChip(date) {
  try {
    const v = JSON.parse(sessionStorage.getItem("hl-visit") || "null");
    if (v && date >= v.from && date <= v.to) {
      return `<a class="place-chip visit-context" href="#/atlas"
        title="back to the Atlas">◀ part of: ${esc(v.label)} · ${esc(v.from)} → ${esc(v.to)}</a>`;
    }
  } catch { /* no visit context */ }
  return "";
}

let dayDial = null; // persists across day changes — never remounted

async function renderDay(date) {
  setNav("day");
  const existingShell = document.getElementById("day-content") && dayDial;
  if (!existingShell) {
    dayDial = null; // stale handle from a torn-down view
    view.innerHTML = `
      <span class="eyebrow">day view</span>
      <div id="day-dial"></div>
      <div id="day-content"><div class="loading">reading the day…</div></div>`;
    // The dial mounts ONCE per visit to Day view; spinning it swaps only
    // the content below.
    const spanDays = Number(sessionStorage.getItem("hl-day-dial-span")) || 60;
    const d0 = Date.parse(`${date}T12:00:00Z`) / 86400000;
    const dISO = (n) => new Date(n * 86400000).toISOString().slice(0, 10);
    const { mountDial } = await import("/assets/dial.js");
    dayDial = mountDial(document.getElementById("day-dial"), {
      from: dISO(d0 - spanDays / 2),
      to: dISO(d0 + spanDays / 2),
      filters: () => ({}),
      onWindow: (from, to) => {
        const a = Date.parse(`${from}T12:00:00Z`) / 86400000;
        const b = Date.parse(`${to}T12:00:00Z`) / 86400000;
        sessionStorage.setItem("hl-day-dial-span", String(Math.round(b - a)));
        const mid = dISO((a + b) / 2);
        if (`#/day/${mid}` !== location.hash) location.hash = `#/day/${mid}`;
      },
      onPick: (bucket) => {
        const day = bucket.length === 10 ? bucket
          : bucket.length === 7 ? `${bucket}-15` : `${bucket}-07-01`;
        location.hash = `#/day/${day}`;
      },
    });
    window.addEventListener("hashchange", () => {
      if (!location.hash.startsWith("#/day/") && !location.hash.startsWith("#/today")) {
        dayDial?.destroy();
        dayDial = null;
      }
    });
  }
  const content = document.getElementById("day-content");
  content.classList.add("day-loading");
  const day = await api(`/v1/recall?on=${encodeURIComponent(date)}&detail=full`);
  content.classList.remove("day-loading");
  // order by wall-clock as displayed: episode's own offset, else browser's
  const browserOffset = -new Date().getTimezoneOffset();
  const wall = (e) =>
    new Date(e.event_time || 0).getTime() +
    (e.tz_offset_minutes ?? browserOffset) * 60_000;
  const episodes = (day.episodes || []).slice().sort((a, b) => wall(a) - wall(b));

  const places = [...new Set(
    episodes.flatMap((e) => (e.entities || [])
      .filter(([, type, role]) => role === "visited")
      .map(([name]) => name))
  )];

  // source filters: sticky across days, only offered for sources present
  const counts = {};
  for (const e of episodes) counts[e.source] = (counts[e.source] || 0) + 1;
  const present = Object.keys(counts).sort((a, b) => counts[b] - counts[a]);
  let hidden = new Set(JSON.parse(localStorage.getItem("hearthloom-day-hidden") || "[]"));
  const activeEpisodes = () => episodes.filter((e) => !hidden.has(e.source));

  const renderItems = (list) => list.map((e) => {
    const hue = SOURCE_HUES[e.source] || "var(--ink-faint)";
    const kind = KIND_LABEL[e.kind] || e.kind;
    const people = (e.entities || [])
      .filter(([, type, role]) => role !== "touched" && role !== "visited" && role !== "repo" && role !== "workspace")
      .slice(0, 4);
    const context = (e.entities || [])
      .filter(([, , role]) => role === "repo" || role === "workspace")
      .slice(0, 1);
    return `
      <div class="tl-item" style="--dot:${hue}">
        <span class="tl-time">${hhmm(e.event_time, e.tz_offset_minutes)}</span>
        <div class="card expandable" data-episode="${esc(e.id)}">
          <span class="kind">${esc(kind)}${context.length ? " · " + esc(context[0][0]) : ""}</span>
          <div class="title">${esc(e.title)}</div>
          ${people.length ? `<div class="entchips">${people
            .map(([name, , role]) => `<a class="entchip" href="#/entity/${encodeURIComponent(name)}">${esc(role)}: ${esc(name)}</a>`)
            .join("")}</div>` : ""}
        </div>
      </div>`;
  });

  const edges = (day.valid_edges || []).slice(0, 30).map((edge) => `
    <div class="edge-row"><b>${esc(edge.src.name)}</b>
      <span class="edge-rel">—${esc(edge.relation)}→</span>
      <b>${esc(edge.dst.name)}</b></div>`);

  const filterBar = () => present.map((source) => {
    const hue = SOURCE_HUES[source] || "var(--ink-faint)";
    const off = hidden.has(source);
    return `<button class="filterchip ${off ? "off" : ""}" data-source="${esc(source)}"
      style="--dot:${hue}">${esc(source)} · ${counts[source]}</button>`;
  }).join("") + (hidden.size
    ? `<button class="filterchip clear" data-source="*">show all</button>` : "");

  content.innerHTML = `
    <h1 class="pagehead">${prettyDate(date)}</h1>
    ${visitChip(date)}
    <div class="day-nav">
      <button id="day-prev">← previous</button>
      <input type="date" id="day-pick" value="${esc(date)}">
      <button id="day-next">next →</button>
      <span class="statchip" id="day-count"></span>
    </div>
    <div id="otd"></div>
    ${places.length ? `<div class="day-places">${places
      .map((p) => `<a class="place-chip" href="#/entity/${encodeURIComponent(p)}">${esc(p)}</a>`)
      .join("")}</div>` : ""}
    ${present.length > 1 ? `<div class="filterbar" id="filterbar"></div>` : ""}
    <div id="day-timeline"></div>
    ${edges.length ? `<div class="edgepanel"><h2>standing that day</h2>${edges.join("")}</div>` : ""}
  `;

  const paint = () => {
    const list = activeEpisodes();
    const bar = document.getElementById("filterbar");
    if (bar) bar.innerHTML = filterBar();
    document.getElementById("day-count").textContent =
      hidden.size ? `${list.length} of ${episodes.length} episodes`
                  : `${episodes.length} episodes`;
    const timeline = document.getElementById("day-timeline");
    timeline.innerHTML = list.length
      ? `<div class="timeline">${renderItems(list).join("")}</div>`
      : `<div class="empty">${episodes.length ? "everything filtered out" : "nothing recorded on this day"}</div>`;
    bindCards(timeline);
    bar?.querySelectorAll(".filterchip").forEach((chip) => {
      chip.onclick = () => {
        const source = chip.dataset.source;
        if (source === "*") hidden.clear();
        else hidden.has(source) ? hidden.delete(source) : hidden.add(source);
        localStorage.setItem("hearthloom-day-hidden", JSON.stringify([...hidden]));
        paint();
      };
    });
  };
  paint();

  const shift = (days) => {
    const at = new Date(date + "T12:00:00");
    at.setDate(at.getDate() + days);
    location.hash = `#/day/${at.toISOString().slice(0, 10)}`;
  };
  // Memories resurfacing: this date in earlier years.
  api(`/v1/onthisday?date=${date}`).then((data) => {
    const memories = data.memories || [];
    const host = document.getElementById("otd");
    if (!host || !memories.length) return;
    host.innerHTML = `<div class="otd-strip">
      <span class="otd-label">on this day</span>
      ${memories.slice(0, 6).map((m) => `
        <a class="otd-card" href="#/day/${esc(m.day)}" title="${esc(m.title)}">
          <span class="otd-year">${esc(m.day.slice(0, 4))}</span>
          <span class="otd-title">${esc(m.title.length > 44 ? m.title.slice(0, 43) + "…" : m.title)}</span>
        </a>`).join("")}
    </div>`;
  }).catch(() => {});

  document.getElementById("day-prev").onclick = () => shift(-1);
  document.getElementById("day-next").onclick = () => shift(1);
  document.getElementById("day-pick").onchange = (e) =>
    (location.hash = `#/day/${e.target.value}`);

}

/* ————— search ————— */
async function renderSearch(query) {
  setNav("search");
  view.innerHTML = `
    <span class="eyebrow">hybrid search</span>
    <h1 class="pagehead">Ask the brain</h1>
    <p class="subhead">Full-text and semantic recall over everything — every hit cites its episode.</p>
    <div class="searchbar">
      <input id="q" placeholder="what was I doing / thinking / deciding…" value="${esc(query)}" autofocus>
      <button class="btn" id="go">search</button>
    </div>
    <div id="results"></div>`;
  const input = document.getElementById("q");
  const go = () => (location.hash = `#/search/${encodeURIComponent(input.value)}`);
  document.getElementById("go").onclick = go;
  input.addEventListener("keydown", (e) => e.key === "Enter" && go());

  if (!query) {
    const stats = window.__stats;
    if (stats?.top_entities) {
      document.getElementById("results").innerHTML = `
        <div class="section-title">gravitational centers</div>
        <div class="entchips">${stats.top_entities
          .map((t) => `<a class="entchip" href="#/entity/${encodeURIComponent(t.name)}">${esc(t.name)} · ${t.episodes}</a>`)
          .join("")}</div>`;
    }
    return;
  }
  document.getElementById("results").innerHTML = `<div class="loading">searching…</div>`;
  // Live notes are files, not episodes — give them their own leg so
  // Search sees what hasn't settled into the record yet.
  const [data, noteData] = await Promise.all([
    api(`/v1/search?q=${encodeURIComponent(query)}&limit=20`),
    api(`/v1/notes?q=${encodeURIComponent(query)}&limit=6`).catch(() => null),
  ]);

  // The Dial above the results, zoomed to the span of the hits: WHERE in
  // your life this query lives. Clicking a bucket hands off to Browse
  // pre-filtered to the query + that period.
  {
    const times = (data.hits || [])
      .map((h) => h.event_time && h.event_time.slice(0, 10))
      .filter(Boolean)
      .sort();
    if (times.length > 1) {
      const host = document.createElement("div");
      host.id = "search-dial";
      document.getElementById("results").before(host);
      const { mountDial } = await import("/assets/dial.js");
      const dial = mountDial(host, {
        from: times[0],
        to: times[times.length - 1],
        filters: () => ({ q: query }),
        onPick: (bucket) => {
          const from = bucket.length === 4 ? `${bucket}-01-01`
            : bucket.length === 7 ? `${bucket}-01` : bucket;
          const to = bucket.length === 4 ? `${bucket}-12-31`
            : bucket.length === 7 ? `${bucket}-28` : bucket;
          window.__browsePrefill?.({ q: query, from, to, place: "" });
          location.hash = "#/browse/episodes";
        },
      });
      // leaving the page tears it down with the DOM; stop its timers
      window.addEventListener("hashchange", () => dial.destroy(), { once: true });
    }
  }
  if (noteData?.notes?.length) {
    const host = document.createElement("div");
    host.className = "pm-section";
    host.innerHTML = `
      <div class="pm-section-title">notes <span class="statchip">${noteData.notes.length}</span>
        <span class="meta" style="font-weight:400">live on the desk — they settle into the record when they rest</span></div>
      ${noteData.notes.map((n) => `
        <div class="note-row" data-note="${esc(n.id)}">
          <span class="note-title">${noteTitle(n.title)}</span>
          <span class="meta">${n.updated_at ? esc(n.updated_at.slice(0, 10)) : "—"}</span>
        </div>`).join("")}`;
    document.getElementById("results").before(host);
    host.querySelectorAll(".note-row").forEach((row) => {
      row.onclick = () => window.__openNoteById?.(row.dataset.note);
    });
  }
  const hits = (data.hits || []).map((hit) => {
    const hue = SOURCE_HUES[hit.source] || "var(--ink-faint)";
    const snippet = esc(hit.snippet).replaceAll("»", "<mark>").replaceAll("«", "</mark>");
    return `
      <div class="tl-item" style="--dot:${hue}">
        <span class="tl-time">${esc((hit.event_time || "").slice(0, 10))}</span>
        <div class="card expandable" data-episode="${esc(hit.episode_id)}">
          <span class="kind">${esc(hit.source)}/${esc(hit.kind)}</span>
          <div class="title">${esc(hit.title)}</div>
          <div class="hit-snippet">${snippet}</div>
        </div>
      </div>`;
  });
  document.getElementById("results").innerHTML = hits.length
    ? `<div class="timeline">${hits.join("")}</div>`
    : `<div class="empty">no matches</div>`;
  bindCards(document.getElementById("results"));
}

/* ————— entity dossier ————— */
async function renderEntity(name) {
  setNav("");
  if (!name) return renderSearch("");
  view.innerHTML = `<div class="loading">opening dossier…</div>`;
  const d = await api(`/v1/entity/${encodeURIComponent(name)}`);
  const entity = d.entity;
  const attrs = (d.attributes || [])
    .map(([k, v]) => `<div class="attr-row"><span class="k">${esc(k)}</span><span>${esc(v)}</span></div>`)
    .join("");
  const edges = (d.edges || []).slice(0, 40).map((edge) => `
    <div class="edge-row">
      ${edge.direction === "out" ? "" : `<b>${esc(edge.other.name)}</b>`}
      <span class="edge-rel">—${esc(edge.relation)}→</span>
      ${edge.direction === "out"
        ? `<a class="entchip" href="#/entity/${encodeURIComponent(edge.other.name)}">${esc(edge.other.name)}</a>`
        : ""}
      <span class="meta" style="color:var(--ink-faint)"> · ${edge.provenance.length} citations</span>
    </div>`).join("");
  const recent = (d.recent_episodes || []).map(([id, role, title, at]) => `
    <div class="tl-item" style="--dot:var(--accent)">
      <span class="tl-time">${esc((at || "").slice(0, 10))}</span>
      <div class="card"><span class="kind">${esc(role)}</span>
        <div class="title">${esc(title)}</div></div>
    </div>`).join("");

  view.innerHTML = `
    <span class="eyebrow">dossier</span>
    <h1 class="pagehead">${esc(entity.name)} <span class="typebadge">${esc(entity.entity_type)}</span></h1>
    <p class="subhead">cited by ${d.episode_count} episodes · scope ${esc(entity.scope)}
      · <a href="#/constellation/${encodeURIComponent(entity.name)}">view constellation ✦</a>
      · <span class="importance" id="imp-ctl" title="importance — outranks evidence counts in the sky and on the dial">${[1, 2, 3]
        .map((l) => `<button class="imp-star${(entity.declared_importance || 0) >= l ? " lit" : ""}" data-level="${l}">★</button>`)
        .join("")}</span></p>
    <div class="dossier-grid">
      <div>
        <div class="section-title">recent evidence</div>
        <div class="timeline">${recent || `<div class="empty">no episodes</div>`}</div>
      </div>
      <div>
        ${attrs ? `<div class="section-title">identities</div>${attrs}` : ""}
        <div class="section-title" style="margin-top:var(--s5)">relations</div>
        ${edges || `<div class="empty">none</div>`}
      </div>
    </div>`;

  // Relationships: chips (confirm/reject the predictions), declare,
  // and the idle-written brief.
  try {
    const rel = await api(`/v1/relationships?entity=${encodeURIComponent(name)}`);
    const conns = await api(`/v1/connections?name=${encodeURIComponent(name)}`).catch(() => null);
    const connChips = (conns?.connections || []).map((c) => `<span class="task-tag plain-tag"
      title="from the ${esc(c.network)} connections list — a claim, not evidence">${esc(c.network)}
      ${c.status === "removed" ? `friend until ${esc((c.ended_at || "").slice(0, 4))}` : `friend${c.connected_at ? ` since ${esc(c.connected_at.slice(0, 4))}` : ""}`}${
      c.position || c.company ? ` · ${esc([c.position, c.company].filter(Boolean).join(", "))}` : ""}</span>`).join("");
    const host = document.createElement("div");
    host.innerHTML = `
      <div class="section-title" style="margin-top:var(--s5)">relationship</div>
      <div class="rel-chips">
        ${connChips}
        ${(rel.relationships || []).map((r) => r.status === "confirmed"
          ? `<span class="rel-chip confirmed">${esc(r.kind)}</span>`
          : r.status === "proposed"
            ? `<span class="rel-chip proposed" data-rel="${esc(r.id)}">${esc(r.kind)}?
                <button class="rel-yes">✓</button><button class="rel-no">✗</button></span>`
            : "").join("")}
        <select class="rel-declare"><option value="">relate…</option>
          <optgroup label="is my…">${(rel.kinds || []).filter((k) => k !== "me")
            .map((k) => `<option value="${k}">${k}</option>`).join("")}</optgroup>
          <optgroup label="was my… (former)">${(rel.kinds || []).filter((k) => k !== "me")
            .map((k) => `<option value="former:${k}">${k}</option>`).join("")}</optgroup>
        </select>
        <a class="meta" href="#/relationships" style="margin-left:6px">directory →</a>
      </div>
      ${rel.brief ? `<div class="card" style="margin-top:var(--s3)"><span class="kind">how things stand · written at idle</span>
        <div class="md">${md(rel.brief.body.split("\ndrawn from:")[0])}</div></div>` : ""}`;
    view.querySelector(".dossier-grid").before(host);
    host.querySelectorAll(".rel-chip.proposed").forEach((chip) => {
      chip.querySelector(".rel-yes").onclick = async () => {
        await apiPost(`/v1/relationships/${chip.dataset.rel}/resolve`,
          { actor: "human:dashboard", action: "confirm" });
        renderEntity(name);
      };
      chip.querySelector(".rel-no").onclick = async () => {
        await apiPost(`/v1/relationships/${chip.dataset.rel}/resolve`,
          { actor: "human:dashboard", action: "reject" });
        chip.remove();
      };
    });
    host.querySelector(".rel-declare").onchange = (e) => {
      if (!e.target.value) return;
      askRelContext(e.target, async (ctx) => {
        const former = e.target.value.startsWith("former:");
        await apiPost("/v1/relationships", {
          actor: "human:dashboard", entity: name,
          kind: former ? e.target.value.slice(7) : e.target.value,
          ended: former,
          note: ctx || null,
        });
        renderEntity(name);
      });
    };
  } catch { /* entity may not be a person */ }

  // Tasks and notes mentioning them — tasks first (time-sensitive),
  // on the full page just like the modal.
  try {
    const [tasks, notes] = await Promise.all([
      api(`/v1/tasks?mentions=${encodeURIComponent(name)}&limit=8`).catch(() => null),
      api(`/v1/notes?mentions=${encodeURIComponent(name)}&limit=5`).catch(() => null),
    ]);
    const host = document.createElement("div");
    host.className = "pm-grid";
    host.innerHTML = `
      <div class="pm-section">
        <div class="pm-section-title">tasks mentioning them ${tasks?.tasks?.length ? `<span class="statchip">${tasks.tasks.length}</span>` : ""}</div>
        ${(tasks?.tasks || []).map((t) => `
          <div class="pm-task${t.status === "done" ? " pm-task-done" : ""}">
            <span class="kind">${esc(t.status)}</span>
            ${esc(t.description).replace(MENTION_RE, "<b>@$1</b>")}
            ${t.remind_at ? `<span class="meta"> · due ${esc(t.remind_at.slice(0, 10))}</span>` : ""}
          </div>`).join("") || `<p class="meta" style="color:var(--ink-faint)">none open.</p>`}
      </div>
      <div class="pm-section">
        <div class="pm-section-title">notes mentioning them ${notes?.notes?.length ? `<span class="statchip">${notes.notes.length}</span>` : ""}
          <button class="btn btn-quiet" id="dossier-note-new">+ note</button></div>
        ${(notes?.notes || []).map((n) => `
          <div class="note-row" data-note="${esc(n.id)}">
            <span class="note-title">${noteTitle(n.title)}</span>
            <span class="meta">${n.updated_at ? esc(n.updated_at.slice(0, 10)) : "—"}</span>
          </div>`).join("") || `<p class="meta" style="color:var(--ink-faint)">none yet.</p>`}
      </div>`;
    view.querySelector(".dossier-grid").before(host);
    host.querySelector("#dossier-note-new").onclick = () => window.__newNoteFor?.(name);
    host.querySelectorAll(".note-row").forEach((row) => {
      row.onclick = () => window.__openNoteById?.(row.dataset.note);
    });
  } catch { /* mentions need a resolvable entity */ }

  // Importance stars: click a lit top star to clear back to unset.
  document.getElementById("imp-ctl").querySelectorAll(".imp-star").forEach((star) => {
    star.onclick = async () => {
      const level = +star.dataset.level;
      const current = (entity.declared_importance || 0);
      await apiPost(`/v1/entity/${encodeURIComponent(entity.name)}/importance`,
        { actor: "human:dashboard", level: level === current ? 0 : level });
      renderEntity(name);
    };
  });
}

/* ————— Ask debug trace: full observability for one query —————
   ⌗ on a history row → every step (tool, args, why, FULL result),
   per-round LLM latency + tokens, per-call ledger rows, and priced
   totals. Server-side vault gate: locked vault = 403, rendered as the
   unlock card (traces can quote vaulted content — Jon 2026-08-16). */
const fmtTok = (n) => (n == null ? "—" : Number(n).toLocaleString());
const fmtUsd = (v) => (v == null ? "no price on file"
  : `$${v < 0.01 ? v.toFixed(5) : v.toFixed(3)}`);
async function openAskTrace(id) {
  document.getElementById("ask-trace-modal")?.remove();
  const overlay = document.createElement("div");
  overlay.className = "note-overlay person-overlay";
  overlay.id = "ask-trace-modal";
  overlay.innerHTML = `<div class="person-card trace-card"><div class="ghost-row" style="width:50%"></div><div class="ghost-row"></div></div>`;
  document.body.appendChild(overlay);
  const close = () => overlay.remove();
  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
  const onKey = (e) => {
    if (e.key === "Escape") { close(); document.removeEventListener("keydown", onKey); }
  };
  document.addEventListener("keydown", onKey);
  const card = () => overlay.querySelector(".person-card");

  let data;
  try {
    data = await api(`/v1/ask/${encodeURIComponent(id)}/trace`);
  } catch (err) {
    const locked = String(err.message || "").includes("vault locked");
    card().innerHTML = locked ? `
      <h3>🔒 Traces are vault-locked</h3>
      <p class="meta">Query traces can quote private memories and reveal private
        activity, so the debug view only opens while the vault is unlocked.
        Unlock it from the masthead 🔒 (or ☰ → Privacy) and try again.</p>`
      : `<h3>trace unavailable</h3><p class="meta">${esc(err.message)}</p>`;
    return;
  }

  const { ask, usage, totals } = data;
  const steps = ask.transcript || [];
  const pre2026_08 = steps.length && steps.every((st) => st.llm_ms == null);
  const stepCard = (st) => {
    const kind = st.kind || (st.tool ? "tool" : "llm");
    const meta = [
      st.llm_ms != null ? `llm ${st.llm_ms}ms` : "",
      st.tokens_in != null ? `${fmtTok(st.tokens_in)} in / ${fmtTok(st.tokens_out)} out` : "",
      st.tool_ms != null ? `tool ${st.tool_ms}ms` : "",
    ].filter(Boolean).join(" · ");
    const body = st.tool ? `
      <div class="meta mono">${esc(st.tool)} ${esc(JSON.stringify(st.args || {}))}</div>
      ${st.why ? `<div class="meta">why: ${esc(st.why)}</div>` : ""}
      ${st.result_full || st.result_preview ? `
        <details><summary class="meta">result (${fmtTok((st.result_full || st.result_preview).length)} chars)</summary>
          <pre class="trace-result">${esc(st.result_full || st.result_preview)}</pre>
        </details>` : ""}` : "";
    return `
      <div class="pm-section">
        <div class="task-row"><b>round ${st.round ?? "?"} — ${esc(kind)}</b>
          <span class="meta">${esc(meta)}</span></div>
        ${body}
      </div>`;
  };
  card().innerHTML = `
    <button class="ask-close" id="trace-close" title="close">×</button>
    <h3>⌗ ${esc(ask.question)}</h3>
    <p class="meta">${esc((ask.ts || "").slice(0, 16).replace("T", " "))} ·
      ${esc(ask.status)} · ${ask.rounds ?? 0} rounds ·
      ${ask.duration_ms != null ? `${(ask.duration_ms / 1000).toFixed(1)}s total` : ""}</p>
    <div class="pm-section">
      <div class="task-row"><b>cost accounting</b>
        <span class="meta">${totals.calls} LLM calls · model ${esc(totals.model || "?")}</span></div>
      <div class="meta">tokens: ${fmtTok(totals.input_tokens)} in ·
        ${fmtTok(totals.output_tokens)} out
        ${totals.cache_read_tokens ? ` · ${fmtTok(totals.cache_read_tokens)} cache-read` : ""}
        · LLM time ${(totals.llm_ms / 1000).toFixed(1)}s
        ${totals.errors ? ` · <span style="color:var(--danger,#c25b4e)">${totals.errors} failed calls</span>` : ""}</div>
      <div><b>${fmtUsd(totals.cost_usd)}</b> <span class="meta">est. — tokens are recorded truth, price computed from the current table</span></div>
    </div>
    ${pre2026_08 ? `<p class="meta">recorded before per-round accounting (0.5.58) —
      steps show tool previews only, and no token ledger exists for this ask.</p>` : ""}
    ${steps.map(stepCard).join("") || `<p class="meta">no tool rounds — the model answered directly.</p>`}
    ${usage.length ? `
      <div class="pm-section">
        <div class="task-row"><b>ledger rows</b><span class="meta">v50 llm_usage · ref ${esc(id)}</span></div>
        ${usage.map((u) => `
          <div class="meta mono">${esc(u.ts.slice(11, 19))} ${esc(u.model)}
            ${fmtTok(u.input_tokens)}→${fmtTok(u.output_tokens)} ${u.latency_ms}ms
            ${u.status !== "ok" ? "✗" : ""} ${fmtUsd(u.cost_usd)}</div>`).join("")}
      </div>` : ""}`;
  card().querySelector("#trace-close").onclick = close;
}

/* ————— the person card: a dossier MODAL that keeps your place —————
   Any entity link anywhere opens this instead of navigating away; the
   full dossier page stays one click deeper (class "no-modal" opts a
   link back into real navigation, as does ⌘/ctrl-click). */
async function openPersonModal(name) {
  document.getElementById("person-modal")?.remove();
  const overlay = document.createElement("div");
  overlay.className = "note-overlay person-overlay";
  overlay.id = "person-modal";
  overlay.innerHTML = `
    <div class="person-card">
      <div class="ghost-row" style="width:50%"></div>
      <div class="ghost-row"></div>
      <div class="ghost-row" style="width:80%"></div>
    </div>`;
  document.body.appendChild(overlay);
  const close = () => overlay.remove();
  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });
  const onKey = (e) => {
    if (e.key === "Escape") { close(); document.removeEventListener("keydown", onKey); }
  };
  document.addEventListener("keydown", onKey);

  const [dossier, rel, notes, tasks, conns, friendsData] = await Promise.allSettled([
    api(`/v1/entity/${encodeURIComponent(name)}`),
    api(`/v1/relationships?entity=${encodeURIComponent(name)}`),
    api(`/v1/notes?mentions=${encodeURIComponent(name)}&limit=5`),
    api(`/v1/tasks?mentions=${encodeURIComponent(name)}&limit=8`),
    api(`/v1/connections?name=${encodeURIComponent(name)}`),
    api("/v1/friends"),
  ]).then((r) => r.map((x) => (x.status === "fulfilled" ? x.value : null)));
  if (!dossier) {
    overlay.querySelector(".person-card").innerHTML =
      `<p class="meta">no dossier for ${esc(name)}</p>`;
    return;
  }
  const entity = dossier.entity;
  const isProject = entity.entity_type === "Project";
  // a Project's contained repos (its whole point) replace the
  // relationship section — projects have members, not relationships
  const projDetail = isProject
    ? await api(`/v1/projects/${encodeURIComponent(entity.id)}/detail`).catch(() => null)
    : null;
  // a friend connection bound to this Person → the connected-brain chip
  const brainLink = (friendsData?.friends || []).find(
    (f) => f.entity && (f.entity === entity.id || f.entity === entity.name));
  const handle = (dossier.attributes || []).find(([k]) => k === "email")?.[1]
    || (dossier.attributes || []).find(([k]) => k === "phone")?.[1];
  const recent = (dossier.recent_episodes || []).slice(0, 5).map(([, role, title, at]) => `
    <div class="pm-ep"><span class="meta">${esc((at || "").slice(0, 10))}</span>
      <span class="kind">${esc(role)}</span> ${esc(title)}</div>`).join("");
  const qual = (r) => r.note ? ` · ${esc(r.note.length > 28 ? r.note.slice(0, 28) + "…" : r.note)}` : "";
  const relChips = (rel?.relationships || []).map((r) => {
    if (r.status === "confirmed")
      return `<span class="rel-chip confirmed" data-rel="${esc(r.id)}"
        title="${esc(r.origin)}${r.note ? ` · ${esc(r.note)}` : ""} · since ${esc((r.t_valid || "").slice(0, 10))} — × ends it">
        ${esc(r.kind)}${qual(r)} <button class="rel-end" title="no longer true">×</button></span>`;
    if (r.status === "proposed")
      return `<span class="rel-chip proposed" data-rel="${esc(r.id)}">${esc(r.kind)}?
        <button class="rel-yes" title="yes">✓</button><button class="rel-no" title="no">✗</button></span>`;
    if (r.status === "ended")
      return `<span class="rel-chip ended" title="ended ${esc((r.t_invalid || "").slice(0, 10))}${r.note ? ` · ${esc(r.note)}` : ""}">was ${esc(r.kind)}${qual(r)}</span>`;
    return "";
  }).join("");
  const kinds = (rel?.kinds || []).filter((k) => k !== "me");
  const noteRows = (notes?.notes || []).map((n) => `
    <div class="note-row" data-note="${esc(n.id)}">
      <span class="note-title">${noteTitle(n.title)}</span>
      <span class="meta">${n.updated_at ? esc(n.updated_at.slice(0, 10)) : "—"}</span>
    </div>`).join("");
  const taskRows = (tasks?.tasks || []).map((t) => `
    <div class="pm-task${t.status === "done" ? " pm-task-done" : ""}">
      <span class="kind">${esc(t.status)}</span>
      ${esc(t.description).replace(MENTION_RE, "<b>@$1</b>")}
      ${t.remind_at ? `<span class="meta"> · due ${esc(t.remind_at.slice(0, 10))}</span>` : ""}
    </div>`).join("");

  overlay.querySelector(".person-card").innerHTML = `
    <div class="pm-head">
      <div>
        <span class="eyebrow">${esc(entity.entity_type || "")}${handle ? ` · <span class="rel-handle">${esc(handle)}</span>` : ""}${brainLink ? ` · <a class="no-modal" href="#/network" title="their brain is connected — Ask can reach what they share">🧠 connected brain</a>` : ""}</span>
        <h2 class="pm-name">${esc(entity.name)}</h2>
      </div>
      <span class="importance" id="pm-imp">${[1, 2, 3].map((l) =>
        `<button class="imp-star${(entity.declared_importance || 0) >= l ? " lit" : ""}" data-level="${l}">★</button>`).join("")}</span>
      <button class="ask-close" id="pm-close">×</button>
    </div>
    ${isProject ? `
    <div class="pm-section">
      <div class="pm-section-title">description</div>
      ${projDetail?.primary_note
        ? `<button class="goal-note-open" id="pm-primary-open" data-note="${esc(projDetail.primary_note.id)}">✎ ${esc(projDetail.primary_note.title)}</button>
           ${projDetail.primary_note.excerpt ? `<div class="meta" style="margin-top:4px">${esc(projDetail.primary_note.excerpt)}${projDetail.primary_note.excerpt.length >= 240 ? "…" : ""}</div>` : ""}`
        : `<button class="btn btn-quiet" id="pm-primary-new">+ project note</button>`}
    </div>
    <div class="pm-section">
      <div class="pm-section-title">repos in this project ${projDetail?.repos?.length ? `<span class="statchip">${projDetail.repos.length}</span>` : ""}</div>
      <div id="pm-repos">
        ${(projDetail?.repos || []).map((r) => `
          <div class="pm-task"><a class="mention no-modal" href="#/constellation/${encodeURIComponent(r.name)}">${esc(r.name)}</a>
            <button class="btn btn-quiet pm-detach" data-child="${esc(r.name)}" title="remove from this project">×</button></div>`).join("")
          || `<p class="meta" style="color:var(--ink-faint)">no repos yet — compose one below.</p>`}
      </div>
      <div class="searchbar" style="margin-top:var(--s2); position:relative">
        <input id="pm-attach" placeholder="include a GitHub repo — name or URL…" autocomplete="off">
        <div id="pm-attach-pop" class="ne-mention-pop ne-pop-under" hidden></div>
      </div>
    </div>
    <div class="pm-section">
      <div class="pm-section-title">goals it advances ${projDetail?.goals?.length ? `<span class="statchip">${projDetail.goals.length}</span>` : ""}</div>
      <div class="rel-chips" id="pm-proj-goals">
        ${(projDetail?.goals || []).map((gn) => `<span class="task-tag project-chip" data-goal="${esc(gn)}">⭑ ${esc(gn)}
          <button class="goal-proj-rm pm-goal-rm" data-goal="${esc(gn)}" title="unlink">×</button></span>`).join("")}
        <button class="btn btn-quiet" id="pm-goal-link">+ goal</button>
      </div>
    </div>` : `
    <div class="pm-section">
      <div class="pm-section-title">relationship</div>
      <div class="rel-chips" id="pm-rel">
        ${(conns?.connections || []).map((c) => `<span class="task-tag plain-tag"
          title="from the ${esc(c.network)} connections list — a claim, not evidence">${esc(c.network)}
          ${c.status === "removed" ? `friend until ${esc((c.ended_at || "").slice(0, 4))}` : `friend${c.connected_at ? ` since ${esc(c.connected_at.slice(0, 4))}` : ""}`}${
          c.position || c.company ? ` · ${esc([c.position, c.company].filter(Boolean).join(", "))}` : ""}</span>`).join("")}
        ${relChips}
        <select class="rel-declare" id="pm-declare">
          <option value="">relate…</option>
          <optgroup label="is my…">${kinds.map((k) => `<option value="${k}">${k}</option>`).join("")}</optgroup>
          <optgroup label="was my… (former)">${kinds.map((k) => `<option value="former:${k}">${k}</option>`).join("")}</optgroup>
        </select>
      </div>
      <div class="searchbar" style="margin-top:var(--s2)">
        <input id="pm-add-identity" placeholder="another email/handle for ${esc(entity.name)}…"
          title="claim it onto this person — merges any existing owner in, reversibly" style="flex:1">
        <button class="btn btn-quiet" id="pm-add-identity-go">add identity</button>
      </div>
      <p class="meta" id="pm-add-identity-note" hidden></p>
      ${rel?.brief ? `<div class="pm-brief"><span class="kind">how things stand · written at idle</span>
        <div class="md">${md(rel.brief.body.split("\ndrawn from:")[0])}</div></div>` : ""}
    </div>`}
    <div class="pm-section">
      <div class="pm-section-title">tasks mentioning them ${tasks?.tasks?.length ? `<span class="statchip">${tasks.tasks.length}</span>` : ""}</div>
      ${taskRows || `<p class="meta" style="color:var(--ink-faint)">none open — @${esc(entity.name)} in a task links it here.</p>`}
    </div>
    <div class="pm-section">
      <div class="pm-section-title">notes mentioning them ${notes?.notes?.length ? `<span class="statchip">${notes.notes.length}</span>` : ""}
        <button class="btn btn-quiet" id="pm-note-new">+ note</button></div>
      ${noteRows || `<p class="meta" style="color:var(--ink-faint)">none yet — @${esc(entity.name)} in a note links it here.</p>`}
    </div>
    <div class="pm-section">
      <div class="pm-section-title">recent evidence <span class="statchip">${dossier.episode_count}</span></div>
      ${recent || `<p class="meta">none</p>`}
    </div>
    <div class="pm-foot">
      <a class="no-modal" href="#/entity/${encodeURIComponent(entity.name)}">full dossier →</a>
      <a class="no-modal" href="#/constellation/${encodeURIComponent(entity.name)}">constellation ✦</a>
    </div>`;

  overlay.querySelector("#pm-close").onclick = close;
  // — sharing (v46): abbreviated two-way summary for a connected brain,
  // plus "share this relationship with…" for any person with confirmed
  // relationship rows (grants access to that row even when the
  // relationships class is categorically off) —
  // share targets: outbound friends ∪ active grants (inbound-only
  // connections — the common posture — must be pickable too)
  const grantsData = await api("/v1/grants").catch(() => ({ grants: [] }));
  const allFriends = [
    ...(friendsData?.friends || []),
    ...(grantsData.grants || [])
      .filter((g) => !g.revoked_at && g.friend !== "__default__"
        && !(friendsData?.friends || []).some((f) => f.name === g.friend))
      .map((g) => ({ name: g.friend })),
  ];
  const confirmedRels = (rel?.relationships || []).filter((r) => r.status === "confirmed");
  if (brainLink || (allFriends.length && confirmedRels.length)) {
    const shareSec = document.createElement("div");
    shareSec.className = "pm-section";
    shareSec.innerHTML = `<div class="pm-section-title">sharing</div><p class="meta">…</p>`;
    overlay.querySelector(".pm-foot")?.before(shareSec);
    const paintShareSec = async () => {
      const [mine, theirs, relShares] = await Promise.allSettled([
        brainLink ? api(`/v1/sharing?friend=${encodeURIComponent(brainLink.name)}`) : Promise.reject(),
        brainLink ? api(`/v1/friends/${encodeURIComponent(brainLink.name)}/offer`) : Promise.reject(),
        confirmedRels.length
          ? Promise.all(confirmedRels.map((r) =>
              api(`/v1/sharing?kind=relationship&item_id=${encodeURIComponent(r.id)}`)
                .then((d) => ({ rel: r, shares: d.shares || [] })).catch(() => ({ rel: r, shares: [] }))))
          : Promise.resolve([]),
      ]).then((r) => r.map((x) => (x.status === "fulfilled" ? x.value : null)));
      const shares = mine?.shares || [];
      const offer = theirs;
      const icon = { task: "☑", note: "✎", relationship: "◉" };
      shareSec.innerHTML = `
        <div class="pm-section-title">sharing${brainLink ? ` with ${esc(brainLink.name)}` : ""}</div>
        ${brainLink ? `
        <div class="meta">you share: ${shares.length
          ? shares.slice(0, 6).map((s) => `<span class="task-tag plain-tag">${icon[s.kind] || ""} ${esc(s.kind)}</span>`).join(" ")
            + (shares.length > 6 ? ` +${shares.length - 6} more` : "")
          : "no pinned items"} — <a class="no-modal" href="#/network">manage on Network →</a></div>
        <div class="meta" style="margin-top:2px">they share: ${offer
          ? esc((offer.classes || []).join(" · ") + (offer.spheres?.length ? ` — ${offer.spheres.join(", ")}` : "") + ` · ${offer.detail || "excerpts"}`)
          : "(their node doesn't say)"}</div>` : ""}
        ${(relShares || []).map(({ rel: rr, shares: ss }) => `
          <div class="meta" style="margin-top:var(--s1)">◉ <b>${esc(rr.kind)}</b> relationship:
            ${ss.map((s) => `<span class="task-tag plain-tag">${esc(s.friend)}
              <button class="linkish pm-rel-unshare" data-rel="${esc(rr.id)}" data-friend="${esc(s.friend)}">×</button></span>`).join(" ")}
            ${allFriends.length ? `<select class="pm-rel-share-sel" data-rel="${esc(rr.id)}">
              <option value="">share with…</option>
              ${allFriends.map((f) => `<option>${esc(f.name)}</option>`).join("")}</select>` : ""}
          </div>`).join("")}`;
      shareSec.querySelector("a")?.addEventListener("click", () => close());
      shareSec.querySelectorAll(".pm-rel-share-sel").forEach((sel) => {
        sel.onchange = async () => {
          if (!sel.value) return;
          await apiPost("/v1/sharing", {
            actor: "human:dashboard", action: "share", kind: "relationship",
            item_id: sel.dataset.rel, friend: sel.value,
          }).catch(() => {});
          paintShareSec();
        };
      });
      shareSec.querySelectorAll(".pm-rel-unshare").forEach((btn) => {
        btn.onclick = async () => {
          await apiPost("/v1/sharing", {
            actor: "human:dashboard", action: "unshare", kind: "relationship",
            item_id: btn.dataset.rel, friend: btn.dataset.friend,
          }).catch(() => {});
          paintShareSec();
        };
      });
    };
    paintShareSec();
  }
  overlay.querySelector("#pm-note-new").onclick = () => {
    close();
    window.__newNoteFor?.(entity.name);
  };
  // add-identity: claim an email/handle onto this person (merges any
  // existing owner in — the manual-merge path without the CLI)
  overlay.querySelector("#pm-add-identity-go")?.addEventListener("click", async () => {
    const inp = overlay.querySelector("#pm-add-identity");
    const note = overlay.querySelector("#pm-add-identity-note");
    const value = inp.value.trim();
    if (!value) return;
    const kind = value.includes("@") ? "email" : "handle";
    note.hidden = false; note.textContent = "claiming…";
    const res = await apiPost("/v1/identity/claim", {
      actor: "human:dashboard", entity: entity.name, kind, value,
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    note.textContent = res.merged
      ? `✓ ${value} claimed — merged an existing entry into ${entity.name}`
      : `✓ ${value} is now ${entity.name}'s`;
    inp.value = "";
    setTimeout(() => openPersonModal(name), 700);
  });
  overlay.querySelectorAll(".note-row").forEach((row) => {
    row.onclick = () => { close(); window.__openNoteById?.(row.dataset.note); };
  });
  overlay.querySelectorAll(".pm-foot a").forEach((a) => { a.onclick = () => close(); });
  const relActor = { actor: "human:dashboard" };
  // — project composition: attach typeahead + per-repo detach —
  if (isProject) {
    overlay.querySelectorAll(".pm-detach").forEach((btn) => {
      btn.onclick = async () => {
        await apiPost(`/v1/projects/${encodeURIComponent(entity.id)}/detach`,
          { ...relActor, child: btn.dataset.child });
        window.__refreshDeskProjects?.();
        openPersonModal(name);
      };
    });
    const attach = async (childName) => {
      // a repo the record doesn't know yet? declare it as a Project first
      const known = await api(`/v1/browse/entities?q=${encodeURIComponent(childName)}&type=Project&limit=3`).catch(() => null);
      if (!(known?.items || []).some((i) => i.name.toLowerCase() === childName.toLowerCase()))
        await apiPost("/v1/projects", { ...relActor, name: childName });
      await apiPost(`/v1/projects/${encodeURIComponent(entity.id)}/attach`, { ...relActor, child: childName });
      window.__refreshDeskProjects?.();
      openPersonModal(name);
    };
    const repoName = (v) => {
      // a GitHub URL → "owner/repo" (or just the repo segment)
      const m = v.match(/github\.com[/:]([^/\s]+)\/([^/\s#?]+?)(?:\.git)?(?:[/#?].*)?$/i);
      return m ? `${m[1]}/${m[2]}` : v.trim();
    };
    const attachEl = overlay.querySelector("#pm-attach");
    const attachPop = overlay.querySelector("#pm-attach-pop");
    attachEl?.addEventListener("input", async () => {
      const q = attachEl.value.trim();
      if (q.length < 2) { attachPop.hidden = true; return; }
      // known git repos are Project entities — filter to those
      const data = await api(`/v1/browse/entities?q=${encodeURIComponent(q)}&type=Project&limit=6`).catch(() => null);
      const hits = (data?.items || []).filter((i) => i.name !== entity.name).slice(0, 6);
      const isUrl = /^(https?:\/\/)?(www\.)?github\.com\//i.test(q);
      attachPop.innerHTML = hits.map((h) =>
        `<button class="ne-mention-opt" data-name="${esc(h.name)}">◈ ${esc(h.name)}<span class="meta">repo</span></button>`).join("")
        + (isUrl ? `<button class="ne-mention-opt" data-name="${esc(repoName(q))}">+ add ${esc(repoName(q))}<span class="meta">from URL</span></button>` : "");
      attachPop.hidden = !attachPop.innerHTML;
      attachPop.querySelectorAll(".ne-mention-opt").forEach((opt) => opt.onclick = () => attach(opt.dataset.name));
    });
    attachEl?.addEventListener("keydown", (e) => {
      if (e.key === "Enter" && attachEl.value.trim()) attach(repoName(attachEl.value));
    });
    // description note
    overlay.querySelector("#pm-primary-open")?.addEventListener("click", (e) =>
      { close(); window.__openNoteById?.(e.target.dataset.note); });
    overlay.querySelector("#pm-primary-new")?.addEventListener("click", async () => {
      const r = await apiPost("/v1/notes", {
        ...relActor, title: `${entity.name} — project notes`,
        body: `# ${entity.name}\n\n`, scope: "work", primary_for: entity.id,
      });
      close();
      window.__openNoteById?.(r.id);
    });
    // goals it advances
    overlay.querySelectorAll(".pm-goal-rm").forEach((rm) => rm.onclick = async () => {
      await apiPost("/v1/goals/project", { ...relActor, goal: rm.dataset.goal, project: entity.id, link: false });
      openPersonModal(name);
    });
    overlay.querySelector("#pm-goal-link")?.addEventListener("click", async () => {
      const { intents } = await api("/v1/intents").catch(() => ({ intents: [] }));
      const active = (intents || []).filter((g) => g.status === "active");
      const btn = overlay.querySelector("#pm-goal-link");
      const wrap = document.createElement("span");
      wrap.style.position = "relative";
      wrap.innerHTML = `<input class="rel-ctx" placeholder="goal…"><div class="ne-mention-pop ne-pop-under" hidden></div>`;
      btn.replaceWith(wrap);
      const input = wrap.querySelector("input");
      const pop = wrap.querySelector(".ne-mention-pop");
      input.focus();
      input.addEventListener("input", () => {
        const q = input.value.toLowerCase();
        const hits = active.filter((g) => g.name.toLowerCase().includes(q)).slice(0, 6);
        pop.innerHTML = hits.map((g) => `<button class="ne-mention-opt" data-goal="${esc(g.name)}">⭑ ${esc(g.name)}</button>`).join("");
        pop.hidden = !hits.length;
        pop.querySelectorAll(".ne-mention-opt").forEach((o) => o.onclick = async () => {
          await apiPost("/v1/goals/project", { ...relActor, goal: o.dataset.goal, project: entity.id, link: true });
          openPersonModal(name);
        });
      });
    });
  }
  overlay.querySelectorAll(".rel-chip.proposed").forEach((chip) => {
    chip.querySelector(".rel-yes").onclick = async () => {
      await apiPost(`/v1/relationships/${chip.dataset.rel}/resolve`, { ...relActor, action: "confirm" });
      openPersonModal(name);
    };
    chip.querySelector(".rel-no").onclick = async () => {
      await apiPost(`/v1/relationships/${chip.dataset.rel}/resolve`, { ...relActor, action: "reject" });
      chip.remove();
    };
  });
  overlay.querySelectorAll(".rel-chip.confirmed .rel-end").forEach((end) => {
    end.onclick = async () => {
      await apiPost(`/v1/relationships/${end.closest(".rel-chip").dataset.rel}/resolve`,
        { ...relActor, action: "end" });
      openPersonModal(name);
    };
  });
  const declareEl = overlay.querySelector("#pm-declare");
  if (declareEl) declareEl.onchange = (e) => {
    if (!e.target.value) return;
    askRelContext(e.target, async (ctx) => {
      const former = e.target.value.startsWith("former:");
      await apiPost("/v1/relationships", {
        ...relActor, entity: entity.name,
        kind: former ? e.target.value.slice(7) : e.target.value,
        ended: former,
        note: ctx || null,
      });
      openPersonModal(name);
    });
  };
  overlay.querySelectorAll("#pm-imp .imp-star").forEach((star) => {
    star.onclick = async () => {
      const level = +star.dataset.level;
      const current = entity.declared_importance || 0;
      await apiPost(`/v1/entity/${encodeURIComponent(entity.name)}/importance`,
        { ...relActor, level: level === current ? 0 : level });
      openPersonModal(name);
    };
  });
}
window.__openPersonModal = openPersonModal;

/* Declaring a kind offers an optional freeform qualifier ("accountant
   — since 2019"): the relationships.note column, finally
   surfaced. Enter saves (empty is fine), Escape backs out. */
function askRelContext(selEl, save) {
  const input = document.createElement("input");
  input.className = "rel-ctx";
  input.placeholder = "context, optional — Enter saves";
  selEl.after(input);
  selEl.hidden = true;
  input.focus();
  input.addEventListener("keydown", async (e) => {
    if (e.key === "Enter") { e.stopPropagation(); await save(input.value.trim()); }
    if (e.key === "Escape") {
      e.stopPropagation();
      input.remove();
      selEl.hidden = false;
      selEl.value = "";
    }
  });
}
window.__askRelContext = askRelContext;

// The cross-connecting door: every entity link in the app opens the
// person card in place. ⌘/ctrl-click and .no-modal links still navigate.
document.addEventListener("click", (e) => {
  const a = e.target.closest('a[href^="#/entity/"]');
  if (!a || a.classList.contains("no-modal") || e.metaKey || e.ctrlKey || e.button !== 0) return;
  e.preventDefault();
  openPersonModal(decodeURIComponent(a.getAttribute("href").slice("#/entity/".length)));
});

/* ————— constellation (three.js, lazy-loaded) —————
   Two altitudes: no focus = the GALAXY (the record seen whole —
   sources as suns, evidence as gravity); a focus = the classic
   neighborhood. Clicking a planet descends; "◌ whole galaxy" ascends. */
let galaxyView = null;
async function renderGalaxy() {
  setNav("constellation");
  view.className = "view wide";
  if (galaxyView) { galaxyView.destroy(); galaxyView = null; }
  view.innerHTML = `
    <div class="constellation-wrap">
      <canvas id="constellation-canvas"></canvas>
      <div class="constellation-hud">
        <div class="searchbar">
          <input id="cq" placeholder="descend to an entity…">
        </div>
        <div class="hud-card" id="hud-card"><div class="meta">charting…</div></div>
      </div>
      <div class="hud-hint">suns = sources · planets = who they cite · drag orbits ·
        click a planet to descend · spin the dial to watch the galaxy form
        · <button class="ambient-btn" id="ambient-btn" title="ambient hearth mode">✦ ambient</button></div>
      <div class="const-bottom"><div id="const-dial"></div></div>
    </div>`;
  const input = document.getElementById("cq");
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && input.value.trim())
      location.hash = `#/constellation/${encodeURIComponent(input.value.trim())}`;
  });
  const wrapEl = view.querySelector(".constellation-wrap");
  document.getElementById("ambient-btn")?.addEventListener("click", () => {
    wrapEl.classList.add("ambient");
    wrapEl.requestFullscreen?.();
  });
  const { mount } = await import("/assets/galaxy.js");
  galaxyView = await mount(document.getElementById("constellation-canvas"), {
    // a person stays IN context — their card pops over the stars
    onPerson: (n) => window.__openPersonModal?.(n),
    // anything else descends to Memories about them (+source when
    // clicked inside a sun's system)
    onMemories: (n, source) => {
      window.__browsePrefill?.({ q: n, from: "", to: "", place: "" });
      browseState?.episodes?.sel?.source?.clear?.();
      if (source) browseState?.episodes?.sel?.source?.add?.(source);
      location.hash = "#/browse/episodes";
    },
    onBrowseSource: (source) => {
      window.__browsePrefill?.({ q: "", from: "", to: "", place: "" });
      browseState?.episodes?.sel?.source?.clear?.();
      browseState?.episodes?.sel?.source?.add?.(source);
      location.hash = "#/browse/episodes";
    },
    onDay: (day) => (location.hash = `#/day/${day}`),
    onNote: (id) => window.__openNoteById?.(id),
    onNotesSun: () => window.__openNotesPanel?.(),
    hud: document.getElementById("hud-card"),
    api,
    esc,
  });
  window.__galaxy = galaxyView; // verification hook
  {
    const { mountDial } = await import("/assets/dial.js");
    mountDial(document.getElementById("const-dial"), {
      from: undefined,
      to: new Date().toISOString().slice(0, 10),
      compact: true,
      filters: () => ({}),
      onWindow: (from, to) => galaxyView?.setWindow?.(from, to),
      onPick: () => {},
    });
  }
}

async function renderConstellation(name) {
  if (!name) return renderGalaxy();
  setNav("constellation");
  view.className = "view wide";
  const focus = name;
  view.innerHTML = `
    <div class="constellation-wrap">
      <canvas id="constellation-canvas"></canvas>
      <div class="constellation-hud">
        <div class="searchbar">
          <input id="cq" placeholder="focus an entity…" value="${esc(focus)}">
        </div>
        <div class="hud-card" id="hud-card"><div class="meta">loading…</div></div>
      </div>
      <div class="navpad" id="navpad">
        <button data-pan="rotl" title="orbit left ( [ )">⟲</button>
        <button data-pan="up" title="pan up">↑</button>
        <button data-pan="rotr" title="orbit right ( ] )">⟳</button>
        <button data-pan="left" title="pan left">←</button>
        <button data-pan="home" title="recenter">⌂</button>
        <button data-pan="right" title="pan right">→</button>
        <button data-pan="tiltu" title="orbit up">⤒</button>
        <button data-pan="down" title="pan down">↓</button>
        <button data-pan="tiltd" title="orbit down">⤓</button>
        <div class="zoomcol">
          <button data-pan="in" title="zoom in">＋</button>
          <button data-pan="out" title="zoom out">－</button>
          <button data-pan="tour" id="tour-btn" title="tour mode: slow orbit">▶</button>
        </div>
      </div>
      <div class="hud-hint"><a href="#/constellation" style="color:inherit">◌ whole galaxy</a>
        · drag orbits · ⟲⟳ rotate around the center · ▶ tour · click refocuses · double-click opens the dossier
        · <button class="ambient-btn" id="ambient-btn" title="ambient hearth mode — fullscreen, slow orbit">✦ ambient</button></div>
      <div class="const-bottom">
        <div id="const-trail"></div>
        <div id="const-dial"></div>
      </div>
    </div>`;
  const input = document.getElementById("cq");
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter")
      location.hash = `#/constellation/${encodeURIComponent(input.value)}`;
  });

  // The wandering trail: stars visited this session, retraceable.
  {
    const trail = JSON.parse(sessionStorage.getItem("hl-const-trail") || "[]");
    if (focus && trail[trail.length - 1] !== focus) trail.push(focus);
    sessionStorage.setItem("hl-const-trail", JSON.stringify(trail.slice(-8)));
    const host = document.getElementById("const-trail");
    if (trail.length > 1) {
      host.innerHTML = `<div class="const-trail">${trail.slice(0, -1).map((n) =>
        `<a href="#/constellation/${encodeURIComponent(n)}" class="trail-chip">${esc(n)}</a>`
      ).join('<span class="trail-sep">→</span>')}<span class="trail-sep">→</span><span class="trail-here">${esc(focus)}</span></div>`;
    }
  }

  const wrapEl = view.querySelector(".constellation-wrap");
  document.getElementById("ambient-btn")?.addEventListener("click", () => {
    wrapEl.classList.add("ambient");
    wrapEl.requestFullscreen?.();
    document.getElementById("tour-btn")?.click();
  });
  document.addEventListener("fullscreenchange", () => {
    if (!document.fullscreenElement) wrapEl?.classList.remove("ambient");
  }, { once: true });

  const { mount } = await import("/assets/constellation.js");
  const sky = await mount(document.getElementById("constellation-canvas"), focus, {
    onFocus: (n) => (location.hash = `#/constellation/${encodeURIComponent(n)}`),
    onOpen: (n) => (location.hash = `#/entity/${encodeURIComponent(n)}`),
    hud: document.getElementById("hud-card"),
    api,
    esc,
  });

  // The Dial as the sky's time machine: spin to an era and the
  // constellation re-lights by that window's evidence (restyle, not
  // rebuild — the layout holds still while the light changes).
  {
    const today = new Date();
    const iso = (d) => d.toISOString().slice(0, 10);
    const yearAgo = new Date(today.getTime() - 365 * 86400000);
    const { mountDial } = await import("/assets/dial.js");
    mountDial(document.getElementById("const-dial"), {
      from: iso(yearAgo),
      to: iso(today),
      compact: true,
      filters: () => ({}),
      onWindow: (from, to) => sky?.setWindow?.(from, to),
      onPick: () => {},
    });
    window.__sky = sky; // verification hook
    await sky?.setWindow?.(iso(yearAgo), iso(today));
  }
}

/* ————— tend: the 30-second gardening glance ————— */
async function renderTend() {
  setNav("tend");
  view.innerHTML = `
    <span class="eyebrow">tend</span>
    <h1 class="pagehead">Tend the fire</h1>
    <p class="subhead">Everything the brain wants a human yes/no on. This is the only
      gardening Hearthloom will ever ask of you.</p>
    <div id="tend-trips"></div>
    <div id="tend-reviews"></div>
    <div id="tend-quarantine"></div>
    <div id="tend-lint"></div>
    <div id="tend-selfmodel"></div>
    <div id="tend-opportunities"></div>
    <div id="tend-intents"></div>`;

  const [reviews, quarantine, intents, selfmodel, opportunities, lint] = await Promise.all([
    api("/v1/reviews"), api("/v1/quarantine"), api("/v1/intents"),
    api("/v1/selfmodel"), api("/v1/opportunities"),
    api("/v1/lint").catch(() => ({ findings: [] })),
  ]);

  // "Were you in the car?" — a car's drive the evidence couldn't settle
  // (v60). Yes/no only; the car's settings decide who drove.
  const tripsEl = document.getElementById("tend-trips");
  const paintTrips = async () => {
    const res = await api("/v1/trips/questions").catch(() => ({ pending: [] }));
    const qs = res.pending || [];
    if (!qs.length) { tripsEl.innerHTML = ""; return; }
    tripsEl.innerHTML = `
      <div class="section-title">were you in the car? <span class="statchip">${qs.length}</span></div>
      ${qs.map((q) => `
        <div class="card tend-card" data-trip="${esc(q.key)}">
          <p>${esc(q.summary)}</p>
          <div class="tend-actions">
            <button class="btn" data-ans="yes">yes</button>
            <button class="btn" data-ans="no">no</button>
            <button class="btn btn-quiet" data-ans="dismiss">don't ask about this one</button>
          </div>
        </div>`).join("")}`;
    tripsEl.querySelectorAll("[data-trip] [data-ans]").forEach((btn) => {
      btn.onclick = async () => {
        const card = btn.closest("[data-trip]");
        card.querySelectorAll("button").forEach((b) => { b.disabled = true; });
        try {
          await apiPost(`/v1/trips/questions/${encodeURIComponent(card.dataset.trip)}`, { answer: btn.dataset.ans });
          card.remove();
          if (!tripsEl.querySelector("[data-trip]")) tripsEl.innerHTML = "";
        } catch (e) {
          card.querySelectorAll("button").forEach((b) => { b.disabled = false; });
          card.insertAdjacentHTML("beforeend", `<p class="meta">${esc(e.message)}</p>`);
        }
      };
    });
  };
  paintTrips();

  const reviewsEl = document.getElementById("tend-reviews");
  const allPending = reviews.pending || [];
  // Search reaches the WHOLE queue (names + identities), not just the
  // dozen on screen — matches render up to the same cap.
  let tendQuery = "";
  const matchesQuery = (r) => {
    if (!tendQuery) return true;
    const hay = [r.entity_a?.name, r.entity_b?.name,
      ...(r.attributes_a || []).map(([, v]) => v),
      ...(r.attributes_b || []).map(([, v]) => v)]
      .join(" ").toLowerCase();
    return hay.includes(tendQuery);
  };
  const reviewCard = (r, parked) => {
    const side = (e, attrs, n) => `
      <span class="review-side" data-entity="${esc(e.id)}">
        <b>${esc(e.name)}</b> <span class="typebadge">${esc(e.entity_type)}</span>
        <span class="review-evidence">${attrs.length
          ? attrs.map(([k, v]) => `<span class="review-attr" title="${esc(k)} — × removes it from ${esc(e.name)}">${esc(v)}
              <button class="review-attr-rm" data-kind="${esc(k)}" data-value="${esc(v)}">×</button></span>`).join("")
          : `<span class="review-attr dim">no identities</span>`}
          <span class="review-attr dim">${n} episode${n === 1 ? "" : "s"}</span></span>
      </span>`;
    return `
      <div class="card tend-card" data-review="${esc(r.id)}">
        <div class="tend-pair">
          ${side(r.entity_a, r.attributes_a || [], r.episodes_a ?? 0)}
          <span class="edge-rel">≟</span>
          ${side(r.entity_b, r.attributes_b || [], r.episodes_b ?? 0)}
        </div>
        <div class="meta">${esc(r.reason)}</div>
        <div class="tend-actions">
          <button class="btn" data-act="same">same — merge</button>
          <button class="btn" data-act="distinct">different</button>
          ${parked
            ? `<button class="btn btn-quiet" data-act="reopen">back to the queue</button>`
            : `<button class="btn btn-quiet" data-act="unknown">don't know</button>`}
        </div>
      </div>`;
  };
  const paintReviews = () => {
    const filtered = allPending.filter(matchesQuery);
    const pending = filtered.slice(0, 12);
    reviewsEl.innerHTML = `
    <div class="section-title">same or different? <span class="statchip">${allPending.length}</span>
      ${allPending.length ? `<button class="btn btn-quiet" id="tend-retriage"
        title="auto-clear pairs that just share a first name (conflicting surnames)"
        style="float:right">tidy the queue</button>` : ""}</div>
    ${allPending.length ? `<div class="searchbar" style="margin-bottom:var(--s2)">
      <input id="tend-q" placeholder="search the whole queue — names, emails…"
        value="${esc(tendQuery)}"></div>` : ""}
    ${allPending.length ? "" : `<p class="meta" style="color:var(--ink-faint)">no identity questions.</p>`}
    ${tendQuery && !filtered.length ? `<p class="meta" style="color:var(--ink-faint)">nothing in the queue matches "${esc(tendQuery)}".</p>` : ""}
    ${filtered.length > pending.length
      ? `<p class="meta" style="color:var(--ink-faint)">showing ${pending.length} of ${filtered.length} — the rest surface as you answer.</p>` : ""}
    ${pending.map((r) => reviewCard(r, false)).join("")}
    <details class="tend-confirmed" id="tend-unknowns"><summary class="meta">the don't-know pile</summary>
      <div id="tend-unknowns-list"><div class="meta">…</div></div></details>`;
    reviewsEl.querySelector("#tend-retriage")?.addEventListener("click", async (e) => {
      const btn = e.currentTarget;
      btn.disabled = true; btn.textContent = "tidying…";
      const res = await apiPost("/v1/reviews/retriage", { actor: "human:dashboard" }).catch(() => null);
      toast(res ? `cleared ${res.cleared} same-first-name pair(s)` : "couldn't tidy");
      renderTend();
    });
    const q = reviewsEl.querySelector("#tend-q");
    q?.addEventListener("input", () => {
      tendQuery = q.value.trim().toLowerCase();
      const at = q.selectionStart;
      paintReviews();
      const q2 = reviewsEl.querySelector("#tend-q");
      q2?.focus();
      q2?.setSelectionRange(at, at);
    });
    bindReviewCards(reviewsEl, false);
    const unknownsEl = document.getElementById("tend-unknowns");
    unknownsEl.addEventListener("toggle", async () => {
      if (!unknownsEl.open) return;
      const listEl = document.getElementById("tend-unknowns-list");
      const parked = (await api("/v1/reviews?state=unknown")).pending || [];
      listEl.innerHTML = parked.length
        ? parked.map((r) => reviewCard(r, true)).join("")
        : `<p class="meta" style="color:var(--ink-faint)">nothing parked.</p>`;
      bindReviewCards(listEl, true);
    });
  };

  const bindReviewCards = (rootEl, parked) => {
    rootEl.querySelectorAll(".tend-card[data-review]").forEach((card) => {
      card.querySelectorAll("[data-act]").forEach((button) => {
        button.onclick = async () => {
          button.disabled = true;
          await apiPost(`/v1/reviews/${card.dataset.review}`,
            { actor: "human:dashboard", resolution: button.dataset.act });
          if (parked && button.dataset.act === "reopen") renderTend();
          else card.remove();
          refreshChips();
        };
      });
      // "this identity is not them": detach one attribute from one side
      card.querySelectorAll(".review-attr-rm").forEach((rm) => {
        rm.onclick = async (e) => {
          e.stopPropagation();
          const chip = rm.closest(".review-attr");
          await apiPost("/v1/attributes/remove", {
            actor: "human:dashboard",
            entity_id: rm.closest(".review-side").dataset.entity,
            kind: rm.dataset.kind,
            value: rm.dataset.value,
          });
          chip.remove();
        };
      });
    });
  };
  paintReviews();

  const quarantineEl = document.getElementById("tend-quarantine");
  const proposals = quarantine.pending || [];
  const decided = quarantine.resolved || [];
  // Where an accepted memory landed, so a past decision is traceable.
  const landedIn = (p) => p.status === "rejected" ? "rejected"
    : p.kind === "topic_page" ? "→ saved to Notes" : "→ saved to Memories";
  // How accepting it would land, as the server decides it (http.rs
  // resolve_quarantine): never as the owner's own words.
  const proposalLanding = (p) => {
    const c = (p.content && typeof p.content === "object") ? p.content : {};
    if (p.kind === "topic_page" && p.actor === "agent:consolidation") return "lands as a topic page in Notes";
    if (c.promoted_from && c.source) return `lands as ${c.source} (promoted) · agent trust`;
    return "lands as a memory · agent trust";
  };
  const decidedText = (p) => {
    const c = p.content;
    const t = (c && typeof c === "object") ? (c.title || c.text || "") : String(c ?? "");
    return String(t).replace(/\s+/g, " ").slice(0, 90);
  };
  quarantineEl.innerHTML = `
    <div class="section-title" style="margin-top:var(--s6)">proposed memories
      <span class="statchip">${proposals.length}</span></div>
    ${proposals.length ? "" : `<p class="meta" style="color:var(--ink-faint)">agents have proposed nothing.</p>`}
    ${proposals.map((p) => `
      <div class="card tend-card" data-proposal="${esc(p.id)}">
        <span class="kind">${esc(p.kind)} · from ${esc(p.actor)} · ${esc(proposalLanding(p))}</span>
        <div class="md">${md(typeof p.content === "object" ? (p.content.text || JSON.stringify(p.content)) : String(p.content))}</div>
        <div class="tend-actions">
          <button class="btn" data-act="accept">accept into the brain</button>
          <button class="btn" data-act="reject">reject</button>
        </div>
      </div>`).join("")}
    ${decided.length ? `
      <details style="margin-top:var(--s3)">
        <summary class="meta" style="cursor:pointer">recently decided (${decided.length}) — where they went</summary>
        ${decided.map((p) => `
          <div class="meta" style="display:flex;gap:var(--s2);padding:var(--s1) 0;border-bottom:1px solid var(--line)">
            <span class="statchip" style="color:${p.status === "accepted" ? "var(--accent)" : "var(--ink-faint)"}">${esc(p.status)}</span>
            <span style="flex:1;color:var(--ink)">${esc(decidedText(p))}</span>
            <span style="white-space:nowrap">${esc(landedIn(p))} · ${esc(p.proposed_at.slice(0, 10))}</span>
          </div>`).join("")}
      </details>` : ""}`;
  quarantineEl.querySelectorAll(".tend-card").forEach((card) => {
    card.querySelectorAll("button").forEach((button) => {
      button.onclick = async () => {
        button.disabled = true;
        await apiPost(`/v1/quarantine/${card.dataset.proposal}`,
          { actor: "human:dashboard", resolution: button.dataset.act });
        card.remove();
        refreshChips();
      };
    });
  });

  /* — the record lint: report-only findings; a human closes each one — */
  const lintEl = document.getElementById("tend-lint");
  const findings = lint.findings || [];
  const LINT_LABEL = {
    contradiction: "contradiction",
    superseded: "superseded",
    orphan: "orphans",
    gap: "coverage gap",
  };
  lintEl.innerHTML = `
    <div class="section-title" style="margin-top:var(--s6)">the record lint
      ${findings.length ? `<span class="statchip">${findings.length}</span>` : ""}</div>
    ${findings.length ? "" : `<p class="meta" style="color:var(--ink-faint)">nothing flagged — the sweep runs weekly (☰ → Settings).</p>`}
    ${findings.map((f) => `
      <div class="card tend-card" data-lint="${esc(f.id)}">
        <span class="kind">${esc(LINT_LABEL[f.kind] || f.kind)} · ${esc(f.created_at.slice(0, 10))}</span>
        <div><b>${esc(f.title)}</b></div>
        <div class="md">${md(f.body)}</div>
        <div class="tend-actions">
          <button class="btn" data-act="fixed">fixed</button>
          <button class="btn" data-act="not_an_issue">not an issue</button>
        </div>
      </div>`).join("")}`;
  lintEl.querySelectorAll(".tend-card[data-lint]").forEach((card) => {
    card.querySelectorAll("button").forEach((button) => {
      button.onclick = async () => {
        button.disabled = true;
        await apiPost(`/v1/lint/${card.dataset.lint}/resolve`,
          { actor: "human:dashboard", resolution: button.dataset.act });
        card.remove();
        refreshChips();
      };
    });
  });

  /* — who you are: the self-model, human-gated — */
  const selfmodelEl = document.getElementById("tend-selfmodel");
  const props = selfmodel.propositions || [];
  const proposed = props.filter((p) => p.status === "proposed");
  const confirmed = props.filter((p) => p.status === "confirmed");
  selfmodelEl.innerHTML = `
    <div class="section-title" style="margin-top:var(--s6)">who you are
      ${proposed.length ? `<span class="statchip">${proposed.length} proposed</span>` : ""}</div>
    ${proposed.map((p) => `
      <div class="card tend-card" data-prop="${esc(p.id)}">
        <span class="kind">${esc(p.sphere)} · ${esc(p.kind)} · ${p.origin === "inferred"
          ? `inferred from ${p.evidence.length} episode${p.evidence.length === 1 ? "" : "s"}` : "declared"}${
          p.back_after ? ` · back to check after you retired “${esc(p.back_after)}”` : ""}</span>
        <div class="prop-text" contenteditable="true" spellcheck="false">${esc(p.text)}</div>
        <div class="tend-actions">
          <button class="btn" data-act="confirm">that's me</button>
          <button class="btn" data-act="reject">no</button>
        </div>
      </div>`).join("")}
    ${confirmed.length ? `
      <details class="tend-confirmed"><summary class="meta">${confirmed.length} confirmed</summary>
      ${confirmed.map((p) => `
        <div class="card tend-card" data-prop="${esc(p.id)}" style="opacity:.85">
          <span class="kind">${esc(p.sphere)} · ${esc(p.kind)} · ${esc(p.origin)}</span>
          <div>${esc(p.text)}</div>
          <div class="tend-actions"><button class="btn" data-act="retire">no longer true</button></div>
        </div>`).join("")}</details>` : ""}
    <div class="searchbar" style="margin-top:var(--s3)">
      <select id="prop-sphere">${["work", "personal", "family", "health", "private"]
        .map((s) => `<option>${s}</option>`).join("")}</select>
      <select id="prop-kind">${["identity", "offering", "seeking", "strength", "constraint"]
        .map((k) => `<option>${k}</option>`).join("")}</select>
      <input id="prop-text" placeholder="state something true about yourself…">
      <button class="btn" id="prop-add">declare</button>
    </div>`;
  selfmodelEl.querySelectorAll(".tend-card[data-prop]").forEach((card) => {
    card.querySelectorAll("button").forEach((button) => {
      button.onclick = async () => {
        button.disabled = true;
        const body = { actor: "human:dashboard", action: button.dataset.act };
        const edited = card.querySelector(".prop-text")?.textContent.trim();
        if (button.dataset.act === "confirm" && edited) body.text = edited;
        await apiPost(`/v1/selfmodel/${card.dataset.prop}/resolve`, body);
        card.remove();
        refreshChips();
      };
    });
  });
  document.getElementById("prop-add").onclick = async () => {
    const text = document.getElementById("prop-text").value.trim();
    if (!text) return;
    await apiPost("/v1/selfmodel", {
      actor: "human:dashboard",
      sphere: document.getElementById("prop-sphere").value,
      kind: document.getElementById("prop-kind").value,
      text,
    });
    renderTend();
  };

  /* — opportunities: seeking × new episodes — */
  const oppsEl = document.getElementById("tend-opportunities");
  const opps = opportunities.opportunities || [];
  oppsEl.innerHTML = `
    <div class="section-title" style="margin-top:var(--s6)">opportunities
      ${opps.length ? `<span class="statchip">${opps.length}</span>` : ""}</div>
    ${opps.length ? "" : `<p class="meta" style="color:var(--ink-faint)">nothing matches what you're seeking — declare a “seeking” above to start the watch.</p>`}
    ${opps.map((o) => `
      <div class="card tend-card" data-opp="${esc(o.id)}">
        <span class="kind">seeking: ${esc(o.seeking_text)}</span>
        <div><a href="#/episode/${esc(o.episode_id)}"><b>${esc(o.episode_title)}</b></a>
          ${o.episode_time ? `<span class="meta"> · ${esc(o.episode_time.slice(0, 10))}</span>` : ""}</div>
        ${o.reason ? `<div class="meta">${esc(o.reason)}</div>` : ""}
        <div class="tend-actions">
          <button class="btn" data-act="keep">worth keeping</button>
          <button class="btn" data-act="dismiss">not this</button>
        </div>
      </div>`).join("")}`;
  oppsEl.querySelectorAll(".tend-card[data-opp]").forEach((card) => {
    card.querySelectorAll("button").forEach((button) => {
      button.onclick = async () => {
        button.disabled = true;
        await apiPost(`/v1/opportunities/${card.dataset.opp}/resolve`,
          { actor: "human:dashboard", action: button.dataset.act });
        card.remove();
        refreshChips();
      };
    });
  });

  const intentsEl = document.getElementById("tend-intents");
  const paintIntents = (list) => {
    intentsEl.innerHTML = `
      <div class="section-title" style="margin-top:var(--s6)">standing watches</div>
      <div class="searchbar">
        <input id="intent-name" placeholder="name (e.g. fusion-news)" style="flex:0.6">
        <input id="intent-query" placeholder="watch for… (FTS query)">
        <button class="btn" id="intent-add">watch</button>
      </div>
      ${list.map((intent) => `
        <div class="card tend-card">
          <div class="tend-pair"><b>${esc(intent.name)}</b>
            <span class="mono meta">${esc(intent.query)}</span>
            ${intent.unseen_matches ? `<span class="statchip" style="color:var(--accent)">${intent.unseen_matches} new</span>` : ""}
          </div>
          <div class="tend-actions">
            <a class="btn" href="#/search/${encodeURIComponent(intent.query)}">view matches</a>
            <button class="btn" data-rm="${esc(intent.name)}">stop watching</button>
          </div>
        </div>`).join("")}`;
    document.getElementById("intent-add").onclick = async () => {
      const name = document.getElementById("intent-name").value.trim();
      const query = document.getElementById("intent-query").value.trim();
      if (!name || !query) return;
      await apiPost("/v1/intents", { actor: "human:dashboard", name, query });
      renderTend();
    };
    intentsEl.querySelectorAll("[data-rm]").forEach((button) => {
      button.onclick = async () => {
        await apiDelete(`/v1/intents/${encodeURIComponent(button.dataset.rm)}?actor=human:dashboard`);
        renderTend();
      };
    });
  };
  paintIntents(intents.intents || []);
}

/* ————— temporal ribbons ————— */
async function renderRibbons() {
  setNav("ribbons");
  view.innerHTML = `
    <span class="eyebrow">temporal ribbons</span>
    <h1 class="pagehead">What was true, when</h1>
    <p class="subhead">Each ribbon is an entity's presence in your life — brighter months
      carry more evidence. The visual form of the bi-temporal graph.</p>
    <div id="ribbon-lanes" class="loading">weaving…</div>`;
  const data = await api("/v1/ribbons");
  const lanes = data.ribbons || [];
  if (!lanes.length) {
    document.getElementById("ribbon-lanes").innerHTML =
      `<div class="empty">not enough history yet</div>`;
    return;
  }
  const months = data.months;
  const max = Math.max(...lanes.flatMap((l) => l.counts));
  const laneHtml = lanes.map((lane) => {
    const hue = { Person: "#ffb26b", Project: "#ffd166", Place: "#7fd7c4", Org: "#ef8354" }[lane.type] || "#b8b2a6";
    const cells = lane.counts.map((count, i) => {
      const opacity = count ? 0.18 + 0.82 * Math.sqrt(count / max) : 0;
      return `<i title="${months[i]}: ${count}" style="background:${hue};opacity:${opacity}"></i>`;
    }).join("");
    return `
      <div class="ribbon-row">
        <a class="ribbon-name" href="#/entity/${encodeURIComponent(lane.name)}">${esc(lane.name)}</a>
        <div class="ribbon-band">${cells}</div>
      </div>`;
  }).join("");
  document.getElementById("ribbon-lanes").className = "";
  document.getElementById("ribbon-lanes").innerHTML = `
    <div class="ribbon-head"><span></span>
      <div class="ribbon-scale"><span>${months[0]}</span><span>${months[months.length - 1]}</span></div>
    </div>
    ${laneHtml}`;
}

/* ————— welcome (first-run wizard) ————— */
async function renderWelcome() {
  setNav("");
  const summary = await api("/v1/monitor/summary").catch(() => ({}));
  const cloud = summary.this_node === "cloud";
  // An OPEN node (no admin yet) that isn't provisioner-managed gets the
  // protect-this-brain step — the wizard is scripts/bootstrap.sh for
  // browser users: the admin exists → the door locks (the posture rule).
  const me = await fetch("/v1/auth/me").then((r) => r.json()).catch(() => ({}));
  const showBootstrap = me.auth === "open" && !me.provisioner;
  let step = 0;
  view.innerHTML = `
    <span class="eyebrow">first light</span>
    <h1 class="pagehead">Welcome to your second brain</h1>
    <p class="subhead">${cloud
      ? "This is your always-on cloud brain — reachable from anywhere, yours alone."
      : "Hearthloom keeps your memory in one file on your own disk — no cloud required, every answer cited."}
      A few minutes of kindling; skip anything and come back later.</p>

    <div class="card">
      <div class="section-title">restoring from an export?</div>
      <p class="meta">If you have a Hearthloom export zip, restore it now — it rebuilds
        this node exactly as exported (memories, notes, files, and your old sign-in).
        Only possible while this record is still empty; skip this if you're starting fresh.</p>
      <div class="tend-actions">
        <button class="btn btn-quiet" id="wz-restore">upload an export zip…</button>
      </div>
      <p class="meta" id="wz-restore-note" hidden></p>
    </div>

    ${showBootstrap ? `
    <div class="card">
      <div class="section-title">${++step} · protect this brain</div>
      <p class="meta">Right now anyone who can reach this page can read the record.
        Choose the email and password that become its one login — from that moment
        every visit (browser, CLI, API) signs in first. The password never leaves
        this node.</p>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="wz-admin-email" type="email" placeholder="your email" autocomplete="email" style="flex:1.4; min-width:180px">
        <input id="wz-admin-pass" type="password" placeholder="password (min 8 chars)" autocomplete="new-password" style="flex:1">
        <input id="wz-admin-pass2" type="password" placeholder="again" autocomplete="new-password" style="flex:1">
        <button class="btn" id="wz-admin-create">create my login</button>
        <button class="btn btn-quiet" id="wz-admin-skip">skip</button>
      </div>
      <p class="meta" id="wz-admin-note" hidden></p>
    </div>` : ""}

    <div class="card">
      <div class="section-title">${++step} · who are you?</div>
      <p class="meta">Your name and emails are how every source — commits, invites, mail,
        messages — resolves to <b>one person: you</b>. A work email labels its material
        <i>work</i>; a personal one stays <i>personal</i>. Do this before the first import
        and the record sorts itself.</p>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="wz-name" placeholder="your name" style="flex:1">
      </div>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="wz-email-1" placeholder="you@personal…" style="flex:1">
        <select id="wz-scope-1"><option value="personal">personal</option><option value="work">work</option></select>
      </div>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="wz-email-2" placeholder="you@work… (optional)" style="flex:1">
        <select id="wz-scope-2"><option value="work">work</option><option value="personal">personal</option></select>
        <button class="btn" id="wz-claim">that's me</button>
      </div>
      <p class="meta" id="wz-claim-note" hidden></p>
    </div>

    <div class="section-title" style="margin-top:var(--s4)">${++step} · connect the thinking
      <span class="meta" style="font-weight:400">— optional; only Ask, digests, and
        idle-time understanding need it</span></div>
    <div id="wz-ai"><div class="loading">…</div></div>

    <div class="section-title" style="margin-top:var(--s4)">${++step} · give search meaning
      <span class="meta" style="font-weight:400">— optional; a one-time ~90MB local model download</span></div>
    <div id="wz-embeddings"></div>

    <div id="wz-gmail-step" hidden>
      <div class="section-title" style="margin-top:var(--s4)">${++step} · keep mail current
        <span class="meta" style="font-weight:400">— optional; live Gmail pull past your last export</span></div>
      <div id="wz-gmail"></div>
    </div>

    <div class="card">
      <div class="section-title">${++step} · one hearth or two?</div>
      <p class="meta">
        <label><input type="radio" name="wz-mode" value="solo" checked>
          <b>This is my first node</b> — start here, add a counterpart anytime.</label><br>
        <label><input type="radio" name="wz-mode" value="peer">
          <b>I already run another node</b> — keep the two in sync.</label>
      </p>
      <div id="wz-peer" hidden>
        ${cloud ? `
          <p class="meta">Your other machine does the connecting (it can reach this node;
            this node can't reach it). Mint it a key here, then paste the key on that
            machine's <b>☰ → Peering</b> page along with this address.</p>
          <button class="btn" id="wz-mint">mint a pairing key for my other machine</button>
          <div id="wz-mint-out"></div>` : `
          <p class="meta">Enter your always-on node's address and a <b>peer</b>-scope key
            minted on its ☰ → API Keys page:</p>
          <div class="searchbar" style="flex-wrap:wrap">
            <input id="wz-peer-url" placeholder="https://your-node…" style="flex:1.2">
            <input id="wz-peer-key" placeholder="hl_…" style="flex:1">
            <button class="btn" id="wz-pair">pair</button>
          </div>
          <p class="meta" id="wz-pair-note" hidden></p>`}
      </div>
    </div>

    <div class="section-title" style="margin-top:var(--s4)">${++step} · bring your phone
      <span class="meta" style="font-weight:400">— the Hearthloom app pairs with one scan</span></div>
    <div id="wz-device"></div>

    <div class="card">
      <div class="section-title">${++step} · feed the record</div>
      ${cloud ? "" : `
      <p class="meta"><b>Instant, no permissions needed</b> — already on this machine:</p>
      <div class="tend-actions" style="flex-wrap:wrap">
        <button class="btn wz-local" data-adapter="claude-code">AI coding sessions</button>
        <button class="btn wz-local" data-adapter="chrome">browsing history</button>
        <span class="meta">a git repo:</span>
        <input id="wz-git" placeholder="/path/to/repo" style="flex:1; min-width:160px">
        <button class="btn wz-local" data-adapter="git">ingest it</button>
        <button class="btn btn-quiet" id="wz-git-scan">scan for my repos</button>
      </div>
      <div id="wz-git-found"></div>
      <p class="meta" id="wz-local-note" hidden></p>`}
      <div id="wz-firstvalue-out" hidden></div>
      <p class="meta" style="margin-top:var(--s2)">Then the big ones: mail exports, calendars,
        chat logs, health data, photos, social archives, a markdown vault — each with a
        one-click upload, Downloads auto-detect, or instructions on
        <a href="#/integrations">Integrations</a>. Everything is reversible and
        private-by-default.</p>
      <div class="tend-actions">
        <!-- Every onward CTA (.wz-exit) marks the wizard DONE before
             navigating — they are the wizard's own exits, not abandonment.
             Without this, the leave-setup intercept fired on the wizard's
             own "open the imports" button and bounced the user back to a
             blank re-rendered wizard (a hosted tenant's
             onboarding loop, 2026-07-28). The click listener is wired
             below the template. -->
        <a class="btn wz-exit" href="#/integrations">open the imports</a>
        <a class="btn btn-quiet wz-exit" href="#/day" id="wz-firstvalue" hidden>show me my own last week →</a>
        <a class="btn btn-quiet wz-exit" href="#/desk" id="wz-done">I'm all set — take me to the Desk →</a>
      </div>
    </div>`;
  // The wizard's exits: mark setup done on the click itself, before the
  // link's hash change reaches the leave-setup intercept.
  view.querySelectorAll(".wz-exit").forEach((exit) =>
    exit.addEventListener("click", () => setOnboarding("done")));
  // restore-from-export: fresh nodes only; the daemon restarts itself
  view.querySelector("#wz-restore")?.addEventListener("click", () => {
    const note = view.querySelector("#wz-restore-note");
    const input = document.createElement("input");
    input.type = "file";
    input.accept = ".zip";
    input.onchange = async () => {
      const file = input.files?.[0];
      if (!file) return;
      note.hidden = false;
      note.textContent = `uploading ${file.name} (${Math.round(file.size / 1e6)} MB)…`;
      try {
        const res = await fetch("/v1/import/restore", { method: "POST", body: file });
        const data = await res.json();
        if (!res.ok) { note.textContent = `✗ ${data.error || res.statusText}`; return; }
        note.textContent = `✓ ${data.note} — this page will reload once the node is back`;
        const wait = setInterval(async () => {
          try {
            const h = await fetch("/health");
            if (h.ok) { clearInterval(wait); location.reload(); }
          } catch { /* still restarting */ }
        }, 2000);
      } catch (err) {
        note.textContent = `✗ ${err.message}`;
      }
    };
    input.click();
  });
  // step 1: identity claims — the self Person + scoped emails, BEFORE
  // the first import so scope inference labels material correctly.
  view.querySelector("#wz-claim")?.addEventListener("click", async () => {
    const note = view.querySelector("#wz-claim-note");
    const name = view.querySelector("#wz-name").value.trim();
    note.hidden = false;
    if (!name) { note.textContent = "✗ your name is the one required field"; return; }
    const claims = [1, 2]
      .map((i) => ({
        value: view.querySelector(`#wz-email-${i}`).value.trim(),
        scope: view.querySelector(`#wz-scope-${i}`).value,
      }))
      .filter((c) => c.value.includes("@"));
    if (!claims.length) { note.textContent = "✗ add at least one email"; return; }
    note.textContent = "claiming…";
    try {
      for (const c of claims) {
        await apiPost("/v1/identity/claim", {
          actor: "human:onboarding", entity: name,
          kind: "email", value: c.value, scope: c.scope,
        });
      }
      note.textContent = `✓ ${claims.length} identit${claims.length === 1 ? "y" : "ies"} claimed —
        every future import resolves to you, scoped correctly`;
    } catch (err) {
      note.textContent = `✗ ${err.message}`;
    }
  });
  // The first-value moment: after anything real lands, show the user
  // their OWN last week inline — a recall, not a promise.
  const paintFirstValue = async () => {
    const out = view.querySelector("#wz-firstvalue-out");
    view.querySelector("#wz-firstvalue").hidden = false;
    if (!out || !out.hidden) return; // already shown
    const to = new Date().toISOString().slice(0, 10);
    const from = new Date(Date.now() - 7 * 864e5).toISOString().slice(0, 10);
    const data = await api(`/v1/recall?from=${from}&to=${to}&detail=summary`).catch(() => null);
    const eps = (data?.episodes || []).slice(0, 5);
    if (!eps.length) return;
    out.hidden = false;
    out.innerHTML = `
      <p class="meta" style="margin-top:var(--s2)"><b>Your own last week, already recalled:</b></p>
      ${eps.map((e) => `<p class="meta" style="margin:2px 0">· ${esc((e.event_time || "").slice(0, 10))}
        — ${esc(e.title || e.kind)} <span style="color:var(--ink-faint)">(${esc(e.source)})</span></p>`).join("")}`;
  };
  // step: instant local sources → the first-value moment.
  view.querySelectorAll(".wz-local").forEach((btn) => {
    btn.onclick = async () => {
      const note = view.querySelector("#wz-local-note");
      const adapter = btn.dataset.adapter;
      const path = adapter === "git" ? view.querySelector("#wz-git").value.trim() : "";
      note.hidden = false;
      if (adapter === "git" && !path) { note.textContent = "✗ enter the repo's path first"; return; }
      const label = btn.textContent;
      btn.disabled = true;
      btn.innerHTML = `<span class="spinner"></span> ingesting…`;
      const res = await apiPost("/v1/ingest/local",
        { actor: "human:onboarding", adapter, ...(path ? { path } : {}) })
        .catch((e) => ({ error: e.message }));
      btn.disabled = false;
      btn.textContent = label;
      note.textContent = res?.error
        ? `✗ ${res.error}`
        : `✓ ${res.summary || res.note || "ingested"} — search works on it already`;
      if (!res?.error) paintFirstValue();
    };
  });
  // repo auto-discovery: scan → confirm → ingest the checked ones.
  view.querySelector("#wz-git-scan")?.addEventListener("click", async (e) => {
    const scanBtn = e.target;
    const found = view.querySelector("#wz-git-found");
    scanBtn.disabled = true;
    scanBtn.innerHTML = `<span class="spinner"></span> scanning…`;
    const data = await api("/v1/ingest/discover-git").catch(() => null);
    scanBtn.disabled = false;
    scanBtn.textContent = "scan for my repos";
    const repos = data?.repos || [];
    if (!repos.length) {
      found.innerHTML = `<p class="meta">no git repos found under your home folder.</p>`;
      return;
    }
    found.innerHTML = `
      <p class="meta" style="margin-top:var(--s2)">${repos.length} repo${repos.length === 1 ? "" : "s"} found —
        the pre-checked ones look like yours (you authored commits, not a monster clone):</p>
      ${repos.map((r, i) => `
        <label class="meta" style="display:block; margin:2px 0">
          <input type="checkbox" class="wz-repo" data-path="${esc(r.path)}" ${r.recommended ? "checked" : ""}>
          <b>${esc(r.name)}</b> — ${r.commits} commits${r.authored ? "" : " · none authored by you"}
          <span style="color:var(--ink-faint)">${esc(r.path)}</span>
        </label>`).join("")}
      <div class="tend-actions" style="margin-top:var(--s2)">
        <button class="btn" id="wz-repo-ingest">ingest the checked repos</button>
      </div>
      <p class="meta" id="wz-repo-note" hidden></p>`;
    found.querySelector("#wz-repo-ingest").onclick = async () => {
      const picked = [...found.querySelectorAll(".wz-repo:checked")].map((c) => c.dataset.path);
      const note = found.querySelector("#wz-repo-note");
      note.hidden = false;
      if (!picked.length) { note.textContent = "✗ nothing checked"; return; }
      let done = 0;
      for (const path of picked) {
        note.textContent = `ingesting ${done + 1}/${picked.length} — ${path}…`;
        const res = await apiPost("/v1/ingest/local",
          { actor: "human:onboarding", adapter: "git", path })
          .catch((err) => ({ error: err.message }));
        if (!res?.error) done += 1;
      }
      note.textContent = `✓ ${done}/${picked.length} repos ingested — search works on them already`;
      if (done) paintFirstValue();
    };
  });
  // The Integrations cards ARE the wizard steps (AI provider, Gmail
  // live, pair-a-device): one implementation, mounted here too.
  import("/assets/integrations.js").then((intg) => {
    intg.initHelpers({ api, apiPost });
    intg.paintAnthropic(view.querySelector("#wz-ai")).catch(() => {});
    intg.paintEmbeddings(view.querySelector("#wz-embeddings")).catch(() => {});
    const gmailStep = view.querySelector("#wz-gmail-step");
    intg.paintGmail(view.querySelector("#wz-gmail")).then(() => {
      // paintGmail empties its host when the status endpoint is missing
      if (view.querySelector("#wz-gmail").innerHTML) gmailStep.hidden = false;
      else gmailStep.remove();
    }).catch(() => gmailStep.remove());
    intg.paintPair(view.querySelector("#wz-device"));
  }).catch(() => {});
  // The UI bootstrap: creates the admin and signs them straight in (the
  // endpoint sets the session cookie), so the wizard just continues —
  // but from here on, this record requires the login.
  view.querySelector("#wz-admin-create")?.addEventListener("click", async () => {
    const note = view.querySelector("#wz-admin-note");
    note.hidden = false;
    const pass = view.querySelector("#wz-admin-pass").value;
    if (pass !== view.querySelector("#wz-admin-pass2").value) {
      note.textContent = "✗ the passwords don't match";
      return;
    }
    note.textContent = "creating your login…";
    try {
      const res = await fetch("/v1/auth/bootstrap", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          email: view.querySelector("#wz-admin-email").value.trim(),
          password: pass,
        }),
      });
      const data = await res.json();
      if (!res.ok) { note.textContent = `✗ ${data.error || res.statusText}`; return; }
      note.textContent = `✓ ${data.bootstrapped} is signed in — this record now requires your login everywhere`;
      view.querySelectorAll("#wz-admin-email, #wz-admin-pass, #wz-admin-pass2, #wz-admin-create, #wz-admin-skip")
        .forEach((el) => { el.disabled = true; });
    } catch (err) {
      note.textContent = `✗ ${err.message}`;
    }
  });
  view.querySelector("#wz-admin-skip")?.addEventListener("click", (e) => {
    e.target.closest(".card").querySelector(".searchbar").hidden = true;
    const note = view.querySelector("#wz-admin-note");
    note.hidden = false;
    note.textContent = "skipped — the record stays open to anyone who can reach this page until a login exists; come back here anytime.";
  });
  view.querySelectorAll('[name="wz-mode"]').forEach((r) => r.onchange = () => {
    view.querySelector("#wz-peer").hidden =
      view.querySelector('[name="wz-mode"]:checked').value !== "peer";
  });
  view.querySelector("#wz-mint")?.addEventListener("click", async () => {
    const res = await apiPost("/v1/keys", {
      actor: "human:onboarding", name: "my-other-machine", scopes: ["peer"],
    }).catch((e) => ({ error: e.message }));
    view.querySelector("#wz-mint-out").innerHTML = res?.error
      ? `<p class="meta">✗ ${esc(res.error)}</p>`
      : `<div class="card" style="border-color:var(--accent)">
           <b>on your other machine: ☰ → Peering → pair</b>
           <div class="searchbar"><input value="${esc(res.secret)}" readonly></div>
           <p class="meta">address: <code>${esc(location.origin)}</code> · shown once</p>
         </div>`;
    // click selects the whole key, ready to copy
    view.querySelector("#wz-mint-out input")?.addEventListener("click", (e) => e.currentTarget.select());
  });
  view.querySelector("#wz-pair")?.addEventListener("click", async () => {
    const note = view.querySelector("#wz-pair-note");
    note.hidden = false; note.textContent = "checking the peer…";
    const res = await apiPost("/v1/peer", {
      actor: "human:onboarding", name: "cloud",
      url: view.querySelector("#wz-peer-url").value.trim(),
      key: view.querySelector("#wz-peer-key").value.trim(),
    }).catch((e) => ({ error: e.message }));
    note.textContent = res?.error ? `✗ ${res.error}` : "✓ paired — syncing starts within minutes.";
  });
}

/* ————— the masthead vault toggle (definitions were accidentally
   dropped in ef5e41b — a module-level ReferenceError here kills the
   WHOLE dashboard, so this block must stay above its first use) ————— */
const vaultButton = document.getElementById("vault-toggle");
let vaultLocked = true;
function paintVault(status) {
  const flipped = vaultLocked !== status.locked;
  vaultLocked = status.locked;
  // Every route to a lock change lands here (this button, the Privacy
  // card, the 30s poll that notices the timed relock).
  if (flipped) askVaultChanged().catch(() => {});
  if (status.locked) {
    vaultButton.textContent = "🔒";
    vaultButton.title = "the vault is locked — private is invisible everywhere. click to unlock for 15 minutes";
    vaultButton.classList.remove("open");
  } else {
    const minutes = Math.max(1, Math.round(status.seconds_remaining / 60));
    vaultButton.textContent = "🔓";
    vaultButton.title = `the vault is OPEN — relocks in ~${minutes} min. click to lock now`;
    vaultButton.classList.add("open");
  }
}
async function refreshVault() {
  try {
    paintVault(await api("/v1/vault"));
  } catch { /* daemon warming up */ }
}
vaultButton.onclick = async () => {
  vaultButton.disabled = true;
  try {
    paintVault(await apiPost("/v1/vault", {
      actor: "human:dashboard",
      action: vaultLocked ? "unlock" : "lock",
    }));
    bodyCache.clear(); // cached episode bodies may cross the lock boundary
    refreshChips();
    route(); // repaint: visible data changes with the lock
  } catch (err) {
    vaultButton.title = err.message;
  }
  vaultButton.disabled = false;
};
setInterval(refreshVault, 30_000);
refreshVault();

/* ————— privacy: rules + the vault, managed ————— */
async function renderPrivacy() {
  setNav("privacy");
  view.innerHTML = `
    <span class="eyebrow">privacy</span>
    <h1 class="pagehead">The vault</h1>
    <p class="subhead">Anything scoped <b>private</b> lives in the vault: locked (the default),
      it is excluded from every read path — search, recall, dossiers, constellation, ribbons,
      Ask, agents — with no trace that anything is hidden. Unlocks relock themselves.</p>
    <div class="digest" id="vault-card"><div class="meta">checking the lock…</div></div>
    <div class="section-title" style="margin-top:var(--s6)">full export</div>
    <div class="card" id="export-card">
      <p class="meta">One zip that can rebuild your entire record on a fresh node —
        the database snapshot, every memory's raw content, notes and media, plus a
        readable JSON copy of every table (exclusions are listed in its manifest,
        each with a reason). <b>Requires the vault unlocked</b>: an export contains
        everything, private included.</p>
      <div class="tend-actions">
        <button class="btn" id="export-start">prepare my export</button>
        <a class="btn" id="export-download" href="/v1/export/download" download hidden>⬇ download export</a>
      </div>
      <div id="export-progress" class="meta" style="margin-top:var(--s2)" hidden></div>
    </div>
    <div class="section-title" style="margin-top:var(--s6)">vaulted notes</div>
    <p class="meta" style="color:var(--ink-mute);max-width:60ch">Vault a single note with the
      <b>🔒 button in its editor</b>. The count shows always; the titles reveal only while the
      vault is unlocked.</p>
    <div id="vaulted-notes"><div class="meta">counting…</div></div>
    <div class="section-title" style="margin-top:var(--s6)">notes by origin</div>
    <p class="meta" style="color:var(--ink-mute);max-width:60ch">Vault whole ORIGINS at
      once — e.g. everything imported from Notion. Vaulting covers the live notes AND
      their settled snapshots; unvaulting re-checks the rules, which always win.</p>
    <div id="note-origins"><div class="meta">counting…</div></div>
    <div class="section-title" style="margin-top:var(--s6)">privacy rules</div>
    <p class="meta" style="color:var(--ink-mute);max-width:60ch">Rules mark matching material
      private — retroactively when added, and automatically at ingest forever after.
      Content is never altered; only the scope label moves.</p>
    <div class="searchbar" style="margin-top:var(--s4)">
      <select id="rule-kind" class="btn">
        <option value="domain">domain</option>
        <option value="term">search term</option>
        <option value="entity">entity</option>
        <option value="place">place</option>
      </select>
      <input id="rule-pattern" placeholder="e.g. privateclinic.example">
      <button class="btn" id="rule-add">add rule</button>
    </div>
    <div id="rule-log" class="meta" style="color:var(--accent)"></div>
    <div id="rule-list"></div>`;
  wireExportCard();

  const paintVaultCard = (status) => {
    paintVault(status);
    document.getElementById("vault-card").innerHTML = status.locked
      ? `<h2>🔒 locked</h2>
         <p class="meta">Private material is invisible on every surface. Unlock to see it
         here and everywhere else for 15 minutes.</p>
         <div class="tend-actions"><button class="btn" id="vault-act" data-act="unlock">unlock for 15 minutes</button></div>`
      : `<h2>🔓 open</h2>
         <p class="meta">Private material is visible. Relocks itself in
         ~${Math.max(1, Math.round(status.seconds_remaining / 60))} minute(s).</p>
         <div class="tend-actions"><button class="btn" id="vault-act" data-act="lock">lock now</button></div>`;
    document.getElementById("vault-act").onclick = async (event) => {
      const status = await apiPost("/v1/vault", {
        actor: "human:dashboard",
        action: event.target.dataset.act,
      });
      bodyCache.clear();
      refreshChips();
      paintVault(status); // masthead lock + the Ask history follow at once
      paintVaultCard(status);
      // the gated sections reveal/hide with the lock
      paintVaultedNotes();
      paintRules();
    };
  };
  paintVaultCard(await api("/v1/vault"));

  // — note origins: vault/unvault whole imports in one gesture —
  const paintNoteOrigins = async () => {
    const { origins } = await api("/v1/notes/origins");
    const host = document.getElementById("note-origins");
    if (!host) return;
    host.innerHTML = (origins || []).map((o) => `
      <div class="card tend-card">
        <div class="tend-pair">
          <span class="typebadge">${esc(o.origin)}</span>
          <b>${o.total} note${o.total === 1 ? "" : "s"}</b>
          <span class="meta">${o.private ? `${o.private} in the vault` : "none vaulted"}</span>
          <span style="margin-left:auto; display:flex; gap:6px">
            ${o.private < o.total ? `<button class="btn" data-vault="${esc(o.origin)}">🔒 vault all</button>` : ""}
            ${o.private > 0 ? `<button class="btn btn-quiet" data-unvault="${esc(o.origin)}">unvault</button>` : ""}
          </span>
        </div>
      </div>`).join("") || `<p class="meta" style="color:var(--ink-faint)">no notes yet.</p>`;
    host.querySelectorAll("[data-vault]").forEach((b) => {
      b.onclick = async () => {
        b.disabled = true;
        const r = await apiPost("/v1/notes/scope",
          { actor: "human:dashboard", origin: b.dataset.vault, scope: "private" });
        toastLine(`${r.notes} ${b.dataset.vault} notes moved into the vault`);
        paintNoteOrigins();
      };
    });
    host.querySelectorAll("[data-unvault]").forEach((b) => {
      b.onclick = async () => {
        b.disabled = true;
        const r = await apiPost("/v1/notes/scope",
          { actor: "human:dashboard", origin: b.dataset.unvault, scope: "personal" });
        toastLine(`${r.notes} ${b.dataset.unvault} notes re-scoped (rule matches stay private)`);
        paintNoteOrigins();
      };
    });
  };
  const toastLine = (msg) => {
    const el = document.getElementById("rule-log");
    if (el) el.textContent = msg;
  };
  paintNoteOrigins();

  const paintRules = async () => {
    const { rules, locked } = await api("/v1/privacy/rules");
    document.getElementById("rule-list").innerHTML = rules.length
      ? rules.map((rule) => `
        <div class="card tend-card">
          <div class="tend-pair">
            <span class="typebadge">${esc(rule.kind)}</span>
            ${rule.pattern === null
              ? `<b class="mono" style="color:var(--ink-faint)">🔒 unlock to reveal</b>`
              : `<b class="mono">${esc(rule.pattern)}</b>`}
            <span class="meta" style="color:var(--ink-faint)">since ${esc(rule.created_at.slice(0, 10))}</span>
            ${rule.pattern === null ? "" : `<button class="btn" style="margin-left:auto" data-rm="${esc(rule.id)}">remove</button>`}
          </div>
        </div>`).join("")
      : `<p class="meta" style="color:var(--ink-faint);margin-top:var(--s4)">no rules yet.</p>`;
    if (locked && rules.length) {
      document.getElementById("rule-list").insertAdjacentHTML("afterbegin",
        `<p class="meta" style="color:var(--ink-faint)">🔒 ${rules.length} rule${rules.length === 1 ? "" : "s"} — unlock the vault to see what they match.</p>`);
    }
    document.querySelectorAll("[data-rm]").forEach((button) => {
      button.onclick = async () => {
        await apiDelete(`/v1/privacy/rules/${button.dataset.rm}?actor=human:dashboard`);
        document.getElementById("rule-log").textContent =
          "rule removed — already-private episodes stay private (correct them in Browse)";
        paintRules();
      };
    });
  };
  paintRules();

  // vaulted notes: count always, titles only when unlocked
  const paintVaultedNotes = async () => {
    const host = document.getElementById("vaulted-notes");
    if (!host) return;
    const { count, notes, locked } = await api("/v1/notes/vaulted");
    if (!count) { host.innerHTML = `<p class="meta" style="color:var(--ink-faint)">no notes in the vault.</p>`; return; }
    if (locked) {
      host.innerHTML = `<p class="meta">🔒 ${count} note${count === 1 ? "" : "s"} in the vault — unlock to see which.</p>`;
      return;
    }
    host.innerHTML = notes.map((n) => `
      <div class="note-row" data-note="${esc(n.id)}">
        <span class="note-title">${noteTitle(n.title)}</span>
        <span class="meta">🔒 ${n.updated_at ? esc(n.updated_at.slice(0, 10)) : "—"}
          <button class="btn btn-quiet vn-unvault" data-note="${esc(n.id)}" data-v="${n.version}" title="take out of the vault">unvault</button></span>
      </div>`).join("");
    host.querySelectorAll(".note-row").forEach((row) => {
      row.onclick = (e) => { if (!e.target.closest("button")) window.__openNoteById?.(row.dataset.note); };
    });
    host.querySelectorAll(".vn-unvault").forEach((btn) => {
      btn.onclick = async () => {
        await apiPost(`/v1/notes/${btn.dataset.note}`, { actor: "human:dashboard", scope: "personal", version: +btn.dataset.v });
        paintVaultedNotes();
      };
    });
  };
  paintVaultedNotes();

  document.getElementById("rule-add").onclick = async () => {
    const pattern = document.getElementById("rule-pattern").value.trim();
    if (!pattern) return;
    const log = document.getElementById("rule-log");
    try {
      const result = await apiPost("/v1/privacy/rules", {
        actor: "human:dashboard",
        kind: document.getElementById("rule-kind").value,
        pattern,
      });
      log.textContent = `rule added — ${result.episodes_relabeled} existing episode(s) moved to the vault`;
      document.getElementById("rule-pattern").value = "";
      refreshChips();
      paintRules();
    } catch (err) {
      log.textContent = err.message;
    }
  };
}

/* ————— browse: the data explorer's workbench ————— */
const SCOPES = ["work", "personal", "family", "health", "private"];
const BROWSE_TABS = [
  ["episodes", "Episodes"],
  ["entities", "Entities"],
  ["identities", "Identities"],
  ["edges", "Edges"],
  ["sources", "Sources"],
  ["retired", "Retired"],
];
// Per-tab filter state, sticky while the dashboard stays open.
let browseDial = null; // the episodes tab's Dial instance

const browseState = {
  episodes: { sel: { source: new Set(), kind: new Set(), scope: new Set(), trust: new Set() },
              from: "", to: "", q: "", place: "", sort: "time", dir: "desc" },
  entities: { sel: { entity_type: new Set(), scope: new Set() }, q: "", sort: "episodes", dir: "desc" },
  identities: { sel: { kind: new Set() }, q: "" },
  edges: { sel: { relation: new Set(), status: new Set() } },
  sources: { sel: {} },
  retired: { sel: {} },
};
const browseSelection = new Set(); // bulk-selected episode ids
let browseToken = 0;
let browseLoadMore = null;
window.addEventListener("scroll", () => {
  if (browseLoadMore && document.getElementById("browse-sentinel") &&
      window.innerHeight + window.scrollY >= document.body.offsetHeight - 600) browseLoadMore();
});

/* The Atlas visit panel hands off to Browse with filters pre-set. */
window.__browsePrefill = (patch) => {
  Object.assign(browseState.episodes, patch);
};

function browseQuery(tab, offset) {
  const st = browseState[tab];
  const p = new URLSearchParams();
  for (const [dim, set] of Object.entries(st.sel)) {
    if (set.size) p.set(dim === "entity_type" ? "type" : dim, [...set].join(","));
  }
  if (st.q?.trim()) p.set("q", st.q.trim());
  if (st.place?.trim()) p.set("place", st.place.trim());
  if (st.from) p.set("from", st.from);
  if (st.to) p.set("to", st.to);
  if (st.sort) { p.set("sort", st.sort); p.set("dir", st.dir); }
  if (st.lifecycle) p.set("lifecycle", "all");
  p.set("offset", offset);
  p.set("limit", "50");
  return p.toString();
}

function scopePick(attr, key, current) {
  return `<select class="scope-pick" ${attr}="${esc(key)}">
    ${SCOPES.map((s) => `<option ${s === current ? "selected" : ""}>${s}</option>`).join("")}
  </select>`;
}
const dotSpan = (source) =>
  `<span class="dot" style="--dot:${SOURCE_HUES[source] || "var(--ink-faint)"}"></span>`;

const BROWSE_COLUMNS = {
  episodes: [
    ["", null, `<input type="checkbox" id="sel-all" title="select page">`],
    ["when", "time"], ["source", "source"], ["kind", "kind"],
    ["title", "title"], ["scope", "scope"], ["trust", null],
  ],
  entities: [["name", "name"], ["type", null], ["episodes", "episodes"], ["first seen", "created"], ["scope", null]],
  identities: [["value", null], ["kind", null], ["entity", null], ["type", null], ["scope", null]],
  edges: [["from", null], ["relation", null], ["to", null], ["status", null], ["valid since", null], ["recorded", null]],
  sources: [["source", null], ["kind", null], ["adapter", null], ["episodes", null], ["first seen", null], ["last refresh", null], ["", null]],
  retired: [["when", null], ["what", null], ["state", null], ["changed here", null], ["why", null], ["", null]],
};
const RETIRED_TABLES = {
  episodes: "memories", episode_entities: "links", edge_provenance: "supports", edges: "connections",
  intent_matches: "goal matches", opportunities: "opportunities", location_points: "map points",
  semantic_pairs: "pairings", entities: "hidden entities", self_model: "beliefs to Tend", sources: "source",
};

const BROWSE_ROW = {
  episodes: (e, life) => `
    <tr class="brow expandable-row ${life ? "retired-row" : ""}" data-episode="${esc(e.id)}">
      <td class="selcol"><input type="checkbox" data-sel="${esc(e.id)}" ${browseSelection.has(e.id) ? "checked" : ""}></td>
      <td class="mono nowrap faint">${esc((e.event_time || e.ingest_time || "").slice(0, 16).replace("T", " "))}</td>
      <td class="nowrap">${dotSpan(e.source)}${esc(e.source)}</td>
      <td class="mono">${esc(KIND_LABEL[e.kind] || e.kind)}</td>
      <td class="tcell">${esc(e.title)}${life ? ` <span class="lifebadge ${life.startsWith("retired:") ? "retired" : ""}">${
        life.startsWith("retired:") ? "retired" : "replaced"}</span>` : ""}</td>
      <td>${scopePick("data-scope-episode", e.id, e.scope)}</td>
      <td class="mono faint">${esc(e.trust)}</td>
    </tr>`,
  entities: (e) => `
    <tr class="brow">
      <td><a class="entchip" href="#/entity/${encodeURIComponent(e.name)}">${esc(e.name)}</a></td>
      <td class="mono faint">${esc(e.entity_type)}</td>
      <td class="mono">${e.episode_count.toLocaleString()}</td>
      <td class="mono nowrap faint">${esc((e.created_at || "").slice(0, 10))}</td>
      <td>${scopePick("data-scope-entity", e.name, e.scope)}</td>
    </tr>`,
  identities: (i) => `
    <tr class="brow">
      <td class="mono">${esc(i.value)}</td>
      <td class="mono faint">${esc(i.kind)}</td>
      <td><a class="entchip" href="#/entity/${encodeURIComponent(i.entity_name)}">${esc(i.entity_name)}</a></td>
      <td class="mono faint">${esc(i.entity_type)}</td>
      <td class="mono faint">${esc(i.scope || "—")}</td>
    </tr>`,
  edges: (e) => `
    <tr class="brow">
      <td><a class="entchip" href="#/entity/${encodeURIComponent(e.src_name)}">${esc(e.src_name)}</a></td>
      <td class="mono" style="color:var(--accent)">${esc(e.relation)}</td>
      <td><a class="entchip" href="#/entity/${encodeURIComponent(e.dst_name)}">${esc(e.dst_name)}</a></td>
      <td class="mono ${e.status === "current" ? "" : "faint"}">${esc(e.status)}</td>
      <td class="mono nowrap faint">${esc((e.t_valid || "—").slice(0, 10))}</td>
      <td class="mono nowrap faint">${esc((e.t_created || "").slice(0, 10))}</td>
    </tr>`,
  sources: (s) => `
    <tr class="brow">
      <td class="nowrap">${dotSpan(s.name)}${esc(s.name)}</td>
      <td><span class="typebadge">${esc(s.kind)}</span></td>
      <td class="mono faint">${esc(s.adapter || "—")}</td>
      <td class="mono">${s.episode_count.toLocaleString()}</td>
      <td class="mono nowrap faint">${esc((s.first_seen || "").slice(0, 10))}</td>
      <td class="mono nowrap faint">${esc((s.last_refresh || "—").slice(0, 16).replace("T", " "))}</td>
      <td class="nowrap">${s.retired_by
        ? `<span class="lifebadge retired">retired</span> <button class="btn tiny" data-unretire="${esc(s.retired_by)}">undo</button>`
        : s.kind === "materialized" ? `<button class="btn tiny ghost" data-retire-source="${esc(s.name)}">retire…</button>` : ""}</td>
    </tr>`,
  retired: (r) => {
    const changed = Object.entries(r.changed || {})
      .map(([table, n]) => `${Number(n).toLocaleString()} ${RETIRED_TABLES[table] || table}`).join(" · ");
    return `
    <tr class="brow ${r.state === "undone" ? "faint" : ""}">
      <td class="mono nowrap faint">${esc((r.created_at || "").slice(0, 16).replace("T", " "))}</td>
      <td>${r.kind === "source" ? `<span class="typebadge">source</span> ` : ""}${esc(r.label)}</td>
      <td class="mono">${esc(r.state === "done" ? "retired" : r.state)}${r.undone_at ? ` <span class="faint">${esc(r.undone_at.slice(0, 10))}</span>` : ""}</td>
      <td class="meta">${changed || (r.state === "undone" ? "all put back" : "—")}</td>
      <td class="meta">${esc(r.reason || "")}</td>
      <td>${r.state === "undone" ? "" : `<button class="btn tiny" data-unretire="${esc(r.id)}">undo</button>`}</td>
    </tr>`;
  },
};

async function toggleBrowseEpisode(tr, id, colspan) {
  const next = tr.nextElementSibling;
  if (next?.classList.contains("exp-row")) { next.remove(); return; }
  const holder = document.createElement("tr");
  holder.className = "exp-row";
  holder.innerHTML = `<td colspan="${colspan}"><div class="meta">opening episode…</div></td>`;
  tr.after(holder);
  try {
    if (!bodyCache.has(id)) bodyCache.set(id, await api(`/v1/episode/${id}`));
    const episode = bodyCache.get(id);
    holder.firstElementChild.innerHTML = `
      <div class="episode-provenance">
        <span>${esc(episode.source)}/${esc(episode.kind)}</span>
        <span>trust ${esc(episode.trust)}</span>
        <span class="mono">${esc(id)}</span>
        ${lifecycleControls(episode, id)}
      </div>
      <div class="md">${md(episode.body || "(no content)")}</div>`;
    wireLifecycle(holder, episode, id, () => { bodyCache.delete(id); tr.classList.add("retired-row"); holder.remove(); });
  } catch (err) {
    holder.firstElementChild.innerHTML = `<div class="meta">${esc(err.message)}</div>`;
  }
}

async function renderBrowse(tab) {
  // The tab comes from the URL: only an own key of browseState is a tab
  // ("__proto__" would otherwise hand back Object.prototype to write into).
  if (!Object.hasOwn(browseState, tab)) tab = "episodes";
  setNav("browse");
  const st = browseState[tab];
  const token = ++browseToken;
  const columns = BROWSE_COLUMNS[tab];
  let offset = 0, total = 0, done = false, loading = false;
  browseSelection.clear();

  const controls = tab === "sources" || tab === "retired" ? "" : `
    <div class="browse-controls">
      ${tab === "edges" ? "" : `<input id="bq" placeholder="filter by ${
        { episodes: "title", entities: "name", identities: "value or entity" }[tab]
      }…" value="${esc(st.q || "")}">`}
      ${tab === "episodes" ? `
        <input id="bplace" placeholder="place…" value="${esc(st.place || "")}" title="filter by cited place">
        <input type="date" id="bfrom" value="${esc(st.from)}" title="from">
        <span class="meta" style="color:var(--ink-faint)">→</span>
        <input type="date" id="bto" value="${esc(st.to)}" title="to">
        <label class="meta nowrap" title="include memories you retired and older versions that were replaced">
          <input type="checkbox" id="blife" ${st.lifecycle ? "checked" : ""}> retired &amp; replaced</label>` : ""}
    </div>`;

  view.innerHTML = `
    <span class="eyebrow">memories</span>
    <h1 class="pagehead">Memories</h1>
    <p class="subhead">Everything the hearth remembers — spin the dial, facet the
      grid, correct a scope. Content never changes; only its labels do.</p>
    <div class="browse-tabs">${BROWSE_TABS.map(([key, label]) =>
      `<a href="#/browse/${key}" class="${key === tab ? "active" : ""}">${label}</a>`).join("")}</div>
    ${tab === "episodes" ? `<div id="albums"></div><div id="browse-dial"></div>` : ""}
    ${controls}
    <div id="browse-facets"></div>
    <div class="browse-meta" id="browse-total"></div>
    <table class="browse">
      <thead id="browse-head"></thead>
      <tbody id="browse-body"></tbody>
    </table>
    <div id="browse-sentinel" class="browse-meta"></div>
    <div class="bulkbar" id="bulkbar" hidden></div>`;

  const body = document.getElementById("browse-body");
  const head = document.getElementById("browse-head");
  const sentinel = document.getElementById("browse-sentinel");
  const totalEl = document.getElementById("browse-total");
  const bulkbar = document.getElementById("bulkbar");

  const paintHead = () => {
    head.innerHTML = `<tr>${columns.map(([label, sortKey, rawHtml]) => rawHtml
      ? `<th class="selcol">${rawHtml}</th>`
      : `<th ${sortKey ? `data-sort="${sortKey}"` : ""}>${esc(label)}${
          sortKey && st.sort === sortKey ? `<span class="arrow"> ${st.dir === "asc" ? "↑" : "↓"}</span>` : ""
        }</th>`).join("")}</tr>`;
    const all = document.getElementById("sel-all");
    if (all) all.onchange = () => {
      body.querySelectorAll("input[data-sel]").forEach((box) => {
        box.checked = all.checked;
        all.checked ? browseSelection.add(box.dataset.sel) : browseSelection.delete(box.dataset.sel);
      });
      paintBulkbar();
    };
  };
  head.onclick = (event) => {
    const th = event.target.closest("th[data-sort]");
    if (!th) return;
    const key = th.dataset.sort;
    if (st.sort === key) st.dir = st.dir === "asc" ? "desc" : "asc";
    else { st.sort = key; st.dir = key === "name" ? "asc" : "desc"; }
    reload();
  };

  // Facets, decluttered: one compact line per dimension — active chips
  // first and always visible, then the top few by count; everything
  // else waits behind a per-dimension "+N" expander.
  const facetOpen = (window.__facetOpen ||= {});
  const paintFacets = (facets) => {
    const wrap = document.getElementById("browse-facets");
    if (!facets) { wrap.innerHTML = ""; return; }
    const PEEK = 5;
    wrap.innerHTML = Object.entries(facets).map(([dim, counts]) => {
      const active = st.sel[dim];
      if (!active || !counts.length) return "";
      const activeChips = counts.filter((f) => active.has(f.value));
      const rest = counts.filter((f) => !active.has(f.value));
      const open = !!facetOpen[dim];
      const shown = open ? [...activeChips, ...rest]
        : [...activeChips, ...rest.slice(0, Math.max(PEEK - activeChips.length, 2))];
      const hidden = counts.length - shown.length;
      return `<div class="facetbar compact"><span class="dim">${esc(dim.replace("_", " "))}</span>
        ${shown.map((f) => `<button class="filterchip ${
            active.has(f.value) ? "on" : active.size ? "off" : ""
          }" data-facet="${esc(dim)}" data-value="${esc(f.value)}"
          style="--dot:${dim === "source" ? SOURCE_HUES[f.value] || "var(--ink-faint)" : "var(--ink-faint)"}">
          ${esc(f.value)}<span class="chipcount">${f.count.toLocaleString()}</span></button>`).join("")}
        ${hidden > 0 || open ? `<button class="facet-more" data-more="${esc(dim)}">${
          open ? "less" : `+${hidden}`}</button>` : ""}
      </div>`;
    }).join("");
    wrap.querySelectorAll("[data-facet]").forEach((chip) => {
      chip.onclick = () => {
        const set = st.sel[chip.dataset.facet];
        set.has(chip.dataset.value) ? set.delete(chip.dataset.value) : set.add(chip.dataset.value);
        reload();
      };
    });
    wrap.querySelectorAll("[data-more]").forEach((btn) => {
      btn.onclick = () => {
        facetOpen[btn.dataset.more] = !facetOpen[btn.dataset.more];
        paintFacets(facets);
      };
    });
  };

  const paintBulkbar = () => {
    bulkbar.hidden = !browseSelection.size;
    if (bulkbar.hidden) return;
    bulkbar.innerHTML = `<span class="mono">${browseSelection.size} selected — set scope</span>
      ${SCOPES.map((s) => `<button class="btn ${s === "private" ? "lock" : ""}" data-bulk="${s}">${
        s === "private" ? "🔒 private" : s}</button>`).join("")}
      <button class="btn" data-bulk-clear>clear</button>`;
  };
  bulkbar.onclick = async (event) => {
    if (event.target.closest("[data-bulk-clear]")) {
      browseSelection.clear();
      body.querySelectorAll("input[data-sel]").forEach((box) => (box.checked = false));
      return paintBulkbar();
    }
    const button = event.target.closest("[data-bulk]");
    if (!button) return;
    const scope = button.dataset.bulk;
    const ids = [...browseSelection];
    button.disabled = true;
    try {
      await apiPost("/v1/scope", { actor: "human:dashboard", episodes: ids, scope });
      for (const id of ids) {
        const pick = body.querySelector(`select[data-scope-episode="${CSS.escape(id)}"]`);
        if (pick) pick.value = scope;
        const box = body.querySelector(`input[data-sel="${CSS.escape(id)}"]`);
        if (box) box.checked = false;
      }
      browseSelection.clear();
      paintBulkbar();
    } catch (err) {
      sentinel.textContent = err.message;
      button.disabled = false;
    }
  };

  body.onclick = async (event) => {
    const retireSource = event.target.closest("[data-retire-source]");
    if (retireSource) {
      if (await retireFlow("source", retireSource.dataset.retireSource, retireSource.dataset.retireSource)) reload();
      return;
    }
    const unretire = event.target.closest("button[data-unretire]");
    if (unretire && !unretire.closest(".exp-row")) {
      unretire.disabled = true;
      try {
        await undoRetireFlow(unretire.dataset.unretire);
        reload();
      } catch (err) {
        unretire.disabled = false;
        sentinel.textContent = err.message;
      }
      return;
    }
    const tr = event.target.closest("tr.expandable-row");
    if (!tr || event.target.closest("a, select, input, button")) return;
    toggleBrowseEpisode(tr, tr.dataset.episode, columns.length);
  };
  body.onchange = async (event) => {
    const target = event.target;
    if (target.matches("input[data-sel]")) {
      target.checked ? browseSelection.add(target.dataset.sel) : browseSelection.delete(target.dataset.sel);
      return paintBulkbar();
    }
    const isEpisode = target.matches("select[data-scope-episode]");
    if (isEpisode || target.matches("select[data-scope-entity]")) {
      target.disabled = true;
      try {
        await apiPost("/v1/scope", isEpisode
          ? { actor: "human:dashboard", episodes: [target.dataset.scopeEpisode], scope: target.value }
          : { actor: "human:dashboard", entity: target.dataset.scopeEntity, scope: target.value });
      } catch (err) {
        sentinel.textContent = err.message;
      }
      target.disabled = false;
    }
  };

  async function load() {
    if (loading || done || token !== browseToken) return;
    loading = true;
    sentinel.textContent = "loading…";
    const first = offset === 0;
    try {
      if (tab === "sources") {
        const data = await api("/v1/browse/sources");
        if (token !== browseToken) return;
        total = data.sources.length;
        done = true;
        body.insertAdjacentHTML("beforeend", data.sources.map(BROWSE_ROW.sources).join(""));
      } else if (tab === "retired") {
        const data = await api("/v1/retirements");
        if (token !== browseToken) return;
        total = data.retirements.length;
        done = true;
        body.insertAdjacentHTML("beforeend", data.retirements.map(BROWSE_ROW.retired).join(""));
      } else {
        const data = await api(`/v1/browse/${tab}?${browseQuery(tab, offset)}`);
        if (token !== browseToken) return;
        total = data.total;
        offset += data.items.length;
        done = !data.items.length || offset >= total;
        const life = data.lifecycle || {};
        body.insertAdjacentHTML("beforeend", data.items.map((item) => BROWSE_ROW[tab](item, life[item.id])).join(""));
        if (first) paintFacets(data.facets);
      }
      totalEl.textContent = `${total.toLocaleString()} rows`;
      sentinel.textContent = done ? (total ? "· end ·" : "nothing matches") : "";
    } catch (err) {
      if (token === browseToken) sentinel.textContent = err.message;
    }
    loading = false;
  }
  const reload = () => {
    offset = 0; total = 0; done = false;
    body.innerHTML = "";
    paintHead();
    load();
  };
  browseLoadMore = () => load();

  const bq = document.getElementById("bq");
  if (bq) {
    let debounce;
    bq.addEventListener("input", () => {
      clearTimeout(debounce);
      debounce = setTimeout(() => { st.q = bq.value; reload(); }, 300);
    });
  }
  const bplace = document.getElementById("bplace");
  if (bplace) {
    let placeDebounce;
    bplace.addEventListener("input", () => {
      clearTimeout(placeDebounce);
      placeDebounce = setTimeout(() => { st.place = bplace.value; reload(); }, 300);
    });
  }

  // Albums: named saved views over the memories (localStorage — UI
  // preference, not substrate; the substrate stays untouched).
  const paintAlbums = () => {
    const host = document.getElementById("albums");
    if (!host) return;
    const albums = JSON.parse(localStorage.getItem("hl-albums") || "[]");
    const hasFilters = !!(st.q || st.place || st.from || st.to ||
      Object.values(st.sel).some((set) => set.size));
    host.innerHTML = `<div class="albums-row">
      <span class="otd-label" style="writing-mode:horizontal-tb;transform:none">albums</span>
      ${albums.map((a, i) => `<span class="album-chip" data-album="${i}">${esc(a.name)}
        <button class="album-x" data-del="${i}" title="forget this album">×</button></span>`).join("")}
      ${hasFilters ? `<button class="facet-more" id="album-save" title="save the current filters as an album">☆ save view</button>` : ""}
      ${!albums.length && !hasFilters ? `<span class="browse-meta">filter something, then ☆ save it as an album</span>` : ""}
    </div>`;
    host.querySelectorAll("[data-album]").forEach((chip) => {
      chip.onclick = (e) => {
        if (e.target.closest("[data-del]")) return;
        const a = albums[+chip.dataset.album];
        st.q = a.q || ""; st.place = a.place || ""; st.from = a.from || ""; st.to = a.to || "";
        for (const [dim, vals] of Object.entries(a.sel || {})) {
          if (st.sel[dim]) st.sel[dim] = new Set(vals);
        }
        renderBrowse("episodes"); // full repaint picks up every control
      };
    });
    host.querySelectorAll("[data-del]").forEach((btn) => {
      btn.onclick = async () => {
        const a = albums[+btn.dataset.del];
        if (!(await confirmDanger({
          title: `Forget the album “${a.name}”?`,
          body: "Only the saved view is removed — every memory in it stays.",
          action: "Forget it",
        }))) return;
        albums.splice(+btn.dataset.del, 1);
        localStorage.setItem("hl-albums", JSON.stringify(albums));
        paintAlbums();
      };
    });
    const save = document.getElementById("album-save");
    if (save) save.onclick = async () => {
      const name = await confirmDanger({
        title: "Save this view as an album",
        body: "The current filters — query, place, dates, chips — become a one-click way back.",
        action: "Save album",
        input: "album name — e.g. Paris trips",
      });
      if (!name) return;
      albums.push({
        name, q: st.q, place: st.place, from: st.from, to: st.to,
        sel: Object.fromEntries(Object.entries(st.sel).map(([d, set]) => [d, [...set]])),
      });
      localStorage.setItem("hl-albums", JSON.stringify(albums));
      paintAlbums();
    };
  };
  paintAlbums();

  // The Dial: the drum IS the from/to filter. Spin → grid follows;
  // type dates → drum glides there; other filters shape its density.
  const dialHost = document.getElementById("browse-dial");
  if (dialHost) {
    if (browseDial) { browseDial.destroy(); browseDial = null; }
    import("/assets/dial.js").then(({ mountDial }) => {
      if (token !== browseToken) return; // navigated away meanwhile
      browseDial = mountDial(dialHost, {
        from: st.from || undefined,
        to: st.to || undefined,
        filters: () => ({ q: st.q, place: st.place, source: [...st.sel.source][0] || "" }),
        onWindow: (from, to) => {
          if (st.from === from && st.to === to) return;
          st.from = from; st.to = to;
          const bf = document.getElementById("bfrom"), bt = document.getElementById("bto");
          if (bf) bf.value = from;
          if (bt) bt.value = to;
          reload();
        },
        onPick: (bucket) => {
          // A bucket click narrows the grid to exactly that period.
          const from = bucket.length === 4 ? `${bucket}-01-01`
            : bucket.length === 7 ? `${bucket}-01` : bucket;
          const to = bucket.length === 4 ? `${bucket}-12-31`
            : bucket.length === 7 ? `${bucket}-28` : bucket;
          st.from = from; st.to = to;
          browseDial?.setWindow(from, to);
          const bf = document.getElementById("bfrom"), bt = document.getElementById("bto");
          if (bf) bf.value = from;
          if (bt) bt.value = to;
          reload();
        },
      });
    });
  }
  const syncDial = () => browseDial?.setWindow(st.from, st.to);
  for (const [id, key] of [["bfrom", "from"], ["bto", "to"]]) {
    const input = document.getElementById(id);
    if (input) input.onchange = () => { st[key] = input.value; syncDial(); reload(); };
  }
  const life = document.getElementById("blife");
  if (life) life.onchange = () => { st.lifecycle = life.checked; reload(); };
  // A retirement or an undo anywhere refreshes the list in view.
  window.__browseReload = () => { if (token === browseToken) reload(); };

  reload();
}
window.addEventListener("hl-retirement-changed", () => {
  if (location.hash.startsWith("#/browse")) window.__browseReload?.();
});

/* ————— Ask Hearthloom (slide-in RLM chat, history, debug) ————— */
const askPanel = document.getElementById("ask-panel");
const askLog = document.getElementById("ask-log");
const askInput = document.getElementById("ask-input");
const askDebug = document.getElementById("ask-debug");
askDebug.checked = localStorage.getItem("hearthloom-ask-debug") === "1";
askDebug.onchange = () =>
  localStorage.setItem("hearthloom-ask-debug", askDebug.checked ? "1" : "0");
let askBusy = false;

// human phrasings for each tool while debug is off — the alive feel
const ALIVE_VERBS = {
  search: ["searching your memories", "sifting the archive", "listening for echoes"],
  sql: ["counting the evidence", "tracing patterns across time", "weighing the record"],
  recall: ["reliving that day", "walking back through time"],
  traverse: ["following the threads", "walking the constellation"],
  read_episode: ["reading the original", "opening the source"],
};
let aliveIndex = 0;

const ASK_PRESETS = [
  "How did my sleep change after I started running? Chart the before and after.",
  "Where did I travel in 2019? Show me a map.",
  "How has my resting heart rate trended over the years?",
  "Who have I known the longest, and how did we meet?",
  "Chart my commits by project over the last three months.",
  "What was I obsessed with in 2011?",
];
const ASK_HINT = `<div class="ask-hint">Ask anything your history can answer — it reasons
        across commits, messages, mail, places, and articles, and cites its
        evidence. <em>Each question spends LLM tokens.</em>
        <div class="ask-presets">${ASK_PRESETS.map((q) =>
          `<button class="ask-preset" data-q="${q.replace(/"/g, "&quot;")}">${q}</button>`).join("")}</div>
        <div class="ask-presets ask-recipes"></div></div>`;

/* — recipes: questions worth asking again. A recipe is a note tagged
   #ask-recipe — its title is the name on the chip, its body is the
   question — so it is edited, renamed and deleted in Notes like any
   other note. ☆ on a question saves one; a chip here runs it.
   One click sends the body as the user's OWN words (what the Ask
   outbound gate trusts), so only notes the ☆ button saved count: kind
   "recipe" (the server makes one only on a note written in the app) and
   origin "hearthloom" (an agent's edit refiles a note as "agent"). The
   tag alone means nothing — imports and agents can carry it. — */
const RECIPE_TAG = "ask-recipe";
const RECIPE_KIND = "recipe";
const isRecipe = (note) => note?.kind === RECIPE_KIND && note?.origin === "hearthloom";
const askQuestionHtml = (question) => `<div class="ask-q" data-q="${esc(question)}">${esc(question)}
  <button class="linkish ask-save-recipe" title="save this question as a recipe — run it again with one click">☆</button></div>`;
function askShowHint() {
  askLog.innerHTML = ASK_HINT;
  askPaintRecipes();
}
async function askPaintRecipes() {
  let notes = [];
  // Fetch past the chips' 30 so imported notes wearing the tag can't
  // crowd the real recipes out.
  try { notes = (await api(`/v1/notes?tag=${RECIPE_TAG}&limit=200`)).notes || []; } catch { /* no recipes row */ }
  notes = notes.filter(isRecipe).slice(0, 30);
  const host = askLog.querySelector(".ask-recipes");
  if (!host) return; // a question was asked meanwhile
  host.innerHTML = notes.length
    ? `<span class="meta ask-recipes-label">your recipes</span>` + notes.map((n) =>
        `<button class="ask-preset ask-recipe" data-note="${esc(n.id)}" title="run this saved question">${esc(n.title)}</button>`).join("")
    : "";
}
/// The question a recipe note holds: its body, minus a leading heading.
const recipeQuestion = (body, title) =>
  String(body || "").split("\n").filter((line, i) => !(i === 0 && /^#{1,4}\s/.test(line))).join("\n").trim() || title;
async function askRunRecipe(noteId) {
  if (askBusy) return;
  let data;
  try { data = await api(`/v1/notes/${encodeURIComponent(noteId)}`); } catch { return; }
  // Re-checked at run time: the note may have been refiled since the
  // chips were painted.
  if (!isRecipe(data.note)) { askPaintRecipes(); return; }
  askInput.value = recipeQuestion(data.body, data.note?.title || "");
  if (askInput.value) askSend();
}
async function askSaveRecipe(button) {
  const question = button.closest(".ask-q")?.dataset.q || "";
  if (!question || button.disabled) return;
  button.disabled = true;
  // A question asked with the vault open may name private things: its
  // recipe is private too.
  let locked = true;
  try { locked = (await api("/v1/vault")).locked !== false; } catch { /* assume locked */ }
  const title = question.replace(/\s+/g, " ").trim();
  try {
    const made = await apiPost("/v1/notes", {
      actor: "human:dashboard",
      title: title.length > 70 ? `${title.slice(0, 69)}…` : title,
      body: question,
      tags: [RECIPE_TAG],
      kind: RECIPE_KIND,
      scope: locked ? "personal" : "private",
    });
    button.outerHTML = `<button class="linkish ask-recipe-saved" data-note="${esc(made.id || "")}"
      title="saved as a recipe — click to rename or edit it">★ saved</button>`;
  } catch (err) {
    button.disabled = false;
    button.title = `could not save: ${err.message}`;
  }
}

// Clear the thread and drop back to a fresh question — so selecting a past
// query never traps you in read-only history (no page reload needed).
let askThread = []; // ask ids of the current conversation, panel order

function askReset() {
  if (askBusy) return;
  askThread = [];
  askShowHint();
  askPaintSeal();
  askHistoryList.querySelectorAll(".active").forEach((el) => el.classList.remove("active"));
  askInput.value = "";
  askInput.focus();
}
document.getElementById("ask-new").onclick = askReset;
askLog.addEventListener("click", (e) => {
  const recipe = e.target.closest(".ask-recipe");
  if (recipe) { askRunRecipe(recipe.dataset.note); return; }
  const save = e.target.closest(".ask-save-recipe");
  if (save) { askSaveRecipe(save); return; }
  const saved = e.target.closest(".ask-recipe-saved");
  if (saved) { window.__openNoteById?.(saved.dataset.note); return; }
  const preset = e.target.closest(".ask-preset");
  if (preset) {
    askInput.value = preset.dataset.q;
    askSend();
  }
});

// Publish the masthead's live height so fixed panels can start below it
// (app.css .ask-panel). Re-measured on resize/wrap — never hardcode it.
{
  const masthead = document.querySelector(".masthead");
  const publish = () => document.documentElement.style.setProperty(
    "--masthead-h", `${masthead ? Math.round(masthead.getBoundingClientRect().height) : 0}px`);
  publish();
  if (masthead && "ResizeObserver" in window) new ResizeObserver(publish).observe(masthead);
  window.addEventListener("resize", publish);
}

document.getElementById("ask-open").onclick = () => {
  askPanel.classList.add("open");
  askHistoryLoad(true);
  // an empty panel shows the starters and the saved recipes
  if (!askBusy && !askThread.length && !askLog.querySelector(".ask-q")) askShowHint();
  askInput.focus();
};
document.getElementById("ask-close").onclick = () => askPanel.classList.remove("open");
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") askPanel.classList.remove("open");
});
// clicking anywhere outside the panel closes it, same as the ×
// (overlays — editor, person card, confirms — don't count as outside)
document.addEventListener("click", (e) => {
  if (!askPanel.classList.contains("open")) return;
  if (e.target.closest("#ask-panel, #ask-open, .note-overlay, .confirm-overlay")) return;
  askPanel.classList.remove("open");
});

function askAppend(html) {
  askLog.insertAdjacentHTML("beforeend", html);
  askLog.scrollTop = askLog.scrollHeight;
  return askLog.lastElementChild;
}
/* Citation chips — icon + human label, resolved server-side and routed
   to the RIGHT context (note→slider, person→card, task/goal→Desk,
   event→its day, else Memories). Falls back to the raw id chip if the
   resolver is unreachable. */
const CITE_ICONS = {
  note: "✎", person: "◍", org: "◍", place: "⚑",
  task: "☐", goal: "⭑", episode: "•", entity: "•",
};
function renderAnswer(answer, citations, rounds, askId) {
  const charts = [];
  const node = askAppend(`<div class="ask-a"><div class="md">${md(answer || "", charts)}</div>
    ${(citations || []).length ? `<div class="ask-citations" data-loading="1"></div>` : ""}
    ${rounds != null && askDebug.checked
      ? `<div class="meta" style="color:var(--ink-faint);margin-top:var(--s2)">${rounds} tool rounds</div>` : ""}
  </div>`);
  bindCharts(node, charts, askId);
  const host = node.querySelector(".ask-citations");
  if (host) resolveCitations(citations, host);
}

async function resolveCitations(citations, host) {
  const ids = (citations || []).filter(Boolean);
  if (!ids.length) return;
  let refs = [];
  try {
    const data = await api(`/v1/resolve?ids=${encodeURIComponent(ids.join(","))}`);
    refs = data.refs || [];
  } catch { /* fall back to raw chips below */ }
  const byId = new Map(refs.map((r) => [r.id, r]));
  host.innerHTML = ids.map((id) => {
    const r = byId.get(id);
    if (!r) {
      // unresolvable (vault-hidden or gone) — a plain Memories lookup
      return `<a class="citechip" href="#/search/${encodeURIComponent(id)}">${esc(id)}</a>`;
    }
    const icon = CITE_ICONS[r.kind] || "•";
    const label = r.kind === "note" ? `Note: ${r.label}`
      : r.kind === "task" ? `Task: ${r.label}`
      : r.kind === "goal" ? `Goal: ${r.label}`
      : r.label;
    const sub = r.sub ? ` <span class="cite-sub">${esc(r.sub)}</span>` : "";
    return `<button class="citechip" data-route="${esc(r.route)}" data-arg="${esc(r.arg)}"
      title="${esc(r.label)}"><span class="cite-ic">${icon}</span> ${esc(label)}${sub}</button>`;
  }).join("");
  host.querySelectorAll(".citechip[data-route]").forEach((chip) => {
    chip.onclick = () => openCitation(chip.dataset.route, chip.dataset.arg);
  });
}

function openCitation(route, arg) {
  if (route === "note") return window.__openNoteById?.(arg);
  if (route === "person") return window.__openPersonModal?.(arg);
  if (route === "day") { location.hash = `#/day/${arg}`; return; }
  if (route === "task") { location.hash = "#/desk"; return; }
  if (route === "goal") { location.hash = "#/desk"; return; }
  // search fallback → Memories
  window.__browsePrefill?.({ q: arg, from: "", to: "", place: "" });
  location.hash = "#/browse/episodes";
}

/* — history sidebar: search + infinite scroll — */
const askHistoryList = document.getElementById("ask-history-list");
const askSearch = document.getElementById("ask-search");
let historyOffset = 0;
let historyDone = false;
let historyLoading = false;
async function askHistoryLoad(reset) {
  if (reset) {
    historyOffset = 0;
    historyDone = false;
    askHistoryList.innerHTML = "";
  }
  if (historyDone || historyLoading) return;
  historyLoading = true;
  try {
    const q = askSearch.value.trim();
    const data = await api(
      `/v1/ask/history?offset=${historyOffset}&limit=30${q ? `&q=${encodeURIComponent(q)}` : ""}`);
    const items = data.items || [];
    if (items.length < 30) historyDone = true;
    historyOffset = data.next_offset;
    // One item per CONVERSATION: its opening question, with a count when
    // there were follow-ups. `data-ask` is the first turn; the open
    // conversation stays highlighted across reloads.
    const openRoot = askThread[0];
    for (const item of items) {
      const turns = item.turns || 1;
      askHistoryList.insertAdjacentHTML("beforeend", `
        <div class="ask-hitem${item.id === openRoot ? " active" : ""}${item.private ? " private" : ""}" data-ask="${esc(item.id)}"
             data-last="${esc(item.last_id || item.id)}" data-turns="${turns}">
          ${!item.private
            ? `<button class="ask-seal" data-private="1" title="Make this conversation private — hidden whenever the vault is locked">🔒</button>`
            : item.can_unseal
              ? `<button class="ask-seal" data-private="0" title="Make this conversation public again">🔓</button>` : ""}
          <button class="ask-debug" title="Debug trace of the latest question — steps, latency & token cost">⌗</button>
          <button class="ask-del" title="Delete this conversation forever">×</button>
          <div class="q">${esc(item.question)}</div>
          <div class="when">${esc(item.ts.slice(0, 16).replace("T", " "))}
            ${item.private ? `<span class="lockmark" title="${item.can_unseal
              ? "private — you sealed this conversation"
              : "private — a question here was asked with the vault open"}">🔒 private</span>` : ""}
            ${turns > 1 ? `<span class="turns" title="${turns} questions in this conversation">${turns} questions</span>` : ""}
            ${item.status === "error" ? `<span class="err">✗ failed</span>` : ""}</div>
        </div>`);
    }
    if (!askHistoryList.children.length) {
      askHistoryList.innerHTML = `<div class="ask-history-more">no past questions${q ? " match" : ""}</div>`;
    }
  } catch { /* daemon busy */ }
  historyLoading = false;
}
askHistoryList.addEventListener("scroll", () => {
  if (askHistoryList.scrollTop + askHistoryList.clientHeight >=
      askHistoryList.scrollHeight - 120) askHistoryLoad(false);
});
let searchDebounce;
askSearch.addEventListener("input", () => {
  clearTimeout(searchDebounce);
  searchDebounce = setTimeout(() => askHistoryLoad(true), 250);
});
askHistoryList.addEventListener("click", async (event) => {
  const seal = event.target.closest(".ask-seal");
  if (seal) {
    askSetPrivate(seal.closest(".ask-hitem").dataset.ask, seal.dataset.private === "1");
    return;
  }
  const dbg = event.target.closest(".ask-debug");
  if (dbg) {
    const hitem = dbg.closest(".ask-hitem");
    openAskTrace(hitem.dataset.last || hitem.dataset.ask);
    return;
  }
  const del = event.target.closest(".ask-del");
  if (del) {
    const hitem = del.closest(".ask-hitem");
    const question = hitem.querySelector(".q")?.textContent || "this question";
    const turns = Number(hitem.dataset.turns) || 1;
    const confirmed = await confirmDanger({
      title: turns > 1 ? "Delete this conversation?" : "Delete this question?",
      body: turns > 1
        ? `“${question.slice(0, 120)}” and its ${turns - 1} follow-up${turns > 2 ? "s" : ""} — every ` +
          "question, answer, and transcript in this conversation will be permanently removed " +
          "from your Ask history. There is no undo."
        : `“${question.slice(0, 120)}” — the question, its answer, and its full ` +
          "transcript will be permanently removed from your Ask history. There is no undo.",
      action: "Delete forever",
    });
    if (!confirmed) return;
    try {
      await apiDelete(`/v1/ask/${hitem.dataset.ask}`);
    } catch {
      return; // daemon busy or already gone; leave the item for a reload
    }
    const wasOpen = hitem.classList.contains("active");
    hitem.remove();
    if (wasOpen) askReset();
    if (!askHistoryList.querySelector(".ask-hitem")) askHistoryLoad(true);
    return;
  }
  const item = event.target.closest(".ask-hitem");
  if (!item) return;
  askHistoryList.querySelectorAll(".active").forEach((el) => el.classList.remove("active"));
  item.classList.add("active");
  askOpenConversation(item.dataset.ask);
});

/// Replay a whole conversation into the panel (and make it the one a new
/// question continues). Returns false when it isn't readable — deleted,
/// or sealed by the vault.
async function askOpenConversation(id) {
  let turns = [];
  try { turns = (await api(`/v1/ask/${id}/thread`)).turns || []; } catch { /* treated as unreadable */ }
  if (!turns.length) return false;
  askLog.innerHTML = "";
  askThread = turns.map((t) => t.id); // continuing extends this conversation
  for (const record of turns) {
    askAppend(askQuestionHtml(record.question));
    if (askDebug.checked) {
      for (const step of record.transcript || []) {
        askAppend(`<div class="ask-round"><b>${esc(step.tool)}</b>
          <span>${esc(JSON.stringify(step.args || {}).slice(0, 90))}</span></div>`);
      }
    }
    if (record.status === "error") {
      askAppend(`<div class="ask-a">⚠ this one failed: <span class="mono">${esc(record.error || "unknown")}</span></div>`);
    } else if (record.status === "ok") {
      renderAnswer(record.answer, record.citations, record.rounds, record.id);
    }
  }
  askPaintSeal();
  return true;
}

/// The header lock button: seal the open conversation by hand (no
/// question needed), or unseal one that was only ever sealed by hand.
/// Hidden when nothing is open; a conversation with a vault-open
/// question shows a fixed "private" mark instead — that one can't be undone.
const askSealBtn = document.getElementById("ask-seal");
async function askPaintSeal() {
  const root = askThread[0];
  if (!root) { askSealBtn.hidden = true; return; }
  let p;
  try { p = await api(`/v1/ask/${root}/privacy`); } catch { askSealBtn.hidden = true; return; }
  if (askThread[0] !== root) return; // moved on while we asked
  askSealBtn.hidden = false;
  askSealBtn.disabled = p.private && !p.can_unseal;
  askSealBtn.dataset.private = p.private ? "0" : "1";
  askSealBtn.classList.toggle("on", p.private);
  askSealBtn.textContent = p.private ? "🔒 private" : "🔒 make private";
  askSealBtn.title = !p.private
    ? "Hide this whole conversation whenever the vault is locked"
    : p.can_unseal ? "Private — click to make it public again"
      : "Private for good: a question here was asked with the vault open, so its answers may quote private material";
}
askSealBtn.onclick = () => {
  if (askThread[0]) askSetPrivate(askThread[0], askSealBtn.dataset.private === "1");
};

/// Seal/unseal a conversation (header button or a history item's 🔒).
async function askSetPrivate(id, makePrivate) {
  if (askBusy) return;
  let res;
  try {
    res = await apiPost(`/v1/ask/${id}/privacy`, { private: makePrivate });
  } catch (err) {
    askAppend(`<div class="ask-a">⚠ ${esc(err.message)}</div>`);
    return;
  }
  const isOpen = askThread[0] === id;
  if (!res.visible) {
    // Sealed while the vault is locked: it leaves the screen now.
    if (isOpen) {
      askThread = [];
      askShowHint();
      askPaintSeal();
    }
    askLog.insertAdjacentHTML("afterbegin",
      `<div class="ask-sealed-note">🔒 Sealed. That conversation is private now — it will reappear in your history when you unlock the vault.</div>`);
  } else if (isOpen) {
    askPaintSeal();
  }
  askHistoryLoad(true);
}

/// The vault flipped (button, Privacy page, or the timed relock). The
/// history must show only what the current lock state allows, and a
/// conversation the lock now seals must leave the screen — not linger
/// until the next click.
async function askVaultChanged() {
  if (askBusy) return; // the running ask finishes; its `finish` reloads history
  // "it will reappear when you unlock" is stale the moment the lock flips
  askLog.querySelectorAll(".ask-sealed-note").forEach((el) => el.remove());
  if (askThread.length && !(await askOpenConversation(askThread[0]))) {
    askThread = [];
    askShowHint();
    askPaintSeal();
  }
  askHistoryLoad(true);
}

/* — a "do it" request: the hand-off card. Ask itself never acts; this card
   shows what governance decided — waiting for your OK, queued, running,
   done — and lets you approve or stop it. It keeps itself current until
   the task ends (or the conversation is left). — */
async function askShowTask(card) {
  const ag = await import("/assets/agents.js");
  const node = askAppend(`<div class="ask-a agent-card"></div>`);
  let timer = null;
  const paint = (c) => {
    const t = c.task;
    const live = ["proposed", "queued", "running"].includes(t.status);
    const pending = (c.approvals || []).filter((a) => a.status === "pending");
    const statusText = {
      proposed: pending.length ? "waiting for your OK" : (c.agent ? "waiting" : "no agent can do this yet"),
      queued: "about to start", running: "working on it", done: "done", failed: "couldn't finish",
      cancelled: "stopped", refused: "not allowed by your agent settings",
    }[t.status] || t.status;
    node.innerHTML = `
      <div class="agent-card-head"><span class="eyebrow">${c.agent ? `task for ${esc(c.agent.name)}` : "a task to hand off"}</span>
        ${c.depth && ag.DEPTH[c.depth] ? `<span class="agent-depth" data-depth="${esc(c.depth)}" title="${esc(ag.DEPTH[c.depth][1])}">${esc(ag.DEPTH[c.depth][0])}</span>` : ""}
        <span class="agent-status-tag" data-status="${esc(t.status)}">${esc(statusText)}</span></div>
      <details><summary class="meta">what it was asked to do</summary><div class="md">${md(t.brief)}</div></details>
      <div>${(t.classes || []).map((cl) => `<span class="task-tag plain-tag">${esc(ag.CLASS_LABEL[cl] || cl)}</span>`).join(" ")}</div>
      ${t.status === "refused" && t.error ? `<p class="meta">${esc(t.error)}</p>` : ""}
      ${c.offline ? `<p class="meta">⏸ waiting for ${esc(c.offline.node)} — it hasn't been seen since
        ${esc(c.offline.since.slice(0, 16).replace("T", " "))}. The task runs when it's back.</p>` : ""}
      ${pending.length ? `<div class="agent-card-approvals">${ag.approvalRows(pending)}</div>` : ""}
      ${c.guidance ? `<div class="agent-guide"><b>${esc(c.guidance.title)}</b>
        <ol>${c.guidance.steps.map((s) => `<li>${esc(s)}</li>`).join("")}</ol>
        <a class="btn" href="#/agents">set up an agent</a></div>` : ""}
      ${t.result ? `<div class="md agent-result">${md(t.result)}</div>` : ""}
      ${t.status === "failed" && t.error ? `<p class="meta">✗ ${esc(t.error)}</p>` : ""}
      ${live && c.agent ? `<div class="tend-actions"><button class="btn btn-quiet agent-card-stop">stop</button></div>` : ""}`;
    ag.wireApprovals(node, apiPost, refresh);
    const stop = node.querySelector(".agent-card-stop");
    if (stop) stop.onclick = async () => {
      try { paint(await apiPost(`/v1/agents/tasks/${encodeURIComponent(t.id)}/cancel`, {})); } catch { /* shown on next refresh */ }
    };
    clearTimeout(timer);
    if (live && c.agent && document.body.contains(node)) timer = setTimeout(refresh, 3000);
  };
  const refresh = async () => {
    if (!document.body.contains(node)) return; // the conversation was left
    try { paint(await api(`/v1/agents/tasks/${encodeURIComponent(card.task.id)}`)); } catch { /* keep the last view */ }
  };
  paint(card);
}

/* — asking — */
async function askSend() {
  const question = askInput.value.trim();
  if (!question || askBusy) return;
  askBusy = true;
  askInput.value = "";
  askAppend(askQuestionHtml(question));
  const thinking = askAppend(
    `<div class="ask-thinking"><span class="flame"></span><span class="verb">consulting the brain</span></div>`);
  const verbEl = thinking.querySelector(".verb");

  const threadParam = askThread.length
    ? `&thread=${encodeURIComponent(askThread.join(","))}` : "";
  const source = new EventSource(
    `/v1/query/stream?question=${encodeURIComponent(question)}${threadParam}`);
  let settled = false;
  // The answer as it is being written (`delta` events). It is a preview:
  // the `answer` event carries the checked text and replaces it, and
  // `answer_reset` takes back a reply that did not stand.
  let streamEl = null;
  let streamText = "";
  let streamPaint = 0;
  const dropStream = () => {
    if (streamEl) streamEl.remove();
    streamEl = null;
    streamText = "";
  };
  const finish = () => {
    if (settled) return;
    settled = true;
    source.close(); // must close ourselves or EventSource reconnects and re-runs the query
    dropStream();
    thinking.remove();
    askBusy = false;
    askHistoryLoad(true);
    askPaintSeal(); // a first answer makes this a conversation worth sealing
  };
  source.addEventListener("round", (event) => {
    const { step } = JSON.parse(event.data);
    if (askDebug.checked) {
      const argsShort = JSON.stringify(step.args || {});
      askLog.insertBefore(
        Object.assign(document.createElement("div"), {
          className: "ask-round",
          innerHTML: `<b>${esc(step.tool)}</b> <span>${esc(argsShort.length > 90 ? argsShort.slice(0, 89) + "…" : argsShort)}</span>`,
        }),
        thinking);
    } else {
      const verbs = ALIVE_VERBS[step.tool] || ["thinking"];
      verbEl.style.opacity = 0;
      setTimeout(() => {
        verbEl.textContent = verbs[aliveIndex++ % verbs.length] + "…";
        verbEl.style.opacity = 1;
      }, 200);
    }
    askLog.scrollTop = askLog.scrollHeight;
  });
  source.addEventListener("meta", (event) => {
    // Every turn — including one that later fails — joins the thread,
    // so "try again" has the failed attempt as context.
    try { askThread.push(JSON.parse(event.data).ask_id); } catch {}
  });
  source.addEventListener("delta", (event) => {
    streamText += JSON.parse(event.data).text || "";
    if (!streamEl) {
      streamEl = Object.assign(document.createElement("div"),
        { className: "ask-a ask-streaming", innerHTML: `<div class="md"></div>` });
      askLog.insertBefore(streamEl, thinking);
      verbEl.textContent = "writing…";
    }
    if (streamPaint) return;
    streamPaint = requestAnimationFrame(() => {
      streamPaint = 0;
      if (!streamEl) return;
      // A chart or action block still being written would show as raw
      // JSON — hold it back until its fence closes.
      let text = streamText;
      if ((text.match(/```/g) || []).length % 2) text = text.slice(0, text.lastIndexOf("```")) + "…";
      // follow the text only while the reader is already at the bottom
      const follow = askLog.scrollHeight - askLog.scrollTop - askLog.clientHeight < 80;
      streamEl.firstElementChild.innerHTML = md(text);
      if (follow) askLog.scrollTop = askLog.scrollHeight;
    });
  });
  source.addEventListener("answer_reset", dropStream);
  // A "do it" request's card arrives just BEFORE the answer; it is shown
  // right under the answer.
  let taskCard = null;
  source.addEventListener("task", (event) => {
    try { taskCard = JSON.parse(event.data).card; } catch { /* a malformed card is skipped */ }
  });
  source.addEventListener("answer", (event) => {
    const data = JSON.parse(event.data);
    // Swapping the preview for the final text must not move the page
    // under someone already reading it.
    const keepScroll = streamEl ? askLog.scrollTop : null;
    dropStream();
    // the meta event pushed this turn's ask id onto the thread
    renderAnswer(data.answer, data.citations, data.rounds,
      askThread[askThread.length - 1]);
    if (taskCard) askShowTask(taskCard);
    if (keepScroll != null) askLog.scrollTop = keepScroll;
    finish();
  });
  source.addEventListener("fail", (event) => {
    let message = "the engine stumbled";
    try { message = JSON.parse(event.data).message; } catch {}
    askAppend(`<div class="ask-a">⚠ ${esc(message)}<div class="meta" style="color:var(--ink-faint);margin-top:4px">this failure was traced for the QA loop</div></div>`);
    finish();
  });
  source.addEventListener("done", finish);
  source.onerror = () => {
    // built-in connection error (distinct from our "fail" event)
    if (!settled) {
      askAppend(`<div class="ask-a">⚠ lost the connection — the trace was still recorded</div>`);
      finish();
    }
  };
}
document.getElementById("ask-send").onclick = askSend;
askInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); askSend(); }
});

/* ————— the front door: when an admin exists, everything waits for a
   session. The overlay covers the app until /v1/auth/me says we're in;
   logout lives in the hamburger. Open installs never see any of it. */
async function configNudge() {
  try {
    const s = await (await fetch("/v1/monitor/summary", { headers: authHeaders() })).json();
    const missing = s.llm_configured === false ? 1 : 0;
    const hint = document.getElementById("int-hint");
    window.__hamFlags = window.__hamFlags || { update: false, llm: false };
    window.__hamFlags.llm = !!missing;
    paintHamBadge();
    if (hint && missing) hint.textContent = "⚠ set your Anthropic key";
  } catch { /* daemon will say so elsewhere */ }
}

async function authGate() {
  let me = null;
  try {
    me = await (await fetch("/v1/auth/me")).json();
  } catch { /* daemon unreachable — the app's own errors will say so */ }
  if (!me || me.auth !== "required") return;
  // the static Sign out lives in the hamburger (index.html), hidden until
  // an admin exists; unhide + wire it here.
  const logout = document.getElementById("nav-logout");
  if (logout) {
    logout.hidden = false;
    const hint = document.getElementById("nav-logout-user");
    if (hint) hint.textContent = me.user || "";
    logout.onclick = async (e) => {
      e.preventDefault();
      await fetch("/v1/auth/logout", { method: "POST" });
      location.reload();
    };
  }
  if (me.user) return; // live session — carry on
  await new Promise(() => {
    const overlay = document.createElement("div");
    overlay.className = "note-overlay";
    overlay.style.zIndex = "300";
    document.body.appendChild(overlay);
    const passwordForm = () => {
      overlay.innerHTML = `
      <div class="person-card" style="max-width:380px">
        <span class="eyebrow">hearthloom</span>
        <h2 class="pm-name">Sign in</h2>
        <p class="meta">This record is protected. Sign in with the email and
          password chosen when it was set up.</p>
        <div class="searchbar" style="flex-direction:column; align-items:stretch; gap:var(--s2)">
          <input id="login-user" placeholder="email" type="email" autocomplete="email">
          <input id="login-pass" placeholder="password" type="password" autocomplete="current-password">
          <button class="btn" id="login-go">sign in</button>
          <p class="meta" id="login-err" hidden></p>
        </div>
      </div>`;
      const go = async () => {
        const err = overlay.querySelector("#login-err");
        const res = await fetch("/v1/auth/login", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            email: overlay.querySelector("#login-user").value.trim(),
            password: overlay.querySelector("#login-pass").value,
          }),
        });
        if (res.ok) { location.reload(); return; }
        err.hidden = false;
        err.textContent = (await res.json()).error || "that didn't work";
      };
      overlay.querySelector("#login-go").onclick = go;
      overlay.addEventListener("keydown", (e) => { if (e.key === "Enter") go(); });
      overlay.querySelector("#login-user").focus();
    };
    // A provisioner-managed node (hearthlaunch): hand the user back to
    // the provisioner's own sign-in — it re-enters via the trusted-issuer
    // handoff — instead of presenting a second, confusing password form.
    // The password path stays reachable (the provisioner could be down;
    // the user owns the substrate).
    if (me.provisioner) {
      const dest = me.provisioner.replace(/\/$/, "") +
        "/?open=" + encodeURIComponent(location.origin);
      let host = me.provisioner;
      try { host = new URL(me.provisioner).host; } catch { /* show as-is */ }
      overlay.innerHTML = `
      <div class="person-card" style="max-width:380px">
        <span class="eyebrow">hearthloom</span>
        <h2 class="pm-name">Sign in</h2>
        <p class="meta">This brain is managed through <b>${host}</b> —
          taking you there to sign in…</p>
        <div class="searchbar" style="flex-direction:column; align-items:stretch; gap:var(--s2)">
          <button class="btn" id="login-provisioner">continue to ${host} →</button>
          <p class="meta"><a href="#" id="login-local">sign in with a password instead</a></p>
        </div>
      </div>`;
      const timer = setTimeout(() => { location.href = dest; }, 1500);
      overlay.querySelector("#login-provisioner").onclick = () => {
        clearTimeout(timer);
        location.href = dest;
      };
      overlay.querySelector("#login-local").onclick = (e) => {
        e.preventDefault();
        clearTimeout(timer);
        passwordForm();
      };
    } else {
      passwordForm();
    }
    // the promise never resolves — the reload after login restarts the app
  });
}

/* ————— the full export card (Privacy): start → live progress with an
   ETA → download button. Progress survives reloads (server-side). ————— */
let exportPoll = null;
async function paintExportProgress() {
  const el = document.getElementById("export-progress");
  const dl = document.getElementById("export-download");
  const start = document.getElementById("export-start");
  if (!el) { clearInterval(exportPoll); exportPoll = null; return; }
  const s = await api("/v1/export/status").catch(() => null);
  if (!s) return;
  el.hidden = false;
  if (s.phase === "ready") {
    el.innerHTML = `✓ export ready (${Math.round((s.bytes || 0) / 1e6)} MB, finished ${esc((s.finished || "").slice(0, 16).replace("T", " "))})`;
    dl.hidden = false;
    start.textContent = "prepare a fresh export";
    start.disabled = false;
    if (!s.running) { clearInterval(exportPoll); exportPoll = null; }
  } else if (s.phase === "error") {
    el.textContent = `✗ export failed: ${s.error}`;
    start.disabled = false;
    clearInterval(exportPoll); exportPoll = null;
  } else if (s.running) {
    const eta = s.eta_seconds > 0
      ? s.eta_seconds > 90 ? `~${Math.ceil(s.eta_seconds / 60)}m left` : `~${s.eta_seconds}s left`
      : "estimating…";
    el.innerHTML = `<span class="spinner"></span> ${esc(s.phase)} — ${s.pct ?? 0}% (${eta})`;
    start.disabled = true;
  } else if (s.phase === "idle") {
    el.hidden = true;
  }
}
function wireExportCard() {
  const start = document.getElementById("export-start");
  if (!start) return;
  start.onclick = async () => {
    const el = document.getElementById("export-progress");
    el.hidden = false;
    try {
      const res = await apiPost("/v1/export/start", {});
      el.textContent = res.started ? "starting…" : res.note;
      start.disabled = true;
      if (!exportPoll) exportPoll = setInterval(paintExportProgress, 2000);
    } catch (err) {
      el.textContent = `✗ ${err.message}`; // e.g. "unlock the vault first…"
    }
  };
  // resume display if a job is already running / a file is ready
  paintExportProgress();
  if (!exportPoll) exportPoll = setInterval(paintExportProgress, 2000);
}

/* ————— the killswitch screen: while a provisioner has restricted this
   node, everything narrows to the survivors — the activator message +
   link, notifications, key revocation, sign out. The 402s carry the
   same message for API clients. ————— */
async function killswitchGate() {
  let me = null;
  try {
    me = await (await fetch("/v1/auth/me")).json();
  } catch { return; }
  if (!me?.killswitch) return;
  const data = await api("/v1/notifications").catch(() => ({ notifications: [] }));
  const ks = (data.notifications || []).find((n) => n.dedupe_key === "killswitch") || {};
  const keys = await api("/v1/keys").catch(() => ({ keys: [] }));
  const active = (keys.keys || []).filter((k) => !k.revoked_at);
  const overlay = document.createElement("div");
  overlay.className = "note-overlay";
  overlay.style.zIndex = "310";
  overlay.innerHTML = `
    <div class="person-card" style="max-width:560px; max-height:85vh; overflow:auto">
      <span class="eyebrow">this node is restricted</span>
      <h2 class="pm-name">${esc(ks.title || "Access is limited right now")}</h2>
      <p>${esc(ks.body || "Your provisioner has restricted this node.")}</p>
      ${ks.link ? `<p><a class="btn" href="${esc(ks.link)}">${esc(ks.link_label || "resolve this →")}</a></p>` : ""}
      <div class="section-title" style="margin-top:var(--s3)">notifications</div>
      ${(data.notifications || []).slice(0, 12).map((n) => `
        <div class="meta" style="margin:var(--s1) 0"><b>${esc(n.title)}</b>
          <span>· ${esc(n.origin)} · ${esc((n.ts || "").slice(0, 16).replace("T", " "))}</span>
          ${n.body ? `<br>${esc(n.body)}` : ""}</div>`).join("")}
      <div class="section-title" style="margin-top:var(--s3)">API keys (revoke only)</div>
      ${active.length ? active.map((k) => `
        <div class="task-row meta"><b>${esc(k.name)}</b>
          <button class="btn btn-quiet ks-revoke" data-id="${esc(k.id)}">revoke</button></div>`).join("")
        : `<p class="meta">no active keys</p>`}
      <div class="section-title" style="margin-top:var(--s3)">your data stays yours</div>
      <p class="meta">Even while restricted, you can export everything — one zip that
        rebuilds your whole record elsewhere. Unlock the vault, then prepare it.</p>
      <div class="tend-actions">
        <button class="btn btn-quiet" id="ks-unlock">unlock the vault</button>
        <button class="btn" id="ks-export">prepare my export</button>
        <a class="btn" id="ks-download" href="/v1/export/download" download hidden>⬇ download export</a>
      </div>
      <p class="meta" id="ks-export-note" hidden></p>
      <div class="tend-actions" style="margin-top:var(--s3)">
        <button class="btn btn-quiet" id="ks-logout">sign out</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);
  overlay.querySelectorAll(".ks-revoke").forEach((btn) => {
    btn.onclick = async () => {
      await apiPost(`/v1/keys/${btn.dataset.id}/revoke`, { actor: "human:dashboard" }).catch(() => {});
      btn.closest(".task-row").remove();
    };
  });
  // the data-liberty path: vault unlock + export survive the killswitch
  overlay.querySelector("#ks-unlock").onclick = async () => {
    const note = overlay.querySelector("#ks-export-note");
    note.hidden = false;
    try {
      await apiPost("/v1/vault", { actor: "human:dashboard", action: "unlock" });
      note.textContent = "✓ vault unlocked for 15 minutes — now prepare the export";
    } catch (err) { note.textContent = `✗ ${err.message}`; }
  };
  overlay.querySelector("#ks-export").onclick = async () => {
    const note = overlay.querySelector("#ks-export-note");
    note.hidden = false;
    try {
      const res = await apiPost("/v1/export/start", {});
      note.textContent = res.started ? "preparing…" : res.note;
      const poll = setInterval(async () => {
        const st = await api("/v1/export/status").catch(() => null);
        if (!st) return;
        if (st.phase === "ready") {
          clearInterval(poll);
          note.textContent = `✓ ready (${Math.round((st.bytes || 0) / 1e6)} MB)`;
          overlay.querySelector("#ks-download").hidden = false;
        } else if (st.phase === "error") {
          clearInterval(poll);
          note.textContent = `✗ ${st.error}`;
        } else if (st.running) {
          const eta = st.eta_seconds > 0 ? ` — ~${st.eta_seconds > 90 ? Math.ceil(st.eta_seconds / 60) + "m" : st.eta_seconds + "s"} left` : "";
          note.textContent = `${st.phase} ${st.pct ?? 0}%${eta}`;
        }
      }, 2000);
    } catch (err) { note.textContent = `✗ ${err.message}`; }
  };
  overlay.querySelector("#ks-logout").onclick = async () => {
    await fetch("/v1/auth/logout", { method: "POST" });
    location.reload();
  };
  await new Promise(() => {}); // never resolves — the app stays gated
}

/* ————— the notification bell ————— */
const notifBell = document.getElementById("notif-bell");
const notifBadge = document.getElementById("notif-badge");
async function refreshNotifBadge() {
  try {
    const d = await api("/v1/notifications");
    const n = d.unread || 0;
    notifBadge.hidden = !n;
    notifBadge.textContent = n > 99 ? "99+" : n;
    notifBell.classList.toggle("notif-critical",
      (d.notifications || []).some((x) => x.severity === "critical" && !x.read_at && !x.dismissed_at));
  } catch { /* daemon warming up */ }
}
function notifRow(n) {
  const when = (n.ts || "").slice(0, 16).replace("T", " ");
  const sev = n.severity === "critical" ? "⛔" : n.severity === "warn" ? "⚠" : "·";
  // link safety is enforced server-side (allowlist at create); origin
  // renders on every row so a plugin can never impersonate the system
  return `
    <div class="notif-row ${n.read_at ? "notif-read" : ""}" data-id="${esc(n.id)}">
      <div><b>${sev} ${esc(n.title)}</b>
        <span class="meta">${esc(n.origin)} · ${esc(when)}</span></div>
      ${n.body ? `<div class="meta">${esc(n.body)}</div>` : ""}
      ${n.link ? `<a class="meta" href="${esc(n.link)}">${esc(n.link_label || n.link)}</a>` : ""}
      <button class="btn btn-quiet notif-dismiss" title="dismiss">×</button>
    </div>`;
}
async function openNotifPanel() {
  const existing = document.getElementById("notif-panel");
  if (existing) { existing.remove(); return; }
  const d = await api("/v1/notifications").catch(() => ({ notifications: [] }));
  const panel = document.createElement("div");
  panel.id = "notif-panel";
  panel.innerHTML = `
    <div class="task-row" style="justify-content:space-between">
      <b>Notifications</b>
      <button class="btn btn-quiet" id="notif-read-all">mark all read</button>
    </div>
    ${(d.notifications || []).length
      ? (d.notifications || []).map(notifRow).join("")
      : `<p class="meta">nothing yet — peering, imports, and connected systems land here</p>`}`;
  document.body.appendChild(panel);
  panel.querySelector("#notif-read-all").onclick = async () => {
    await apiPost("/v1/notifications/read-all", {}).catch(() => {});
    panel.remove();
    refreshNotifBadge();
  };
  panel.querySelectorAll(".notif-dismiss").forEach((btn) => {
    btn.onclick = async (e) => {
      e.stopPropagation();
      const row = btn.closest(".notif-row");
      await apiPost(`/v1/notifications/${row.dataset.id}/dismiss`, {}).catch(() => {});
      row.remove();
      refreshNotifBadge();
    };
  });
  panel.querySelectorAll(".notif-row").forEach((row) => {
    row.onclick = async () => {
      await apiPost(`/v1/notifications/${row.dataset.id}/read`, {}).catch(() => {});
      row.classList.add("notif-read");
      refreshNotifBadge();
    };
  });
  const close = (e) => {
    if (!panel.contains(e.target) && e.target !== notifBell) {
      panel.remove();
      document.removeEventListener("click", close);
    }
  };
  setTimeout(() => document.addEventListener("click", close), 0);
}
if (notifBell) {
  notifBell.onclick = openNotifPanel;
  setInterval(refreshNotifBadge, 60_000);
}

authGate().then(async () => {
  await killswitchGate();
  refreshNotifBadge();
  loadPluginUi();
  loadOnboarding().then(paintOnboardingHero);
  refreshChips().then(() => {
    // first run: an empty brain lands on the wizard
    if (window.__stats && window.__stats.episodes === 0 && !location.hash) {
      location.hash = "#/welcome";
    }
    paintOnboardingHero(); // re-judge now that the real counts exist
  });
  setInterval(refreshChips, 60_000);
  configNudge();
  setInterval(configNudge, 120_000);
  route();
});
