// integrations.js — the connections drawer. HearthFS (the optional
// semantic-filesystem sidecar) and the imports directory: every way data
// gets into the record, each with (a) a direct web upload into the inbox
// drop folder, (b) local file/CLI instructions, (c) where to obtain the
// export. This page is the future home of federations, MCP clients, and
// API-based progressive upkeep — add them as cards here.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&#39;", "'": "&#39;" }[c]));

let api, apiPost, confirmDanger;
const ACTOR = "human:dashboard";
// set by mount(); lets deep handlers trigger a clean repaint (e.g. when
// a detached import finishes)
let REMOUNT = () => {};
const DETACHED_ADAPTERS = ["mbox", "openai", "facebook", "markdown", "notion", "linkedin", "apple-health"];

/* ————— agent access: who can read this record, and what they did —
   pure aggregation over records that already exist (keys, friend-key
   reads, quarantined proposals, Ask runs); docs/agent-surface.md is the
   contract this card makes visible. ————— */

async function paintAgentAccess(host) {
  const [keys, accesses, quarantine] = await Promise.all([
    api("/v1/keys").catch(() => null),
    api("/v1/network/accesses").catch(() => null),
    api("/v1/quarantine").catch(() => null),
  ]);
  const active = (keys?.keys || []).filter((k) => !k.revoked_at);
  const fmt = (ts) => ts ? esc(String(ts).slice(0, 16).replace("T", " ")) : "never";
  const keyRows = active.slice(0, 6).map((k) => `
    <div class="task-row meta" style="gap:var(--s2)">
      <b>${esc(k.name)}</b>
      <span class="statchip">${esc((Array.isArray(k.scopes) ? k.scopes : String(k.scopes || "").split(",")).join(" · "))}</span>
      <span>last used ${fmt(k.last_used_at)}</span>
    </div>`).join("");
  const pendingProposals = (quarantine?.pending || []).length;
  const recentReads = (accesses?.accesses || []).length;
  host.innerHTML = `
    <div class="card">
      <div class="task-row" style="align-items:baseline">
        <b>Agent access</b>
        <span class="meta">agents read; humans write — enforcement lives at read time, not in this card</span>
      </div>
      ${active.length
        ? keyRows + (active.length > 6 ? `<div class="meta">…and ${active.length - 6} more</div>` : "")
        : `<div class="meta">no active API keys — remote agents can't reach this record at all</div>`}
      <div class="meta" style="margin-top:var(--s2)">
        ${pendingProposals
          ? `<a href="#/tend"><b>${pendingProposals}</b> agent-proposed memor${pendingProposals === 1 ? "y" : "ies"} awaiting your review</a> · `
          : "no agent proposals awaiting review · "}
        ${recentReads
          ? `${recentReads} recent friend-key read${recentReads === 1 ? "" : "s"} (<a href="#/network">network</a>) · `
          : ""}
        every Ask run is traced in its history, failures included
      </div>
      <p class="meta" style="margin-top:var(--s2)">The contract: four read-only MCP tools;
        the only agent write is a <i>proposal</i> into quarantine; the vault and scope guard
        filter every read regardless of key scopes. Full text: <code>docs/agent-surface.md</code>.
        Honest gap: loopback MCP reads aren't per-call logged — set
        <code>HEARTHLOOM_REQUIRE_KEYS=1</code> and per-agent keys if that matters to you.</p>
    </div>`;
}

/* ————— the AI provider (multi-provider, tiered) ————— */

const KEY_HINT = {
  anthropic: "sk-ant-…", openai: "sk-…", gemini: "AIza…", grok: "xai-…",
  custom: "optional",
};
// Models offered in each tier's dropdown (you can still type any id).
// The seed defaults stay where they are — these are choices, not changes.
const MODEL_CHOICES = {
  anthropic: ["claude-opus-4-8", "claude-opus-5-5", "claude-fable-5-1", "claude-sonnet-4-6",
    "claude-sonnet-5-5", "claude-haiku-4-5"],
  openai: ["gpt-5", "gpt-5-mini"],
  gemini: ["gemini-2.5-pro", "gemini-2.5-flash"],
  grok: ["grok-4", "grok-4-fast"],
};
// Effort only exists on the always-thinking generation.
const takesEffort = (model) => /opus-5|sonnet-5|fable|mythos/i.test(model || "");
const EFFORTS = ["", "low", "medium", "high", "xhigh", "max"];
const escAttr = (v) => String(v ?? "").replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");

/// The wizard (#/welcome) reuses paintAnthropic/paintGmail/paintPair as
/// its own steps — call initHelpers first when mounting them without
/// mount() (which sets the module-level api/apiPost itself).
export function initHelpers(helpers) {
  api = helpers.api;
  apiPost = helpers.apiPost;
  if (helpers.confirmDanger) confirmDanger = helpers.confirmDanger;
}

export async function paintAnthropic(host) {
  const s = await api("/v1/integrations/anthropic").catch(() => null);
  if (!s) { host.innerHTML = `<p class="meta">status unavailable</p>`; return; }
  const chip = s.active
    ? `<span class="statchip" style="color:var(--ok,#7a9e7e)">● connected</span>`
    : s.configured
      ? `<span class="statchip">○ key saved — restarting to load it</span>`
      : `<span class="statchip" style="color:var(--danger,#c25b4e)">◌ no key — thinking features off</span>`;
  const active = s.active_provider || "anthropic";
  const provOpts = (s.providers || [{ name: "anthropic" }]).map((p) =>
    `<option value="${p.name}" ${p.name === active ? "selected" : ""}>${p.name}${p.configured ? " ✓" : ""}</option>`).join("");
  const tierRow = (tier, label) => `
    <label class="meta" style="display:flex;gap:var(--s2);align-items:center">
      <span style="width:150px">${label}</span>
      <input id="ai-${tier}" value="${escAttr((s.models && s.models[tier]) || "")}" list="ai-model-choices"
        placeholder="${escAttr((s.defaults && s.defaults[tier]) || "")}" style="flex:1" spellcheck="false">
      <select id="ai-effort-${tier}" title="reasoning effort — only the newest models take one" hidden>
        ${EFFORTS.map((e) => `<option value="${e}" ${e === ((s.efforts && s.efforts[tier]) || "") ? "selected" : ""}>${
          e ? `effort: ${e}` : `effort: ${tier} (default)`}</option>`).join("")}</select>
    </label>`;
  // Per-tier routing: send ONE tier to a different provider (a local
  // model for Low, say). Collapsed unless a route is already set.
  const routes = s.routes || {};
  const anyRoute = ["low", "medium", "high"].some((t) => routes[t]?.provider);
  const routeRow = (tier, label) => {
    const r = routes[tier] || {};
    const opts = [`<option value="">same as above</option>`].concat(
      (s.providers || []).map((p) =>
        `<option value="${p.name}" ${p.name === r.provider ? "selected" : ""}>${p.name}</option>`)).join("");
    return `
    <label class="meta" style="display:flex;gap:var(--s2);align-items:center">
      <span style="width:150px">${label}</span>
      <select id="ai-route-${tier}">${opts}</select>
      <input id="ai-route-model-${tier}" value="${escAttr(r.model || "")}" placeholder="model id on that provider"
        style="flex:1" spellcheck="false" ${r.provider ? "" : "hidden"}>
    </label>`;
  };
  host.innerHTML = `
    <div class="card">
      <div class="task-row" style="align-items:baseline"><b>AI provider</b> ${chip}</div>
      <p class="meta">Powers the thinking parts: Ask, digests, and idle-time understanding of new
        memories. One provider is active at a time; you can configure others and switch. Keys stay
        sealed on this machine (and, if you enable it, sync encrypted to your own peer).</p>
      <div class="searchbar" style="flex-wrap:wrap;gap:var(--s2)">
        <select id="ai-provider">${provOpts}</select>
        <input id="ai-key" type="password" placeholder="key (${KEY_HINT[active] || "…"}) — leave blank to keep" autocomplete="off" style="flex:1">
      </div>
      <div id="ai-custom" style="margin-top:var(--s2)" hidden>
        <input id="ai-base-url" value="${escAttr(s.base_url || "")}" placeholder="http://localhost:11434/v1"
          spellcheck="false" autocomplete="off" style="width:100%">
        <p class="meta">Any OpenAI-compatible endpoint — Ollama, LM Studio, vLLM, OpenRouter. The address
          stays on this machine (it is not synced to your peer), and it has no default models: name them below.</p>
      </div>
      <p class="meta" style="margin-top:var(--s2)"><b>Models per tier</b> — how you spend on this
        provider (blank = recommended default). Low = basic, Medium = real extraction, High = complex:</p>
      <datalist id="ai-model-choices"></datalist>
      <div style="display:flex;flex-direction:column;gap:var(--s1);margin:var(--s1) 0">
        ${tierRow("low", "Low")}
        ${tierRow("medium", "Medium")}
        ${tierRow("high", "High")}
      </div>
      <details id="ai-routes" ${anyRoute ? "open" : ""} style="margin:var(--s2) 0">
        <summary class="meta">Send one tier to a different provider (advanced)</summary>
        <p class="meta">For example Low → a local model while Medium and High stay on the provider above.
          Applies to this machine only.</p>
        <div style="display:flex;flex-direction:column;gap:var(--s1)">
          ${routeRow("low", "Low")}
          ${routeRow("medium", "Medium")}
          ${routeRow("high", "High")}
        </div>
      </details>
      <button class="btn" id="ai-save">save & restart</button>
      ${s.blocked ? `
        <div class="meta" style="margin-top:var(--s2); color:var(--danger,#c25b4e)">
          ⛔ <b>thinking is paused</b> — the provider refused with a billing/credentials
          problem. Fix it at the provider, then:
          <button class="btn" id="ai-resume">billing's fixed — resume</button>
        </div>` : ""}
      <p class="meta" id="ai-note" hidden></p>
    </div>`;
  const sel = host.querySelector("#ai-provider");
  const TIERS = ["low", "medium", "high"];
  const routeSel = (t) => host.querySelector(`#ai-route-${t}`);
  // The address field shows only while something actually uses it.
  const syncCustom = () => {
    const usesCustom = sel.value === "custom" || TIERS.some((t) => routeSel(t).value === "custom");
    host.querySelector("#ai-custom").hidden = !usesCustom;
    TIERS.forEach((t) => { host.querySelector(`#ai-route-model-${t}`).hidden = !routeSel(t).value; });
  };
  TIERS.forEach((t) => { routeSel(t).onchange = syncCustom; });
  syncCustom();
  // Offer this provider's models, and show the effort choice only on a
  // tier whose model actually takes one.
  const syncModels = () => {
    host.querySelector("#ai-model-choices").innerHTML =
      (MODEL_CHOICES[sel.value] || []).map((m) => `<option value="${m}">`).join("");
    TIERS.forEach((t) => {
      const input = host.querySelector(`#ai-${t}`);
      const model = input.value.trim() || input.placeholder;
      host.querySelector(`#ai-effort-${t}`).hidden = !(sel.value === "anthropic" && takesEffort(model));
    });
  };
  TIERS.forEach((t) => { host.querySelector(`#ai-${t}`).addEventListener("input", syncModels); });
  syncModels();
  sel.onchange = () => {
    host.querySelector("#ai-key").placeholder = `key (${KEY_HINT[sel.value] || "…"}) — leave blank to keep`;
    // switching provider: clear tier fields so its own defaults apply
    TIERS.forEach((t) => {
      const input = host.querySelector(`#ai-${t}`);
      input.value = "";
      input.placeholder = sel.value === active ? ((s.defaults && s.defaults[t]) || "") : "";
    });
    syncCustom();
    syncModels();
  };
  host.querySelector("#ai-resume")?.addEventListener("click", async () => {
    const note = host.querySelector("#ai-note");
    note.hidden = false;
    const res = await apiPost("/v1/integrations/llm-resume", {}).catch((e) => ({ error: e.message }));
    note.textContent = res?.error ? `✗ ${res.error}`
      : "✓ resumed — thinking passes pick the backlog back up from the next cycle";
  });
  host.querySelector("#ai-save").onclick = async () => {
    const note = host.querySelector("#ai-note");
    note.hidden = false; note.textContent = "validating with the provider…";
    const res = await apiPost("/v1/integrations/anthropic", {
      actor: ACTOR,
      provider: sel.value,
      key: host.querySelector("#ai-key").value.trim() || undefined,
      low: host.querySelector("#ai-low").value.trim(),
      medium: host.querySelector("#ai-medium").value.trim(),
      high: host.querySelector("#ai-high").value.trim(),
      base_url: host.querySelector("#ai-base-url").value.trim() || undefined,
      efforts: Object.fromEntries(TIERS.map((t) => {
        const pick = host.querySelector(`#ai-effort-${t}`);
        return [t, pick.hidden ? "" : pick.value];
      })),
      routes: Object.fromEntries(TIERS.map((t) => [t, {
        provider: routeSel(t).value,
        model: routeSel(t).value ? host.querySelector(`#ai-route-model-${t}`).value.trim() : "",
      }])),
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    note.textContent = `✓ ${res.note}`;
    setTimeout(() => location.reload(), 4000);
  };
}

/* ————— the embedding model (semantic search) ————— */

export async function paintEmbeddings(host) {
  const s = await api("/v1/integrations/embeddings").catch(() => null);
  if (!s) { host.innerHTML = ""; return; }
  const chip = s.active
    ? `<span class="statchip" style="color:var(--ok,#7a9e7e)">● hybrid search on</span>`
    : s.present
      ? `<span class="statchip">○ installed — loads at next restart</span>`
      : `<span class="statchip" style="color:var(--ink-faint)">◌ not installed — search is text-only</span>`;
  const pct = s.total ? Math.min(100, Math.round((s.bytes / s.total) * 100)) : null;
  host.innerHTML = `
    <div class="card">
      <div class="task-row" style="align-items:baseline"><b>Semantic search</b> ${chip}</div>
      <p class="meta">A small local embedding model (all-MiniLM, ~90MB) lets search find
        memories by <i>meaning</i>, not just words — and powers semantic pairing, merge
        suggestions, and opportunity spotting. It runs entirely on this machine; the one-time
        download comes from Hugging Face with your say-so. Without it everything still works,
        text-only.</p>
      ${s.active || s.present ? "" : s.downloading ? `
        <p class="meta">downloading… ${pct != null ? `<b>${pct}%</b> (${Math.round(s.bytes / 1e6)}MB of ${Math.round(s.total / 1e6)}MB)` : `${Math.round(s.bytes / 1e6)}MB so far`}</p>
        <div style="height:6px;border-radius:3px;background:var(--line,#333);overflow:hidden">
          <div style="height:100%;width:${pct ?? 5}%;background:var(--accent)"></div>
        </div>` : s.done ? `
        <p class="meta">✓ downloaded + validated — the node is restarting to load it…</p>` : `
        <div class="tend-actions">
          <button class="btn" id="emb-dl">download the model (~90MB, one time)</button>
        </div>
        ${s.error ? `<p class="meta" style="color:var(--danger,#c25b4e)">✗ last attempt: ${esc(s.error)} — the button retries.</p>` : ""}`}
      <p class="meta" id="emb-note" hidden></p>
    </div>`;
  host.querySelector("#emb-dl")?.addEventListener("click", async () => {
    const note = host.querySelector("#emb-note");
    note.hidden = false;
    note.textContent = "starting the download…";
    const res = await apiPost("/v1/integrations/embeddings", {}).catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    paintEmbeddings(host);
  });
  if (s.downloading || s.done) {
    setTimeout(async () => {
      if (!document.contains(host)) return;
      if (s.done) {
        // the daemon exits once the model validates; wait for it to return
        const h = await fetch("/health").catch(() => null);
        if (h?.ok) { window.location.reload(); return; }
      }
      paintEmbeddings(host);
    }, 1500);
  }
}

/* ————— HearthFS ————— */

async function paintHearthfs(host) {
  const s = await api("/v1/integrations/hearthfs").catch(() => null);
  if (!s) { host.innerHTML = `<p class="meta">integration status unavailable</p>`; return; }
  const dot = s.enabled && s.online
    ? `<span class="statchip" style="color:var(--ok,#7a9e7e)">● mounted</span>`
    : s.enabled
      ? `<span class="statchip">○ enabled, mount not up</span>`
      : `<span class="statchip" style="color:var(--ink-faint)">◌ disabled</span>`;
  host.innerHTML = `
    <div class="card">
      <div class="task-row" style="align-items:baseline">
        <b>HearthFS</b> ${dot}
        ${s.agent_installed ? `<span class="meta">launch agent installed — the mount survives reboots</span>` : ""}
      </div>
      <p class="meta">A local-first <i>semantic filesystem</i>: point it at any folder and
        Ask/deep-query gains read handles over it (vsearch/vfetch) — results stay at origin
        until you explicitly promote one into the record. Optional; everything else works
        without it.</p>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="hfs-source" placeholder="folder to index — e.g. ~/Documents"
          value="${esc(s.source_dir || "")}" style="flex:2">
        <input id="hfs-port" placeholder="port" value="${esc(String(s.port || ""))}" style="flex:0.4">
        ${s.enabled
          ? `<button class="btn" id="hfs-disable">disable</button>
             <button class="btn btn-quiet" id="hfs-apply" title="re-enable with the new folder/port">apply changes</button>`
          : `<button class="btn" id="hfs-enable">enable</button>`}
      </div>
      ${!s.binary ? `<p class="meta">⚠ no hearthfs binary found — build it first
        (<code>cargo build --release</code> in the hearthfs repo), then enable.</p>` : ""}
      <details class="meta" style="margin-top:var(--s2)">
        <summary>details &amp; manual mount</summary>
        <div style="padding:var(--s2) 0">
          mountpoint: <code>${esc(s.mount)}</code><br>
          binary: <code>${esc(s.binary || "not found")}</code><br>
          ${s.manual_command ? `run by hand: <code>${esc(s.manual_command)}</code><br>` : ""}
          disable stops advertising it to Ask and unmounts; your files are never touched.
        </div>
      </details>
      <p class="meta" id="hfs-note" hidden></p>
    </div>`;
  const note = host.querySelector("#hfs-note");
  const act = async (action) => {
    note.hidden = false;
    note.textContent = action === "enable" ? "mounting… (first index can take a minute)" : "working…";
    const body = {
      actor: ACTOR, action,
      source_dir: host.querySelector("#hfs-source").value.trim() || undefined,
      port: parseInt(host.querySelector("#hfs-port").value, 10) || undefined,
    };
    const res = await apiPost("/v1/integrations/hearthfs", body).catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    paintHearthfs(host);
  };
  host.querySelector("#hfs-enable")?.addEventListener("click", () => act("enable"));
  host.querySelector("#hfs-apply")?.addEventListener("click", () => act("enable"));
  host.querySelector("#hfs-disable")?.addEventListener("click", () => act("disable"));
}

/* ————— Google Mail (live pull) ————— */

export async function paintGmail(host) {
  const s = await api("/v1/integrations/gmail").catch(() => null);
  if (!s) { host.innerHTML = ""; return; }
  const dot = s.configured
    ? `<span class="statchip" style="color:var(--ok,#7a9e7e)">● connected${s.user ? ` — ${esc(s.user)}` : ""}</span>`
    : `<span class="statchip" style="color:var(--ink-faint)">◌ not connected</span>`;
  host.innerHTML = `
    <div class="card">
      <div class="task-row" style="align-items:baseline"><b>Google Mail (live)</b> ${dot}</div>
      <p class="meta">Keeps your mail current past the last Takeout: new messages are pulled
        over IMAP every cycle into a live archive (searchable at need), and mail you wrote,
        personal mail, and starred mail become episodes — deduped against the Takeout, so
        connecting after an import never doubles anything.</p>
      ${s.configured ? `
        <div class="searchbar" style="flex-wrap:wrap">
          <button class="btn" id="gm-pull">pull now</button>
          <button class="btn btn-quiet" id="gm-disconnect">disconnect</button>
          ${s.last_ran_at ? `<span class="meta">last pull: ${esc(s.last_ran_at)}</span>` : ""}
        </div>` : `
        <details class="meta" style="margin:var(--s2) 0" open>
          <summary>you'll need a Google <b>app password</b> (not your account password)</summary>
          <ol style="margin:var(--s2) 0 0 1.2em;padding:0">
            <li>Turn on <b>2-Step Verification</b> for the account if it isn't already
              (myaccount.google.com → Security).</li>
            <li>Open <a href="https://myaccount.google.com/apppasswords" target="_blank"
              rel="noopener">myaccount.google.com/apppasswords</a> (hidden from the menus,
              the direct link works), name it e.g. <code>hearthloom</code>, and create.</li>
            <li>Paste the 16-character code below — spaces are fine. It's stored sealed on
              this node and never synced anywhere.</li>
          </ol>
        </details>
        <div class="searchbar" style="flex-wrap:wrap">
          <input id="gm-user" placeholder="you@gmail.com" style="flex:1.2" autocomplete="off">
          <input id="gm-pass" type="password" placeholder="app password" style="flex:1.2"
            autocomplete="off">
          <button class="btn" id="gm-connect">connect</button>
        </div>`}
      <p class="meta" id="gm-note" hidden></p>
    </div>`;
  const note = host.querySelector("#gm-note");
  const say = (msg) => { note.hidden = false; note.textContent = msg; };
  host.querySelector("#gm-connect")?.addEventListener("click", async () => {
    say("checking the sign-in with Gmail…");
    const res = await apiPost("/v1/integrations/gmail", {
      actor: ACTOR,
      user: host.querySelector("#gm-user").value.trim(),
      password: host.querySelector("#gm-pass").value,
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { say(`✗ ${res.error}`); return; }
    say("✓ connected — first pull starts within the hour (or use pull now)");
    paintGmail(host);
  });
  host.querySelector("#gm-disconnect")?.addEventListener("click", async () => {
    say("disconnecting…");
    const res = await apiPost("/v1/integrations/gmail", { actor: ACTOR, disconnect: true })
      .catch((e) => ({ error: e.message }));
    if (res?.error) { say(`✗ ${res.error}`); return; }
    paintGmail(host);
  });
  host.querySelector("#gm-pull")?.addEventListener("click", async () => {
    const btn = host.querySelector("#gm-pull");
    btn.disabled = true; btn.innerHTML = `<span class="spinner"></span> pulling…`;
    // The run endpoint is fire-and-forget (the pass runs detached);
    // poll the card status until last_ran_at moves, then repaint.
    const before = s.last_ran_at || "";
    const res = await apiPost("/v1/settings/automations/gmail_pull/run", {})
      .catch((e) => ({ error: e.message }));
    if (res?.error) {
      btn.disabled = false; btn.textContent = "pull now";
      say(`✗ ${res.error}`); return;
    }
    say("pull running — the result lands in Monitoring; big backfills take a few minutes…");
    const poll = setInterval(async () => {
      if (!document.contains(btn)) { clearInterval(poll); return; }
      const now = await api("/v1/integrations/gmail").catch(() => null);
      if (now && (now.last_ran_at || "") !== before) {
        clearInterval(poll);
        paintGmail(host);
      }
    }, 5000);
  });
}

/* ————— pair a device (QR) ————— */

/// Load the vendored QR encoder once (classic script → window.qrcode).
function loadQr() {
  if (window.qrcode) return Promise.resolve();
  return new Promise((ok, err) => {
    const s = document.createElement("script");
    s.src = "/assets/qrcode.js";
    s.onload = ok; s.onerror = err;
    document.head.append(s);
  });
}

/// `visitRule` adds the phone's calendar setting (the Integrations page
/// passes it; the wizard's pairing step stays lean).
export function paintPair(host, { visitRule = false } = {}) {
  host.innerHTML = `
    <div class="card">
      <div class="task-row" style="align-items:baseline"><b>Pair a device</b>
        <span class="meta">phone / tablet → this brain, one scan</span></div>
      <p class="meta">Mints a device token (read + ingest + notify — it can stream
        health and photo locations but never write or administer) and shows it as
        a QR the Hearthloom app can scan. Revocable any time under API Keys.</p>
      <div class="searchbar"><button class="btn" id="pair-mint">show pairing code</button></div>
      <div id="pair-out" hidden style="margin-top:var(--s2)"></div>
      ${visitRule ? `
      <div id="pair-rule" hidden style="margin-top:var(--s3)">
        <label style="display:flex; gap:var(--s2); align-items:baseline">
          <input type="checkbox" id="pair-calendar-alone">
          <span><b>Count a calendar event on its own as a visit</b>
            <span class="meta">— off, a visit needs your phone or a photo to confirm you
            were there; on, any event with an address becomes a visit, including
            invites other people send.</span></span>
        </label>
        <p class="meta" id="pair-rule-note" hidden></p>
      </div>` : ""}
    </div>`;
  if (visitRule) paintVisitRule(host).catch(() => {});
  host.querySelector("#pair-mint").addEventListener("click", async () => {
    const out = host.querySelector("#pair-out");
    out.hidden = false;
    out.innerHTML = `<p class="meta">minting…</p>`;
    try {
      await loadQr();
      const res = await apiPost("/v1/keys", {
        actor: ACTOR,
        name: `paired device (QR ${new Date().toISOString().slice(0, 10)})`,
        scopes: ["read", "ingest", "notify"],
        expires_at: null,
      });
      const secret = res?.secret;
      if (!secret) throw new Error(res?.error || "key mint failed");
      const payload = `hearthloom://pair?url=${encodeURIComponent(location.origin)}&pat=${encodeURIComponent(secret)}`;
      const qr = window.qrcode(0, "M");
      qr.addData(payload);
      qr.make();
      // White quiet-zone box so the code scans in the dark theme too.
      out.innerHTML = `
        <div style="display:inline-block;background:#fff;padding:12px;border-radius:8px">
          ${qr.createSvgTag({ cellSize: 4, margin: 0 })}
        </div>
        <p class="meta">Scan from the Hearthloom app's connect screen (or paste the
          token by hand: <code style="user-select:all">${esc(secret)}</code>).
          This token is shown ONCE — it lives only in this box and on the device
          that scans it. Done pairing? You can leave this page; unused tokens can
          be revoked under <a href="#/apikeys">API Keys</a>.</p>`;
    } catch (e) {
      out.innerHTML = `<p class="meta">✗ ${esc(e.message || String(e))}</p>`;
    }
  });
}

/// The visit pass's calendar setting. It sits on the phone's card because
/// the phone's calendar is the feed that places events on the map (an
/// .ics import makes no visits). Stored on visits_finalize's settings row;
/// it syncs to your other node, and applies to visits made from then on.
async function paintVisitRule(host) {
  const settings = await api("/v1/settings/automations");
  const row = (settings?.automations || []).find((a) => a.key === "visits_finalize");
  if (typeof row?.calendar_alone !== "boolean") return;
  const box = host.querySelector("#pair-calendar-alone");
  const note = host.querySelector("#pair-rule-note");
  box.checked = row.calendar_alone;
  host.querySelector("#pair-rule").hidden = false;
  box.addEventListener("change", async () => {
    const want = box.checked;
    box.disabled = true;
    try {
      const saved = await apiPost("/v1/settings/automations",
        { key: "visits_finalize", calendar_alone: want });
      box.checked = !!saved.calendar_alone;
      note.textContent = `✓ ${box.checked ? "on" : "off"} — applies to visits made from now on`;
    } catch (e) {
      box.checked = !want;
      note.textContent = `✗ not saved: ${e.message || e}`;
    }
    note.hidden = false;
    box.disabled = false;
  });
}

/* ————— the imports directory ————— */

/* Each entry: how to OBTAIN the export, then the ways in — `upload`
   (accept = file-picker filter, lands in the inbox drop folder),
   `local` (copy/CLI instructions verbatim), `adapter` (daemon-side
   run-now via /v1/ingest/local, for TCC-gated or archive-dir sources).
   `key` ties the card to the Downloads scanner (item 16) — cards with
   one get "found in Downloads" rows and the move-into-place path input.
   `localOnly` marks adapters that read a personal machine's own files —
   a cloud node hides their run button (item 17). */
const IMPORTS = [
  {
    name: "Mail (Gmail / mbox)",
    key: "mbox",
    what: "email becomes searchable episodes; senders become people.",
    obtain: "Google Takeout → Mail → export. Other providers: any RFC-4180 .mbox file.",
    upload: ".mbox",
    uploadNote: "up to 250MB here; bigger archives need the filtered pipeline:",
    local: "hearthloom ingest mbox <file> --include-labels Sent,Personal,Starred\n# or drop a ≤250MB .mbox into ~/.hearthloom/inbox/",
    extract: [{ source: "mbox", label: "mbox" }],
  },
  {
    name: "Calendar (.ics)",
    key: "ics",
    what: "events with places and attendees.",
    obtain: "Google Calendar → Settings → export; Apple Calendar → File → Export.",
    upload: ".ics",
    local: "drop the .ics into ~/.hearthloom/inbox/  # or: hearthloom ingest ics <file>",
  },
  {
    name: "Claude.ai conversations",
    key: "chatlog",
    what: "your Claude conversations, owner-aware.",
    obtain: "Claude → Settings → Privacy → export data. You get conversations.json.",
    upload: ".json",
    local: "drop conversations.json into ~/.hearthloom/inbox/  # or: hearthloom ingest chatlog <file>",
    extract: [{ source: "chatlog-claude", label: "Claude" }],
  },
  {
    name: "Claude Code sessions",
    localOnly: true,
    what: "coding sessions as work episodes.",
    obtain: "nothing to obtain — reads ~/.claude on this machine.",
    adapter: "claude-code",
    local: "hearthloom ingest claude-code",
  },
  {
    name: "iMessage",
    key: "imessage",
    localOnly: true,
    what: "message history, thread-day bundles.",
    obtain: "nothing to obtain — reads the local Messages database.",
    adapter: "imessage",
    adapterNote: "needs Full Disk Access on the daemon; if it fails, Finder-drag the file instead (Finder always has access).",
    upload: "chat.db",
    local: "Finder-drag ~/Library/Messages/chat.db into ~/.hearthloom/inbox/",
    extract: [{ source: "imessage", label: "iMessage" }],
  },
  {
    name: "Apple Photos (location trace)",
    key: "photos",
    localOnly: true,
    what: "where you were, from photo metadata — feeds the Atlas.",
    obtain: "nothing to obtain — reads the local Photos library.",
    adapter: "photos",
    upload: "Photos.sqlite",
    local: "Finder-drag ~/Pictures/Photos Library.photoslibrary/database/Photos.sqlite into ~/.hearthloom/inbox/",
  },
  {
    name: "Chrome history",
    localOnly: true,
    what: "browsing as a research trace.",
    obtain: "nothing to obtain — reads the local Chrome profile.",
    adapter: "chrome",
    local: "hearthloom ingest chrome",
  },
  {
    name: "Git repositories",
    localOnly: true,
    what: "commits become a work spine; repos become Projects.",
    obtain: "nothing to obtain — point it at any local clone.",
    local: "hearthloom ingest git <repo-path>",
  },
  {
    name: "Substack",
    what: "your published writing (works on custom domains).",
    obtain: "nothing to download — fetches from the archive API.",
    local: "hearthloom ingest substack <publication-url>",
    extract: [{ source: "substack", label: "Substack" }],
  },
  {
    name: "Apple Health",
    key: "apple-health",
    adapter: "apple-health",
    adapterNote: "runs the staged export at ~/.hearthloom/archives/apple-health (upload or import it first)",
    what: "one health_day aggregate per day + workouts/ECG/clinical; numbers go to the query sidecar.",
    obtain: "Health app → profile picture → Export All Health Data → export.zip.",
    upload: ".zip",
    local: "drop export.zip into ~/.hearthloom/inbox/",
    extract: [{ source: "apple-health", label: "Apple Health" }],
  },
  {
    name: "Facebook",
    key: "facebook",
    upload: ".zip",
    uploadNote: "the original DYI zip works — it stages itself; then press run.",
    what: "posts, albums, messages, events, groups — structural, no LLM burn.",
    obtain: "facebook.com/dyi → Download Your Information → JSON format, all time.",
    adapter: "facebook",
    adapterNote: "extract the archive first (see below), then run.",
    local: "unzip the DYI archive to ~/.hearthloom/archives/facebook/  # then press run",
  },
  {
    name: "LinkedIn",
    key: "linkedin",
    upload: ".zip",
    uploadNote: "the original export zip works — it stages itself; then press run.",
    what: "career spine (positions, education, honors, articles) + the richest connections registry.",
    obtain: "linkedin.com → Settings → Data privacy → Get a copy of your data (full archive).",
    adapter: "linkedin",
    adapterNote: "extract the archive first (see below), then run.",
    local: "unzip the export to ~/.hearthloom/archives/linkedin/  # then press run",
  },
  {
    name: "ChatGPT",
    key: "openai",
    upload: ".zip",
    uploadNote: "the original export zip works — it stages itself; then press run.",
    what: "every conversation becomes an episode; images preserved.",
    obtain: "ChatGPT → Settings → Data controls → Export data. You get a zip.",
    adapter: "openai",
    adapterNote: "extract the export first (see below), then run.",
    local: "unzip the export to ~/.hearthloom/archives/openai/  # then press run",
    estimateOpenai: true,
    extract: [{ source: "openai", label: "ChatGPT" }],
    methodNote: "One export, two ways in — use ONE, never both (they'd ingest the same conversations twice). <b>Full export</b> (this card's run/upload): parses the whole zip — every conversation plus preserved images; the complete path. <b>Single conversations.json</b>: drop just that file into the inbox — no images, and huge accounts may find it truncated; only for when that file is all you have.",
  },
  {
    name: "Notion",
    key: "notion",
    upload: ".zip",
    uploadNote: "the original export zip works — it stages itself; then press run.",
    what: "pages become notes (re-imports update, never duplicate; images preserved).",
    obtain: "Notion → Settings → Workspace → Export all workspace content (Markdown & CSV).",
    adapter: "notion",
    adapterNote: "extract the export first (see below), then run.",
    local: "unzip the export to ~/.hearthloom/archives/notion/  # then press run",
  },
  {
    name: "Rewind (rescue)",
    what: "the lifelogger that was remote-killed on 2025-12-19 left years of screen-time and call transcripts stranded on your disk — this rescues them: app sessions become one episode per day, recorded conversations become transcripts. Reads the frozen database in place.",
    obtain: "nothing to fetch — if Rewind ever ran on this Mac, its database is at ~/Library/Application Support/com.memoryvault.MemoryVault/. Newer installs encrypted it (db-enc.sqlite3): decrypt once with sqlcipher (run now shows the exact commands), then paste the decrypted copy's path below.",
    adapter: "rewind",
    localOnly: true,
    pathInput: "(optional) /path/to/decrypted/rewind.sqlite3 — blank uses the default location",
    local: "POST /v1/ingest/local {\"adapter\":\"rewind\"}  # or point path at a decrypted copy",
  },
  {
    name: "Markdown vault (Obsidian · Logseq · Reflect)",
    what: "every .md becomes a live note — wiki-links, tags, frontmatter dates and images preserved; the vault is read in place, never moved or written; re-imports update, never duplicate.",
    obtain: "any folder of markdown files — an Obsidian vault, a Logseq graph, a Reflect Open export. Paste its full path below.",
    adapter: "markdown",
    localOnly: true,
    pathInput: "/full/path/to/your/vault",
    mount: true,
    adapterNote: "imports the vault at the path above (staying in Obsidian? mount instead)",
    local: 'still living in the vault day-to-day? "mount as virtual source" keeps it at origin — Ask searches it live, and you promote only what\'s worth keeping. "run now" imports it as editable Hearthloom notes.',
  },
];

// Extraction-tier options, one <select> per source a card feeds. `tiers`
// is the source→{tier,default_tier} map from /v1/integrations/extraction;
// a source absent from the map (LLM not configured, say) still renders at
// its own resolved value. "disabled" turns entity mining off for that
// source — the record still ingests, it just skips the LLM pass.
const EXTRACT_TIERS = ["disabled", "low", "medium", "high"];
const EXTRACT_TIER_LABEL = {
  disabled: "no extraction", low: "low", medium: "medium", high: "high",
};

function extractRow(entry, tiers) {
  if (!entry.extract) return "";
  const multi = entry.extract.length > 1;
  const selects = entry.extract.map((e) => {
    const cur = tiers[e.source] || {};
    const val = cur.tier || "disabled";
    const def = cur.default_tier || "disabled";
    const opts = EXTRACT_TIERS.map((t) =>
      `<option value="${t}" ${t === val ? "selected" : ""}>${EXTRACT_TIER_LABEL[t]}${
        t === def ? " · recommended" : ""}</option>`).join("");
    return `<label class="meta" style="display:flex;gap:var(--s2);align-items:center">
      ${multi ? `<span style="width:64px">${esc(e.label)}</span>` : ""}
      <select class="imp-extract" data-source="${esc(e.source)}">${opts}</select></label>`;
  }).join("");
  const rerun = entry.extract.map((e) =>
    `<a href="#" class="imp-rerun" data-source="${esc(e.source)}">re-interpret ${esc(e.label)} at high quality</a>`
  ).join(" · ");
  return `<details class="meta"><summary>entity extraction${
    multi ? "" : ` — <span class="imp-extract-cur">${esc(
      EXTRACT_TIER_LABEL[(tiers[entry.extract[0].source] || {}).tier || "disabled"])}</span>`}</summary>
    <div style="margin:var(--s1) 0 0; display:flex; flex-direction:column; gap:var(--s1)">
      <span>how hard the LLM mines people, orgs and places from this source's text.</span>
      ${selects}
      <span class="meta">${rerun} — stronger verdicts replace weaker ones, never pile up.</span>
    </div></details>`;
}

// One "found in Downloads" row (item 16): import = MOVE into place by
// default; the checkbox keeps the original. An over-limit mbox gets the
// warning instead of a dead-end move.
function foundRow(f) {
  const size = f.bytes ? ` (${Math.round(f.bytes / 1e6) || "<1"} MB)` : "";
  return `
    <div class="imp-found-row" data-path="${esc(f.path)}" data-key="${esc(f.key)}">
      <span class="meta">📥 found <b>${esc(f.name)}</b>${size} in Downloads</span>
      ${f.warn
        ? `<span class="meta imp-warn">⚠ ${esc(f.warn)}</span>`
        : `<button class="btn imp-place">import → ${esc(f.dest)}</button>
           <label class="meta"><input type="checkbox" class="imp-keep"> keep a copy</label>`}
    </div>`;
}

/* ————— plugins: the local install/manage surface (docs/plugins.md).
   NO central index and NO signing here — distribution is the provisioner's
   job later; every install is from a folder or zip the user chose and
   is labeled accordingly. ————— */
async function paintPlugins(host) {
  const [data, reg, peer] = await Promise.all([
    api("/v1/plugins").catch(() => null),
    api("/v1/plugins/registry/index").catch(() => null),
    api("/v1/plugins/peer").catch(() => null),
  ]);
  const plugins = data?.plugins || [];
  const available = (reg?.embers || []).filter(
    (e) => !plugins.some((p) => p.name === e.name));
  const scopeChips = (scopes) =>
    (scopes || []).map((s) => `<span class="chip">${esc(s)}</span>`).join(" ") || "<span class='meta'>no access</span>";
  const semverGt = (a, b) => {
    const pa = String(a || "").replace(/^v/, "").split(/[.\-+]/).map((n) => Number(n) || 0);
    const pb = String(b || "").replace(/^v/, "").split(/[.\-+]/).map((n) => Number(n) || 0);
    for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
      if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) > (pb[i] || 0);
    }
    return false;
  };
  const latestOf = (name) => (reg?.embers || []).find((e) => e.name === name)?.latestVersion;
  const runnerLine = (p) => {
    if (!p.enabled || !p.runner?.node) return "";
    const here = p.running;
    return `<span class="chip">${here ? "running here" : p.runner.node.startsWith("cloud")
      ? "runs on your always-on node" : `runs on ${esc(p.runner.node.split(":")[1] || p.runner.node)}`}</span>`;
  };

  const installedCards = plugins.map((p) => {
    const latest = latestOf(p.name);
    const updatable = latest && semverGt(latest, p.version);
    return `
      <div class="card" data-plugin="${esc(p.name)}">
        <h3 class="imp-name">${esc(p.name)}
          <span class="chip" style="float:right">${p.enabled ? (p.running ? "running" : "on") : "off"}</span></h3>
        <p class="imp-what">${esc(p.manifest?.description || "")}</p>
        <p class="meta">can see: ${scopeChips(p.granted_scopes)} ${runnerLine(p)}
          ${updatable ? `<span class="chip" style="color:var(--accent)">update available</span>` : ""}</p>
        <div class="tend-actions">
          ${Array.isArray(p.manifest?.config) && p.manifest.config.length && p.enabled
            ? `<button class="btn btn-quiet plg-config">configure…</button>` : ""}
          ${((window.__pluginUi?.card) || []).filter((c) => c.plugin === p.name && p.enabled)
            .map((c) => `<a class="btn btn-quiet" href="#/plugin/${esc(c.plugin)}/${esc(c.page)}">${esc(c.label)} →</a>`).join("")}
          ${updatable ? `<button class="btn plg-update" data-version="${esc(latest)}">update to ${esc(latest)}</button>` : ""}
          <button class="btn btn-quiet plg-toggle" data-enabled="${p.enabled ? 1 : 0}">${p.enabled ? "turn off" : "turn on"}</button>
          <button class="btn btn-quiet plg-uninstall">remove\u2026</button>
        </div>
      </div>`;
  }).join("");

  const availCard = (e) => `
    <div class="card" data-avail="${esc(e.name)}">
      <h3 class="imp-name">${esc(e.displayName || e.name)}
        <span class="meta" style="float:right">${esc(e.latestVersion || "")}</span></h3>
      <p class="imp-what">${esc(e.description || "")}</p>
      <p class="meta">by ${esc(e.publisher || "?")}</p>
      <div class="tend-actions"><button class="btn plg-get">add to your hearth</button></div>
    </div>`;

  host.innerHTML = `
    ${plugins.length ? installedCards
      : `<p class="meta">No embers yet \u2014 they're optional companions (a screen-time
         tracker, new charts, new sources) you can add below.</p>`}
    ${(() => {
      if (!peer?.paired) return "";
      const local = new Set(plugins.map((p) => p.name));
      if (peer.plugins === null) return `<p class="meta">your other node is paired but unreachable right now \u2014 its embers will show here when it answers.</p>`;
      const remote = (peer.plugins || []).filter((e) => !local.has(e.name));
      if (!remote.length) return "";
      const why = (e) =>
        e.placement === "local" ? "it reads that machine, so it runs (and is managed) only there"
        : e.placement === "cloud" ? "it belongs on the always-on node"
        : "manage it from that node";
      return `<div class="section-title" style="margin-top:var(--s3);font-size:0.95em">on your other node</div>` +
        remote.map((e) => `
          <div class="card" style="opacity:.85">
            <h3 class="imp-name">${esc(e.name)} <span class="meta">v${esc(e.version)}</span>
              <span class="chip" style="float:right">${e.running ? "running there" : e.enabled ? "on there" : "off there"}</span></h3>
            <p class="imp-what">${esc(e.description || "")}</p>
            <p class="meta">installed on your other node \u2014 ${why(e)}.${
              e.sync === "mirror" ? " Its data syncs here either way." : ""}</p>
          </div>`).join("");
    })()}
    ${available.length || reg ? `<div class="section-title" style="margin-top:var(--s3);font-size:0.95em">available from the hearth</div>
      <div class="searchbar"><input id="plg-search" placeholder="search for more\u2026" style="flex:1"></div>
      <div id="plg-gallery">${available.map(availCard).join("") ||
        "<p class='meta'>nothing new right now \u2014 more embers appear here as they're published.</p>"}</div>`
      : `<p class="meta">couldn't reach the ember registry just now.</p>`}
    <details class="meta" style="margin-top:var(--s3)"><summary>install one you built yourself</summary>
      <div class="card" style="margin-top:var(--s2)">
        <p class="imp-what">Point at a folder (with <code>hearthember.json</code>) or upload a zip.
          You'll review what it can see before anything is granted.
          <b>Unreviewed \u2014 installing means you trust its author with your machine.</b></p>
        <div class="searchbar" style="flex-wrap:wrap">
          <input id="plg-path" placeholder="/path/to/ember-folder" style="flex:2;min-width:220px">
          <button class="btn" id="plg-inspect">review\u2026</button>
          <label class="btn btn-quiet" style="cursor:pointer">upload zip
            <input type="file" id="plg-zip" accept=".zip" hidden></label>
        </div>
      </div>
    </details>
    <div id="plg-review" hidden></div>
    <p class="meta" id="plg-msg"></p>`;

  const msg = host.querySelector("#plg-msg");
  const review = host.querySelector("#plg-review");
  // segment-wise "current satisfies >=req" check for the host gate
  const hostOk = (req) => {
    if (!req) return true;
    const cur = window.__version || "0.0.0";
    const p = (v) => String(v).replace(/^v/, "").split(/[.\-+]/).map((n) => Number(n) || 0);
    const need = p(req.replace(/^[><=]+/, "")), have = p(cur);
    for (let i = 0; i < 3; i++) { if ((have[i]||0) !== (need[i]||0)) return (have[i]||0) > (need[i]||0); }
    return true;
  };
  const approveAndInstall = (info, path) => {
    // the human gate: what it can see, then one deliberate click
    const scopes = info.scopes || info.requested_scopes || [];
    const req = info.requires_host;
    review.hidden = false;
    if (req && !hostOk(req)) {
      review.innerHTML = `<div class="card">
        <p class="meta"><b>${esc(info.name)}</b> v${esc(info.version)} needs hearthloom ${esc(req)},
          but this node is v${esc(window.__version || "?")}.</p>
        <div class="tend-actions">
          <a class="btn" href="#/upgrade">upgrade hearthloom first →</a>
          <button class="btn btn-quiet" id="plg-cancel">cancel</button>
        </div></div>`;
      review.querySelector("#plg-cancel").onclick = () => { review.hidden = true; };
      return;
    }
    review.innerHTML = `<div class="card">
      <p class="meta"><b>${esc(info.name)}</b> v${esc(info.version)}
        ${info.publisher ? `by ${esc(info.publisher)}` : ""} ${
        info.reviewed ? "\u00b7 \u2713 reviewed by the registry" : "\u00b7 <b>unreviewed</b>"}</p>
      <p class="meta">it will be able to: ${scopeChips(scopes)}</p>
      <div class="tend-actions">
        <button class="btn" id="plg-approve">allow &amp; add</button>
        <button class="btn btn-quiet" id="plg-cancel">cancel</button>
      </div></div>`;
    review.querySelector("#plg-cancel").onclick = () => { review.hidden = true; };
    review.querySelector("#plg-approve").onclick = async () => {
      msg.textContent = "adding\u2026";
      try {
        const res = await apiPost("/v1/plugins/install", { path, scopes });
        msg.textContent = `\u2713 added ${res.installed} \u2014 reloading for its pages\u2026`;
        setTimeout(() => window.location.reload(), 900);
      } catch (e) {
        msg.textContent = `couldn't add it: ${e.message}`;
      }
    };
  };

  const wireGets = (root) => {
    root.querySelectorAll(".card[data-avail] .plg-get").forEach((btn) => {
      btn.onclick = async () => {
        const name = btn.closest(".card").dataset.avail;
        msg.textContent = `fetching ${name} (checking its signature)\u2026`;
        try {
          const staged = await apiPost("/v1/plugins/registry/stage", { name });
          msg.textContent = "";
          approveAndInstall(staged, staged.path);
        } catch (e) {
          msg.textContent = e.message;
        }
      };
    });
  };
  wireGets(host);

  const searchEl = host.querySelector("#plg-search");
  if (searchEl) {
    let t;
    searchEl.oninput = () => {
      clearTimeout(t);
      t = setTimeout(async () => {
        const res = await api(`/v1/plugins/registry/index?q=${encodeURIComponent(searchEl.value)}`).catch(() => null);
        const gallery = host.querySelector("#plg-gallery");
        if (!res || !gallery) return;
        const names = new Set(plugins.map((p) => p.name));
        gallery.innerHTML = (res.embers || []).filter((e) => !names.has(e.name))
          .map(availCard).join("") || "<p class='meta'>nothing matched.</p>";
        wireGets(gallery);
      }, 350);
    };
  }

  // sideload flow (the details block)
  host.querySelector("#plg-inspect").onclick = async () => {
    const path = host.querySelector("#plg-path").value.trim();
    if (!path) return;
    msg.textContent = "";
    try {
      const info = await api(`/v1/plugins/inspect?path=${encodeURIComponent(path)}`);
      approveAndInstall(info, path);
    } catch (e) {
      msg.textContent = e.message;
    }
  };
  host.querySelector("#plg-zip").onchange = async (ev) => {
    const file = ev.target.files?.[0];
    if (!file) return;
    msg.textContent = "uploading\u2026";
    try {
      const res = await fetch("/v1/plugins/upload", { method: "POST", body: file });
      const data = await res.json();
      if (!res.ok) throw new Error(data.error || res.statusText);
      msg.textContent = "";
      approveAndInstall(data, data.path);
    } catch (e) {
      msg.textContent = `upload failed: ${e.message}`;
    }
  };

  // installed-card actions
  host.querySelectorAll(".card[data-plugin]").forEach((card) => {
    const name = card.dataset.plugin;
    const schema = plugins.find((x) => x.name === name)?.manifest?.config;
    card.querySelector(".plg-config")?.addEventListener("click", async () => {
      let panel = card.querySelector(".plg-config-panel");
      if (panel) { panel.remove(); return; } // toggle
      const current = (await api(`/v1/plugins/${encodeURIComponent(name)}/config`).catch(() => null))?.values || {};
      panel = document.createElement("div");
      panel.className = "plg-config-panel";
      panel.style.marginTop = "var(--s2)";
      panel.innerHTML = (schema || []).map((f) => {
        const val = current[f.key] ?? f.default ?? "";
        const help = f.help ? `<div class="meta" style="opacity:.8">${esc(f.help)}</div>` : "";
        if (f.type === "boolean") {
          return `<label class="meta" style="display:block;margin:6px 0">
            <input type="checkbox" data-key="${esc(f.key)}" data-type="boolean" ${val ? "checked" : ""}>
            ${esc(f.label)}${help}</label>`;
        }
        if (f.type === "list") {
          const lines = Array.isArray(val) ? val.join("\n") : "";
          return `<label class="meta" style="display:block;margin:6px 0">${esc(f.label)}${help}
            <textarea data-key="${esc(f.key)}" data-type="list" rows="3"
              style="width:100%;font:inherit">${esc(lines)}</textarea></label>`;
        }
        return `<label class="meta" style="display:block;margin:6px 0">${esc(f.label)}${help}
          <input data-key="${esc(f.key)}" data-type="${esc(f.type)}" value="${esc(String(val))}"
            style="width:100%;font:inherit"></label>`;
      }).join("") + `
        <div class="tend-actions">
          <button class="btn plg-config-save">save</button>
          <span class="meta plg-config-note"></span>
        </div>`;
      card.append(panel);
      panel.querySelector(".plg-config-save").onclick = async () => {
        const values = {};
        panel.querySelectorAll("[data-key]").forEach((el) => {
          const t = el.dataset.type;
          values[el.dataset.key] =
            t === "boolean" ? el.checked
            : t === "number" ? Number(el.value) || 0
            : t === "list" ? el.value.split("\n").map((x) => x.trim()).filter(Boolean)
            : el.value;
        });
        try {
          await apiPost(`/v1/plugins/${encodeURIComponent(name)}/config`, { values });
          panel.querySelector(".plg-config-note").textContent =
            "saved ✓ — the ember picks it up within a few minutes";
        } catch (e) {
          panel.querySelector(".plg-config-note").textContent = e.message;
        }
      };
    });
    card.querySelector(".plg-update")?.addEventListener("click", async (ev) => {
      msg.textContent = "fetching the update (checking its signature)\u2026";
      try {
        const staged = await apiPost("/v1/plugins/registry/stage", {
          name, version: ev.target.dataset.version });
        msg.textContent = "";
        approveAndInstall(staged, staged.path);
      } catch (e) {
        msg.textContent = e.message;
      }
    });
    card.querySelector(".plg-toggle").onclick = async (ev) => {
      const enabled = ev.target.dataset.enabled === "1";
      await apiPost(`/v1/plugins/${encodeURIComponent(name)}/enable`, { enabled: !enabled });
      paintPlugins(host);
    };
    card.querySelector(".plg-uninstall").onclick = async () => {
      const ok = await confirmDanger({
        title: `Put out ${name}?`,
        body: "It stops and loses its access. Anything it saved stays " +
          "on disk \u2014 nothing of yours is deleted.",
        action: "remove",
      });
      if (!ok) return;
      const res = await fetch(`/v1/plugins/${encodeURIComponent(name)}`, { method: "DELETE" });
      const data = await res.json();
      if (res.ok) {
        alertNote(`removed \u2014 its saved data stays at ${data.data_dir_kept}`);
        paintPlugins(host);
      }
    };
  });
}

