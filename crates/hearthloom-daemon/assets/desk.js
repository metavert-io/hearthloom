// desk.js — the present tense. Tasks (plain / checklist / workout / form
// via templates), goals (intents grown up: decomposed standing queries,
// matches, cascades), and notes (markdown files the user owns). Finished
// things become memories: completing a task or settling a note mints an
// episode — this view is the workshop, the record lives everywhere else.
//
// Lazy-loaded like atlas/pulse; helpers passed in from app.js.

import { buildNoteEditor, paintNoteList, attachMentionTypeahead, MENTION_RE } from "/assets/notes.js";

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&#39;", "'": "&#39;" }[c]));

// Escape text AND turn bare http(s):// URLs into clickable links. Trailing
// sentence punctuation is left outside the link; only http/https is linked
// (never javascript:, mailto:, etc.), and the whole string stays escaped.
const URL_RE = /https?:\/\/[^\s]+/g;
function linkify(raw) {
  const s = String(raw ?? "");
  let out = "", last = 0, m;
  URL_RE.lastIndex = 0;
  while ((m = URL_RE.exec(s)) !== null) {
    out += esc(s.slice(last, m.index));
    let url = m[0], trail = "";
    const t = url.match(/[.,;:!?)\]}'"]+$/);
    if (t) { trail = url.slice(url.length - t[0].length); url = url.slice(0, url.length - t[0].length); }
    const safe = esc(url);
    out += `<a href="${safe}" class="task-link" target="_blank" rel="noopener noreferrer">${safe}</a>${esc(trail)}`;
    last = m.index + m[0].length;
  }
  out += esc(s.slice(last));
  return out;
}

let api, apiPost, confirmDanger, mdRender;
const ACTOR = "human:dashboard";

// A "list of items" glyph for checklist cards — clicking it opens the
// checklist (rather than completing the task outright).
const LIST_ICON = `<svg viewBox="0 0 16 16" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M5.5 4h8M5.5 8h8M5.5 12h8"/><path d="M2.2 4h.01M2.2 8h.01M2.2 12h.01"/></svg>`;
// a dumbbell for workouts, a clipboard for form tasks — shown on the card's
// completion button in place of the kind label
const WORKOUT_ICON = `<svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6 8v8M18 8v8M3 10.5v3M21 10.5v3M6 12h12"/></svg>`;
const FORM_ICON = `<svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="5" y="3.5" width="14" height="17" rx="2"/><path d="M9 3.5h6v2.5H9z"/><path d="M8.5 11h7M8.5 15h4"/></svg>`;

