// peering.js — the Peering page (☰ → Peering): pair this machine with
// an always-on counterpart, watch the sync reach completion (live,
// self-refreshing while open), and un-peer behind a styled confirm
// that revokes this machine's key on the counterpart.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&#39;", "'": "&#39;" }[c]));

let api, apiPost, confirmDanger;
const ACTOR = "human:dashboard";

function pct(cursor, head) {
  if (!head || head <= 0) return 0;
  return Math.min(100, Math.round((100 * cursor) / head));
}

// The server's per-category replication progress (episodes are only one
// slice of what moves — scopes and the desk/notes mirror weigh in by
// their actual item share). Absent on heartbeats from an older peer;
// callers fall back to the episodes-only numbers.
const CATEGORY_LABELS = [
  ["episodes", "memories"],
  ["scopes", "privacy labels"],
  ["operational", "desk & notes"],
];

function breakdownLine(prog) {
  const parts = CATEGORY_LABELS
    .filter(([k]) => prog.categories?.[k])
    .map(([k, label]) => {
      const c = prog.categories[k];
      return `${label} ${Number(c.done).toLocaleString()}/${Number(c.total).toLocaleString()}`;
    });
  return parts.join(" · ");
}

function progressTotal(prog) {
  return Object.values(prog?.categories || {})
    .reduce((n, c) => n + (Number(c.total) || 0), 0);
}

