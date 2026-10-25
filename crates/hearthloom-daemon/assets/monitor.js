// monitor.js — ☰ → Monitoring: node-tagged health & observability.
// Banner says which node you're connected to; tabs switch between the
// local-device and cloud-based node's telemetry (default = where you
// are); sections: 24h health strips per check, vitals charts
// (chart-svg.js), and the paged/sortable system log.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&#39;", "'": "&#39;" }[c]));

let api;
const NODE_LABEL = { local: "local-device", cloud: "cloud-based" };
const CHECK_LABEL = {
  daemon: "Hearthloom itself",
  internet: "internet reachability",
  peer_node: "the peer node",
  anthropic_api: "Anthropic API",
};

function stripFor(buckets) {
  // 48 half-hour cells; color by ok-ratio; missing = quiet gap
  const byBucket = Object.fromEntries((buckets || []).map((b) => [b.bucket, b]));
  const cells = [];
  const now = new Date();
  for (let i = 47; i >= 0; i--) {
    const t = new Date(now.getTime() - i * 30 * 60000);
    const key = t.toISOString().slice(0, 15) + (t.getUTCMinutes() < 30 ? "0" : "3");
    const b = byBucket[key];
    const color = !b ? "var(--bg-raise)"
      : b.ok >= b.total ? "var(--ok, #7a9e7e)"
      : b.ok === 0 ? "var(--danger, #c25b4e)" : "#c9a15a";
    const title = !b ? "no data" : `${b.ok}/${b.total} ok`;
    cells.push(`<span title="${esc(key.replace('T',' '))}0 UTC — ${title}"
      style="flex:1; height:14px; background:${color}; border-radius:2px"></span>`);
  }
  return `<div style="display:flex; gap:2px">${cells.join("")}</div>`;
}

function chart(el, title, unit, rows, key) {
  if (typeof buildChartSVG !== "function" || !rows?.length) {
    el.innerHTML = `<p class="meta">${esc(title)}: not enough data yet</p>`;
    return;
  }
  el.innerHTML = buildChartSVG({
    type: "line", title: `${title} (${unit})`, xKey: "hour",
    series: [{ key, label: title }],
    data: rows.map((r) => ({ hour: r.hour.slice(5).replace("T", " "), [key]: r.value })),
  }) || "";
}

async function paintNode(host, node) {
  host.innerHTML = `<div class="loading">reading the gauges…</div>`;
  const h = await api(`/v1/monitor/health?node=${node}&hours=24`).catch(() => null);
  if (!h) { host.innerHTML = `<p class="meta">telemetry unavailable</p>`; return; }
  const checks = Object.keys(h.timeline || {});
  host.innerHTML = `
    <div class="card">
      <div class="section-title">health — last 24 hours</div>
      ${checks.length ? checks.map((c) => `
        <div style="margin:var(--s2) 0">
          <div class="meta" style="margin-bottom:2px">${esc(CHECK_LABEL[c] || c)}</div>
          ${stripFor(h.timeline[c])}
        </div>`).join("")
        : `<p class="meta">no health ticks recorded yet for this node — they arrive
           within a minute of the node running this version (and reach here on the
           next sync cycle for the counterpart).</p>`}
    </div>
    <div class="card">
      <div class="section-title">vitals</div>
      <div class="ask-chart" id="mon-cpu"></div>
      <div class="ask-chart" id="mon-mem"></div>
      <div class="ask-chart" id="mon-disk"></div>
    </div>
    <div class="card">
      <div class="section-title">system log</div>
      <div class="searchbar">
        <select id="log-level"><option value="">all levels</option>
          <option>info</option><option>warn</option><option>error</option></select>
        <select id="log-sort"><option value="ts">newest first</option>
          <option value="level">by level</option><option value="event">by event</option></select>
      </div>
      <div id="log-rows"><div class="loading">…</div></div>
      <div class="tend-actions">
        <button class="btn btn-quiet" id="log-prev">← newer</button>
        <span class="meta" id="log-page"></span>
        <button class="btn btn-quiet" id="log-next">older →</button>
      </div>
    </div>`;
  chart(host.querySelector("#mon-cpu"), "CPU", "%", h.cpu, "cpu");
  chart(host.querySelector("#mon-mem"), "Memory", "MB", h.mem, "mem");
  chart(host.querySelector("#mon-disk"), "Disk free", "GB", h.disk, "disk");

  let offset = 0;
  const LIMIT = 25;
  const loadLogs = async () => {
    const level = host.querySelector("#log-level").value;
    const sort = host.querySelector("#log-sort").value;
    const data = await api(`/v1/monitor/logs?node=${node}&limit=${LIMIT}&offset=${offset}` +
      (level ? `&level=${level}` : "") + `&sort=${sort}`).catch(() => ({ logs: [], total: 0 }));
    host.querySelector("#log-rows").innerHTML = (data.logs || []).map((l) => `
      <div class="pm-ep">
        <span class="meta">${esc(l.ts.slice(0, 16).replace("T", " "))}</span>
        <span class="kind${l.level === "error" ? "" : ""}"
          style="${l.level !== "info" ? "color:var(--danger,#c25b4e)" : ""}">${esc(l.level)}</span>
        <b>${esc(l.event)}</b> ${esc(l.message)}
      </div>`).join("") || `<p class="meta">no log entries.</p>`;
    host.querySelector("#log-page").textContent =
      `${offset + 1}–${Math.min(offset + LIMIT, data.total)} of ${data.total}`;
  };
  host.querySelector("#log-level").onchange = () => { offset = 0; loadLogs(); };
  host.querySelector("#log-sort").onchange = () => { offset = 0; loadLogs(); };
  host.querySelector("#log-prev").onclick = () => { offset = Math.max(0, offset - LIMIT); loadLogs(); };
  host.querySelector("#log-next").onclick = () => { offset += LIMIT; loadLogs(); };
  await loadLogs();
}


