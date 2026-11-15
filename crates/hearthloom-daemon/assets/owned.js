// owned.js — "this source is mine" (v68, core store/owned.rs). The
// owner's list of repositories and publications whose text counts as
// their own words, and the inline "This is mine" control shown where a
// repository (a Project, a commit) or a publication's post is shown.
// Marking needs the vault unlocked (the server refuses otherwise and says
// so); every change asks first through the styled confirm dialog.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&#34;", "'": "&#39;" }[c]));

const WHAT = {
  git: { one: "repository", items: "these commits" },
  substack: { one: "publication", items: "these posts" },
};

// Where the owner's addresses come from — and who sees them: the profile
// card is shown to friends you share your profile with.
const ADDRESSES_FROM = "a connected mailbox and its aliases (Integrations), or the addresses on your profile card in Settings — friends you share your profile with see the card's addresses";

function effect(source, key, identities) {
  if (source === "git") {
    const who = identities.length
      ? `one of your own addresses (${identities.join(", ")})`
      : `one of your own addresses (none is known yet — they come from ${ADDRESSES_FROM}; until then no commit counts)`;
    return `Commits in ${key} by ${who} will count as your own words: Ask may message friends and agents may act under your normal rules after reading these commits. Not counted even then: merges, commits co-written with anyone else (a Co-authored-by line, which Claude Code adds), commits with one of your agents at work anywhere between when they were first written and their last rewrite, and commits an earlier version of Hearthloom imported (their records can't show those things). Git doesn't check author addresses: anyone whose commit gets into this repository can put yours on it, so mark only a repository whose history you control. You can take it back any time.`;
  }
  return `Every post published under ${key}, guest posts included, will count as your own words: Ask may message friends and agents may act under your normal rules after reading these posts. Posts an earlier version of Hearthloom imported stay outside text: it didn't check which host served them. Mark it only if no one else publishes there. You can take it back any time.`;
}

function storedNote(n) {
  if (!n) return "nothing stored under it yet, so it changes nothing for now (a publication with its own domain files its posts under that domain)";
  return n === 1 ? "1 memory stored under it" : `${n} memories stored under it`;
}

function historyLine(h) {
  if (h.op === "addresses") {
    const parts = [
      ...(h.added || []).map((a) => `+${a}`),
      ...(h.removed || []).map((a) => `−${a}`),
    ];
    return `your addresses (${esc(h.place || "")}): ${parts.map(esc).join(", ")}`;
  }
  return `${h.op === "delete" ? "taken off" : "marked"}: ${esc(h.source || "")} ${esc(h.key || "")}`;
}

async function identitiesOf(helpers) {
  const list = await helpers.api("/v1/owned-sources").catch(() => null);
  return list?.identities || [];
}

/** Ask, then mark. Resolves to the entry, or null (cancelled / refused —
 *  the reason lands in `note`). */
async function markMine(source, address, key, helpers, note) {
  const ok = await helpers.confirmDanger({
    title: `Is this ${WHAT[source]?.one || "source"} yours?`,
    body: effect(source, key, await identitiesOf(helpers)),
    action: "this is mine",
  });
  if (!ok) return null;
  try {
    const r = await helpers.apiPost("/v1/owned-sources", { source, address });
    return r.entry ? { ...r.entry, stored: r.stored } : null;
  } catch (err) {
    if (note) { note.hidden = false; note.textContent = `✗ ${err.message}`; }
    return null;
  }
}

/** Ask, then take an entry off the list. */
async function unmarkMine(entry, helpers, note) {
  const ok = await helpers.confirmDanger({
    title: `Not yours after all?`,
    body: `Text from ${entry.key} goes back to being outside text: reading it closes Ask's way to your friends and makes agents ask you before they act.`,
    action: "not mine",
  });
  if (!ok) return false;
  try {
    await helpers.apiDelete(`/v1/owned-sources/${encodeURIComponent(entry.id)}`);
    return true;
  } catch (err) {
    if (note) { note.hidden = false; note.textContent = `✗ ${err.message}`; }
    return false;
  }
}

/* ————— the inline control ————— */

function paintControl(host, { source, key, entry, counts, why }, onMark, onUnmark) {
  const one = WHAT[source]?.one || "source";
  host.innerHTML = entry
    ? `<span class="task-tag plain-tag" title="marked as yours ${esc((entry.created_at || "").slice(0, 10))}">✓ your ${esc(one)} · ${esc(key)}</span>
       ${counts === false && why ? `<span class="meta"> — this one stays outside text: ${esc(why)}</span>` : ""}
       <button class="btn btn-quiet tiny owned-unmark" title="it goes back to being outside text">not mine</button>
       <p class="meta owned-note" hidden></p>`
    : `<button class="btn tiny owned-mark" title="say this ${esc(one)} is yours — asks first">This is mine</button>
       <span class="meta"> ${esc(one)} ${esc(key)} — outside text until you say it's yours</span>
       <p class="meta owned-note" hidden></p>`;
  const note = host.querySelector(".owned-note");
  host.querySelector(".owned-mark")?.addEventListener("click", () => onMark(note));
  host.querySelector(".owned-unmark")?.addEventListener("click", () => onUnmark(note));
}

/** The control for an address the page shows (a Project's `repo-url`). */
export async function mountForAddress(host, source, address, helpers) {
  const check = await helpers
    .api(`/v1/owned-sources/check?source=${encodeURIComponent(source)}&address=${encodeURIComponent(address)}`)
    .catch(() => null);
  if (!check) { host.innerHTML = ""; return; } // no remote address: nothing to mark
  paintControl(host, { source, key: check.key, entry: check.entry },
    async (note) => {
      if (await markMine(source, address, check.key, helpers, note)) mountForAddress(host, source, address, helpers);
    },
    async (note) => {
      if (await unmarkMine(check.entry, helpers, note)) mountForAddress(host, source, address, helpers);
    });
}

