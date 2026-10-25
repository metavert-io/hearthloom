// agents.js — ☰ → Agents: the hands Hearthloom can hand work to, and the
// rules they work under (v55; the design log 2026-10-17).
//
// Two layers of access, by design:
// - what waits for your OK (approvals) needs only you — signed in, or your
//   phone — so approving never means unlocking the vault (decision A);
// - how agents are set up (bridges, keys, rules, the activity log) needs
//   the vault open, even just to look.
// Everything here talks to /v1/agents*; the server enforces both layers.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

let api, apiPost, confirmDanger;

export const CLASS_LABEL = {
  read: "Look things up",
  write_record: "Add tasks & notes to Hearthloom",
  communicate: "Send email & messages",
  schedule: "Change your calendar",
  spend: "Spend money",
  files: "Change files & run commands",
  web: "Use websites",
  friends: "Talk to friends' Hearthlooms",
  other: "Anything else",
};
const MODE_LABEL = { auto: "do it", ask: "ask me first", never: "never" };
export const DEPTH = {
  full: ["full control", "every action it wants to take comes back here for a decision"],
  partial: ["partial control", "Hearthloom decides whether a task is sent, and the commands it flags as dangerous"],
  "dispatch-only": ["sends only", "Hearthloom decides whether a task is sent; after that, the agent's own settings apply"],
};
const GOALS = [
  { key: "files", label: "Work on files, research, write things", classes: ["read", "files", "web"] },
  { key: "communicate", label: "Send email or messages for me", classes: ["communicate"] },
  { key: "schedule", label: "Manage my calendar", classes: ["schedule"] },
  { key: "web", label: "Do things on websites", classes: ["web"] },
];

let data = null;
let view = null;
const ui = { adding: null, openAgent: null, goal: [] };

const depthBadge = (depth) => depth && DEPTH[depth]
  ? `<span class="agent-depth" data-depth="${esc(depth)}" title="${esc(DEPTH[depth][1])}">${esc(DEPTH[depth][0])}</span>` : "";

/* ————— waiting for your OK ————— */
export function approvalRows(approvals, { compact = false } = {}) {
  if (!approvals.length) return compact ? "" : `<p class="meta">Nothing is waiting for you.</p>`;
  return approvals.map((a) => `
    <div class="agent-approval" data-approval="${esc(a.id)}">
      <div><b>${esc(a.kind === "dispatch" ? "Start this task?" : "Allow this action?")}</b>
        ${a.class ? `<span class="task-tag plain-tag">${esc(CLASS_LABEL[a.class] || a.class)}</span>` : ""}</div>
      <div class="meta">${esc(a.summary)}</div>
      ${a.detail?.reason ? `<div class="meta agent-why">${esc(a.detail.reason)}</div>` : ""}
      <div class="tend-actions">
        <button class="btn agent-approve" data-yes="1">approve</button>
        <button class="btn btn-quiet agent-approve" data-yes="0">don't</button>
      </div>
    </div>`).join("");
}

export function wireApprovals(host, apiPostFn, after) {
  host.querySelectorAll(".agent-approve").forEach((btn) => {
    btn.onclick = async () => {
      const row = btn.closest("[data-approval]");
      row.querySelectorAll("button").forEach((b) => { b.disabled = true; });
      try {
        await apiPostFn(`/v1/agents/approvals/${encodeURIComponent(row.dataset.approval)}`,
          { approve: btn.dataset.yes === "1" });
      } catch (err) {
        row.insertAdjacentHTML("beforeend", `<p class="meta">✗ ${esc(err.message)}</p>`);
        return;
      }
      after?.();
    };
  });
}

/* ————— the page ————— */
async function load() {
  try {
    data = await api("/v1/agents");
  } catch (err) {
    view.querySelector("#agents-body").innerHTML = `<div class="card"><p class="meta">${esc(err.message)}</p></div>`;
    return;
  }
  paint();
}