// "finished <when>" for a completed card: today / yesterday / "Mon D".
function finishedLabel(iso) {
  const d = (iso || "").slice(0, 10);
  if (!d) return "";
  const today = new Date().toISOString().slice(0, 10);
  const yest = new Date(Date.now() - 86400000).toISOString().slice(0, 10);
  if (d === today) return "today";
  if (d === yest) return "yesterday";
  return new Date(d + "T12:00:00Z").toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/* — celebration: sparks rise from the hearth when something finishes — */
function emberBurst(fromEl) {
  const rect = fromEl.getBoundingClientRect();
  for (let i = 0; i < 14; i++) {
    const spark = document.createElement("span");
    spark.className = "ember-spark";
    spark.style.left = `${rect.x + rect.width / 2}px`;
    spark.style.top = `${rect.y + rect.height / 2}px`;
    spark.style.setProperty("--dx", `${(Math.random() - 0.5) * 90}px`);
    spark.style.setProperty("--dy", `${-40 - Math.random() * 90}px`);
    spark.style.setProperty("--dur", `${650 + Math.random() * 500}ms`);
    spark.style.setProperty("--size", `${2 + Math.random() * 3.5}px`);
    document.body.appendChild(spark);
    setTimeout(() => spark.remove(), 1300);
  }
}

/* — streak from the 14-day completions series: consecutive days with a
   finish, ending today or yesterday (a day off doesn't cool the fire
   until it's two) — */
function streakOf(completions) {
  const days = new Set((completions || []).map(([d]) => d));
  const DAY = 86400000;
  let cursor = Date.now();
  if (!days.has(new Date(cursor).toISOString().slice(0, 10))) cursor -= DAY;
  let streak = 0;
  while (days.has(new Date(cursor).toISOString().slice(0, 10))) {
    streak += 1;
    cursor -= DAY;
  }
  return streak;
}

const TOAST_LINES = [
  "the fire is fed",
  "another log on the hearth",
  "the record grows",
  "warmth earned",
];
function toast(text) {
  const el = document.createElement("div");
  el.className = "desk-toast";
  el.textContent = text;
  document.body.appendChild(el);
  requestAnimationFrame(() => el.classList.add("in"));
  setTimeout(() => { el.classList.remove("in"); setTimeout(() => el.remove(), 400); }, 2600);
}

/* — a freshly-added task floats in at the top of the list: it grows from
   nothing while fading up, and the cards beneath roll down to make room.
   The next full paint settles it into its real sorted position. — */
function flashNewCard(cardEl) {
  if (!cardEl || !cardEl.animate) return;
  const h = cardEl.offsetHeight;
  if (!h) return;
  cardEl.style.overflow = "hidden";
  const anim = cardEl.animate(
    [
      { height: "0px", opacity: 0, transform: "translateY(-6px)" },
      { height: `${h}px`, opacity: 1, transform: "none" },
    ],
    { duration: 460, easing: "cubic-bezier(.22,.61,.36,1)" },
  );
  anim.onfinish = () => { cardEl.style.overflow = ""; cardEl.style.height = ""; };
}

/* ————— tasks ————— */

function taskPayloadSummary(task) {
  const payload = task.payload || {};
  if (task.kind === "checklist") {
    const items = payload.items || [];
    const done = items.filter((i) => i.done).length;
    return items.length ? `${done} of ${items.length} done` : "";
  }
  if (task.kind === "workout") {
    const n = (payload.exercises || []).length;
    return n ? `${n} exercise${n === 1 ? "" : "s"}` : "";
  }
  return "";
}

// The card meta, minus the kind label (the kind now reads from the button
// icon): recurrence · due/overdue · payload summary. Goal is rendered
// separately so it can carry its own leading separator.
function taskMetaCore(task) {
  const parts = [];
  if (task.frequency && task.frequency !== "one-time") parts.push(esc(task.frequency));
  if (task.remind_at) {
    const d = task.remind_at.slice(0, 10);
    const overdue = d < new Date().toISOString().slice(0, 10) && task.status === "open";
    parts.push(overdue
      ? `<span class="task-overdue">overdue — was due ${esc(d)}</span>`
      : `due ${esc(d)}`);
  }
  const summ = taskPayloadSummary(task);
  if (summ) parts.push(esc(summ));
  return parts.join(" · ");
}

function renderExerciseLine(ex) {
  const bits = [ex.category, ex.name];
  if (ex.minutes) bits.push(`${ex.minutes}min`);
  if (ex.zone) bits.push(ex.zone);
  if (ex.distance) bits.push(`${ex.distance}mi`);
  if (ex.reps && ex.weight) bits.push(`${ex.reps}×${ex.weight}lb`);
  if (ex.seconds) bits.push(`${ex.seconds}s`);
  return bits.filter(Boolean).join(" · ");
}

let taskTagFilter = null;
let myShares = [];   // every explicit share — paints the badges
let myFriends = [];  // connected brains — the share picker
async function paintTasks(host, templates, goals, openNoteEditor, opts = {}) {
  const data = await api("/v1/tasks");
  [myShares, myFriends] = await Promise.all([
    api("/v1/sharing").then((d) => d.shares || []).catch(() => []),
    // share targets = everyone with an ACTIVE grant (who I share TO) ∪
    // outbound connections — a friend who only connects inbound (the
    // common case: they dial the always-on node) must still be pickable
    Promise.all([
      api("/v1/friends").catch(() => ({ friends: [] })),
      api("/v1/grants").catch(() => ({ grants: [] })),
      api("/v1/groups").catch(() => ({ groups: [] })),
    ]).then(([f, g, gr]) => {
      const targets = new Map();
      // groups first: sharing with the whole group is the common wish
      (gr.groups || []).forEach((x) =>
        targets.set(x.key, { name: x.key, label: `${x.name} (group)` }));
      (f.friends || []).forEach((x) => targets.set(x.name, { name: x.name, entity: x.entity }));
      (g.grants || []).forEach((x) => {
        if (!x.revoked_at && x.friend !== "__default__" && !targets.has(x.friend))
          targets.set(x.friend, { name: x.friend });
      });
      return [...targets.values()];
    }),
  ]);
  const tasks = data.tasks || [];
  const match = (t) => !taskTagFilter || (t.tags || []).includes(taskTagFilter);
  const open = tasks.filter((t) => t.status === "open" && match(t));
  // A just-added task floats to the top for this render (it grows in;
  // the real sort takes over on the next paint / reload).
  if (opts.flashId) {
    const i = open.findIndex((t) => t.id === opts.flashId);
    if (i > 0) open.unshift(open.splice(i, 1)[0]);
  }
  const backlog = tasks.filter((t) => t.status === "backlog" && match(t));
  const completions = data.completions || [];
  const total14 = completions.reduce((a, [, n]) => a + n, 0);
  const todayISO = new Date().toISOString().slice(0, 10);
  const doneTodayCount = (completions.find(([d]) => d === todayISO) || [])[1] || 0;
  const allTags = [...new Set(tasks.flatMap((t) => t.tags || []))].sort();

  const goalName = (id) => goals.find((g) => g.id === id)?.name;
  // @[Name] renders as a person link (→ the modal); bare URLs become links.
  const mentionify = (text) => linkify(text).replace(MENTION_RE, (_, n) =>
    `<a class="mention" href="#/entity/${encodeURIComponent(n)}">@${n}</a>`);
  const tagChip = (tag) =>
    `<span class="task-tag${tag === "urgent" || tag === "important" ? "" : " plain-tag"}">#${esc(tag)}</span>`;
  const card = (task) => `
    <div class="card task-card" data-task="${esc(task.id)}" data-kind="${esc(task.kind)}">
      <div class="task-row">
        ${task.kind === "checklist"
          ? `<button class="task-open-btn" title="open the checklist">${LIST_ICON}</button>`
          : task.kind === "workout"
            ? `<button class="task-done-btn has-icon" title="complete workout">${WORKOUT_ICON}</button>`
            : task.kind === "form"
              ? `<button class="task-done-btn has-icon" title="complete form">${FORM_ICON}</button>`
              : `<button class="task-done-btn" title="complete">✓</button>`}
        <div class="task-main">
          <div class="task-desc">${mentionify(task.description)}${(task.tags || []).map(tagChip).join("")}${
            myShares.filter((s) => s.kind === "task" && s.item_id === task.id)
              .map((s) => window.friendAvatar?.(s.friend, {
                entity: myFriends.find((f) => f.name === s.friend)?.entity,
                label: s.label,
                title: `shared with ${s.label || s.friend}${s.collaborate ? " · collaborative" : ""}`,
              }) || "").join("")}</div>
          <div class="meta">
            <span class="task-metacore">${taskMetaCore(task)}</span>${
              task.goal_id && goalName(task.goal_id)
                ? `<span class="task-goal" data-sep="${taskMetaCore(task) ? "1" : "0"}">⭑ ${esc(goalName(task.goal_id))}</span>`
                : ""}
          </div>
        </div>
        <button class="task-edit-btn" title="edit">✎</button>
      </div>
      <div class="task-detail" hidden></div>
    </div>`;

  host.innerHTML = `
    ${allTags.length ? `<div class="desk-tagbar">
      <button class="loom-chip${!taskTagFilter ? "" : " resting"}" data-tag="">all</button>
      ${allTags.map((t) => `<button class="loom-chip${taskTagFilter === t ? "" : " resting"}"
        data-tag="${esc(t)}">#${esc(t)}</button>`).join("")}</div>` : ""}
    ${open.length ? open.map(card).join("") : `<p class="meta" style="color:var(--ink-faint)">nothing open — the desk is clear.</p>`}
    ${backlog.length ? `
      <details class="task-backlog"><summary class="meta">${backlog.length} in the backlog</summary>
        ${backlog.map(card).join("")}</details>` : ""}
    <details class="task-backlog" id="task-completed"><summary class="meta">completed${
      doneTodayCount ? ` — ${doneTodayCount} today` : ""}</summary>
      <div id="task-completed-list"><div class="meta">…</div></div></details>
    <details class="task-backlog" id="task-archive"><summary class="meta">the archive</summary>
      <div id="task-archive-list"><div class="meta">…</div></div></details>
    <details class="task-backlog" id="task-deleted"><summary class="meta">recently deleted</summary>
      <div id="task-deleted-list"><div class="meta">…</div></div></details>
    ${total14 ? `<p class="meta" style="margin-top:var(--s3)">${total14} finished in the last two weeks${
      streakOf(completions) >= 2 ? ` · <span class="task-tag">🜂 ${streakOf(completions)}-day streak</span>` : ""}</p>` : ""}`;

  // The completed drawer: loaded on open — everything finished today, plus
  // up to 10 more recent, newest first. Un-checking a row reopens the task
  // (it comes back to the desk); the finished date shows on each card.
  const completedEl = host.querySelector("#task-completed");
  completedEl.addEventListener("toggle", async () => {
    if (!completedEl.open) return;
    const listEl = host.querySelector("#task-completed-list");
    const doneData = await api("/v1/tasks?status=done&limit=60");
    const done = (doneData.tasks || []).filter(match);
    const todays = done.filter((t) => (t.completed_at || "").slice(0, 10) === todayISO);
    const older = done.filter((t) => (t.completed_at || "").slice(0, 10) !== todayISO).slice(0, 10);
    const shown = todays.concat(older);
    listEl.innerHTML = shown.length ? shown.map((t) => `
      <div class="card task-card done-card" data-done="${esc(t.id)}">
        <div class="task-row">
          <input type="checkbox" class="done-restore" checked
            title="uncheck to bring it back to the desk">
          <div class="task-main">
            <div class="task-desc">${linkify(t.description)}</div>
            <div class="meta">${esc(t.kind)} · finished ${esc(finishedLabel(t.completed_at))}</div>
          </div>
        </div>
      </div>`).join("")
      : `<p class="meta" style="color:var(--ink-faint)">nothing finished yet.</p>`;
    listEl.querySelectorAll(".done-card").forEach((cardEl) => {
      const id = cardEl.dataset.done;
      cardEl.querySelector(".done-restore").onchange = async (e) => {
        if (e.target.checked) return; // only un-checking brings it back
        await apiPost(`/v1/tasks/${id}/uncomplete`, { actor: ACTOR });
        toast("back on the desk");
        paintTasks(host, templates, goals, openNoteEditor);
      };
    });
  });

  // The archive drawer: loaded on open; restore brings a task back to
  // the desk, delete-forever purges the row (its completed history, if
  // any, already lives in the record as episodes and stays).
  const archiveEl = host.querySelector("#task-archive");
  archiveEl.addEventListener("toggle", async () => {
    if (!archiveEl.open) return;
    const listEl = host.querySelector("#task-archive-list");
    const data = await api("/v1/tasks?status=archived");
    const archived = data.tasks || [];
    listEl.innerHTML = archived.length ? archived.map((t) => `
      <div class="card task-card" data-arch="${esc(t.id)}" style="opacity:.75">
        <div class="task-row">
          <div class="task-main">
            <div class="task-desc">${linkify(t.description)}</div>
            <div class="meta">${esc(t.kind)} · archived</div>
          </div>
          <button class="btn btn-quiet arch-restore">restore</button>
          <button class="btn btn-quiet arch-purge">delete forever</button>
        </div>
      </div>`).join("")
      : `<p class="meta" style="color:var(--ink-faint)">nothing archived.</p>`;
    listEl.querySelectorAll(".task-card").forEach((cardEl) => {
      const id = cardEl.dataset.arch;
      cardEl.querySelector(".arch-restore").onclick = async () => {
        await apiPost(`/v1/tasks/${id}`, { actor: ACTOR, status: "open" });
        paintTasks(host, templates, goals, openNoteEditor);
      };
      cardEl.querySelector(".arch-purge").onclick = async () => {
        if (!(await confirmDanger({
          title: "Delete this task forever?",
          body: "The task row is permanently removed. Any completions it minted are episodes — those stay in the record.",
          action: "delete forever",
        }))) return;
        await fetch(`/v1/tasks/${id}?actor=${encodeURIComponent(ACTOR)}`, { method: "DELETE" });
        cardEl.remove();
      };
    });
  });

  // The recently-deleted drawer (v43 op_journal): hard-deleted tasks are
  // recoverable from their journaled post-image — restore re-inserts the
  // full row and journals the restore itself.
  const deletedEl = host.querySelector("#task-deleted");
  deletedEl.addEventListener("toggle", async () => {
    if (!deletedEl.open) return;
    const listEl = host.querySelector("#task-deleted-list");
    const data = await api("/v1/tasks/deleted").catch(() => ({ tasks: [] }));
    const rows = data.tasks || data.deleted || [];
    listEl.innerHTML = rows.length ? rows.map((t) => `
      <div class="card task-card" data-del="${esc(t.id)}" style="opacity:.6">
        <div class="task-row">
          <div class="task-main">
            <div class="task-desc">${linkify(t.description || "(untitled)")}</div>
            <div class="meta">deleted ${esc((t.deleted_at || "").slice(0, 16).replace("T", " "))}</div>
          </div>
          <button class="btn btn-quiet del-restore">restore</button>
        </div>
      </div>`).join("")
      : `<p class="meta" style="color:var(--ink-faint)">nothing deleted recently.</p>`;
    listEl.querySelectorAll(".task-card").forEach((cardEl) => {
      cardEl.querySelector(".del-restore").onclick = async () => {
        await apiPost(`/v1/tasks/${cardEl.dataset.del}/restore`, { actor: ACTOR });
        toast("restored from the journal");
        paintTasks(host, templates, goals, openNoteEditor);
      };
    });
  });

  host.querySelectorAll(".desk-tagbar .loom-chip").forEach((chip) => {
    chip.onclick = () => {
      taskTagFilter = chip.dataset.tag || null;
      paintTasks(host, templates, goals, openNoteEditor);
    };
  });
  host.querySelectorAll(".task-card").forEach((cardEl) => {
    const id = cardEl.dataset.task;
    const task = tasks.find((t) => t.id === id);

    const detail = cardEl.querySelector(".task-detail");
    const descBox = cardEl.querySelector(".task-desc");
    const refresh = () => paintTasks(host, templates, goals, openNoteEditor);

    // The ✓ button (non-checklist kinds) completes the task outright.
    const doneBtn = cardEl.querySelector(".task-done-btn");
    if (doneBtn) doneBtn.onclick = async (e) => {
      // Workouts and forms want their payload finalized in the detail
      // panel; completing from the ✓ sends whatever is currently saved.
      const before = streakOf(completions);
      emberBurst(e.currentTarget);
      const result = await apiPost(`/v1/tasks/${id}/complete`, { actor: ACTOR });
      const doneToday = (completions.find(([d]) => d === new Date().toISOString().slice(0, 10))?.[1] || 0) + 1;
      const line = TOAST_LINES[Math.floor(doneToday % TOAST_LINES.length)];
      const streak = Math.max(before, streakOf(completions.concat([[new Date().toISOString().slice(0, 10), 1]])));
      toast(streak >= 2 ? `${line} — day ${streak} of the streak` : `${line} — ${doneToday} today`);
      if (result.note_prefill) {
        openNoteEditor(null, result.note_prefill);
      }
      paintTasks(host, templates, goals, openNoteEditor);
    };

    // Checklist cards lead with a list icon that opens the checklist
    // (view/check/add items, then "Complete Task" from inside).
    const openBtn = cardEl.querySelector(".task-open-btn");
    if (openBtn) openBtn.onclick = () => {
      if (!detail.hidden) { detail.hidden = true; return; }
      detail.hidden = false;
      paintTaskDetail(detail, task, templates, refresh);
    };

    // The ✎ edit icon: edit the description IN PLACE (no redundant field)
    // and open the detail for kind-payload + due date. Toggling closes it.
    cardEl.querySelector(".task-edit-btn").onclick = () => {
      if (!detail.hidden) {
        detail.hidden = true;
        cardEl.classList.remove("editing");
        descBox.innerHTML = mentionify(task.description) + (task.tags || []).map(tagChip).join("");
        return;
      }
      cardEl.classList.add("editing");
      // Hashtags are stored split-out (server re-extracts on save), so show
      // them inline in the editable text — item 10.
      const rawWithTags = [task.description, ...(task.tags || []).map((t) => `#${t}`)]
        .join(" ").trim();
      descBox.innerHTML =
        `<input class="task-inline-desc" value="${esc(rawWithTags)}" title="edit — Enter saves" style="width:100%">`;
      const inp = descBox.querySelector(".task-inline-desc");
      attachMentionTypeahead(inp);
      attachHashtagTypeahead(inp, allTags);
      inp.focus();
      inp.setSelectionRange(inp.value.length, inp.value.length);
      inp.addEventListener("keydown", async (e) => {
        if (e.key !== "Enter" || !inp.value.trim()) return;
        await apiPost(`/v1/tasks/${id}`, { actor: ACTOR, description: inp.value.trim() });
        refresh();
      });
      detail.hidden = false;
      paintTaskDetail(detail, task, templates, refresh);
    };
  });

  if (opts.flashId) {
    flashNewCard(host.querySelector(`.task-card[data-task="${opts.flashId}"]`));
  }
}

// Hashtag typeahead for task descriptions (item 10): local completion from
// the tags already in use. Mirrors attachMentionTypeahead's shape but needs
// no API — tags are a known, small set. Inserts `#tag `.
export function attachHashtagTypeahead(fieldEl, tags) {
  const pop = document.createElement("div");
  pop.className = "ne-mention-pop ne-pop-under";
  pop.hidden = true;
  const parent = fieldEl.parentElement;
  if (getComputedStyle(parent).position === "static") parent.style.position = "relative";
  fieldEl.insertAdjacentElement("afterend", pop);
  let hashAt = -1;
  const hide = () => { pop.hidden = true; hashAt = -1; };
  fieldEl.addEventListener("input", () => {
    const caret = fieldEl.selectionStart;
    const before = fieldEl.value.slice(0, caret);
    const at = before.lastIndexOf("#");
    if (at < 0 || (at > 0 && /\S/.test(before[at - 1]) && before[at - 1] !== "#")) { hide(); return; }
    const q = before.slice(at + 1).toLowerCase();
    if (/[\s#]/.test(q)) { hide(); return; }
    const hits = (tags || []).filter((t) => t.toLowerCase().startsWith(q)).slice(0, 6);
    if (!hits.length) { hide(); return; }
    hashAt = at;
    pop.innerHTML = hits.map((t) => `<button class="ne-mention-opt" data-tag="${esc(t)}">#${esc(t)}</button>`).join("");
    pop.hidden = false;
    pop.querySelectorAll(".ne-mention-opt").forEach((opt) => {
      opt.onclick = () => {
        const caretNow = fieldEl.selectionStart;
        fieldEl.value = fieldEl.value.slice(0, hashAt) + `#${opt.dataset.tag} ` + fieldEl.value.slice(caretNow);
        hide();
        fieldEl.focus();
      };
    });
  });
  fieldEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !pop.hidden) { e.stopImmediatePropagation(); hide(); }
    if (e.key === "Enter" && !pop.hidden) {
      const first = pop.querySelector(".ne-mention-opt");
      if (first) { e.preventDefault(); e.stopImmediatePropagation(); first.click(); }
    }
  });
  return hide;
}