function alertNote(text) {
  // house rule: never browser alert(); a transient meta line instead
  const el = document.createElement("p");
  el.className = "meta";
  el.textContent = text;
  document.querySelector("#int-plugins")?.prepend(el);
  setTimeout(() => el.remove(), 8000);
}

// adapter name → the `<kind>_ingest_done` event prefix
const STATUS_KEY = { "apple-health": "health" };
function importStatusLine(entry, imports) {
  const key = STATUS_KEY[entry.adapter] || entry.adapter;
  if (!key) return "";
  let out = "";
  const s = imports?.last?.[key];
  if (s) {
    const when = (s.ts || "").slice(0, 16).replace("T", " ");
    const nums = Object.entries(s.summary || {})
      .filter(([, v]) => typeof v === "number" && v > 0)
      .slice(0, 4)
      .map(([k, v]) => `${v.toLocaleString()} ${k.replace(/_/g, " ")}`)
      .join(" · ");
    out += `<p class="meta imp-status">✓ last import ${esc(when)}${nums ? ` — ${esc(nums)}` : ""}</p>`;
  }
  // the LLM phase: the import isn't really "done" until interpretation
  // has chewed through the backlog — show progress + a rough forecast
  const interp = imports?.interpreting?.[key];
  if (interp) {
    out += `<p class="meta imp-status imp-interp" data-interp-key="${esc(key)}">${interpLine(interp, imports, key)}</p>`;
  }
  return out;
}