/* ————— LLM usage: the token/cost reconciliation view (v50 ledger) —————
   Covers ALL LLM spend — Ask rounds and every consolidation pass, both
   nodes once peered. Tokens are the stored truth; the daemon prices
   rows at read time (config llm_pricing overrides win). Vault-locked
   nodes 403 and this renders the unlock card instead. */
const fmtTok = (n) => Number(n || 0).toLocaleString();
const fmtUsd = (v) => (v == null ? "—" : `$${v < 0.01 ? v.toFixed(5) : v.toFixed(2)}`);

async function paintLlmUsage(host, days, node) {
  host.innerHTML = `<div class="loading">tallying the ledger…</div>`;
  let data;
  try {
    data = await api(`/v1/llm/usage?days=${days}${node ? `&node=${node}` : ""}`);
  } catch (err) {
    const locked = String(err.message || "").includes("vault locked");
    host.innerHTML = `<div class="card">${locked
      ? `<b>🔒 unlock the vault to view LLM usage</b>
         <p class="meta">Traces and spend patterns can reveal private activity, so
         accounting only opens while the vault is unlocked (masthead 🔒).</p>`
      : `<p class="meta">usage unavailable: ${esc(err.message)}</p>`}</div>`;
    return;
  }
  const sum = (rows, k) => rows.reduce((a, r) => a + (r[k] || 0), 0);
  const cost = (rows) => rows.some((r) => r.cost_usd != null)
    ? rows.reduce((a, r) => a + (r.cost_usd || 0), 0) : null;
  const roll = (rows) => {
    // collapse the model dimension for display; keep cost honest
    const by = new Map();
    for (const r of rows) {
      const g = by.get(r.key) || { key: r.key, calls: 0, input_tokens: 0,
        output_tokens: 0, cache_read_tokens: 0, errors: 0, cost: 0, priced: false, models: new Set() };
      g.calls += r.calls; g.input_tokens += r.input_tokens;
      g.output_tokens += r.output_tokens; g.cache_read_tokens += r.cache_read_tokens;
      g.errors += r.errors; g.models.add(r.model);
      if (r.cost_usd != null) { g.cost += r.cost_usd; g.priced = true; }
      by.set(r.key, g);
    }
    return [...by.values()];
  };
  const totalCost = cost(data.purposes);
  const totalIn = sum(data.purposes, "input_tokens");
  const totalOut = sum(data.purposes, "output_tokens");
  const totalCalls = sum(data.purposes, "calls");
  const dailyCost = roll(data.daily).map((d) => ({
    day: d.key.slice(5), cost: d.priced ? +d.cost.toFixed(4) : 0 }));
  const table = (rows, label) => `
    <table class="mon-usage-table">
      <thead><tr><th>${esc(label)}</th><th>calls</th><th>in</th><th>out</th><th>est. cost</th></tr></thead>
      <tbody>${rows.map((g) => `
        <tr><td>${esc(g.key)}<span class="meta"> ${esc([...g.models].join(", "))}</span></td>
          <td>${fmtTok(g.calls)}${g.errors ? ` <span style="color:var(--danger,#c25b4e)">(${g.errors}✗)</span>` : ""}</td>
          <td>${fmtTok(g.input_tokens)}</td><td>${fmtTok(g.output_tokens)}</td>
          <td>${g.priced ? fmtUsd(g.cost) : "no price on file"}</td></tr>`).join("")}
      </tbody>
    </table>`;
  host.innerHTML = `
    <div class="card">
      <div class="task-row" style="align-items:baseline">
        <div class="section-title" style="margin:0">totals — last ${days} days</div>
        <b>${fmtUsd(totalCost)}</b>
      </div>
      <p class="meta">${fmtTok(totalCalls)} API calls · ${fmtTok(totalIn)} input +
        ${fmtTok(totalOut)} output tokens · tokens are the recorded truth, dollar
        figures priced at read time (override via <code>llm_pricing</code> in config
        to reconcile against your provider's console).</p>
      <div class="ask-chart" id="llm-cost-chart"></div>
    </div>
    <div class="card"><div class="section-title">by purpose</div>
      ${data.purposes.length ? table(roll(data.purposes), "purpose") : `<p class="meta">no LLM calls recorded yet — rows appear as Ask and the consolidation passes run on 0.5.58+.</p>`}</div>
    <div class="card"><div class="section-title">by model</div>
      ${data.models.length ? table(roll(data.models), "provider · tier") : `<p class="meta">nothing yet.</p>`}</div>`;
  const chartEl = host.querySelector("#llm-cost-chart");
  if (typeof buildChartSVG === "function" && dailyCost.length > 1) {
    chartEl.innerHTML = buildChartSVG({
      type: "bar", title: "est. cost per day (USD)", xKey: "day",
      series: [{ key: "cost", label: "cost" }], data: dailyCost,
    }) || "";
  } else {
    chartEl.innerHTML = "";
  }
}