// Turn a pasted chunk of text into checklist items: one per line,
// trimmed, with a leading list marker stripped (bullets - * • · – — +,
// numbers "1." / "1)" / "(1)", and a markdown checkbox "[ ]" / "[x]").
// Blank lines and marker-only lines drop out.
function splitChecklistText(text) {
  return String(text || "")
    .split(/\r\n|[\r\n\u2028\u2029]/)
    .map((line) => line
      .trim()
      .replace(/^\s*(?:[-*•·–—+]|\d+[.)]|\(\d+\))\s+/, "")
      .replace(/^\[[ xX]?\]\s*/, "")
      .trim())
    .filter(Boolean);
}

function paintTaskDetail(detail, task, templates, refresh) {
  const payload = task.payload || {};
  let inner = "";
  if (task.kind === "checklist") {
    const items = payload.items || [];
    inner = `
      ${items.map((item, i) => `
        <label class="check-item${item.done ? " done" : ""}"><input type="checkbox" data-i="${i}" ${item.done ? "checked" : ""}>
          <span>${linkify(item.text)}</span>
          <button class="linkish check-edit" data-i="${i}" title="edit this item">✎</button>
          <button class="linkish check-del" data-i="${i}" title="delete this item">×</button></label>`).join("")}
      <div class="searchbar" style="margin-top:var(--s2)">
        <input class="check-new" placeholder="add an item… (or paste a list)">
      </div>`;
  } else if (task.kind === "workout") {
    const exercises = payload.exercises || [];
    inner = `
      ${exercises.map((ex, i) => `
        <div class="exercise-line"><span>${esc(renderExerciseLine(ex))}</span>
          <button class="linkish ex-rm" data-i="${i}">×</button></div>`).join("")}
      <div class="exercise-add">
        <select class="ex-cat">
          <option value="strength">strength</option>
          <option value="cardio">cardio</option>
          <option value="flexibility">flexibility</option>
        </select>
        <input class="ex-name" placeholder="exercise…" style="flex:1"
          list="ex-suggest-${esc(task.id)}" autocomplete="off">
        <datalist id="ex-suggest-${esc(task.id)}"></datalist>
        <input class="ex-a" type="number" placeholder="reps" style="width:70px">
        <input class="ex-b" type="number" placeholder="lb" style="width:70px">
        <select class="ex-zone" hidden>
          <option value="">zone…</option>
          ${["zone1", "zone2", "zone3", "zone4", "zone5", "vo2max"]
            .map((z) => `<option>${z}</option>`).join("")}
        </select>
        <input class="ex-dist" type="number" step="0.1" placeholder="mi" style="width:70px" hidden>
        <select class="ex-sets" title="log this as N sets">
          ${[1, 2, 3, 4, 5].map((n) => `<option value="${n}">×${n}</option>`).join("")}
        </select>
        <button class="btn ex-add">add</button>
      </div>`;
  } else if (task.kind === "form") {
    const template = templates.find((t) => t.id === task.template_id);
    const values = payload.values || {};
    const fields = (template?.fields || []);
    inner = fields.map((f) => `
      <label class="form-field">${esc(f.label || f.key)}
        ${f.type === "checkbox"
          ? `<input type="checkbox" data-key="${esc(f.key)}" ${values[f.key] ? "checked" : ""}>`
          : f.type === "select"
            ? `<select data-key="${esc(f.key)}">${(f.options || []).map((o) =>
                `<option ${values[f.key] === o ? "selected" : ""}>${esc(o)}</option>`).join("")}</select>`
            : `<input type="${f.type === "number" ? "number" : "text"}"
                 data-key="${esc(f.key)}" value="${esc(values[f.key] ?? "")}">`}
      </label>`).join("");
  }
  detail.innerHTML = `
    <div class="task-due-row" style="display:flex; gap:var(--s2); align-items:center; margin-bottom:var(--s2); flex-wrap:wrap">
      <label class="meta">due <input class="task-edit-due" type="date"
        value="${esc((task.remind_at || "").slice(0, 10))}" title="due date — clear it to remove"></label>
      <button class="btn btn-quiet task-due-quick" data-days="1">tomorrow</button>
      <button class="btn btn-quiet task-due-quick" data-days="7">next week</button>
    </div>
    ${inner}
    ${myFriends.length ? `
    <div class="searchbar task-share-row" style="margin-top:var(--s2); flex-wrap:wrap; align-items:center">
      <span class="meta">share:</span>
      <select class="task-share-friend">
        <option value="">(choose who)</option>
        ${myFriends.map((f) => `<option value="${esc(f.name)}">${esc(f.label || f.name)}</option>`).join("")}
      </select>
      ${task.kind === "checklist"
        ? `<label class="meta"><input type="checkbox" class="task-share-collab"> they can tick items</label>` : ""}
      <button class="btn btn-quiet task-share-btn">share</button>
      ${myShares.filter((s) => s.kind === "task" && s.item_id === task.id).map((s) =>
        `<span class="task-tag plain-tag">${esc(s.label || s.friend)}${s.collaborate ? " ✎" : ""}
          <button class="linkish task-unshare" data-friend="${esc(s.friend)}" title="un-share">×</button></span>`).join("")}
    </div>` : ""}
    <div class="tend-actions" style="margin-top:var(--s3)">
      ${task.kind === "checklist"
        ? `<button class="btn task-complete-btn" title="finish this checklist">Complete Task</button>` : ""}
      <button class="btn task-backlog-btn">${task.status === "backlog" ? "back to open" : "to backlog"}</button>
      <button class="btn task-del" title="off the desk, restorable from the archive">archive</button>
    </div>`;

  // — sharing (v46): pin this task to a connected brain / un-pin.
  // Updates land IN PLACE: the local share cache mutates, the detail
  // re-renders (staying open), and the card's badges redraw — no full
  // repaint, so the user never loses their spot.
  const redrawShares = () => {
    const card = detail.closest(".task-card");
    card?.querySelectorAll(".task-desc .fravatar, .task-desc .fravatar-group").forEach((a) => a.remove());
    const chips = myShares
      .filter((s) => s.kind === "task" && s.item_id === task.id)
      .map((s) => window.friendAvatar?.(s.friend, {
        entity: myFriends.find((f) => f.name === s.friend)?.entity,
        label: s.label,
        title: `shared with ${s.label || s.friend}${s.collaborate ? " · collaborative" : ""}`,
      }) || "").join("");
    card?.querySelector(".task-desc")?.insertAdjacentHTML("beforeend", chips);
    paintTaskDetail(detail, task, templates, refresh);
  };
  detail.querySelector(".task-share-btn")?.addEventListener("click", async () => {
    const friend = detail.querySelector(".task-share-friend")?.value;
    if (!friend) return;
    const collaborate = detail.querySelector(".task-share-collab")?.checked || false;
    const res = await apiPost("/v1/sharing", {
      actor: ACTOR, action: "share", kind: "task", item_id: task.id, friend, collaborate,
    }).catch(() => null);
    if (!res) return;
    myShares = myShares.filter((s) => !(s.kind === "task" && s.item_id === task.id && s.friend === friend));
    myShares.push({ kind: "task", item_id: task.id, friend, collaborate,
      label: myFriends.find((f) => f.name === friend)?.label || friend });
    redrawShares();
  });
  detail.querySelectorAll(".task-unshare").forEach((btn) => {
    btn.onclick = async () => {
      const friend = btn.dataset.friend;
      await apiPost("/v1/sharing", {
        actor: ACTOR, action: "unshare", kind: "task", item_id: task.id, friend,
      }).catch(() => {});
      myShares = myShares.filter((s) => !(s.kind === "task" && s.item_id === task.id && s.friend === friend));
      redrawShares();
    };
  });

  // Description is edited in place on the card (via the ✎ button), not here.
  const dueEl = detail.querySelector(".task-edit-due");
  const saveDue = async (val) => {
    await apiPost(`/v1/tasks/${task.id}`, {
      actor: ACTOR,
      remind_at: val ? `${val}T12:00:00Z` : null,
    });
    refresh();
  };
  dueEl.addEventListener("change", () => saveDue(dueEl.value));
  detail.querySelectorAll(".task-due-quick").forEach((b) => {
    b.onclick = () => {
      const d = new Date();
      d.setDate(d.getDate() + Number(b.dataset.days));
      saveDue(d.toISOString().slice(0, 10));
    };
  });

  const savePayload = async (next) => {
    await apiPost(`/v1/tasks/${task.id}`, { actor: ACTOR, payload: next });
    task.payload = next;
  };
  if (task.kind === "checklist") {
    // Keep the card's "x of y done" count live while the detail is open,
    // so checking or deleting an item updates it without a full repaint.
    const syncCardSummary = () => {
      const metaEl = detail.closest(".task-card")?.querySelector(".meta");
      if (!metaEl) return;
      const core = taskMetaCore(task);
      const mc = metaEl.querySelector(".task-metacore");
      if (mc) mc.innerHTML = core;
      const ge = metaEl.querySelector(".task-goal");
      if (ge) ge.dataset.sep = core ? "1" : "0";
    };
    detail.querySelectorAll(".check-item input").forEach((box) => {
      box.onchange = async () => {
        box.closest(".check-item")?.classList.toggle("done", box.checked);
        const items = (task.payload?.items || []).slice();
        items[+box.dataset.i] = { ...items[+box.dataset.i], done: box.checked };
        await savePayload({ ...task.payload, items });
        syncCardSummary();
      };
    });
    // ✎ on an item: swap the text for an inline editor. Enter or blur
    // saves (a blank edit is a no-op — deletion stays an explicit ×),
    // Escape cancels. The editor lives inside the <label>, so clicks
    // must not toggle the checkbox.
    detail.querySelectorAll(".check-edit").forEach((btn) => {
      btn.onclick = (e) => {
        e.preventDefault();
        e.stopPropagation();
        const i = +btn.dataset.i;
        const row = btn.closest(".check-item");
        const span = row?.querySelector("span");
        if (!span || row.querySelector(".check-edit-input")) return;
        const current = (task.payload?.items || [])[i]?.text ?? "";
        const input = document.createElement("input");
        input.className = "check-edit-input";
        input.value = current;
        span.replaceWith(input);
        input.focus();
        input.setSelectionRange(current.length, current.length);
        let settled = false;
        const finish = async (save) => {
          if (settled) return;
          settled = true;
          const text = input.value.trim();
          if (save && text && text !== current) {
            const items = (task.payload?.items || []).slice();
            items[i] = { ...items[i], text };
            await savePayload({ ...task.payload, items });
          }
          paintTaskDetail(detail, task, [], refresh);
        };
        input.addEventListener("keydown", (ev) => {
          if (ev.key === "Enter") { ev.preventDefault(); finish(true); }
          else if (ev.key === "Escape") { ev.preventDefault(); finish(false); }
        });
        input.addEventListener("blur", () => finish(true));
        input.addEventListener("click", (ev) => { ev.preventDefault(); ev.stopPropagation(); });
      };
    });
    // × on an item: confirm with a modal, then remove it and re-render.
    detail.querySelectorAll(".check-del").forEach((btn) => {
      btn.onclick = async (e) => {
        e.preventDefault();  // it lives inside the <label> — don't toggle the box
        e.stopPropagation();
        const i = +btn.dataset.i;
        const item = (task.payload?.items || [])[i];
        if (!(await confirmDanger({
          title: "Delete this item?",
          body: `“${item?.text ?? ""}” will be removed from the checklist.`,
          action: "delete",
        }))) return;
        const items = (task.payload?.items || []).slice();
        items.splice(i, 1);
        await savePayload({ ...task.payload, items });
        syncCardSummary();
        paintTaskDetail(detail, task, [], refresh);
      };
    });
    const newItem = detail.querySelector(".check-new");
    const addItems = async (texts) => {
      const fresh = texts.map((t) => ({ text: t, done: false }));
      const items = (task.payload?.items || []).concat(fresh);
      await savePayload({ ...(task.payload || {}), items });
      syncCardSummary();
      paintTaskDetail(detail, task, [], refresh);
      // re-render replaced the input — refocus it so the next item can be
      // typed straight away
      detail.querySelector(".check-new")?.focus();
    };
    // add-one-at-a-time (unchanged): type an item, press Enter
    newItem.addEventListener("keydown", async (e) => {
      if (e.key !== "Enter" || !newItem.value.trim()) return;
      await addItems([newItem.value.trim()]);
    });
    // paste a chunk: a multi-line paste is split into one item per line
    // (single-line pastes fall through to the normal add-one flow)
    newItem.addEventListener("paste", async (e) => {
      const text = (e.clipboardData || window.clipboardData)?.getData("text") || "";
      if (!/[\r\n\u2028\u2029]/.test(text)) return; // one line — let the input handle it
      e.preventDefault();
      const parsed = splitChecklistText(text);
      if (parsed.length) await addItems(parsed);
    });
  }
  if (task.kind === "workout") {
    const cat = detail.querySelector(".ex-cat");
    const nameEl = detail.querySelector(".ex-name");
    const suggest = detail.querySelector("datalist");
    const a = detail.querySelector(".ex-a");
    const b = detail.querySelector(".ex-b");
    // the logger remembers: recent exercises for this category suggest
    // themselves, and picking one prefills last time's numbers
    const paintSuggestions = () => {
      suggest.innerHTML = recentExercises
        .filter((ex) => ex.category === cat.value)
        .map((ex) => `<option value="${esc(ex.name)}">`).join("");
    };
    nameEl.addEventListener("input", () => {
      const hit = recentExercises.find((ex) =>
        ex.category === cat.value &&
        ex.name.toLowerCase() === nameEl.value.trim().toLowerCase());
      if (!hit) return;
      if (cat.value === "strength") {
        if (hit.reps && !a.value) a.value = hit.reps;
        if (hit.weight && !b.value) b.value = hit.weight;
      } else if (cat.value === "cardio") {
        if (hit.minutes && !a.value) a.value = Math.round(hit.minutes);
        if (hit.zone && !b.value) b.value = String(hit.zone).replace(/\D/g, "");
      } else if (hit.seconds && !a.value) {
        a.value = hit.seconds;
      }
    });
    const zoneEl = detail.querySelector(".ex-zone");
    const distEl = detail.querySelector(".ex-dist");
    const setsEl = detail.querySelector(".ex-sets");
    const relabel = () => {
      const mode = cat.value;
      a.placeholder = mode === "strength" ? "reps" : mode === "cardio" ? "min" : "sec";
      b.style.display = mode === "strength" ? "" : "none";
      zoneEl.hidden = mode !== "cardio";
      distEl.hidden = mode !== "cardio";
      setsEl.hidden = mode !== "strength";
    };
    cat.onchange = () => { relabel(); paintSuggestions(); };
    relabel();
    paintSuggestions();
    detail.querySelector(".ex-add").onclick = async () => {
      const name = detail.querySelector(".ex-name").value.trim();
      if (!name) return;
      const ex = { category: cat.value, name };
      let copies = 1;
      if (cat.value === "strength") {
        ex.reps = +a.value || 0;
        ex.weight = +b.value || 0;
        copies = +setsEl.value || 1; // each set is its own line, codi-style
      } else if (cat.value === "cardio") {
        ex.minutes = +a.value || 0;
        if (zoneEl.value) ex.zone = zoneEl.value;
        if (distEl.value) ex.distance = +distEl.value;
      } else {
        ex.seconds = +a.value || 0;
      }
      const exercises = (task.payload?.exercises || [])
        .concat(Array.from({ length: copies }, () => ({ ...ex })));
      await savePayload({ ...(task.payload || {}), exercises });
      paintTaskDetail(detail, task, [], refresh);
    };
    detail.querySelectorAll(".ex-rm").forEach((rm) => {
      rm.onclick = async () => {
        const exercises = (task.payload?.exercises || []).slice();
        exercises.splice(+rm.dataset.i, 1);
        await savePayload({ ...(task.payload || {}), exercises });
        paintTaskDetail(detail, task, [], refresh);
      };
    });
  }
  if (task.kind === "form") {
    detail.querySelectorAll("[data-key]").forEach((field) => {
      field.onchange = async () => {
        const values = { ...(task.payload?.values || {}) };
        values[field.dataset.key] =
          field.type === "checkbox" ? field.checked : field.value;
        await savePayload({ ...(task.payload || {}), values });
      };
    });
  }
  // Complete Task (checklist): finish the whole checklist, but warn first
  // if items are still unchecked — a chance to review before closing it out.
  detail.querySelector(".task-complete-btn")?.addEventListener("click", async () => {
    const items = task.payload?.items || [];
    const left = items.filter((i) => !i.done).length;
    if (left > 0 && !(await confirmDanger({
      title: "Complete this task?",
      body: `${left} of ${items.length} item${items.length === 1 ? "" : "s"} ${
        left === 1 ? "is" : "are"} still unchecked. Complete the task anyway, or review it first?`,
      action: "complete anyway",
    }))) return;
    await apiPost(`/v1/tasks/${task.id}/complete`, { actor: ACTOR });
    toast("the fire is fed");
    refresh();
  });
  detail.querySelector(".task-backlog-btn").onclick = async () => {
    await apiPost(`/v1/tasks/${task.id}`, {
      actor: ACTOR,
      status: task.status === "backlog" ? "open" : "backlog",
    });
    refresh();
  };
  // Archiving is reversible (the archive drawer restores) — no modal.
  detail.querySelector(".task-del").onclick = async () => {
    await apiPost(`/v1/tasks/${task.id}`, { actor: ACTOR, status: "archived" });
    refresh();
  };
}