/** The control in an opened memory (`ownable` from /v1/episode/{id}). */
export function mountForEpisode(host, ownable, helpers, refresh) {
  const j = ownable?.judgement;
  if (!j?.key) {
    host.innerHTML = j?.why ? `<span class="meta">${esc(j.why)}</span>` : "";
    return;
  }
  const entry = j.entry ? { id: j.entry, key: j.key } : null;
  paintControl(host, { source: j.source, key: j.key, entry, counts: j.counts, why: j.why },
    async (note) => {
      if (await markMine(j.source, ownable.address, j.key, helpers, note)) refresh();
    },
    async (note) => {
      if (await unmarkMine(entry, helpers, note)) refresh();
    });
}

/* ————— the list (Settings) ————— */

export async function mountList(host, helpers) {
  const paint = async () => {
    const data = await helpers.api("/v1/owned-sources").catch((err) => ({ error: err.message }));
    if (data.error) { host.innerHTML = `<p class="meta">${esc(data.error)}</p>`; return; }
    const sources = data.sources || [];
    const ids = data.identities || [];
    host.innerHTML = `
      <p class="meta">Text from a repository's history or a publication counts as
        <b>outside text</b> — others can write it — so reading it closes Ask's way
        to your friends and makes agents ask you before they act. Mark the ones
        that are yours: Ask may then message friends and agents may act under your
        normal rules after reading them. Adding needs the vault unlocked.</p>
      <div id="owned-entries">
        ${(data.entries || []).map((e) => `
          <div class="task-row" style="align-items:baseline; gap:var(--s2)">
            <span class="task-tag plain-tag">${esc(e.source)}</span>
            <b>${esc(e.key)}</b>
            <span class="meta">since ${esc((e.created_at || "").slice(0, 10))} · ${esc(e.set_by)} · ${esc(storedNote((data.stored || {})[e.id]))}</span>
            <button class="btn btn-quiet tiny owned-rm" data-id="${esc(e.id)}">not mine</button>
          </div>`).join("") || `<p class="meta" style="color:var(--ink-faint)">nothing marked as yours yet.</p>`}
      </div>
      <p class="meta">${ids.length
        ? `A repository's commits count when their author is one of your addresses: ${ids.map(esc).join(", ")} — not merges, commits co-written with anyone else, commits with one of your agents at work anywhere between when they were first written and their last rewrite, or commits (and posts) an earlier version of Hearthloom imported. Git doesn't check author addresses, so mark only repositories whose history you control.`
        : `<b>None of your addresses is known yet</b>, so no commit counts as yours.`}
        Your addresses come from ${esc(ADDRESSES_FROM)}. While a repository is marked, adding an address needs the vault unlocked.</p>
      <div class="searchbar" style="flex-wrap:wrap">
        <select id="owned-source">${sources.map((s) =>
          `<option value="${esc(s.source)}">${esc(s.source)} — ${esc(s.names)}</option>`).join("")}</select>
        <input id="owned-address" placeholder="address" style="flex:1">
        <button class="btn" id="owned-add">this is mine</button>
      </div>
      <p class="meta" id="owned-hint"></p>
      <p class="meta" id="owned-note" hidden></p>
      <details><summary class="meta">sources that can't be marked, and why</summary>
        <ul>${(data.not_ownable || []).map((n) =>
          `<li class="meta"><b>${esc(n.source)}</b>: ${esc(n.why)}</li>`).join("")}</ul></details>
      ${(data.history || []).length ? `<details><summary class="meta">recent changes</summary>
        <ul>${data.history.map((h) => `<li class="meta">${esc((h.ts || "").slice(0, 16).replace("T", " "))}
          ${historyLine(h)}
          <span style="color:var(--ink-faint)">(${esc(h.actor)})</span></li>`).join("")}</ul></details>` : ""}`;
    const sel = host.querySelector("#owned-source");
    const hint = host.querySelector("#owned-hint");
    const showHint = () => {
      const s = sources.find((x) => x.source === sel.value);
      hint.textContent = s ? `give ${s.given_as}; counts ${s.counts}` : "";
    };
    sel.onchange = showHint;
    showHint();
    const note = host.querySelector("#owned-note");
    host.querySelector("#owned-add").onclick = async () => {
      const address = host.querySelector("#owned-address").value.trim();
      if (!address) return;
      const check = await helpers
        .api(`/v1/owned-sources/check?source=${encodeURIComponent(sel.value)}&address=${encodeURIComponent(address)}`)
        .catch((err) => ({ error: err.message }));
      if (check.error) { note.hidden = false; note.textContent = `✗ ${check.error}`; return; }
      if (check.entry) { note.hidden = false; note.textContent = `${check.key} is already yours`; return; }
      const entry = await markMine(sel.value, address, check.key, helpers, note);
      if (entry) {
        await paint();
        const stored = host.querySelector("#owned-note");
        if (stored && !(entry.stored > 0)) {
          stored.hidden = false;
          stored.textContent = `${entry.key}: ${storedNote(0)}`;
        }
      }
    };
    host.querySelectorAll(".owned-rm").forEach((btn) => {
      btn.onclick = async () => {
        const entry = (data.entries || []).find((e) => e.id === btn.dataset.id);
        if (entry && await unmarkMine(entry, helpers, note)) paint();
      };
    });
  };
  await paint();
}