export async function mount(view, helpers) {
  ({ api } = helpers);
  const summary = await api("/v1/monitor/summary").catch(() => null);
  const here = summary?.this_node || "local";
  const tabs = ["local", "cloud"];
  view.innerHTML = `
    <span class="eyebrow">monitoring</span>
    <h1 class="pagehead">Health of the hearths</h1>
    <p class="subhead">You are connected to the <b>${esc(NODE_LABEL[here])}</b> node
      (v${esc(summary?.version || "?")}${summary?.paired
        ? `, peered with ${esc(summary.paired.name)}` : ", no peer configured"}).
      Both nodes' telemetry appears here once they've synced.</p>
    ${summary?.secrets?.plaintext_fallback ? `
      <div class="card" style="border-color:var(--danger,#c25b4e)">
        <b>⚠ secrets are unencrypted on disk</b>
        <p class="meta" style="margin:var(--s1) 0 0">No master key is available, so
          <code>~/.hearthloom/.env</code> sits in plaintext (dev mode). Set a Fly secret
          <code>HEARTHLOOM_MASTER_KEY</code> (cloud) or let the daemon create a login-keychain
          key (macOS) — the next boot seals the file and shreds the plaintext.</p>
      </div>` : ""}
    ${summary?.secrets?.migrated ? `
      <div class="card">
        <b>🔒 secrets sealed this boot</b>
        <p class="meta" style="margin:var(--s1) 0 0">The plaintext <code>.env</code> was
          encrypted into <code>.env.sealed</code> (master key: ${esc(summary.secrets.master_key)})
          and the plaintext was shredded.</p>
      </div>` : ""}
    <div class="section-title" style="margin-top:var(--s4)">LLM usage</div>
    <div class="searchbar" id="llm-scope">
      <button class="loom-chip" data-days="7">7d</button>
      <button class="loom-chip" data-days="30">30d</button>
      <button class="loom-chip resting" data-days="90">90d</button>
      <span style="flex:1"></span>
      <button class="loom-chip" data-lnode="">both nodes</button>
      <button class="loom-chip resting" data-lnode="local">local</button>
      <button class="loom-chip resting" data-lnode="cloud">cloud</button>
    </div>
    <div id="llm-usage-host"></div>
    <div class="section-title" style="margin-top:var(--s4)">node health</div>
    <div class="searchbar" id="mon-tabs">
      ${tabs.map((t) => `<button class="loom-chip${t === here ? "" : " resting"}"
        data-node="${t}">${NODE_LABEL[t]}</button>`).join("")}
    </div>
    <div id="mon-host"></div>`;
  const usageHost = view.querySelector("#llm-usage-host");
  let usageDays = 30, usageNode = "";
  const scope = view.querySelectorAll("#llm-scope .loom-chip");
  scope.forEach((b) => b.onclick = () => {
    if (b.dataset.days) {
      usageDays = +b.dataset.days;
      scope.forEach((x) => { if (x.dataset.days) x.classList.toggle("resting", x !== b); });
    } else {
      usageNode = b.dataset.lnode;
      scope.forEach((x) => { if (x.dataset.lnode != null) x.classList.toggle("resting", x !== b); });
    }
    paintLlmUsage(usageHost, usageDays, usageNode);
  });
  // default chips: 30d + both nodes lit
  scope.forEach((x) => {
    if (x.dataset.days === "30" || x.dataset.lnode === "") x.classList.remove("resting");
    else if (x.dataset.days === "7") x.classList.add("resting");
  });
  paintLlmUsage(usageHost, usageDays, usageNode);
  const hostEl = view.querySelector("#mon-host");
  const tabButtons = view.querySelectorAll("#mon-tabs .loom-chip");
  tabButtons.forEach((b) => b.onclick = () => {
    tabButtons.forEach((x) => x.classList.toggle("resting", x !== b));
    paintNode(hostEl, b.dataset.node);
  });
  await paintNode(hostEl, here);
  return () => {};
}