/* ————— goals ————— */

/* — Projects rail: declared projects, each a container of repos. A
   project is an ENTITY; clicking it opens its card (compose repos,
   tasks/notes mentioning it). Distinct from goals — a container, not
   an aim. — */
async function paintProjects(host) {
  if (!host) return;
  // the person modal (app.js) composes/detaches repos on project cards —
  // let it repaint this rail so the Desk reflects the edit immediately
  window.__refreshDeskProjects = () => {
    if (document.body.contains(host)) paintProjects(host);
  };
  const { projects } = await api("/v1/projects").catch(() => ({ projects: [] }));
  host.innerHTML = `
    <div class="section-title">projects <span class="meta" style="font-weight:400">— containers of work; @-mentionable in tasks &amp; notes</span></div>
    ${(projects || []).map((p) => `
      <div class="card task-card project-card" data-project="${esc(p.name)}" data-project-id="${esc(p.id)}">
        <div class="task-row" style="align-items:baseline">
          <b class="project-open" style="cursor:pointer">◈ ${esc(p.name)}</b>
          <button class="task-edit-btn project-rename" title="rename this project — everything pointing at it follows (by entity id)">✎</button>
          ${p.repos.length ? `<span class="statchip" title="GitHub repos included in this project">${p.repos.length} repo${p.repos.length === 1 ? "" : "s"}</span>` : ""}
          ${p.evidence ? `<span class="meta">${p.evidence} episodes${p.last_activity ? ` · ${esc(p.last_activity)}` : ""}</span>` : ""}
        </div>
        <div class="searchbar project-rename-row" hidden>
          <input class="project-rename-input" value="${esc(p.name)}" style="flex:1">
          <button class="btn project-rename-save">rename</button>
        </div>
        ${p.repos.length ? `<div class="meta">${p.repos.map((r) => esc(r[1])).join(" · ")}</div>` : ""}
      </div>`).join("") || `<p class="meta" style="color:var(--ink-faint)">no projects yet — declare one to include GitHub repos and work under it.</p>`}
    <div class="searchbar" style="margin-top:var(--s3)">
      <input id="project-name" placeholder="declare a project…">
      <button class="btn" id="project-add">create</button>
    </div>`;
  host.querySelectorAll(".project-card").forEach((card) => {
    // open by entity id — the name can case-collide with a same-named
    // repo, which would resolve the modal to the wrong entity. The whole
    // card is the target: repo-bearing cards are mostly metadata, and a
    // click there landing nowhere reads as "the modal doesn't open".
    card.style.cursor = "pointer";
    card.onclick = () => window.__openPersonModal?.(card.dataset.projectId || card.dataset.project);
    // ✎ rename in place (item 12): by ENTITY ID, so tasks/notes/edges/
    // goals follow automatically and a same-named Org/Person is never
    // collapsed in (the server refuses a taken name — merge covers that).
    const row = card.querySelector(".project-rename-row");
    const input = card.querySelector(".project-rename-input");
    card.querySelector(".project-rename").onclick = (e) => {
      e.stopPropagation();
      row.hidden = !row.hidden;
      if (!row.hidden) { input.focus(); input.select(); }
    };
    row.onclick = (e) => e.stopPropagation();
    const save = async () => {
      const name = input.value.trim();
      if (!name || name === card.dataset.project) { row.hidden = true; return; }
      try {
        await apiPost("/v1/entities/rename", { actor: ACTOR, id: card.dataset.projectId, name });
        paintProjects(host);
      } catch (err) {
        input.value = card.dataset.project;
        row.hidden = true;
        toast(`✗ ${err.message}`);
      }
    };
    card.querySelector(".project-rename-save").onclick = (e) => { e.stopPropagation(); save(); };
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") save();
      if (e.key === "Escape") { row.hidden = true; input.value = card.dataset.project; }
    });
  });
  const add = async () => {
    const name = host.querySelector("#project-name").value.trim();
    if (!name) return;
    const res = await apiPost("/v1/projects", { actor: ACTOR, name }).catch(() => null);
    window.__openPersonModal?.(res?.id || name); // straight into composing repos
    paintProjects(host);
  };
  host.querySelector("#project-add").onclick = add;
  host.querySelector("#project-name").addEventListener("keydown", (e) => {
    if (e.key === "Enter") add();
  });
}