function paint() {
  const d = data;
  const body = view.querySelector("#agents-body");
  const waiting = `<div class="section-title">waiting for your OK</div>
    <div id="agents-approvals">${approvalRows(d.approvals || [])}</div>`;
  if (d.locked) {
    body.innerHTML = `${waiting}
      <div class="card" style="margin-top:var(--s5)"><b>🔒 Unlock the vault to see or change your agents</b>
        <p class="meta">How your agents are set up — what they can reach, and what they may do without
        asking — is kept behind the vault (the 🔒 in the top bar). Approving what's waiting above doesn't need it.</p></div>`;
    wireApprovals(body, apiPost, load);
    return;
  }
  const elsewhere = (d.elsewhere || []).map((e) => `
    <div class="card" data-elsewhere="${esc(e.id)}"><div class="task-row" style="align-items:baseline; flex-wrap:wrap">
      <b>${esc(e.name)}</b><span class="meta">${esc((d.catalog.find((c) => c.id === e.harness) || {}).label || e.harness)}
        on ${esc(e.node)}</span>${depthBadge(e.depth)}
      ${d.peer ? `<button class="btn btn-quiet agent-move-here">move it to this machine</button>` : ""}</div>
      <p class="meta agent-status" hidden></p></div>`).join("");
  body.innerHTML = `${waiting}
    <div class="section-title" style="margin-top:var(--s5)">your agents
      <span class="meta" style="font-weight:400">— on this machine, <b id="agent-node-name">${esc(d.node?.name || "")}</b>
      <button class="linkish" id="agent-node-rename">rename</button></span></div>
    ${(d.agents || []).map(agentCard).join("") || `<p class="meta">No agents on this machine yet. Until you add one,
      Ask answers questions and explains what an agent would need when you ask it to do something.</p>`}
    ${elsewhere ? `<div class="section-title" style="margin-top:var(--s4)">on your other machines</div>${elsewhere}` : ""}
    ${addAgent()}
    <div class="section-title" style="margin-top:var(--s5)">rules for every agent</div>
    ${rules()}
    <div class="section-title" style="margin-top:var(--s5)">activity</div>
    ${activity()}`;
  wire(body);
}

