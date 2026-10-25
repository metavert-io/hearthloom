// notes.js — Notes as a first-class view, plus the shared slide-over
// editor (the Desk imports both). Notes are markdown files the user
// owns; quiet ones settle into the record as episodes. The editor keeps
// codi's manners: hashtags typed in the title become tags, optimistic
// version locking, per-note scope (private = vault-hidden while locked).

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const ACTOR = "human:dashboard";

// Both mention forms (item 13): typed @[Type:Name] and legacy @[Name].
// $1 is always the display name — the prefix must be a real entity type
// or the colon stays part of the name (a legacy "@[re: x]" never
// misparses). Server twin: store/notes.rs split_typed_mention.
export const MENTION_RE =
  /@\[(?:(?:person|org|group|project|place|artifact|event|topic|device):\s*)?([^\]]{1,80})\]/gi;

// titles may carry @[Name] mention tokens — show them as plain @Name in
// lists (esc first, then unwrap the bracket form).
export const noteTitle = (t) => esc(t).replace(MENTION_RE, "@$1");

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

/* — compact markdown renderer (escape-first, like app.js's md) with
   @[Name] mentions linkified to dossiers; bare @text stays text — */
export function renderMarkdown(text) {
  let src = esc(String(text ?? ""));
  src = src.replace(/```([\s\S]*?)```/g, (_, code) => `<pre><code>${code.trim()}</code></pre>`);
  const lines = src.split("\n");
  const out = [];
  let list = false;
  const inline = (t) => t
    // images: only substrate media (vault-guarded route) or the web —
    // never arbitrary local paths
    .replace(/!\[([^\]]*)\]\((\/notes-media\/[^)\s]+|https?:[^)\s]+)\)/g,
      '<img class="note-img" src="$2" alt="$1" loading="lazy">')
    // [[Note Title]] wiki-links — resolved on click (an unknown title
    // opens a fresh note with that title: the red-link flow)
    .replace(/\[\[([^\]]{1,200})\]\]/g, (_, title) =>
      `<a class="wikilink" href="#" data-title="${title}">${title}</a>`)
    .replace(MENTION_RE, (_, name) =>
      `<a class="mention" href="#/entity/${encodeURIComponent(name)}">@${name}</a>`)
    // [ep:ID] episode references → an actual link to the episode (item 9);
    // was rendered as raw ugly text before
    .replace(/\[ep:([a-f0-9]{6,})\]/gi, (_, id) =>
      `<a class="ep-link" href="#/episode/${id}" title="episode ${id}">episode ↗</a>`)
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*]+)\*/g, "<em>$1</em>")
    .replace(/`([^`]+)`/g, "<code>$1</code>")
    .replace(/\[([^\]]+)\]\((https?:[^)]+)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>');
  for (const line of lines) {
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
  return out.join("\n");
}

/* — wiki-link clicks, wherever rendered markdown appears: resolve the
   title to a note and open it; no hit → a fresh note with that title.
   One module-level delegate (ES modules load once). — */
document.addEventListener("click", (e) => {
  const a = e.target.closest("a.wikilink");
  if (!a) return;
  e.preventDefault();
  openNoteByTitle(a.dataset.title);
});
export async function openNoteByTitle(title) {
  if (!title) return;
  const data = await api(`/v1/notes?limit=10&q=${encodeURIComponent(title)}`).catch(() => null);
  const hit = (data?.notes || []).find((n) => n.title.toLowerCase() === title.toLowerCase());
  if (hit) window.__openNoteById?.(hit.id);
  else window.__openNotePrefilled?.({ title });
}

/* — the [[ typeahead: the @-mention pattern pointed at note titles.
   Picking inserts a [[Title]] wiki-link; plain typing stays text — */
export function attachWikiTypeahead(fieldEl, popEl) {
  if (!popEl) {
    popEl = document.createElement("div");
    popEl.className = "ne-mention-pop ne-pop-under";
    popEl.hidden = true;
    const parent = fieldEl.parentElement;
    if (getComputedStyle(parent).position === "static") parent.style.position = "relative";
    fieldEl.insertAdjacentElement("afterend", popEl);
  }
  let linkAt = -1;
  const hidePop = () => { popEl.hidden = true; linkAt = -1; };
  fieldEl.addEventListener("input", async () => {
    const caret = fieldEl.selectionStart;
    const before = fieldEl.value.slice(0, caret);
    const at = before.lastIndexOf("[[");
    if (at < 0) { hidePop(); return; }
    const query = before.slice(at + 2);
    if (query.length < 2 || query.includes("]")) { hidePop(); return; }
    try {
      const data = await api(`/v1/notes?limit=6&q=${encodeURIComponent(query)}`);
      const hits = (data.notes || []).filter((n) => n.kind !== "template").slice(0, 6);
      if (!hits.length) { hidePop(); return; }
      linkAt = at;
      popEl.innerHTML = hits.map((n) =>
        `<button class="ne-mention-opt" data-title="${esc(n.title)}">${noteTitle(n.title)}
           <span class="meta">note</span></button>`).join("");
      popEl.hidden = false;
      popEl.querySelectorAll(".ne-mention-opt").forEach((opt) => {
        opt.onclick = () => {
          const title = opt.dataset.title;
          const caretNow = fieldEl.selectionStart;
          fieldEl.value = fieldEl.value.slice(0, linkAt) + `[[${title}]] ` + fieldEl.value.slice(caretNow);
          hidePop();
          fieldEl.focus();
          // a programmatic insert fires no input event — synthesize one
          // so the host's autosave sees the change
          fieldEl.dispatchEvent(new Event("input", { bubbles: true }));
        };
      });
    } catch { hidePop(); }
  });
  fieldEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !popEl.hidden) { e.stopImmediatePropagation(); hidePop(); }
    if (e.key === "Enter" && !popEl.hidden) {
      const first = popEl.querySelector(".ne-mention-opt");
      if (first) { e.preventDefault(); e.stopImmediatePropagation(); first.click(); }
    }
  });
  return hidePop;
}

/* — the @ typeahead, shareable: works on any input/textarea (the note
   editor and the Desk's task fields both use it). Only an explicit pick
   inserts a linkable @[Name]; Esc or typing on leaves plain text — */
export function attachMentionTypeahead(fieldEl, popEl) {
  if (!popEl) {
    popEl = document.createElement("div");
    popEl.className = "ne-mention-pop ne-pop-under";
    popEl.hidden = true;
    const parent = fieldEl.parentElement;
    if (getComputedStyle(parent).position === "static") parent.style.position = "relative";
    fieldEl.insertAdjacentElement("afterend", popEl);
  }
  let mentionAt = -1;
  const hidePop = () => { popEl.hidden = true; mentionAt = -1; };
  fieldEl.addEventListener("input", async () => {
    const caret = fieldEl.selectionStart;
    const before = fieldEl.value.slice(0, caret);
    const at = before.lastIndexOf("@");
    if (at < 0 || (at > 0 && /[\w\]]/.test(before[at - 1]))) { hidePop(); return; }
    const query = before.slice(at + 1);
    if (query.length < 2 || /[\s@\[]/.test(query)) { hidePop(); return; }
    try {
      const data = await api(`/v1/browse/entities?q=${encodeURIComponent(query)}&type=Person,Org,Project&limit=6`);
      const hits = (data.items || data.entities || []).slice(0, 6);
      if (!hits.length) { hidePop(); return; }
      mentionAt = at;
      popEl.innerHTML = hits.map((h) =>
        `<button class="ne-mention-opt" data-name="${esc(h.name)}" data-type="${esc(h.entity_type || h.type || "")}">${esc(h.name)}
           <span class="meta">${esc(h.entity_type || h.type || "")}</span></button>`).join("");
      popEl.hidden = false;
      popEl.querySelectorAll(".ne-mention-opt").forEach((opt) => {
        opt.onclick = () => {
          const name = opt.dataset.name;
          // a pick inserts the TYPED form (item 13) — it disambiguates a
          // Person from an Org sharing the name; legacy @[Name] mentions
          // keep parsing and migrate whenever they're re-picked
          const token = opt.dataset.type ? `${opt.dataset.type}:${name}` : name;
          const caretNow = fieldEl.selectionStart;
          fieldEl.value = fieldEl.value.slice(0, mentionAt) + `@[${token}] ` + fieldEl.value.slice(caretNow);
          hidePop();
          fieldEl.focus();
        };
      });
    } catch { hidePop(); }
  });
  // stopImmediatePropagation: the host's own Enter handler (quick-add,
  // task edit) must not fire while a pick is being made.
  fieldEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !popEl.hidden) { e.stopImmediatePropagation(); hidePop(); }
    if (e.key === "Enter" && !popEl.hidden) {
      const first = popEl.querySelector(".ne-mention-opt");
      if (first) { e.preventDefault(); e.stopImmediatePropagation(); first.click(); }
    }
  });
  return hidePop;
}

/* — the slide-over editor; confirmDanger is borrowed from the host — */
export function buildNoteEditor(container, refreshNotes, confirmDanger) {
  // A body-level singleton: position:fixed inside a transformed ancestor
  // (the sliding notes panel) would be trapped in ITS box — transforms
  // change the containing block for fixed descendants. One editor,
  // always on <body>, replaced per mount.
  document.getElementById("note-editor-overlay")?.remove();
  const overlay = document.createElement("div");
  overlay.className = "note-overlay";
  overlay.id = "note-editor-overlay";
  overlay.hidden = true;
  overlay.innerHTML = `
    <div class="note-editor">
      <div class="searchbar" style="margin-bottom:var(--s3)">
        <input id="ne-title" placeholder="title… (@ to mention)" style="flex:1">
        <div id="ne-title-pop" class="ne-mention-pop" hidden></div>
        <button class="btn ne-vault" id="ne-vault" title="vault this note (make it private)">🔓</button>
        <select id="ne-scope" title="scope" hidden>
          ${["personal", "work", "family", "health", "private"]
            .map((s) => `<option>${s}</option>`).join("")}
        </select>
        <button class="btn" id="ne-share" title="share this note with a connected brain">⇄</button>
        <button class="btn" id="ne-archive" title="snapshot into the record and file away">archive</button>
        <button class="btn" id="ne-delete">delete</button>
        <button class="btn" id="ne-close">✕</button>
      </div>
      <div class="searchbar" id="ne-share-row" hidden style="align-items:center; flex-wrap:wrap">
        <span class="meta">share with:</span>
        <select id="ne-share-friend"></select>
        <button class="btn btn-quiet" id="ne-share-go">share</button>
        <span id="ne-share-badges"></span>
      </div>
      <input id="ne-tags" placeholder="#tags — space or comma separated (hashtags typed in the title land here too)">
      <div class="ne-viewbar">
        <button class="ne-mode active" data-mode="visual">visual</button>
        <button class="ne-mode" data-mode="markdown">markdown</button>
        <button class="ne-mode" data-mode="history">history</button>
        <span class="meta">@ mentions people · [[ links notes — pick from the list · saves as you type</span>
      </div>
      <div id="ne-rendered" class="md ne-rendered" title="click to edit"></div>
      <textarea id="ne-body" spellcheck="true" placeholder="write…" hidden></textarea>
      <div id="ne-history" class="ne-rendered" hidden></div>
      <div id="ne-mention-pop" class="ne-mention-pop" hidden></div>
      <div id="ne-refs" class="ne-refs" hidden></div>
      <div class="meta" id="ne-status"></div>
    </div>`;
  void container; // lifecycle owner varies; the overlay lives on body
  document.body.appendChild(overlay);

  let current = null; // {id, version, linked_task}
  let openSelf; // the returned opener — connection chips reuse it
  const titleEl = overlay.querySelector("#ne-title");
  const bodyEl = overlay.querySelector("#ne-body");
  const tagsEl = overlay.querySelector("#ne-tags");
  const scopeEl = overlay.querySelector("#ne-scope");
  const statusEl = overlay.querySelector("#ne-status");
  const renderedEl = overlay.querySelector("#ne-rendered");
  const popEl = overlay.querySelector("#ne-mention-pop");
  const vaultBtn = overlay.querySelector("#ne-vault");
  const refsEl = overlay.querySelector("#ne-refs");
  let lastOpenScope = "personal"; // restore target when un-vaulting

  // — sharing (v46): pin this note to a connected brain. The row lazily
  // loads connections; badges show who has it, × un-shares. Private
  // notes refuse server-side (the vault rule) — the error shows in status.
  const shareRow = overlay.querySelector("#ne-share-row");
  const paintShareBadges = async () => {
    const host = overlay.querySelector("#ne-share-badges");
    if (!current?.id || !host) return;
    const data = await api(`/v1/sharing?kind=note&item_id=${encodeURIComponent(current.id)}`)
      .catch(() => ({ shares: [] }));
    host.innerHTML = (data.shares || []).map((s) =>
      `<span class="task-tag plain-tag">${esc(s.label || s.friend)}
        <button class="linkish ne-unshare" data-friend="${esc(s.friend)}" title="un-share">×</button></span>`).join(" ");
    host.querySelectorAll(".ne-unshare").forEach((btn) => {
      btn.onclick = async () => {
        await apiPost("/v1/sharing", {
          actor: ACTOR, action: "unshare", kind: "note",
          item_id: current.id, friend: btn.dataset.friend,
        }).catch(() => {});
        paintShareBadges();
      };
    });
  };
  overlay.querySelector("#ne-share").onclick = async () => {
    shareRow.hidden = !shareRow.hidden;
    if (shareRow.hidden) return;
    const sel = overlay.querySelector("#ne-share-friend");
    const [f, g, gr] = await Promise.all([
      api("/v1/friends").catch(() => ({ friends: [] })),
      api("/v1/grants").catch(() => ({ grants: [] })),
      api("/v1/groups").catch(() => ({ groups: [] })),
    ]);
    const names = [...new Set([
      ...(f.friends || []).map((x) => x.name),
      ...(g.grants || []).filter((x) => !x.revoked_at && x.friend !== "__default__")
        .map((x) => x.friend),
    ])];
    sel.innerHTML = [
      ...(gr.groups || []).map((x) => `<option value="${esc(x.key)}">${esc(x.name)} (group)</option>`),
      ...names.map((n) => `<option value="${esc(n)}">${esc(n)}</option>`),
    ].join("") || `<option value="">no connections yet — see ☰ → Network</option>`;
    paintShareBadges();
  };
  overlay.querySelector("#ne-share-go").onclick = async () => {
    const friend = overlay.querySelector("#ne-share-friend").value;
    if (!friend || !current?.id) return;
    const res = await apiPost("/v1/sharing", {
      actor: ACTOR, action: "share", kind: "note", item_id: current.id, friend,
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { statusEl.textContent = `✗ ${res.error}`; return; }
    paintShareBadges();
  };

  const TYPE_ICON = { Project: "◈", Org: "◍", Group: "◍", Person: "◍", Place: "⚑" };
  // ONE references panel at the bottom of the note (item 11), organized
  // by Jon's explicit-vs-semantic observation: EXPLICIT rows first —
  // mentions (@-entities), tags (#hashtags → the Notes view filtered),
  // linked (wiki-links out →, backlinks in ←) — then the IMPLICIT row,
  // resonant (nearest notes by embedding, computed at idle).
  const paintRefs = (data) => {
    const mentions = data?.mentions || [];
    const tags = data?.note?.tags || [];
    const links = data?.links || [];
    const backlinks = data?.backlinks || [];
    const resonant = data?.resonant || [];
    if (!mentions.length && !tags.length && !links.length && !backlinks.length && !resonant.length) {
      refsEl.hidden = true;
      refsEl.innerHTML = "";
      return;
    }
    const noteChip = (n, mark, title) =>
      `<button class="assoc-chip" data-note="${esc(n.id)}" title="${title}">${mark} ${noteTitle(n.title)}</button>`;
    const row = (label, chips) => chips
      ? `<div class="ne-refs-row"><span class="ne-refs-label">${label}</span>${chips}</div>`
      : "";
    refsEl.innerHTML =
      row("mentions", mentions.map((m) =>
        `<button class="assoc-chip" data-name="${esc(m.name)}" title="${esc(m.type)} — open">${TYPE_ICON[m.type] || "•"} ${esc(m.name)}</button>`).join(""))
      + row("tags", tags.map((t) =>
        `<button class="assoc-chip" data-tag="${esc(t)}" title="all notes tagged #${esc(t)}">#${esc(t)}</button>`).join(""))
      + row("linked",
        links.map((n) => noteChip(n, "→", "this note links to it")).join("")
        + backlinks.map((n) => noteChip(n, "←", "links back to this note")).join(""))
      + row("resonant", resonant.map((n) => noteChip(n, "≈", "nearby in meaning — no explicit link")).join(""));
    refsEl.hidden = false;
    refsEl.querySelectorAll("[data-name]").forEach((c) =>
      c.onclick = () => window.__openPersonModal?.(c.dataset.name));
    refsEl.querySelectorAll("[data-note]").forEach((c) =>
      c.onclick = () => { flushSave(); openSelf(c.dataset.note, null); });
    refsEl.querySelectorAll("[data-tag]").forEach((c) =>
      c.onclick = () => {
        sessionStorage.setItem("hl-notes-tag", c.dataset.tag);
        close();
        if (location.hash === "#/notes") window.dispatchEvent(new HashChangeEvent("hashchange"));
        else location.hash = "#/notes";
      });
  };
  const reflectVault = () => {
    const priv = scopeEl.value === "private";
    vaultBtn.textContent = priv ? "🔒" : "🔓";
    vaultBtn.title = priv ? "vaulted — click to unvault" : "vault this note (make it private)";
    vaultBtn.classList.toggle("vaulted", priv);
  };
  vaultBtn.onclick = () => {
    if (scopeEl.value === "private") scopeEl.value = lastOpenScope;
    else { lastOpenScope = scopeEl.value; scopeEl.value = "private"; }
    reflectVault();
    scheduleSave();
  };

  /* — visual ↔ markdown: visual renders; clicking it (or the toggle)
     drops into the raw markdown — */
  const historyEl = overlay.querySelector("#ne-history");
  // — the version browser (v43 op_journal): every save journaled the
  // full body; loading a version into the editor is the human gesture,
  // the normal autosave then makes it the newest version — no silent
  // writes, and nothing is ever lost (the loaded-over state is itself
  // a journaled version).
  const paintHistory = async () => {
    if (!current?.id) {
      historyEl.innerHTML = `<p class="meta">no history yet — this note hasn't been saved.</p>`;
      return;
    }
    historyEl.innerHTML = `<p class="meta">reading the journal…</p>`;
    const data = await api(`/v1/notes/${current.id}/versions`).catch(() => null);
    const versions = data?.versions || [];
    historyEl.innerHTML = versions.length ? versions.map((v) => `
      <div class="task-row ne-ver" data-entry="${esc(v.entry_id)}" style="cursor:pointer; gap:var(--s2)">
        <span class="task-tag plain-tag">${esc(v.op)}</span>
        <b>${esc((v.ts || "").slice(0, 16).replace("T", " "))}</b>
        <span class="meta">${esc(v.title || "")}${v.body_chars != null ? ` · ${v.body_chars} chars` : ""} · ${esc(v.actor)}</span>
      </div>
      <div class="ne-ver-preview" data-for="${esc(v.entry_id)}" hidden style="margin:0 0 var(--s3) var(--s4)"></div>`).join("")
      : `<p class="meta">no journaled versions (notes journal from v43 on — older edits predate the ledger).</p>`;
    historyEl.querySelectorAll(".ne-ver").forEach((row) => {
      row.onclick = async () => {
        const pre = historyEl.querySelector(`.ne-ver-preview[data-for="${row.dataset.entry}"]`);
        if (!pre.hidden) { pre.hidden = true; return; }
        pre.hidden = false;
        pre.innerHTML = `<p class="meta">…</p>`;
        const v = await api(`/v1/notes/${current.id}/versions/${row.dataset.entry}`).catch(() => null);
        if (!v) { pre.innerHTML = `<p class="meta">✗ couldn't load that version</p>`; return; }
        pre.innerHTML = `
          <div class="tend-actions" style="margin-bottom:var(--s2)">
            <button class="btn btn-quiet ne-ver-load">load this version into the editor</button>
          </div>
          <div class="md" style="opacity:.85">${renderMarkdown(v.body || "") || `<p class="meta">(empty)</p>`}</div>`;
        pre.querySelector(".ne-ver-load").onclick = () => {
          bodyEl.value = v.body || "";
          if (v.title) titleEl.value = v.title;
          setMode("markdown");
          // the existing autosave pipeline saves it as the NEWEST version
          bodyEl.dispatchEvent(new Event("input", { bubbles: true }));
        };
      };
    });
  };
  const setMode = (mode) => {
    overlay.querySelectorAll(".ne-mode").forEach((b) =>
      b.classList.toggle("active", b.dataset.mode === mode));
    const visual = mode === "visual";
    renderedEl.hidden = !visual;
    bodyEl.hidden = mode !== "markdown";
    historyEl.hidden = mode !== "history";
    if (visual) {
      renderedEl.innerHTML = renderMarkdown(bodyEl.value) ||
        `<p class="meta" style="color:var(--ink-faint)">nothing yet — click to write</p>`;
    } else if (mode === "markdown") {
      bodyEl.focus();
    } else {
      paintHistory();
    }
  };
  overlay.querySelectorAll(".ne-mode").forEach((b) => {
    b.onclick = () => setMode(b.dataset.mode);
  });
  renderedEl.onclick = (e) => {
    if (e.target.closest("a")) return; // mention/links still navigate
    setMode("markdown");
  };

  /* — @ typeahead: on BOTH the body and the title (Jon: mention in the
     title too), each on its own pop — */
  const hidePop = attachMentionTypeahead(bodyEl, popEl);
  attachMentionTypeahead(titleEl, overlay.querySelector("#ne-title-pop"));
  // [[ typeahead on the body — its own pop, under the caret area
  attachWikiTypeahead(bodyEl, null);

  // codi's flow, kept: hashtags typed anywhere in the title become tags.
  // (An @[Name] token is left alone — that's a mention, not a tag.)
  const gatherTags = () => {
    const fromInput = tagsEl.value.split(/[\s,]+/).map((t) => t.replace(/^#/, "").toLowerCase())
      .filter((t) => t.length >= 2);
    const fromTitle = [...titleEl.value.matchAll(/#([\w-]{2,})/g)].map((m) => m[1].toLowerCase());
    titleEl.value = titleEl.value.replace(/#[\w-]{2,}\s*/g, "").trim();
    return [...new Set([...fromInput, ...fromTitle])];
  };

  const close = () => { flushSave(); overlay.hidden = true; current = null; refreshNotes(); };
  overlay.querySelector("#ne-close").onclick = close;
  overlay.addEventListener("click", (e) => { if (e.target === overlay) close(); });

  /* — AUTOSAVE (Jon: no Save button; save as you type). Debounced;
     creates the note on the first real content, then version-updates.
     A brand-new note needs a title — if you've only written a body, we
     borrow its first line so nothing is ever lost. — */
  let saveTimer = null;
  let saving = false;
  let dirty = false;
  let suppress = false; // don't autosave while loading a note in

  const effectiveTitle = () => {
    const t = titleEl.value.trim();
    if (t) return t;
    const firstLine = (bodyEl.value.split("\n").find((l) => l.trim()) || "").trim();
    return firstLine ? firstLine.slice(0, 80) : "";
  };

  async function doSave() {
    if (saving) { dirty = true; return; }
    const tags = gatherTags();
    tagsEl.value = tags.map((t) => `#${t}`).join(" ");
    const title = effectiveTitle();
    if (!title && !bodyEl.value.trim()) return; // truly empty — nothing yet
    if (!title) return; // body-only with no derivable title (blank lines)
    saving = true;
    dirty = false;
    try {
      if (current?.id) {
        const result = await apiPost(`/v1/notes/${current.id}`, {
          actor: ACTOR, title, body: bodyEl.value, tags,
          scope: scopeEl.value, version: current.version,
        });
        current.version = result.version;
        statusEl.textContent = `saved · ${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}`;
      } else {
        const result = await apiPost("/v1/notes", {
          actor: ACTOR, title, body: bodyEl.value, tags,
          scope: scopeEl.value,
          kind: current?.kind || "note",
          linked_task: current?.linked_task || null,
        });
        current = { ...(current || {}), id: result.id, version: 1 };
        statusEl.textContent = "saved";
        refreshNotes(); // a new note now exists in the lists
      }
    } catch (err) {
      statusEl.textContent = err.message; // version conflicts surface here
    } finally {
      saving = false;
      if (dirty) doSave();
    }
  }
  function scheduleSave() {
    if (suppress) return;
    dirty = true;
    statusEl.textContent = "saving…";
    clearTimeout(saveTimer);
    saveTimer = setTimeout(doSave, 700);
  }
  function flushSave() {
    clearTimeout(saveTimer);
    if (dirty || statusEl.textContent === "saving…") doSave();
  }
  [titleEl, bodyEl, tagsEl].forEach((el) => el.addEventListener("input", scheduleSave));
  scopeEl.addEventListener("change", () => { scheduleSave(); flushSave(); });
  window.addEventListener("beforeunload", flushSave);
  overlay.querySelector("#ne-archive").onclick = async () => {
    if (!current?.id) return;
    await apiPost(`/v1/notes/${current.id}/status`, { actor: ACTOR, status: "archived" });
    close();
  };
  overlay.querySelector("#ne-delete").onclick = async () => {
    if (!current?.id) { close(); return; }
    if (!(await confirmDanger({
      title: "Delete this note?",
      body: "The markdown file stays on disk (nothing raw is ever discarded), but the note leaves the desk.",
      action: "delete",
    }))) return;
    await apiPost(`/v1/notes/${current.id}/status`, { actor: ACTOR, status: "deleted" });
    close();
  };

  openSelf = async function openNoteEditor(noteId, prefill) {
    // The singleton may have been replaced by another view's editor
    // (each mount removes the previous overlay) — a detached overlay
    // un-hides invisibly. Reclaim the body slot on every open.
    if (!overlay.isConnected) {
      document.getElementById("note-editor-overlay")?.remove();
      document.body.appendChild(overlay);
    }
    overlay.hidden = false;
    statusEl.textContent = "";
    suppress = true; // loading fields must not trigger autosave
    paintRefs(null);
    if (noteId) {
      const data = await api(`/v1/notes/${noteId}`);
      current = { id: noteId, version: data.note.version };
      titleEl.value = data.note.title;
      bodyEl.value = data.body;
      tagsEl.value = (data.note.tags || []).map((t) => `#${t}`).join(" ");
      scopeEl.value = data.note.scope || "personal";
      if (scopeEl.value !== "private") lastOpenScope = scopeEl.value;
      paintRefs(data);
      // Notes the consolidator drafts/maintains (origin 'loom') carry an
      // explicit AI badge — the user should know Loom may revise them, while
      // still being free to edit (item 8).
      if (data.note.origin === "loom") {
        statusEl.innerHTML =
          `<span class="task-tag" title="Loom's consolidator drafts and periodically revises this note — your edits are kept, but it may update it">🤖 AI-maintained</span> <span class="meta">— you can still edit it freely</span>`;
      } else if (data.note.origin && data.note.origin !== "hearthloom") {
        statusEl.textContent = `arrived here from ${data.note.origin}${data.note.created_at ? ` · created ${data.note.created_at.slice(0, 10)}` : " · date unknown at the source"}`;
      } else {
        statusEl.textContent = "";
      }
    } else {
      current = prefill?.linked_task ? { linked_task: prefill.linked_task } : null;
      if (prefill?.kind === "template") current = { ...(current || {}), kind: "template" };
      titleEl.value = prefill?.title || "";
      bodyEl.value = prefill?.body || "";
      tagsEl.value = "";
      scopeEl.value = prefill?.scope || "personal";
      lastOpenScope = scopeEl.value === "private" ? "personal" : scopeEl.value;
      if (prefill?.kind === "template") {
        statusEl.textContent = "a template — tasks tagged #its-name will use it";
      }
    }
    hidePop();
    reflectVault();
    setMode(noteId ? "visual" : "markdown");
    (titleEl.value ? bodyEl : titleEl).focus();
    suppress = false; // ready — real edits from here autosave
  };
  return openSelf;
}

/* — a compact list of notes; the Desk shows 5, the Notes view all — */
export async function paintNoteList(listEl, openNoteEditor, { q = "", limit = 200 } = {}) {
  const data = await api(`/v1/notes?limit=${limit}${q ? `&q=${encodeURIComponent(q)}` : ""}`);
  const notes = data.notes || [];
  listEl.innerHTML = notes.length
    ? notes.map((n) => `
      <div class="note-row" data-note="${esc(n.id)}">
        <span class="note-title">${noteTitle(n.title)}
          ${n.kind === "template" ? `<span class="task-tag" title="tasks tagged #name use this">template</span>` : ""}
          ${(n.tags || []).map((t) => `<span class="task-tag plain-tag">#${esc(t)}</span>`).join("")}</span>
        <span class="meta">${n.origin && n.origin !== "hearthloom" ? `<span class="task-tag plain-tag" title="how this note got here">${esc(n.origin)}</span> ` : ""}${n.updated_at ? esc(n.updated_at.slice(0, 10)) : "—"}${n.status === "archived" ? " · archived" : ""}${n.scope === "private" ? " · 🔒" : ""}</span>
      </div>`).join("")
    : `<p class="meta" style="color:var(--ink-faint)">no notes${q ? " match" : " yet"}.</p>`;
  listEl.querySelectorAll(".note-row").forEach((row) => {
    row.onclick = () => openNoteEditor(row.dataset.note, null);
  });
  return notes.length;
}

/* — the full Notes view: the home of your notes. A dial rides on top
   (most-accessed labels); quick tabs for your commonest #tags; a search
   bar where a leading # picks a tag and @ picks a person (typeahead);
   modified-date default sort; infinite scroll through the whole set. — */
export async function mount(container, { confirmDanger }) {
  container.innerHTML = `
    <span class="eyebrow">notes</span>
    <h1 class="pagehead">Notes</h1>
    <p class="subhead">Plain markdown files you own, under <code>~/.hearthloom/notes/</code> —
      searchable here, quoted by Ask, and settled into the record when they rest.
      Type <b>#</b> to filter by tag or <b>@</b> to filter by who a note mentions.</p>
    <div id="notes-dial"></div>
    <div class="notes-dial-mode meta">
      anchor labels by
      <label><input type="radio" name="notes-anchor" value="modified" checked> modified</label>
      <label><input type="radio" name="notes-anchor" value="created"> created</label>
      <span id="notes-window" class="meta"></span>
    </div>
    <div class="searchbar" style="position:relative">
      <input id="notes-q" placeholder="search notes — # for a tag, @ for a person…" autocomplete="off">
      <div id="notes-typeahead" class="ne-mention-pop ne-pop-under" hidden></div>
      <button class="btn" id="notes-new">+ note</button>
      <button class="btn btn-quiet" id="notes-new-template"
        title="a template is a note: frontmatter defines the task form, the body is the note skeleton">+ template</button>
    </div>
    <div id="notes-tabs" class="desk-tagbar"></div>
    <div id="notes-active-filter" class="meta" hidden></div>
    <div id="notes-list"><div class="loading">opening the drawer…</div></div>
    <div id="notes-sentinel"></div>`;

  const listEl = container.querySelector("#notes-list");
  const windowEl = container.querySelector("#notes-window");
  const tabsEl = container.querySelector("#notes-tabs");
  const filterEl = container.querySelector("#notes-active-filter");
  const qEl = container.querySelector("#notes-q");
  const popEl = container.querySelector("#notes-typeahead");
  let refresh = () => {};
  const openNoteEditor = buildNoteEditor(container, () => refresh(), confirmDanger);
  let anchorMode = "modified"; // modified | created — Jon: modified default
  let range = null;            // [fromISO, toISO] from the dial
  let tagFilter = null;        // exact-tag filter (quick tab or #query)
  let mentionFilter = null;    // person a note @-mentions (@query)
  // a tag chip elsewhere (the editor's references panel) hands off here
  const prefillTag = sessionStorage.getItem("hl-notes-tag");
  if (prefillTag) { sessionStorage.removeItem("hl-notes-tag"); tagFilter = prefillTag; }
  const PAGE = 60;
  let offset = 0;
  let exhausted = false;
  let loading = false;

  const anchorOf = (n) =>
    ((anchorMode === "created" ? n.created_at : n.updated_at) || "").slice(0, 10);

  // v46: who each note is shared with — one fetch paints every badge
  let noteShares = {};
  api("/v1/sharing").then((d) => {
    noteShares = {};
    for (const s of d.shares || []) {
      if (s.kind !== "note") continue;
      (noteShares[s.item_id] = noteShares[s.item_id] || []).push(s);
    }
  }).catch(() => {});

  const rowHtml = (n) => `
    <div class="note-row" data-note="${esc(n.id)}">
      <span class="note-title">${noteTitle(n.title)}
        ${(n.tags || []).map((t) => `<span class="task-tag plain-tag">#${esc(t)}</span>`).join("")}${
          (noteShares[n.id] || []).map((s) => window.friendAvatar?.(s.friend, {
            label: s.label, title: `shared with ${s.label || s.friend}` }) || "").join("")}</span>
      <span class="meta">${n.origin && n.origin !== "hearthloom" ? `<span class="task-tag plain-tag" title="how this note got here">${esc(n.origin)}</span> ` : ""}${anchorOf(n) ? esc(anchorOf(n)) : "—"}${n.views > 1 ? ` · read ×${n.views}` : ""}${n.status === "archived" ? " · archived" : ""}${n.scope === "private" ? " · 🔒" : ""}</span>
    </div>`;
  const wire = (scope) => scope.querySelectorAll(".note-row").forEach((row) => {
    if (row.dataset.wired) return;
    row.dataset.wired = "1";
    row.onclick = () => openNoteEditor(row.dataset.note, null);
  });

  // one query URL from the current filter state
  const url = (off) => {
    const p = new URLSearchParams();
    p.set("limit", PAGE);
    p.set("offset", off);
    const q = qEl.value.trim();
    if (mentionFilter) p.set("mentions", mentionFilter);
    else if (tagFilter) p.set("tag", tagFilter);
    else if (q) p.set("q", q);
    return `/v1/notes?${p}`;
  };

  // dial-window filtering is client-side over the loaded page(s)
  const inRange = (n) => !range || (anchorOf(n) >= range[0] && anchorOf(n) <= range[1]);

  refresh = async () => {
    offset = 0; exhausted = false;
    windowEl.textContent = range ? ` · ${range[0]} → ${range[1]} (spin the dial out to clear)` : "";
    if (mentionFilter) { filterEl.hidden = false; filterEl.innerHTML = `notes mentioning <b>@${esc(mentionFilter)}</b> — <a href="#" id="nf-clear">clear</a>`; }
    else if (tagFilter) { filterEl.hidden = false; filterEl.innerHTML = `tag <b>#${esc(tagFilter)}</b> — <a href="#" id="nf-clear">clear</a>`; }
    else filterEl.hidden = true;
    const clear = filterEl.querySelector("#nf-clear");
    if (clear) clear.onclick = (e) => { e.preventDefault(); tagFilter = null; mentionFilter = null; qEl.value = ""; paintTabs(); refresh(); };
    const data = await api(url(0));
    let notes = (data.notes || []).filter(inRange);
    offset = (data.notes || []).length;
    exhausted = (data.notes || []).length < PAGE;
    listEl.innerHTML = notes.length
      ? notes.map(rowHtml).join("")
      : `<p class="meta" style="color:var(--ink-faint)">no notes${qEl.value.trim() || tagFilter || mentionFilter || range ? " in view" : " yet"}.</p>`;
    wire(listEl);
  };

  async function loadMore() {
    if (loading || exhausted || range) return; // range mode is client-filtered; no paging
    loading = true;
    const data = await api(url(offset));
    const notes = data.notes || [];
    offset += notes.length;
    if (notes.length < PAGE) exhausted = true;
    if (notes.length) {
      listEl.insertAdjacentHTML("beforeend", notes.map(rowHtml).join(""));
      wire(listEl);
    }
    loading = false;
  }
  const io = new IntersectionObserver((entries) => {
    if (entries[0].isIntersecting) loadMore();
  }, { rootMargin: "600px" });
  io.observe(container.querySelector("#notes-sentinel"));

  // — quick tabs: your commonest tags —
  async function paintTabs() {
    const { tags } = await api("/v1/notes/tags").catch(() => ({ tags: [] }));
    tabsEl.innerHTML = (tags || []).slice(0, 10).map((t) =>
      `<button class="loom-chip${tagFilter === t.tag ? "" : " resting"}" data-tag="${esc(t.tag)}">#${esc(t.tag)} ${t.n}</button>`).join("");
    tabsEl.querySelectorAll("[data-tag]").forEach((b) => {
      b.onclick = () => {
        tagFilter = tagFilter === b.dataset.tag ? null : b.dataset.tag;
        mentionFilter = null; qEl.value = "";
        paintTabs(); refresh();
      };
    });
  }

  // — the search bar: # → tag typeahead, @ → person typeahead —
  const hidePop = () => { popEl.hidden = true; };
  qEl.addEventListener("input", async () => {
    const v = qEl.value;
    if (v.startsWith("#") && v.length >= 2) {
      const { tags } = await api("/v1/notes/tags").catch(() => ({ tags: [] }));
      const q = v.slice(1).toLowerCase();
      const hits = (tags || []).filter((t) => t.tag.includes(q)).slice(0, 8);
      if (!hits.length) { hidePop(); return; }
      popEl.innerHTML = hits.map((t) =>
        `<button class="ne-mention-opt" data-tag="${esc(t.tag)}">#${esc(t.tag)}<span class="meta">${t.n}</span></button>`).join("");
      popEl.hidden = false;
      popEl.querySelectorAll("[data-tag]").forEach((opt) => opt.onclick = () => {
        tagFilter = opt.dataset.tag; mentionFilter = null; qEl.value = ""; hidePop(); paintTabs(); refresh();
      });
      return;
    }
    if (v.startsWith("@") && v.length >= 3) {
      const data = await api(`/v1/browse/entities?q=${encodeURIComponent(v.slice(1))}&type=Person,Org,Project&limit=6`).catch(() => null);
      const hits = (data?.items || []).slice(0, 6);
      if (!hits.length) { hidePop(); return; }
      popEl.innerHTML = hits.map((h) =>
        `<button class="ne-mention-opt" data-name="${esc(h.name)}">@${esc(h.name)}<span class="meta">${esc(h.entity_type || "")}</span></button>`).join("");
      popEl.hidden = false;
      popEl.querySelectorAll("[data-name]").forEach((opt) => opt.onclick = () => {
        mentionFilter = opt.dataset.name; tagFilter = null; qEl.value = ""; hidePop(); paintTabs(); refresh();
      });
      return;
    }
    hidePop();
    tagFilter = null; mentionFilter = null;
    refresh();
  });
  qEl.addEventListener("blur", () => setTimeout(hidePop, 150));

  container.querySelector("#notes-new").onclick = () => openNoteEditor(null, null);
  container.querySelector("#notes-new-template").onclick = () => openNoteEditor(null, {
    kind: "template",
    title: "my template name",
    body: `---template
field: source | Source | text
field: topic | Topic | text
note-on-complete: true
---
_from task: {{task}} · {{date}}_

**Source:** {{source}}
**Topic:** {{topic}}

## What I learned

`,
  });
  await paintTabs();
  await refresh();

  // — the dial: most-accessed notes as labels, weighted by views —
  async function mountNotesDial() {
    const host = container.querySelector("#notes-dial");
    host.innerHTML = "";
    try {
      const { mountDial } = await import("/assets/dial.js");
      const today = new Date().toISOString().slice(0, 10);
      mountDial(host, {
        from: undefined,
        to: today,
        labelFilter: (ev) => ev.kind === "note-label",
        extraLabels: async (from, to) => {
          const data = await api("/v1/notes?limit=300");
          return (data.notes || [])
            .filter((n) => n.kind !== "template")
            .filter((n) => anchorOf(n) >= from && anchorOf(n) <= to)
            .map((n) => ({
              day: anchorOf(n), title: n.title, kind: "note-label", note_id: n.id,
              score: 20 + (n.views || 0) * 10,
            }));
        },
        onLabel: (ev) => { if (ev.note_id) openNoteEditor(ev.note_id, null); },
        filters: () => ({ q: anchorMode }),
        onWindow: (from, to) => {
          range = to >= today && from <= "2010-01-01" ? null : [from, to];
          refresh();
        },
        onPick: () => {},
      });
    } catch (err) {
      console.warn("notes dial failed to mount:", err);
    }
  }
  container.querySelectorAll("input[name=notes-anchor]").forEach((radio) => {
    radio.onchange = () => {
      anchorMode = radio.value;
      range = null;
      mountNotesDial();
      refresh();
    };
  });
  await mountNotesDial();
  return () => io.disconnect();
}