async function paintGoals(host) {
  const data = await api("/v1/intents");
  const goals = (data.intents || []);
  const active = goals.filter((g) => g.status === "active");
  const rest = goals.filter((g) => g.status !== "active");
  host.innerHTML = `
    <div class="section-title">goals <span class="meta" style="font-weight:400">— standing watches over everything that flows in</span></div>
    ${active.map((g) => `
      <div class="card goal-card" data-goal="${esc(g.name)}">
        <div class="goal-title"><b>${esc(g.name)}</b>
          ${g.matches ? `<a class="statchip goal-matches"
             href="#/search/${encodeURIComponent(g.query)}"
             title="episodes this goal has caught">${g.matches} caught${g.unseen_matches ? ` · ${g.unseen_matches} new` : ""}</a>` : ""}
        </div>
        <div class="goal-chips">
          ${g.sphere ? `<span class="typebadge">${esc(g.sphere)}</span>` : ""}
          ${(g.projects || []).map((p) => `<span class="task-tag project-chip" data-project="${esc(p)}"
            title="project this goal advances — click to open, × to unlink">◈ <span class="project-open" style="cursor:pointer">${esc(p)}</span>
            <button class="goal-proj-rm" data-project="${esc(p)}" title="unlink">×</button></span>`).join("")}
          <button class="btn btn-quiet goal-link-project" title="link a project this goal advances">+ project</button>
        </div>
        <div class="goal-note">
          ${g.primary_note
            ? `<button class="goal-note-open" data-note="${esc(g.primary_note[0])}" title="open the goal's description note">✎ ${esc(g.primary_note[1])}</button>`
            : `<button class="btn btn-quiet goal-note-new" title="write this goal's description as a note">+ goal note</button>`}
        </div>
        <div class="goal-queries">
          ${g.sub_queries.map((q) => `<span class="goal-q">${esc(q)}
            <button class="goal-q-rm" data-q="${esc(q)}" title="stop watching this term">×</button></span>`).join("")}
          ${g.sub_queries.length ? "" : `<span class="meta" style="color:var(--ink-faint)">watch terms are being drawn up — or add your own →</span>`}
          <span class="goal-q-add"><input placeholder="+ watch term (e.g. fusion energy)"
            title="terms are matched against everything that flows in — and the whole record, immediately"></span>
        </div>
        <div class="tend-actions">
          <button class="btn btn-quiet goal-edit" title="edit this goal">edit ✎</button>
          <button class="btn btn-quiet" data-goal-status="achieved" title="mark this goal achieved">mark achieved ✓</button>
          <button class="btn btn-quiet" data-goal-status="abandoned" title="stop pursuing this goal">abandon</button>
        </div>
        <div class="goal-edit-strip" hidden>
          <input class="ge-name" value="${esc(g.name)}" placeholder="name">
          <input class="ge-query" value="${esc(g.query)}" placeholder="what are you after?"
            title="changing the statement redraws the watch terms">
          <button class="btn ge-save">save</button>
        </div>
      </div>`).join("")}
    ${rest.length ? `<details class="tend-confirmed"><summary class="meta">${rest.length} resolved</summary>
      ${rest.map((g) => `<div class="meta" style="padding:2px 0">${esc(g.name)} — ${esc(g.status)}</div>`).join("")}
    </details>` : ""}
    <div class="searchbar" style="margin-top:var(--s3); flex-wrap:wrap">
      <input id="goal-name" placeholder="name" style="flex:0.5">
      <input id="goal-query" placeholder="what are you after? (plain words)">
      <select id="goal-sphere">${["work", "personal", "family", "health"]
        .map((s) => `<option>${s}</option>`).join("")}</select>
      <button class="btn" id="goal-add">watch</button>
    </div>`;
  host.querySelectorAll("[data-goal-status]").forEach((button) => {
    button.onclick = async () => {
      const name = button.closest(".goal-card").dataset.goal;
      await apiPost(`/v1/intents/${encodeURIComponent(name)}`, {
        actor: ACTOR,
        status: button.dataset.goalStatus,
      });
      paintGoals(host);
    };
  });
  // project chip → open the project card; × unlinks; "+ project" adds
  host.querySelectorAll(".goal-card .project-chip .project-open").forEach((el) => {
    el.onclick = () => window.__openPersonModal?.(el.closest(".project-chip").dataset.project);
  });
  host.querySelectorAll(".goal-proj-rm").forEach((rm) => {
    rm.onclick = async () => {
      const goal = rm.closest(".goal-card").dataset.goal;
      await apiPost("/v1/goals/project", { actor: ACTOR, goal, project: rm.dataset.project, link: false });
      paintGoals(host);
    };
  });
  // the goal's 1:1 description NOTE
  host.querySelectorAll(".goal-note-open").forEach((btn) => {
    btn.onclick = () => window.__openNoteById?.(btn.dataset.note);
  });
  host.querySelectorAll(".goal-note-new").forEach((btn) => {
    btn.onclick = async () => {
      const goal = btn.closest(".goal-card").dataset.goal;
      const r = await apiPost("/v1/notes", {
        actor: ACTOR, title: `${goal} — goal notes`,
        body: `# ${goal}\n\n`, scope: "work", primary_for: goal,
      });
      window.__openNoteById?.(r.id);
      setTimeout(() => paintGoals(host), 400);
    };
  });
  host.querySelectorAll(".goal-link-project").forEach((btn) => {
    btn.onclick = async () => {
      const goal = btn.closest(".goal-card").dataset.goal;
      // inline picker — no browser prompt (styled inputs only)
      const wrap = document.createElement("span");
      wrap.style.position = "relative";
      wrap.innerHTML = `<input class="rel-ctx" placeholder="project… (Enter to create+link)">
        <div class="ne-mention-pop ne-pop-under" hidden></div>`;
      btn.replaceWith(wrap);
      const input = wrap.querySelector("input");
      const pop = wrap.querySelector(".ne-mention-pop");
      input.focus();
      const link = async (name) => {
        const n = name.trim();
        if (!n) return;
        const { projects } = await api("/v1/projects").catch(() => ({ projects: [] }));
        if (!(projects || []).some((p) => p.name.toLowerCase() === n.toLowerCase()))
          await apiPost("/v1/projects", { actor: ACTOR, name: n });
        await apiPost("/v1/goals/project", { actor: ACTOR, goal, project: n });
        paintGoals(host);
      };
      input.addEventListener("input", async () => {
        const q = input.value.trim();
        if (q.length < 1) { pop.hidden = true; return; }
        const { projects } = await api("/v1/projects").catch(() => ({ projects: [] }));
        const hits = (projects || []).filter((p) => p.name.toLowerCase().includes(q.toLowerCase())).slice(0, 6);
        if (!hits.length) { pop.hidden = true; return; }
        pop.innerHTML = hits.map((p) => `<button class="ne-mention-opt" data-name="${esc(p.name)}">◈ ${esc(p.name)}</button>`).join("");
        pop.hidden = false;
        pop.querySelectorAll(".ne-mention-opt").forEach((o) => o.onclick = () => link(o.dataset.name));
      });
      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter") link(input.value);
        if (e.key === "Escape") paintGoals(host);
      });
    };
  });
  // goal editing: name, statement (redraws the watch), description
  host.querySelectorAll(".goal-edit").forEach((button) => {
    button.onclick = () => {
      const strip = button.closest(".goal-card").querySelector(".goal-edit-strip");
      strip.hidden = !strip.hidden;
    };
  });
  host.querySelectorAll(".ge-save").forEach((button) => {
    button.onclick = async () => {
      const cardEl = button.closest(".goal-card");
      const name = cardEl.dataset.goal;
      const body = {
        actor: ACTOR,
        query: cardEl.querySelector(".ge-query").value.trim() || undefined,
      };
      const newName = cardEl.querySelector(".ge-name").value.trim();
      if (newName && newName !== name) body.new_name = newName;
      const result = await apiPost(`/v1/intents/${encodeURIComponent(name)}`, body);
      await paintGoals(host);
      if (result.restated) setTimeout(() => paintGoals(host), 8000);
    };
  });

  // watch-term editing: your direction outranks the LLM's suggestions
  host.querySelectorAll(".goal-q-rm").forEach((rm) => {
    rm.onclick = async () => {
      const name = rm.closest(".goal-card").dataset.goal;
      await apiPost(`/v1/intents/${encodeURIComponent(name)}/queries`, {
        actor: ACTOR, remove: [rm.dataset.q],
      });
      paintGoals(host);
    };
  });
  host.querySelectorAll(".goal-q-add input").forEach((input) => {
    input.addEventListener("keydown", async (e) => {
      if (e.key !== "Enter" || !input.value.trim()) return;
      const name = input.closest(".goal-card").dataset.goal;
      await apiPost(`/v1/intents/${encodeURIComponent(name)}/queries`, {
        actor: ACTOR, add: [input.value.trim()],
      });
      // the backlog scan runs in the background — give it a beat
      setTimeout(() => paintGoals(host), 2500);
      paintGoals(host);
    });
  });
  host.querySelector("#goal-add").onclick = async () => {
    const name = host.querySelector("#goal-name").value.trim();
    const query = host.querySelector("#goal-query").value.trim();
    if (!name || !query) return;
    await apiPost("/v1/intents", {
      actor: ACTOR,
      name,
      query,
      sphere: host.querySelector("#goal-sphere").value,
    });
    await paintGoals(host);
    // setup (LLM decomposition + backlog scan) lands within seconds;
    // repaint to show the drawn-up terms and what history already matched
    setTimeout(() => paintGoals(host), 8000);
  };
  return goals;
}