function agentCard(entry) {
  const a = entry.agent;
  const h = data.catalog.find((c) => c.id === a.harness) || { label: a.harness };
  const open = ui.openAgent === a.id;
  const cfg = a.config || {};
  const field = (key, label, placeholder, type = "text") => `
    <label class="meta agent-field">${esc(label)}
      <input class="agent-cfg" data-key="${key}" type="${type}" value="${esc(cfg[key] ?? "")}" placeholder="${esc(placeholder)}"></label>`;
  const fields = a.harness === "claude_code"
    ? field("workdir", "working folder", "/Users/you/agent-work") + field("model", "model (optional)", "default")
      + field("timeout_secs", "time limit, seconds", "1800", "number")
    : field("url", a.harness === "hermes" ? "API server address" : "gateway address",
        a.harness === "hermes" ? "http://127.0.0.1:8642" : "http://127.0.0.1:18789")
      + (a.harness === "openclaw" ? field("agent_id", "OpenClaw agent id (optional)", "main") : field("model", "model (optional)", "hermes-agent"))
      + field("timeout_secs", "time limit, seconds", "1800", "number");
  const overrides = entry.overrides?.modes || {};
  return `
  <div class="card" data-agent="${esc(a.id)}">
    <div class="task-row" style="align-items:baseline; flex-wrap:wrap">
      <b>${esc(a.name)}</b><span class="meta">${esc(h.label)}</span>${depthBadge(entry.depth)}
      <span class="meta">${a.enabled ? "on" : "off"} · ${entry.has_key ? "reads your record" : "no record access"}${
        a.harness !== "claude_code" ? (entry.has_token ? " · token saved" : " · no token yet") : ""}</span>
      <button class="btn btn-quiet agent-open">${open ? "close" : "set up…"}</button>
    </div>
    ${open ? `
    <div class="agent-panel">
      <div class="searchbar" style="flex-wrap:wrap; align-items:end">
        <label class="meta agent-field">name <input class="agent-name" value="${esc(a.name)}" maxlength="60"></label>
        ${fields}
        <label class="meta"><input type="checkbox" class="agent-enabled" ${a.enabled ? "checked" : ""}> on</label>
        <button class="btn agent-save">save</button>
        <button class="btn btn-quiet agent-test">test the connection</button>
      </div>
      <p class="meta agent-status" hidden></p>
      <div class="meta" style="margin-top:var(--s3)"><b>What it can read of your record</b> — its own key: read-only, never
        anything private, and only these areas:</div>
      <div class="searchbar" style="flex-wrap:wrap">
        ${(data.areas || []).map((x) => `<label class="meta"><input type="checkbox" class="agent-area" value="${esc(x)}" checked> ${esc(x)}</label>`).join("")}
        <button class="btn btn-quiet agent-mint">${entry.has_key ? "replace its key" : "give it read access"}</button>
      </div>
      ${a.harness !== "claude_code" ? `
      <div class="searchbar" style="flex-wrap:wrap; margin-top:var(--s2)">
        <label class="meta agent-field">${a.harness === "hermes" ? "API key" : "webhook token"}
          <input class="agent-token" type="password" autocomplete="off" placeholder="${entry.has_token ? "saved — paste to replace" : "paste it here"}"></label>
        <button class="btn btn-quiet agent-token-save">save token</button>
        ${a.harness === "hermes" ? `<button class="btn btn-quiet agent-hook">approval hook script…</button>` : ""}
      </div>
      <pre class="agent-script" hidden></pre>` : ""}
      <div class="meta" style="margin-top:var(--s3)"><b>Its own rules</b> — anything left on "same as all agents" follows the rules below.</div>
      <div class="agent-matrix">${data.classes.map((cls) => `
        <label class="meta agent-mrow"><span>${esc(CLASS_LABEL[cls] || cls)}</span>
          <select class="agent-override" data-class="${esc(cls)}">
            <option value="">same as all agents</option>
            ${["auto", "ask", "never"].map((m) => `<option value="${m}" ${overrides[cls] === m ? "selected" : ""}>${MODE_LABEL[m]}</option>`).join("")}
          </select></label>`).join("")}</div>
      <div class="tend-actions">
        <button class="btn agent-override-save">save its rules</button>
        ${data.peer ? `<button class="btn btn-quiet agent-move">move to ${esc(data.peer.name)}…</button>` : ""}
        <button class="btn btn-quiet agent-delete">remove this agent…</button>
      </div>
    </div>` : ""}
  </div>`;
}

function addAgent() {
  const goalBoxes = GOALS.map((g) => `<label class="meta"><input type="checkbox" class="agent-goal" value="${g.key}"
    ${ui.goal.includes(g.key) ? "checked" : ""}> ${esc(g.label)}</label>`).join("");
  const classes = [...new Set(GOALS.filter((g) => ui.goal.includes(g.key)).flatMap((g) => g.classes))];
  const fits = data.catalog.filter((h) => classes.length && classes.every((c) => h.good_for.includes(c)));
  const pick = ui.adding || fits[0]?.id || null;
  return `
  <div class="card">
    <b>Add an agent</b>
    <p class="meta">What do you want agents to do? Hearthloom suggests a fit, and starts every agent on "ask me first".</p>
    <div class="searchbar" style="flex-wrap:wrap">${goalBoxes}</div>
    <div class="agent-catalog">${data.catalog.map((h) => `
      <button class="agent-choice${pick === h.id ? " on" : ""}" data-harness="${esc(h.id)}">
        <b>${esc(h.label)}</b> ${depthBadge(h.depth)}
        ${fits.some((f) => f.id === h.id) ? `<span class="task-tag plain-tag">fits</span>` : ""}
        <span class="meta">${esc(h.blurb)}</span><span class="meta">runs ${esc(h.runs)}</span></button>`).join("")}</div>
    ${pick ? `<div class="searchbar" style="margin-top:var(--s2)">
      <input id="agent-new-name" placeholder="name it — e.g. ${pick === "claude_code" ? "Desk hand" : "Assistant"}" maxlength="60">
      <button class="btn" id="agent-create">add ${esc(data.catalog.find((h) => h.id === pick)?.label || "")}</button></div>` : ""}
    <p class="meta" id="agent-add-note" hidden></p>
  </div>`;
}