// The one honest sentence about interpretation progress. A local
// forecast only exists when THIS node is doing the extracting — a
// delegating node's backlog drains at the peer's pace, in jumps, as
// verdicts sync back (a frozen "~7m left" reads as a stall).
function interpLine(interp, imports, key) {
  const n = Number(interp.pending).toLocaleString();
  let line;
  if (imports?.llm_blocked) {
    line = `🧠 interpreting — ${n} to go — <b>paused: the AI provider needs attention</b> (see the AI provider card)`;
  } else if (!interp.pending) {
    line = ""; // only the failed warning below
  } else if (imports?.llm_delegated || imports?.llm_here === false) {
    line = `🧠 interpreting on your always-on node — ${n} to go (progress lands here as it syncs back)`;
  } else {
    line = `🧠 interpreting — ${n} to go, ~${interp.forecast_minutes}m left`;
  }
  // Failures never vanish silently: report the count, the captured
  // reason, and the way back into the queue.
  if (interp.failed) {
    const why = interp.error?.error ? ` — ${esc(interp.error.error)}` : "";
    line += `${line ? "<br>" : ""}⚠ ${Number(interp.failed).toLocaleString()} couldn't be interpreted${why} · <a href="#" class="imp-retry-failed" data-source="${esc(key)}">try again</a>`;
  }
  return line;
}