/* ————— mount ————— */

let recentExercises = [];

/* "from your plugins": each enabled plugin's declared desk widget as a
   small sandboxed iframe (same isolation as plugin pages — plugin JS
   never runs in this origin). Invisible when nothing declares one. */
async function paintPluginWidgets(host) {
  if (!host) return;
  const widgets = window.__pluginUi?.desk || [];
  if (!widgets.length) { host.remove(); return; }
  host.className = "desk-panel";
  host.innerHTML = `<div class="section-title">from your embers</div>`;
  for (const w of widgets) {
    const wrap = document.createElement("div");
    wrap.style.marginBottom = "var(--s2)";
    wrap.innerHTML = `
      <div class="meta" style="margin-bottom:4px">${(w.label || w.plugin)
        .replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]))}</div>
      <iframe sandbox="allow-scripts" src="/plugins/${encodeURIComponent(w.plugin)}/ui/${w.page}"
        style="width:100%;height:${Math.min(Math.max(w.height || 160, 80), 480)}px;border:0;border-radius:8px;background:transparent"></iframe>`;
    const frame = wrap.querySelector("iframe");
    frame.addEventListener("load", async () => {
      let token = null;
      try {
        token = (await apiPost(`/v1/plugins/${encodeURIComponent(w.plugin)}/ui-token`, {})).token;
      } catch { /* older daemon or disabled — widget renders tokenless */ }
      const palette = {};
      const styles = getComputedStyle(document.documentElement);
      for (const v of ["--bg", "--ink", "--ink-faint", "--accent", "--card", "--line", "--chart-1", "--chart-2", "--chart-3", "--chart-4", "--chart-5", "--chart-6", "--chart-7", "--chart-8", "--chart-heat"]) {
        palette[v] = styles.getPropertyValue(v).trim();
      }
      frame.contentWindow.postMessage(
        { type: "hearthloom", token, base: location.origin, palette }, "*");
    });
    host.append(wrap);
  }
}