function rules() {
  const p = data.policy;
  return `
  <div class="card">
    <div class="searchbar" style="flex-wrap:wrap">
      <span class="meta">start from</span>
      ${data.presets.map((x) => `<button class="btn btn-quiet agent-preset" data-preset="${esc(x.name)}">${esc(x.name)}</button>`).join("")}
    </div>
    <div class="agent-matrix">${data.classes.map((cls) => `
      <div class="agent-mrow"><span>${esc(CLASS_LABEL[cls] || cls)}</span>
        <span class="spend-segs" role="group">${["auto", "ask", "never"].map((m) => `
          <button class="spend-seg agent-mode${p.modes[cls] === m ? " on" : ""}" data-class="${esc(cls)}" data-mode="${m}">${MODE_LABEL[m]}</button>`).join("")}
        </span></div>`).join("")}</div>
    <label class="meta" style="display:block; margin-top:var(--s3)">
      <input type="checkbox" id="agent-taint" ${p.taint_guard ? "checked" : ""}>
      After a task has read things you didn't write (email, web pages, imports), always ask before it does
      anything — even where it's set to "do it".</label>
    <div class="searchbar" style="flex-wrap:wrap; margin-top:var(--s2)">
      <label class="meta">at most <input id="agent-per-day" type="number" min="1" max="1000" value="${p.tasks_per_day}" style="width:5em"> tasks a day</label>
      <label class="meta">at most <input id="agent-per-task" type="number" min="1" max="10000" value="${p.actions_per_task}" style="width:5em"> actions per task</label>
      <button class="btn" id="agent-rules-save">save the rules</button>
    </div>
    <p class="meta" id="agent-rules-note" hidden></p>
  </div>`;
}

function activity() {
  const rows = (data.tasks || []).map((t) => {
    const agent = (data.agents || []).find((e) => e.agent.id === t.agent_id)?.agent;
    const live = ["proposed", "queued", "running"].includes(t.status);
    return `<tr><td>${esc(t.created_at.slice(0, 16).replace("T", " "))}</td>
      <td>${esc(t.request)}</td><td>${esc(agent?.name || "—")}</td>
      <td><span class="agent-status-tag" data-status="${esc(t.status)}">${esc(t.status)}</span></td>
      <td class="meta">${esc((t.result || t.error || "").slice(0, 200))}</td>
      <td>${live ? `<button class="linkish agent-cancel" data-task="${esc(t.id)}">stop</button>` : ""}</td></tr>`;
  }).join("");
  return rows
    ? `<div class="spend-scroll"><table class="spend-table agent-activity"><thead><tr><th>when</th><th>asked</th><th>agent</th>
        <th>status</th><th>outcome</th><th></th></tr></thead><tbody>${rows}</tbody></table></div>`
    : `<p class="meta">No tasks yet.</p>`;
}