function importCard(entry, idx, tiers, found, node, imports) {
  const cloud = node === "cloud";
  const hits = !cloud && entry.key ? found.filter((f) => f.key === entry.key) : [];
  return `
    <div class="card" data-import="${idx}" ${entry.key ? `data-key="${esc(entry.key)}"` : ""} ${entry.adapter ? `data-adapter="${esc(entry.adapter)}"` : ""}>
      <h3 class="imp-name">${esc(entry.name)}</h3>
      <p class="imp-what">${esc(entry.what)}</p>
      <details class="meta imp-obtain"><summary>where do I get it?</summary>
        <div style="margin-top:var(--s1)">${esc(entry.obtain)}</div></details>
      ${entry.methodNote ? `<details class="meta"><summary>two methods — which one?</summary>
        <div style="margin-top:var(--s1)">${entry.methodNote}</div></details>` : ""}
      ${importStatusLine(entry, imports)}
      ${hits.slice(0, 2).map(foundRow).join("")}
      ${hits.length > 2 ? `
        <a href="#" class="meta imp-found-more">…and ${hits.length - 2} more found in Downloads — review all</a>
        <div class="imp-found-modal" hidden>
          <div class="imp-found-box">
            <div class="task-row" style="justify-content:space-between; align-items:baseline">
              <b>Found in Downloads — ${esc(entry.name)}</b>
              <button class="btn btn-quiet imp-found-close">close</button>
            </div>
            <div class="imp-found-scroll">${hits.map(foundRow).join("")}</div>
          </div>
        </div>` : ""}
      ${entry.pathInput && !cloud ? `
        <div class="searchbar" style="margin:var(--s1) 0">
          <input class="imp-path" placeholder="${esc(entry.pathInput)}" style="flex:1" spellcheck="false">
        </div>` : ""}
      <div class="tend-actions" style="align-items:center; flex-wrap:wrap">
        ${entry.upload ? `
          <button class="btn imp-upload" data-accept="${esc(entry.upload)}">upload ${esc(entry.upload)}</button>` : ""}
        ${entry.adapter && !(cloud && entry.localOnly) ? `
          <button class="btn imp-run" data-adapter="${esc(entry.adapter)}"
            ${entry.adapterNote ? `title="${esc(entry.adapterNote)}"` : ""}>run now</button>` : ""}
        ${entry.mount && !cloud ? `
          <button class="btn btn-quiet imp-mount"
            title="keep the vault at origin; Ask searches it live, promote what's worth keeping">mount as virtual source</button>` : ""}
        ${cloud && entry.localOnly ? `
          <span class="meta">reads a personal machine's own files — run this from your local node</span>` : ""}
        ${!cloud && entry.key ? `
          <a href="#" class="meta imp-choose-toggle">somewhere else? move it into place…</a>` : ""}
        <details class="meta"><summary>running locally</summary>
          <pre style="white-space:pre-wrap; margin:var(--s1) 0 0">${esc(entry.local)}</pre>
          ${entry.uploadNote ? `<div>${esc(entry.uploadNote)}</div>` : ""}
        </details>
        ${extractRow(entry, tiers)}
      </div>
      ${!cloud && entry.key ? `
        <div class="imp-choose searchbar" hidden style="margin-top:var(--s1)">
          <input class="imp-choose-path" placeholder="/full/path/to/the/export (file or extracted folder)" style="flex:1" spellcheck="false">
          <label class="meta"><input type="checkbox" class="imp-keep"> keep a copy</label>
          <button class="btn imp-place-path">move into place</button>
        </div>` : ""}
      ${entry.estimateOpenai ? `<p class="meta imp-estimate" style="margin-top:var(--s1)">sizing the export…</p>` : ""}
      <p class="meta imp-note" hidden></p>
    </div>`;
}

// Human $ / count formatting for the pre-flight estimate.
const fmtUsd = (n) => n < 0.01 ? "<$0.01" : `$${n.toFixed(2)}`;
const fmtNum = (n) => Number(n).toLocaleString();

async function paintOpenaiEstimate(card) {
  const el = card.querySelector(".imp-estimate");
  if (!el) return;
  const est = await api("/v1/integrations/openai/estimate").catch(() => null);
  if (!est) { el.textContent = "estimate unavailable"; return; }
  if (!est.found) {
    el.innerHTML = `<span class="kind">before you run</span> no export found at
      <code>~/.hearthloom/archives/openai/</code> yet — unzip it there first.`;
    return;
  }
  const conv = fmtNum(est.est_conversations);
  const mb = Math.round(est.bytes / 1e6);
  if (est.extract_tier === "disabled") {
    el.innerHTML = `<span class="kind">ready</span> ~${conv} conversations (${mb} MB).
      Ingest is free; entity extraction is <b>off</b> for this source (change it above to spend on mining people/orgs).`;
  } else {
    el.innerHTML = `<span class="kind">before you run</span> ~${conv} conversations (${mb} MB).
      Ingest is free; entity extraction at <b>${esc(est.extract_tier)}</b> tier
      (${esc(est.extract_model || "?")}) is a rough <b>${fmtUsd(est.est_extract_usd)}</b>
      over idle cycles. <span class="meta">${esc(est.rate_note)}</span>`;
  }
}

function wireImports(host) {
  host.querySelectorAll(".imp-upload").forEach((btn) => {
    btn.onclick = () => {
      const note = btn.closest(".card").querySelector(".imp-note");
      const input = document.createElement("input");
      input.type = "file";
      const accept = btn.dataset.accept;
      if (accept.startsWith(".")) input.accept = accept;
      input.onchange = async () => {
        const file = input.files?.[0];
        if (!file) return;
        note.hidden = false;
        note.textContent = `uploading ${file.name}…`;
        try {
          const res = await fetch(`/v1/inbox/upload?filename=${encodeURIComponent(file.name)}`, {
            method: "POST", body: file,
          });
          const data = await res.json();
          note.textContent = res.ok
            ? `✓ ${data.saved} — ${data.note}`
            : `✗ ${data.error || res.statusText}`;
        } catch (err) {
          note.textContent = `✗ ${err.message}`;
        }
      };
      input.click();
    };
  });
  host.querySelectorAll(".imp-run").forEach((btn) => {
    btn.onclick = async () => {
      const note = btn.closest(".card").querySelector(".imp-note");
      // Immediate feedback: disable + spinner while the request is in flight
      // (item 6). Detached ingests return "started"; the spinner then clears
      // and the note explains progress lands in daemon.log.
      const label = btn.textContent;
      btn.disabled = true;
      btn.innerHTML = `<span class="spinner"></span> running…`;
      note.hidden = false;
      note.textContent = "starting… (big archives run detached — progress in daemon.log)";
      // a card with a path field (markdown vault) sends it along
      const path = btn.closest(".card").querySelector(".imp-path")?.value.trim();
      const res = await apiPost("/v1/ingest/local",
        { actor: ACTOR, adapter: btn.dataset.adapter, ...(path ? { path } : {}) })
        .catch((e) => ({ error: e.message }));
      note.textContent = res?.error ? `✗ ${res.error}` : `✓ ${res.summary || res.note || "started"}`;
      // detached archive imports keep the spinner until the daemon says
      // the run finished, then the card repaints with its result line
      if (!res?.error && DETACHED_ADAPTERS.includes(btn.dataset.adapter)) {
        btn.dataset.label = label;
        btn.innerHTML = `<span class="spinner"></span> importing…`;
        const poll = setInterval(async () => {
          if (!document.contains(btn)) { clearInterval(poll); return; }
          const now = await api("/v1/integrations/imports").catch(() => null);
          if (now && !now.running.length) { clearInterval(poll); REMOUNT(); }
        }, 4000);
        return;
      }
      btn.disabled = false;
      btn.textContent = label;
    };
  });
  // 1b's second verb: keep the vault at origin as a plain-dir virtual
  // source (no HearthFS app required) — Ask gets vsearch/vfetch over it,
  // promotion stays the explicit materialization step.
  host.querySelectorAll(".imp-mount").forEach((btn) => {
    btn.onclick = async () => {
      const card = btn.closest(".card");
      const note = card.querySelector(".imp-note");
      note.hidden = false;
      const path = card.querySelector(".imp-path")?.value.trim();
      if (!path) { note.textContent = "✗ enter the vault's full path first"; return; }
      const stem = path.split("/").filter(Boolean).pop() || "notes";
      const name = "vault-" + stem.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 24);
      btn.disabled = true;
      try {
        await apiPost("/v1/sources/virtual", {
          actor: ACTOR, name, adapter: "hearthfs",
          config: { mount: path, magic: false },
        });
        note.textContent = `✓ mounted as virtual source “${name}” — it stays at origin; Ask can search it live, and you can promote files worth keeping into the record`;
      } catch (err) {
        // older builds without virtual sources degrade to import-only
        note.textContent = `✗ couldn't mount (${err.message}) — you can still import it as notes instead`;
      }
      btn.disabled = false;
    };
  });
  // overflowed "found in Downloads" rows live in a scrollable modal
  // instead of eating the card's vertical space
  host.querySelectorAll(".imp-found-more").forEach((a) => {
    a.onclick = (e) => {
      e.preventDefault();
      a.closest(".card").querySelector(".imp-found-modal").hidden = false;
    };
  });
  host.querySelectorAll(".imp-found-close").forEach((btn) => {
    btn.onclick = () => { btn.closest(".imp-found-modal").hidden = true; };
  });
  host.querySelectorAll(".imp-found-modal").forEach((m) => {
    m.onclick = (e) => { if (e.target === m) m.hidden = true; };
  });
  host.querySelectorAll(".imp-rerun").forEach((a) => {
    a.onclick = async (e) => {
      e.preventDefault();
      const note = a.closest(".card").querySelector(".imp-note");
      note.hidden = false;
      note.textContent = "re-queueing…";
      const res = await apiPost("/v1/integrations/extraction/rerun",
        { actor: ACTOR, source: a.dataset.source, tier: "high" })
        .catch((err) => ({ error: err.message }));
      note.textContent = res?.error ? `✗ ${res.error}`
        : `✓ ${res.requeued} episodes re-queued — ${res.note}`;
    };
  });
  host.querySelectorAll(".imp-extract").forEach((sel) => {
    sel.onchange = async () => {
      const card = sel.closest(".card");
      const note = card.querySelector(".imp-note");
      const prev = sel.dataset.prev || sel.value;
      try {
        const res = await apiPost("/v1/integrations/extraction",
          { source: sel.dataset.source, tier: sel.value });
        sel.value = res.tier; // reflect the resolved value
        sel.dataset.prev = res.tier;
        const cur = sel.closest("details")?.querySelector(".imp-extract-cur");
        if (cur) cur.textContent = EXTRACT_TIER_LABEL[res.tier] || res.tier;
        // A tier change moves the cost — refresh any estimate on this card.
        if (card.querySelector(".imp-estimate")) paintOpenaiEstimate(card);
      } catch (err) {
        sel.value = prev;
        note.hidden = false;
        note.textContent = `✗ couldn't save extraction tier: ${err.message}`;
      }
    };
  });
  host.querySelectorAll(".card").forEach((card) => {
    if (card.querySelector(".imp-estimate")) paintOpenaiEstimate(card);
  });

  // — Downloads move-into-place (item 16) —
  const place = async (card, path, key, keepCopy, busyEl) => {
    const note = card.querySelector(".imp-note");
    note.hidden = false;
    note.textContent = keepCopy ? "copying into place…" : "moving into place…";
    if (busyEl) busyEl.disabled = true;
    const res = await apiPost("/v1/integrations/downloads/place",
      { path, key, keep_copy: keepCopy }).catch((e) => ({ error: e.message }));
    if (busyEl) busyEl.disabled = false;
    if (res?.error) { note.textContent = `✗ ${res.error}`; return false; }
    note.textContent = `✓ ${res.note}`;
    if (card.querySelector(".imp-estimate")) paintOpenaiEstimate(card);
    return true;
  };
  host.querySelectorAll(".imp-found-row .imp-place").forEach((btn) => {
    btn.onclick = async () => {
      const row = btn.closest(".imp-found-row");
      const card = btn.closest(".card");
      const keep = row.querySelector(".imp-keep")?.checked || false;
      if (await place(card, row.dataset.path, row.dataset.key, keep, btn)) row.remove();
    };
  });
  host.querySelectorAll(".imp-choose-toggle").forEach((a) => {
    a.onclick = (e) => {
      e.preventDefault();
      const chooser = a.closest(".card").querySelector(".imp-choose");
      chooser.hidden = !chooser.hidden;
      if (!chooser.hidden) chooser.querySelector(".imp-choose-path").focus();
    };
  });
  host.querySelectorAll(".imp-place-path").forEach((btn) => {
    btn.onclick = async () => {
      const card = btn.closest(".card");
      const chooser = card.querySelector(".imp-choose");
      const path = chooser.querySelector(".imp-choose-path").value.trim();
      if (!path) return;
      const keep = chooser.querySelector(".imp-keep").checked;
      if (await place(card, path, card.dataset.key, keep, btn)) chooser.hidden = true;
    };
  });
}