/* ————— shared with you (v46): what connected brains pin to this desk.
   Live-queried from each friend's node (nothing is copied — un-sharing
   on their side makes it vanish here); collaborative checklists tick
   straight back into THEIR record; fork = the one explicit copy. ————— */
async function paintSharedRail(host) {
  if (!host) return;
  const data = await api("/v1/friends/shared").catch(() => ({ shared: [] }));
  const groups = (data.shared || []).filter((g) =>
    (g.tasks?.length || 0) + (g.notes?.length || 0) + (g.relationships?.length || 0) > 0);
  if (!groups.length) { host.hidden = true; return; }
  host.hidden = false;
  host.innerHTML = `
    <div class="section-title">shared with you</div>
    ${groups.map((g) => `
      <div class="shared-group" data-friend="${esc(g.friend)}">
        <div class="meta" style="margin-bottom:var(--s1)">
          ${window.friendAvatar?.(g.friend, {
            entity: g.entity, emoji: g.profile?.emoji, color: g.profile?.color,
            avatar: g.profile?.avatar, displayName: g.profile?.name || g.friend,
            title: `${g.profile?.name || g.friend} — shares these with you · click for their card`,
          }) || ""} <b>from ${esc(g.profile?.name || g.friend)}'s hearth</b></div>
        ${(g.tasks || []).map((t) => `
          <div class="shared-task${t.status === "done" ? " pm-task-done" : ""}" data-task="${esc(t.id)}">
            <div>${esc(t.description)}
              ${t.remind_at ? `<span class="meta"> · due ${esc(t.remind_at.slice(0, 10))}</span>` : ""}
              ${t.status === "done" ? `<span class="kind">done</span>` : ""}
              <button class="linkish shared-fork" data-kind="task" data-id="${esc(t.id)}"
                title="copy to MY desk (yours from then on)">⧉</button></div>
            ${(t.payload?.items || []).map((it, i) => `
              <label class="check-item${it.done ? " done" : ""}" style="cursor:${t.collaborate ? "pointer" : "default"}">
                <input type="checkbox" class="shared-check" data-task="${esc(t.id)}" data-i="${i}"
                  data-text="${esc(it.text)}" ${it.done ? "checked" : ""} ${t.collaborate ? "" : "disabled"}>
                <span>${esc(it.text)}</span></label>`).join("")}
          </div>`).join("")}
        ${(g.notes || []).map((n) => `
          <div class="meta shared-note-row">✎ ${esc(n.title)}
            <button class="linkish shared-fork" data-kind="note" data-id="${esc(n.id)}"
              title="copy into MY notes">⧉</button></div>`).join("")}
        ${(g.relationships || []).map((r) => `
          <div class="meta">◉ ${esc(r.name)} — ${esc(r.kind || "")}${r.note ? ` · ${esc(r.note)}` : ""}</div>`).join("")}
      </div>`).join("")}`;
  host.querySelectorAll(".shared-check").forEach((box) => {
    const friend = box.closest(".shared-group").dataset.friend;
    box.onchange = async () => {
      const res = await apiPost(
        `/v1/friends/${encodeURIComponent(friend)}/tasks/${encodeURIComponent(box.dataset.task)}/toggle`,
        { index: +box.dataset.i, text: box.dataset.text, done: box.checked },
      ).catch((e) => ({ error: e.message }));
      if (res?.error) { box.checked = !box.checked; return; }
      box.closest(".check-item")?.classList.toggle("done", box.checked);
    };
  });
  host.querySelectorAll(".shared-fork").forEach((btn) => {
    const friend = btn.closest(".shared-group").dataset.friend;
    btn.onclick = async () => {
      btn.disabled = true;
      await apiPost(`/v1/friends/${encodeURIComponent(friend)}/fork`, {
        actor: ACTOR, kind: btn.dataset.kind, id: btn.dataset.id,
      }).catch(() => {});
      location.reload();
    };
  });
}