function wire(body) {
  wireApprovals(body, apiPost, load);
  const note = (el, text) => { if (el) { el.hidden = false; el.textContent = text; } };
  const post = async (path, payload, noteEl) => {
    try { return await apiPost(path, payload); } catch (err) { note(noteEl, `✗ ${err.message}`); return null; }
  };

  body.querySelectorAll(".agent-goal").forEach((box) => {
    box.onchange = () => {
      ui.goal = [...body.querySelectorAll(".agent-goal:checked")].map((b) => b.value);
      ui.adding = null;
      paint();
    };
  });
  body.querySelectorAll(".agent-choice").forEach((b) => { b.onclick = () => { ui.adding = b.dataset.harness; paint(); }; });
  const create = body.querySelector("#agent-create");
  if (create) create.onclick = async () => {
    const harness = body.querySelector(".agent-choice.on")?.dataset.harness;
    const name = body.querySelector("#agent-new-name").value.trim() || data.catalog.find((h) => h.id === harness)?.label;
    const res = await post("/v1/agents", { action: "save", agent: { name, harness, config: {} } }, body.querySelector("#agent-add-note"));
    if (res) { ui.openAgent = res.agent.id; ui.adding = null; ui.goal = []; load(); }
  };

  body.querySelectorAll("[data-agent]").forEach((card) => {
    const id = card.dataset.agent;
    const status = card.querySelector(".agent-status");
    card.querySelector(".agent-open").onclick = () => { ui.openAgent = ui.openAgent === id ? null : id; paint(); };
    if (ui.openAgent !== id) return;
    const entry = data.agents.find((e) => e.agent.id === id);
    card.querySelector(".agent-save").onclick = async () => {
      const config = {};
      card.querySelectorAll(".agent-cfg").forEach((i) => {
        if (i.value.trim()) config[i.dataset.key] = i.type === "number" ? Number(i.value) : i.value.trim();
      });
      const res = await post("/v1/agents", { action: "save", id, agent: {
        name: card.querySelector(".agent-name").value, harness: entry.agent.harness, config,
        enabled: card.querySelector(".agent-enabled").checked } }, status);
      if (res) { note(status, "saved"); load(); }
    };
    card.querySelector(".agent-test").onclick = async () => {
      note(status, "testing…");
      const res = await post("/v1/agents", { action: "test", id }, status);
      if (res) note(status, res.test.ok ? `✓ reachable${res.test.claude ? ` — ${res.test.claude}` : ""}`
        : `✗ not ready: ${res.test.error || (res.test.workdir_ok === false ? "the working folder doesn't exist" : `answered ${res.test.status ?? "nothing"}`)}`);
    };
    card.querySelector(".agent-mint").onclick = async () => {
      const areas = [...card.querySelectorAll(".agent-area:checked")].map((b) => b.value);
      if (!areas.length) { note(status, "choose at least one area, or leave it without record access"); return; }
      const res = await post("/v1/agents", { action: "mint_key", id, areas }, status);
      if (res) load();
    };
    const tokenSave = card.querySelector(".agent-token-save");
    if (tokenSave) tokenSave.onclick = async () => {
      const token = card.querySelector(".agent-token").value;
      const res = await post("/v1/agents", { action: "set_token", id, token }, status);
      if (res) load();
    };
    const hook = card.querySelector(".agent-hook");
    if (hook) hook.onclick = async () => {
      const res = await post("/v1/agents", { action: "hook", id }, status);
      if (!res) return;
      const pre = card.querySelector(".agent-script");
      pre.hidden = false;
      pre.textContent = res.script;
      note(status, "Save this as Hermes's approval hook script — it's shown once. Making a new one replaces it.");
    };
    card.querySelector(".agent-override-save").onclick = async () => {
      const modes = {};
      card.querySelectorAll(".agent-override").forEach((s) => { if (s.value) modes[s.dataset.class] = s.value; });
      const res = await post("/v1/agents/policy", { id, policy: { modes } }, status);
      if (res) note(status, "its rules are saved");
    };
    const move = card.querySelector(".agent-move");
    if (move) move.onclick = async () => {
      const ok = await confirmDanger({
        title: `Move ${entry.agent.name} to ${data.peer.name}?`,
        body: "Its settings and saved tokens go to that machine, sealed so only it can open them; it gets its own " +
          "key to your record there, and this machine forgets it. Tasks waiting for it go with it. " +
          "The harness itself (Hermes, OpenClaw) must be reachable from there.",
        action: "move it",
      });
      if (!ok) return;
      const res = await post("/v1/agents", { action: "move_to_peer", id }, status);
      if (res) { ui.openAgent = null; load(); }
    };
    card.querySelector(".agent-delete").onclick = async () => {
      const ok = await confirmDanger({
        title: `Remove ${entry.agent.name}?`,
        body: "Its key to your record is revoked and its saved tokens are deleted. Tasks it already did stay in the activity log.",
        action: "remove",
      });
      if (!ok) return;
      if (await post("/v1/agents", { action: "delete", id }, status)) { ui.openAgent = null; load(); }
    };
  });

  const rulesNote = body.querySelector("#agent-rules-note");
  body.querySelectorAll(".agent-mode").forEach((b) => {
    b.onclick = () => {
      data.policy.modes[b.dataset.class] = b.dataset.mode;
      b.parentElement.querySelectorAll(".agent-mode").forEach((x) => x.classList.toggle("on", x === b));
    };
  });
  body.querySelectorAll(".agent-preset").forEach((b) => {
    b.onclick = () => {
      const preset = data.presets.find((x) => x.name === b.dataset.preset);
      data.policy = { ...preset.policy, tasks_per_day: data.policy.tasks_per_day, actions_per_task: data.policy.actions_per_task };
      paint();
      note(view.querySelector("#agent-rules-note"), `"${preset.name}" loaded — press save to use it`);
    };
  });
  const taint = body.querySelector("#agent-taint");
  taint.onchange = async () => {
    if (taint.checked) return;
    const ok = await confirmDanger({
      title: "Let agents act on what they read, without asking?",
      body: "An email or web page can contain hidden instructions. With this off, an agent that read one could send, " +
        "buy or change things on its own wherever you've chosen \"do it\". Keep it on unless you're sure.",
      action: "turn it off",
    });
    if (!ok) taint.checked = true;
  };
  body.querySelector("#agent-rules-save").onclick = async () => {
    const policy = {
      modes: data.policy.modes,
      taint_guard: taint.checked,
      tasks_per_day: Number(body.querySelector("#agent-per-day").value) || 25,
      actions_per_task: Number(body.querySelector("#agent-per-task").value) || 60,
    };
    const res = await post("/v1/agents/policy", { id: "global", policy }, rulesNote);
    if (res) { note(rulesNote, "saved — these rules apply from the next action on"); data.policy = res.stored; }
  };
  const rename = body.querySelector("#agent-node-rename");
  if (rename) rename.onclick = () => {
    const holder = body.querySelector("#agent-node-name");
    holder.innerHTML = `<input id="agent-node-input" value="${esc(data.node?.name || "")}" maxlength="40" style="width:12em">`;
    const input = holder.querySelector("input");
    input.focus();
    input.onkeydown = async (e) => {
      if (e.key === "Escape") return paint();
      if (e.key !== "Enter") return;
      if (await post("/v1/agents", { action: "rename_node", name: input.value })) load();
    };
  };
  body.querySelectorAll("[data-elsewhere]").forEach((card) => {
    const btn = card.querySelector(".agent-move-here");
    if (!btn) return;
    btn.onclick = async () => {
      btn.disabled = true;
      const res = await post("/v1/agents", { action: "move_from_peer", id: card.dataset.elsewhere }, card.querySelector(".agent-status"));
      if (res) load(); else btn.disabled = false;
    };
  });
  body.querySelectorAll(".agent-cancel").forEach((b) => {
    b.onclick = async () => { if (await post(`/v1/agents/tasks/${encodeURIComponent(b.dataset.task)}/cancel`, {})) load(); };
  });
}

export async function mount(target, helpers) {
  ({ api, apiPost, confirmDanger } = helpers);
  view = target;
  view.innerHTML = `
    <span class="eyebrow">agents</span>
    <h1 class="pagehead">Agents</h1>
    <p class="subhead">Hearthloom remembers; agents act — under the rules you set here. Every agent starts on
      "ask me first", and everything they do is recorded.</p>
    <div id="agents-body"><div class="loading"><span class="spinner"></span> …</div></div>`;
  await load();
}