/* ————— mount ————— */

export async function mount(view, helpers) {
  ({ api, apiPost, confirmDanger } = helpers);
  // Per-source extraction tiers (Phase 3b). Best-effort: a null response
  // leaves every card at its own resolved default via extractRow's guards.
  // The Downloads scan (item 16) rides the same paint: found exports
  // render as import rows on their cards; a cloud node returns none and
  // the cards adapt (item 17 — upload is the primary path there).
  const [extraction, dl, imports] = await Promise.all([
    api("/v1/integrations/extraction").catch(() => null),
    api("/v1/integrations/downloads").catch(() => null),
    api("/v1/integrations/imports").catch(() => null),
  ]);
  const tiers = {};
  for (const s of (extraction?.sources || [])) tiers[s.source] = s;
  const found = dl?.found || [];
  const node = dl?.this_node || "local";
  view.innerHTML = `
    <span class="eyebrow">integrations</span>
    <h1 class="pagehead">The connections drawer</h1>
    <p class="subhead">Three things live here: the AI provider that powers thinking,
      companion systems, and every way to feed the record.</p>
    <div class="section-title">1 · AI provider</div>
    <div id="int-anthropic"><div class="loading">…</div></div>
    <div class="section-title" style="margin-top:var(--s5)">2 · companions</div>
    <div id="int-embeddings"></div>
    <div id="int-gmail"></div>
    <div id="int-pair"></div>
    <div id="int-hearthfs"><div class="loading">…</div></div>
    <div id="int-agents"><div class="loading">…</div></div>
    <div class="card"><p class="meta">
      <a href="#/peering">Peering</a> — your always-on counterpart ·
      <a href="#/network">Network</a> — friends' brains ·
      <a href="#/apikeys">API Keys</a> — tokens for anything else ·
      <a href="#/monitor">Monitoring</a> — health &amp; logs</p></div>
    <div class="section-title" style="margin-top:var(--s5)">3 · imports
      <span class="meta" style="font-weight:400">— exports in, episodes out; nothing leaves your machine</span></div>
    <div id="int-imports">${[...IMPORTS].sort((a, b) => a.name.localeCompare(b.name))
      .map((e, i) => importCard(e, i, tiers, found, node, imports)).join("")}</div>
    <div class="section-title" style="margin-top:var(--s5)">4 · embers
      <span class="meta" style="font-weight:400">— small companions that add new powers to your hearth</span></div>
    <div id="int-plugins"><div class="loading">…</div></div>`;
  paintPlugins(view.querySelector("#int-plugins")).catch(() => {});
  // Buttons FIRST, companion paints in the background: a slow (or
  // wedged-mount) HearthFS status call must never leave run-now/upload
  // dead — that exact chain froze the page on 2026-07-17.
  REMOUNT = () => mount(view, helpers);
  wireImports(view.querySelector("#int-imports"));
  // An import already in flight (or started elsewhere) keeps its card's
  // run button spinning across reloads; poll until it lands, then show
  // the fresh last-import line.
  const applyRunning = (now) => {
    const running = now?.running, interpreting = now?.interpreting;
    view.querySelectorAll(".imp-run").forEach((btn) => {
      const key = STATUS_KEY[btn.dataset.adapter] || btn.dataset.adapter;
      const importing = (running || []).includes(key);
      const interp = interpreting && interpreting[key];
      const active = importing || !!(interp && interp.pending);
      const label = importing ? "importing…"
        : interp && interp.pending
          ? (now?.llm_delegated || now?.llm_here === false)
            ? "interpreting on your always-on node…"
            : `interpreting… ~${interp.forecast_minutes}m`
          : "";
      if (active) {
        if (!btn.dataset.label) btn.dataset.label = btn.textContent;
        btn.disabled = true;
        btn.innerHTML = `<span class="spinner"></span> ${label}`;
      } else if (!active && btn.disabled && btn.dataset.label) {
        btn.disabled = false;
        btn.textContent = btn.dataset.label;
        delete btn.dataset.label;
      }
    });
    // The 🧠 status lines update in place — watching a counter that
    // only moves on a page reload reads as a stall (2026-07-19).
    view.querySelectorAll(".imp-interp[data-interp-key]").forEach((p) => {
      const interp = interpreting && interpreting[p.dataset.interpKey];
      if (interp) p.innerHTML = interpLine(interp, now, p.dataset.interpKey);
      else p.remove(); // done — the completion repaint below adds the ✓ line
    });
  };
  applyRunning(imports);
  // "try again" on a failed-interpretation warning (delegated handler —
  // the poll rewrites those paragraphs, so listeners can't live on them)
  view.addEventListener("click", async (ev) => {
    const link = ev.target.closest(".imp-retry-failed");
    if (!link) return;
    ev.preventDefault();
    link.textContent = "requeuing…";
    try {
      await apiPost("/v1/integrations/extraction/retry-failed", {
        source: link.dataset.source,
      });
      const now = await api("/v1/integrations/imports");
      applyRunning(now);
    } catch {
      link.textContent = "try again (failed — is the daemon up?)";
    }
  });
  if (imports?.running?.length || Object.keys(imports?.interpreting || {}).length) {
    const poll = setInterval(async () => {
      if (!document.contains(view)) { clearInterval(poll); return; }
      const now = await api("/v1/integrations/imports").catch(() => null);
      if (!now) return;
      applyRunning(now);
      if (!now.running.length && !Object.keys(now.interpreting || {}).length) {
        clearInterval(poll);
        // one clean repaint picks up the new last-import lines
        mount(view, helpers);
      }
    }, 5000);
  }
  paintAnthropic(view.querySelector("#int-anthropic")).catch(() => {});
  paintEmbeddings(view.querySelector("#int-embeddings")).catch(() => {});
  paintGmail(view.querySelector("#int-gmail")).catch(() => {});
  paintPair(view.querySelector("#int-pair"), { visitRule: true });
  paintHearthfs(view.querySelector("#int-hearthfs")).catch(() => {});
  paintAgentAccess(view.querySelector("#int-agents")).catch(() => {});
  return () => {};
}