export async function mount(container, helpers) {
  ({ api, apiPost, confirmDanger, md: mdRender } = helpers);
  container.innerHTML = `
    <span class="eyebrow">desk</span>
    <h1 class="pagehead">The desk</h1>
    <p class="subhead">The present tense — whatever you finish here becomes a memory;
      the record writes itself.</p>
    <div class="desk-grid">
      <div class="desk-main">
        <div class="section-title">To-do list
          <span class="meta" style="font-weight:300; font-size:0.8em; opacity:0.85">— <b>#tags</b> (#urgent · #daily · #today · #template-name) and <b>@mentions</b> both work here <i>and</i> in notes</span></div>
        <div class="searchbar" style="flex-wrap:wrap">
          <input id="task-new-desc" placeholder="something to do…" style="flex:2; min-width:200px">
          <select id="task-new-kind">
            <option value="plain">task</option>
            <option value="checklist">checklist</option>
            <option value="workout">workout</option>
            <option value="form">form</option>
          </select>
          <select id="task-new-freq">
            <option value="one-time">once</option>
            <option value="daily">daily</option>
            <option value="weekly">weekly</option>
            <option value="monthly">monthly</option>
          </select>
          <select id="task-new-template" hidden></select>
          <select id="task-new-goal"><option value="">no goal</option></select>
          <input id="task-new-due" type="date" title="due date (optional; #today / #tomorrow also work)">
          <button class="btn" id="task-new-add">add</button>
        </div>
        <div id="desk-tasks"><div class="loading">clearing the desk…</div></div>
      </div>
      <div class="desk-rail">
        <div class="desk-panel" id="desk-shared" hidden></div>
        <div class="desk-panel" id="desk-projects"><div class="loading">…</div></div>
        <div class="desk-panel" id="desk-goals"><div class="loading">…</div></div>
        <div class="desk-panel">
          <div class="section-title">recent notes
            <a class="meta desk-panel-link" href="#/notes">all notes →</a></div>
          <div id="desk-notes-list"><div class="loading">…</div></div>
          <div class="tend-actions" style="margin-top:var(--s2)">
            <button class="btn btn-quiet" id="desk-note-new">+ note</button>
          </div>
        </div>
        <div id="desk-plugins"></div>
      </div>
    </div>`;
  paintPluginWidgets(container.querySelector("#desk-plugins"));
  paintSharedRail(container.querySelector("#desk-shared"));

  const [templatesData, goals, exercisesData] = await Promise.all([
    api("/v1/templates"),
    paintGoals(container.querySelector("#desk-goals")),
    api("/v1/exercises/recent").catch(() => ({ exercises: [] })),
  ]);
  paintProjects(container.querySelector("#desk-projects"));
  const templates = templatesData.templates || [];
  recentExercises = exercisesData.exercises || [];

  // recent notes: five, the rest live in the Notes view
  const notesListEl = container.querySelector("#desk-notes-list");
  let refreshNotes = () => {};
  const openNoteEditor = buildNoteEditor(container, () => refreshNotes(), confirmDanger);
  refreshNotes = () => paintNoteList(notesListEl, openNoteEditor, { limit: 5 });
  await refreshNotes();
  if (!container.querySelector("#desk-note-new")) return () => {}; // same race, earlier window
  container.querySelector("#desk-note-new").onclick = () => openNoteEditor(null, null);

  const tasksHost = container.querySelector("#desk-tasks");
  await paintTasks(tasksHost, templates, goals, openNoteEditor);

  // The first-boot #/welcome redirect can replace the view while the
  // awaits above are in flight — the quick-add hosts are gone; stop
  // cleanly instead of throwing on a null querySelector.
  if (!container.querySelector("#task-new-kind")) return () => {};

  // Quick-add wiring: template picker appears for form tasks; goal
  // picker carries the active goals.
  const kindSel = container.querySelector("#task-new-kind");
  const templateSel = container.querySelector("#task-new-template");
  templateSel.innerHTML = templates.length
    ? templates.map((t) => `<option value="${esc(t.id)}">${esc(t.name)}</option>`).join("")
    : `<option value="">no templates — make one in Notes</option>`;
  kindSel.onchange = () => { templateSel.hidden = kindSel.value !== "form"; };
  const goalSel = container.querySelector("#task-new-goal");
  goalSel.innerHTML = `<option value="">no goal</option>` +
    goals.filter((g) => g.status === "active")
      .map((g) => `<option value="${esc(g.id)}">⭑ ${esc(g.name)}</option>`).join("");

  const add = async () => {
    const description = container.querySelector("#task-new-desc").value.trim();
    if (!description) return;
    const kind = kindSel.value;
    const body = {
      actor: ACTOR,
      kind,
      description,
      frequency: container.querySelector("#task-new-freq").value,
    };
    if (kind === "form" && templateSel.value) body.template_id = templateSel.value;
    if (kind === "checklist") body.payload = { items: [] };
    if (kind === "workout") body.payload = { exercises: [] };
    if (goalSel.value) body.goal_id = goalSel.value;
    const due = container.querySelector("#task-new-due").value;
    if (due) body.remind_at = `${due}T12:00:00Z`;
    const created = await apiPost("/v1/tasks", body);
    container.querySelector("#task-new-desc").value = "";
    container.querySelector("#task-new-due").value = "";
    paintTasks(tasksHost, templates, goals, openNoteEditor, { flashId: created?.id });
  };
  container.querySelector("#task-new-add").onclick = add;
  // @ in a task works like @ in a note: pick from the list to link a
  // person ("Send report to @[Sam]") — they'll see it on their dossier.
  attachMentionTypeahead(container.querySelector("#task-new-desc"));
  container.querySelector("#task-new-desc").addEventListener("keydown", (e) => {
    if (e.key === "Enter") add();
  });

  // Ask handoff: an action suggestion prefills — the human saves.
  const prefillRaw = sessionStorage.getItem("hl-desk-prefill");
  if (prefillRaw) {
    sessionStorage.removeItem("hl-desk-prefill");
    try {
      const prefill = JSON.parse(prefillRaw);
      if (prefill.type === "goal") {
        const goalsEl = container.querySelector("#desk-goals");
        goalsEl.scrollIntoView({ block: "center" });
        goalsEl.querySelector("#goal-name").value = prefill.name || "";
        goalsEl.querySelector("#goal-query").value = prefill.query || "";
        if (prefill.sphere) goalsEl.querySelector("#goal-sphere").value = prefill.sphere;
        goalsEl.querySelector("#goal-name").focus();
        toast("review the goal, then press watch — nothing saved yet");
      } else {
        const descEl = container.querySelector("#task-new-desc");
        descEl.value = prefill.description || "";
        if (prefill.kind) kindSel.value = prefill.kind;
        if (prefill.due) container.querySelector("#task-new-due").value = prefill.due.slice(0, 10);
        descEl.focus();
        toast("review the task, then press add — nothing saved yet");
      }
    } catch { /* stale prefill */ }
  }

  void mdRender;
  return () => {};
}