function statusPanel(s) {
  const prog = s.progress;
  const p = prog ? prog.total_pct : pct(s.push_cursor, s.local_head);
  const detail = prog
    ? progressTotal(prog) === 0
      ? "nothing to sync yet"
      : breakdownLine(prog)
    : `${esc(String(s.push_cursor))} of ${esc(String(s.local_head))} memories`;
  const last = s.last;
  return `
    <div class="card">
      <div class="task-row" style="align-items:baseline">
        <b>${esc(s.paired.name)}</b>
        ${s.peer_version ? `<span class="task-tag plain-tag">running v${esc(s.peer_version)}</span>` : ""}
        <span class="meta">${esc(s.paired.url)}</span>
        ${!s.key_present ? `<span class="statchip">⚠ key missing — re-pair</span>` : ""}
      </div>
      ${s.paused ? `<div class="meta" style="margin:var(--s2) 0">⏸ <b>replication is paused</b> —
        nothing moves in either direction until you resume (the peer knows too).</div>` : ""}
      ${s.llm_configured === false ? `<div class="meta" style="margin:var(--s2) 0; color:var(--danger,#c25b4e)">
        ⚠ this node has no AI key yet.
        ${s.peer_llm_configured
          ? "Your peer holds one — it arrives here automatically (sealed) within a sync cycle; nothing to do."
          : `Neither node has one — <a href="#/integrations">set it once under Integrations → AI provider</a>; it syncs to your peer automatically.`}</div>` : ""}
      ${s.llm_configured && s.peer_llm_configured === false ? `<div class="meta" style="margin:var(--s2) 0">
        ○ the peer has no AI key yet — yours is delivered to it automatically (sealed to the
        peer's own keypair) on the next sync cycle; no action needed.</div>` : ""}
      <div style="margin:var(--s3) 0">
        <div class="meta">sending this record to the peer</div>
        <div style="background:var(--bg-raise); border-radius:var(--radius); height:10px; overflow:hidden; margin:4px 0">
          <div style="width:${p}%; height:100%; background:var(--accent); transition:width .6s"></div>
        </div>
        <div class="meta">${p}% of everything to replicate
          ${p >= 100 ? " · fully caught up ✓" : " · syncing…"}</div>
        <div class="meta">${detail}</div>
      </div>
      <div class="meta">
        ${last
          ? last.ok
            ? `last cycle ${esc((last.ts || "").slice(0, 16).replace("T", " "))} —
               sent ${last.pushed}, received ${last.pulled_new} new (${last.took_ms}ms)`
            : `last cycle failed: ${esc(String(last.error || "")).slice(0, 100)} — will retry on schedule`
          : "no cycle has completed yet"}
      </div>
      ${s.bus ? `<div class="meta">push bus:
        ${s.bus.subscriber_connected
          ? `live — changes move within seconds${s.bus.last_event ? ` (last hint ${esc(s.bus.last_event.slice(0, 16).replace("T", " "))})` : ""}`
          : s.bus.peer_supported === false
            ? "peer doesn't speak it yet — on the 5-minute schedule"
            : "connecting… (the 5-minute schedule still reconciles everything)"}
      </div>` : ""}
      <div class="tend-actions" style="margin-top:var(--s3)">
        ${s.paused
          ? `<button class="btn" id="peer-resume">▶ resume replication</button>`
          : `<button class="btn" id="peer-sync-now">sync now</button>
             <button class="btn btn-quiet" id="peer-pause">⏸ pause replication</button>`}
        <button class="btn btn-quiet" id="peer-unpair">un-peer…</button>
        <button class="btn btn-quiet" id="peer-full"
          title="walk the whole record from the beginning and offer every item to the peer — safe and idempotent">⟳ re-offer everything</button>
      </div>
      <p class="meta" id="peer-note" hidden></p>
    </div>
    <p class="meta">New memories — and privacy changes — flow both ways on a 5-minute schedule
      while both nodes are up. Your peer is a full copy of this record, private included: both
      sides keep it locked behind the vault and your sign-in, and marking something private on
      either side takes effect on both. An offline peer is skipped and retried — nothing here
      ever blocks the rest of Hearthloom.</p>`;
}

function pairPanel() {
  return `
    <div class="card">
      <div class="section-title">how to peer this machine with a cloud instance</div>
      <ol class="meta" style="line-height:1.9; padding-left:1.2em">
        <li><b>Stand up the counterpart.</b> On your own server:
          <code>./scripts/bootstrap.sh --remote</code> (binds publicly; put TLS in front).
          On Fly.io: the repo's <code>Dockerfile</code> + <code>fly.toml</code> are a working
          recipe — <code>fly deploy</code>, then bootstrap it from its own dashboard.</li>
        <li><b>Mint this machine a key over there.</b> On the counterpart's dashboard:
          ☰ → API Keys → name it after this machine, scope <b>peer</b>, mint. Copy the
          <code>hl_…</code> secret — it's shown once.</li>
        <li><b>Pair below.</b> The peer is probed before anything is saved.</li>
      </ol>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="peer-name" placeholder="name it — e.g. cloud" style="flex:0.5">
        <input id="peer-url" placeholder="https://your-node.example.com" style="flex:1.2">
        <input id="peer-key" placeholder="key minted on THAT node (hl_…)" style="flex:1.2">
        <button class="btn" id="peer-pair">pair</button>
      </div>
      <p class="meta" id="peer-note" hidden></p>
    </div>
    <p class="meta">Peering keeps an always-on full copy of this record — locked behind the
      same vault and sign-in — and pulls back whatever the counterpart captures. This machine
      stays the authoritative home of your record.</p>`;
}

function inboundPanel(inb) {
  // an older peer's heartbeat has no progress object — show its
  // episodes-only numbers rather than inventing categories.
  const prog = inb.progress && inb.progress.categories ? inb.progress : null;
  const p = prog ? prog.total_pct : pct(inb.push_cursor, inb.head);
  const detail = prog
    ? progressTotal(prog) === 0
      ? "nothing to sync yet"
      : breakdownLine(prog)
    : `${esc(String(inb.push_cursor))} of ${esc(String(inb.head))} memories`;
  const age = inb.last_contact ? inb.last_contact.slice(0, 16).replace("T", " ") : "?";
  const ver = inb.version ? ` · running v${esc(inb.version)}` : "";
  return `
    <div class="card">
      <div class="task-row" style="align-items:baseline">
        <b>⇣ receiving from ${esc(inb.hostname || "?")}${ver}</b>
        <span class="task-tag plain-tag">key: ${esc(inb.key_name || "?")}</span>
        ${inb.paused ? `<span class="statchip">⏸ paused on their side</span>` : ""}
        ${inb.llm_configured === false ? `<span class="statchip">⚠ no AI key over there</span>` : ""}
      </div>
      <div style="margin:var(--s3) 0">
        <div class="meta">their record arriving here</div>
        <div style="background:var(--bg-raise); border-radius:var(--radius); height:10px; overflow:hidden; margin:4px 0">
          <div style="width:${p}%; height:100%; background:var(--accent); transition:width .6s"></div>
        </div>
        <div class="meta">${p}% of everything to replicate
          ${p >= 100 ? " · fully caught up ✓" : " · syncing…"}</div>
        <div class="meta">${detail} · last contact ${esc(age)} UTC</div>
      </div>
    </div>`;
}

async function paint(host) {
  const s = await api("/v1/peer").catch(() => null);
  if (!s) { host.innerHTML = `<p class="meta">peering status unavailable</p>`; return null; }
  const inbound = s.inbound && s.inbound.hostname ? inboundPanel(s.inbound) : "";
  // a node that's RECEIVING (or outward-paired) must not re-pitch the
  // how-to-pair/mint-keys instructions — that reads as "set up again"
  host.innerHTML = inbound + (s.paired
    ? statusPanel(s)
    : inbound
      ? `<p class="meta">This node is the receiving end of a pairing — the machine above
          initiates every sync. To pair a DIFFERENT machine outward from here, un-peer
          there first; nothing needs setting up on this side.</p>`
      : pairPanel());
  const note = host.querySelector("#peer-note");
  host.querySelector("#peer-pair")?.addEventListener("click", async () => {
    note.hidden = false; note.textContent = "checking the peer…";
    const res = await apiPost("/v1/peer", {
      actor: ACTOR,
      name: host.querySelector("#peer-name").value.trim() || "peer",
      url: host.querySelector("#peer-url").value.trim(),
      key: host.querySelector("#peer-key").value.trim(),
    }).catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    paint(host);
  });
  host.querySelector("#peer-sync-now")?.addEventListener("click", async () => {
    note.hidden = false; note.textContent = "cycle started…";
    apiPost("/v1/peer/sync-now", { actor: ACTOR }).catch(() => {});
    setTimeout(() => paint(host), 1500);
  });
  const setPause = async (paused) => {
    note.hidden = false; note.textContent = paused ? "pausing…" : "resuming…";
    const res = await apiPost("/v1/peer/pause", { actor: ACTOR, paused })
      .catch((e) => ({ error: e.message }));
    if (res?.error) { note.textContent = `✗ ${res.error}`; return; }
    if (res.peer_informed === false) {
      note.textContent = "saved here; the peer is unreachable right now — it'll learn the state when it's back.";
      setTimeout(() => paint(host), 1800);
      return;
    }
    paint(host);
  };
  host.querySelector("#peer-pause")?.addEventListener("click", () => setPause(true));
  host.querySelector("#peer-resume")?.addEventListener("click", () => setPause(false));
  host.querySelector("#peer-full")?.addEventListener("click", async () => {
    const yes = await confirmDanger({
      title: "Re-offer everything to " + s.paired.name + "?",
      body: "This walks your whole record from the beginning and offers every item to the " +
        "peer. It's completely safe — anything the peer already has is skipped — but on a " +
        "big record the first pass can take a long time and real bandwidth. Progress shows " +
        "in the bar above and continues on the normal schedule even if you leave this page.",
      action: "re-offer everything",
    });
    if (!yes) return;
    const btn = host.querySelector("#peer-full");
    btn.disabled = true;
    btn.innerHTML = `<span class="spinner"></span> re-offering…`;
    note.hidden = false;
    note.textContent = "full cycle running — watch the bar above; it keeps going on schedule if you navigate away…";
    const res = await apiPost("/v1/peer/sync-now", { actor: ACTOR, full: true })
      .catch((err) => ({ error: err.message }));
    note.textContent = res?.error
      ? `✗ ${res.error} — progress made so far is kept; the schedule resumes from there`
      : "✓ full cycle finished — the bar above tells the truth now";
    paint(host);
  });
  host.querySelector("#peer-unpair")?.addEventListener("click", async () => {
    // confirmDanger's contract: `action` is the BUTTON LABEL; it
    // resolves truthy on confirm (passing a function here once rendered
    // its source as the button text and did nothing — 2026-07-18)
    const yes = await confirmDanger({
      title: "Un-peer from " + s.paired.name + "?",
      body: "This revokes the key this machine uses to reach the peer — syncing stops " +
        "immediately and re-pairing will need a NEW key minted over there. " +
        "Nothing already synced is deleted on either side.",
      action: "un-peer",
    });
    if (!yes) return;
    const btn = host.querySelector("#peer-unpair");
    btn.disabled = true;
    btn.textContent = "un-peering…";
    note.hidden = false;
    note.textContent = "un-peering — retiring this machine's key on the peer first \
(a few seconds if it's unreachable)…";
    const res = await fetch("/v1/peer", {
      method: "DELETE",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ actor: ACTOR }),
    }).then((r) => r.json()).catch(() => ({}));
    if (res && res.unpaired) {
      note.textContent = res.remote_key_revoked === false
        ? "✓ un-peered. ⚠ the peer was unreachable — its copy of the key still exists; " +
          "revoke it from the peer's API Keys page when it's back."
        : "✓ un-peered.";
    } else {
      note.textContent = `✗ un-peer failed: ${(res && res.error) || "no response"}`;
      btn.disabled = false;
      btn.textContent = "un-peer…";
      return;
    }
    paint(host);
  });
  return s;
}

/* — every node (multi-node M7): name, kind, roles, last seen; and this
   node's own "thinking" role — on / off / auto (one live auto node does
   the paid thinking passes; another takes over if it goes quiet). — */
const THINKING = {
  on: "does the thinking",
  off: "leaves the thinking to others",
  auto: "takes turns at the thinking",
};
async function paintNodes(el) {
  let d;
  try { d = await api("/v1/nodes"); } catch (err) { el.innerHTML = `<p class="meta">${esc(err.message)}</p>`; return; }
  const ago = (iso) => {
    const mins = Math.round((Date.now() - Date.parse(iso)) / 60000);
    return mins < 2 ? "just now" : mins < 120 ? `${mins} min ago` : `${Math.round(mins / 60)} h ago`;
  };
  const thinkers = d.nodes.filter((n) => n.roles?.thinking === "on").length;
  el.innerHTML = `
    ${d.nodes.map((n) => {
      const me = n.node_id === d.this;
      const quiet = Date.now() - Date.parse(n.last_seen) > 20 * 60000;
      return `<div class="task-row" style="align-items:baseline; flex-wrap:wrap; padding:4px 0">
        <b>${esc(n.name)}</b>${me ? `<span class="task-tag plain-tag">this machine</span>` : ""}
        <span class="meta">${n.kind === "cloud" ? "always-on (cloud)" : "computer"} · v${esc(n.version || "?")}
          · ${esc(THINKING[n.roles?.thinking] || "")}${n.roles?.thinking === "auto" && n.node_id === d.thinking_holder ? " (its turn now)" : ""}
          · ${n.roles?.agents ? `${n.roles.agents} agent${n.roles.agents === 1 ? "" : "s"}` : "no agents"}
          · <span${quiet ? ' style="color:var(--accent)"' : ""}>seen ${esc(ago(n.last_seen))}</span></span></div>`;
    }).join("") || `<p class="meta">Only this machine so far.</p>`}
    <div class="searchbar" style="flex-wrap:wrap; margin-top:var(--s3)">
      <span class="meta">This machine</span>
      <select id="node-thinking">${Object.entries(THINKING).map(([k, v]) =>
        `<option value="${k}" ${d.nodes.find((n) => n.node_id === d.this)?.roles?.thinking === k ? "selected" : ""}>${esc(v)}</option>`).join("")}</select>
      <span class="meta" id="node-note">${thinkers > 1 ? `${thinkers} machines all do the thinking — that pays for it ${thinkers} times; one is enough.` : ""}</span>
    </div>`;
  el.querySelector("#node-thinking").onchange = async (e) => {
    try { await apiPost("/v1/nodes", { thinking: e.target.value }); } catch (err) {
      el.querySelector("#node-note").textContent = `✗ ${err.message}`; return;
    }
    paintNodes(el);
  };
}

export async function mount(view, helpers) {
  ({ api, apiPost, confirmDanger } = helpers);
  view.innerHTML = `
    <span class="eyebrow">peering</span>
    <h1 class="pagehead">The other hearth</h1>
    <p class="subhead">An always-on counterpart — your own server or a cloud node — that keeps
      a copy of this record and gathers while this machine sleeps.</p>
    <div id="peering-host"><div class="loading">…</div></div>
    <div class="section-title" style="margin-top:var(--s5)">all your nodes
      <span class="meta" style="font-weight:400">— every machine running Hearthloom for you, as of its last sync</span></div>
    <div id="peering-nodes" class="card"><div class="loading">…</div></div>
    <div class="section-title" style="margin-top:var(--s5)">node health
      <a class="meta desk-panel-link" href="#/monitor">full monitoring →</a></div>
    <div id="peering-health" class="card"><div class="loading">…</div></div>`;
  const host = view.querySelector("#peering-host");
  paintNodes(view.querySelector("#peering-nodes"));
  // compact 24h strip for THIS node (full page under ☰ → Monitoring)
  (async () => {
    const el = view.querySelector("#peering-health");
    const sum = await api("/v1/monitor/summary").catch(() => null);
    const node = sum?.this_node || "local";
    const h = await api(`/v1/monitor/health?node=${node}&hours=24`).catch(() => null);
    if (!h) { el.innerHTML = `<p class="meta">telemetry unavailable</p>`; return; }
    const checks = Object.keys(h.timeline || {});
    el.innerHTML = checks.length
      ? checks.map((c) => {
          const buckets = h.timeline[c];
          const last = buckets[buckets.length - 1];
          const ok = last && last.ok >= last.total;
          return `<div class="meta" style="margin:2px 0">${ok ? "●" : "○"} ${esc(c)} —
            ${ok ? "healthy" : "degraded"} (${esc(String(last?.ok ?? 0))}/${esc(String(last?.total ?? 0))} recent checks ok)</div>`;
        }).join("")
      : `<p class="meta">health ticks start within a minute of the daemon running.</p>`;
  })();
  const first = await paint(host);
  // Live while open: refresh so a running first sync visibly climbs.
  // Repaint-in-place only when nothing is focused (don't eat a paste).
  const timer = setInterval(async () => {
    if (!document.body.contains(host)) { clearInterval(timer); return; }
    if (host.contains(document.activeElement) &&
        document.activeElement?.tagName === "INPUT") return;
    if ((first || !host.querySelector("#peer-key"))) await paint(host);
  }, 5000);
  return () => clearInterval(timer);
}
