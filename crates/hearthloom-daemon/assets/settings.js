// settings.js — ☰ → Settings: per-automation enable + frequency for the
// idle-time consolidation passes, PLUS a manual "run now" trigger with
// live running / last-ran status. Stored as consolidation_state settings.*
// keys daemon-side; schedule changes take effect on the next cycle. A
// manual run is an EXCEPTION to the schedule — it never resets last_run.
// LLM-marked passes only spend when a provider is configured.

const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) =>
  ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&#39;", "'": "&#39;" }[c]));

const FREQ_LABEL = {
  cycle: "every cycle (~6h)",
  daily: "daily",
  weekly: "weekly",
  monthly: "monthly",
};

// Cognition tiers: how much reasoning (and spend) a pass gets. The active
// provider maps each to a concrete model in Integrations.
const TIERS = ["low", "medium", "high"];
const TIER_LABEL = { low: "low tier", medium: "medium tier", high: "high tier" };

function since(ts) {
  if (!ts) return "never run yet";
  const hours = (Date.now() - new Date(ts).getTime()) / 36e5;
  if (hours < 1.5) return "ran within the hour";
  if (hours < 36) return `ran ${Math.round(hours)}h ago`;
  return `ran ${Math.round(hours / 24)}d ago`;
}

export async function mount(view, { api, apiPost }) {
  const data = await api("/v1/settings/automations");
  const llmOk = data.llm_configured;
  const onboarding = await api("/v1/onboarding").catch(() => ({ state: "done" }));
  const newUser = onboarding.state === "" || onboarding.state === "active";

  view.innerHTML = `
    <span class="eyebrow">settings</span>
    <h1 class="pagehead">The record's rhythm</h1>
    <div class="card">
      <label style="display:flex; gap:var(--s2); align-items:baseline">
        <input type="checkbox" id="set-newuser" ${newUser ? "checked" : ""}>
        <span><b>New User Mode</b>
          <span class="meta">— re-enables the setup wizard everywhere (the welcome
          flow, plus a reminder banner until it's finished)</span></span>
      </label>
      <p class="meta" id="set-newuser-note" hidden></p>
    </div>
    <div class="card" id="set-telemetry-card" hidden>
      <div class="section-title">diagnostics</div>
      <label style="display:flex; gap:var(--s2); align-items:baseline">
        <input type="checkbox" id="set-telemetry">
        <span><b>Share error reports</b>
          <span class="meta">— sends crash reports and performance timings to the
          operator of this build's error-tracking service. Never your memories:
          no episode content, no request bodies, no credentials — panics, error
          lines, and route timings only. Off = nothing is ever sent, even on a
          configured build.</span></span>
      </label>
      <p class="meta" id="set-telemetry-note" hidden></p>
    </div>
    <div class="card">
      <div class="section-title">who you are</div>
      <div id="set-profile-card"><div class="loading">…</div></div>
      <div id="set-id-current" class="meta" style="margin:var(--s2) 0"></div>
      <p class="meta">Your name and emails are how commits, invites, mail and messages
        all resolve to <b>one person — you</b>. Claiming an email below writes it into
        the record (a work email labels its material <i>work</i>); the card above is
        your face to connected friends. All identities live under
        <a href="#/browse">Memories → identities</a>.</p>
      <div class="searchbar" style="flex-wrap:wrap">
        <input id="set-id-name" placeholder="your name" style="flex:1">
        <input id="set-id-email" placeholder="an email of yours" style="flex:1">
        <select id="set-id-scope">
          <option value="personal">personal</option><option value="work">work</option>
          <option value="family">family</option><option value="health">health</option>
        </select>
        <button class="btn" id="set-id-claim">claim it as mine</button>
      </div>
      <p class="meta" id="set-id-note" hidden></p>
    </div>
    <p class="subhead">Each idle-time pass can be switched off, slowed down, or
      run on demand. Schedule changes apply from the next consolidation cycle
      (about every six hours); a manual <b>run now</b> is a one-off that never
      changes the schedule. Passes marked
      <span class="task-tag plain-tag">llm</span> spend provider tokens${
      llmOk ? "" :
      " — <b>no LLM is configured</b>, so those passes are dormant regardless"}.</p>
    <div id="settings-list"></div>`;

  // Diagnostics consent: the card only appears when this BUILD has an
  // error-tracking DSN configured (open-source installs without one
  // never see it — there is nothing to consent to). The toggle is the
  // user gate; it applies instantly daemon-side.
  api("/v1/settings/telemetry").then((t) => {
    if (!t || !t.configured) return;
    const card = view.querySelector("#set-telemetry-card");
    const box = view.querySelector("#set-telemetry");
    const note = view.querySelector("#set-telemetry-note");
    card.hidden = false;
    box.checked = !!t.enabled;
    if (!t.explicit_choice && t.default_optin) {
      note.hidden = false;
      note.textContent =
        "on by default for this hosted build — switch it off any time and nothing more is sent";
    }
    box.onchange = async (e) => {
      const r = await apiPost("/v1/settings/telemetry", { enabled: e.target.checked });
      note.hidden = false;
      note.textContent = r.enabled
        ? "error reporting is on — thank you; you can switch it off any time"
        : "error reporting is off — nothing is sent";
    };
  }).catch(() => {});

  // New User Mode: checking re-enables the wizard + banner and offers to
  // jump straight into the welcome flow; unchecking marks onboarding done.
  view.querySelector("#set-newuser").onchange = async (e) => {
    const note = view.querySelector("#set-newuser-note");
    const on = e.target.checked;
    if (window.setOnboarding) await window.setOnboarding(on ? "active" : "done");
    note.hidden = false;
    note.textContent = on
      ? "new user mode is on — the reminder banner returns until setup is finished"
      : "marked as set up — no more reminders";
    if (on) {
      const overlay = document.createElement("div");
      overlay.className = "note-overlay";
      overlay.style.zIndex = "290";
      overlay.innerHTML = `
        <div class="person-card" style="max-width:400px">
          <h2 class="pm-name">Go to setup now?</h2>
          <p class="meta">Would you like to open the first welcome screen right
            now and complete onboarding?</p>
          <div class="tend-actions">
            <button class="btn" id="nu-go">take me there</button>
            <button class="btn btn-quiet" id="nu-later">later</button>
          </div>
        </div>`;
      document.body.appendChild(overlay);
      overlay.querySelector("#nu-go").onclick = () => {
        overlay.remove();
        location.hash = "#/welcome";
      };
      overlay.querySelector("#nu-later").onclick = () => overlay.remove();
    }
  };
  // identity claims — the onboarding "who are you" data, editable later
  // the profile card (shared editor — also on Network) + the identities
  // the record already holds, so "did it save?" is answerable at a glance
  window.mountProfileCard?.(view.querySelector("#set-profile-card"));
  const paintIdentities = async () => {
    const el = view.querySelector("#set-id-current");
    // YOUR identities only — the attributes on your own Person entity
    // (the identities table holds every entity's handles, not just yours)
    const card = await api("/v1/network/mycard").catch(() => ({}));
    if (!card.name) { el.textContent = "save your name above, then claim an email below."; return; }
    const dossier = await api(`/v1/entity/${encodeURIComponent(card.name)}`).catch(() => null);
    const attrs = (dossier?.attributes || []).filter(([k]) => k === "email" || k === "handle" || k === "phone");
    el.innerHTML = attrs.length
      ? `claimed as ${card.name}: ` + attrs.map(([k, v]) =>
          `<span class="task-tag plain-tag">${k}: ${v}
            <button class="linkish set-id-rm" data-kind="${k}" data-value="${v}"
              title="remove this claim (mis-claimed by accident?)">×</button></span>`).join(" ")
      : `no identities claimed for ${card.name} yet — claim one below.`;
    el.querySelectorAll(".set-id-rm").forEach((btn) => {
      btn.onclick = async () => {
        await apiPost("/v1/identity/unclaim", {
          actor: "human:dashboard", kind: btn.dataset.kind, value: btn.dataset.value,
        }).catch(() => {});
        paintIdentities();
      };
    });
  };
  paintIdentities();
  view.querySelector("#set-id-claim").onclick = async () => {
    const note = view.querySelector("#set-id-note");
    const name = view.querySelector("#set-id-name").value.trim();
    const email = view.querySelector("#set-id-email").value.trim();
    note.hidden = false;
    if (!name || !email.includes("@")) {
      note.textContent = "✗ both a name and an email are needed";
      return;
    }
    try {
      await apiPost("/v1/identity/claim", {
        actor: "human:dashboard", entity: name, kind: "email",
        value: email, scope: view.querySelector("#set-id-scope").value,
      });
      note.textContent = `✓ ${email} is yours — future imports through it resolve to ${name}`;
      view.querySelector("#set-id-email").value = "";
      paintIdentities();
    } catch (err) {
      note.textContent = `✗ ${err.message}`;
    }
  };

  const list = view.querySelector("#settings-list");

  // Paint the cards from a fresh rows array and (re)bind handlers. Called on
  // mount and on every poll tick while something is running.
  const paint = (rows) => {
    list.innerHTML = rows.map((a) => {
      const runnable = !a.llm || llmOk; // manual runs bypass the enabled gate
      return `
      <div class="card" data-key="${esc(a.key)}" style="${a.enabled ? "" : "opacity:0.55"}">
        <div class="task-row" style="align-items:baseline; flex-wrap:wrap; gap:var(--s2)">
          <label style="display:flex; align-items:center; gap:var(--s2); cursor:pointer">
            <input type="checkbox" class="auto-enabled" ${a.enabled ? "checked" : ""}>
            <b>${esc(a.label)}</b>
          </label>
          ${a.llm ? `<span class="task-tag plain-tag">llm</span>` : ""}
          ${a.llm ? `
          <select class="auto-tier" ${a.enabled ? "" : "disabled"}
            title="how much reasoning this pass spends — mapped to a model in Integrations">
            ${TIERS.map((t) => `
              <option value="${t}" ${t === a.tier ? "selected" : ""}>
                ${TIER_LABEL[t]}${t === a.default_tier ? " · default" : ""}
              </option>`).join("")}
          </select>` : ""}
          <select class="auto-freq" ${a.enabled ? "" : "disabled"}>
            ${(data.frequencies || []).map((f) => `
              <option value="${esc(f)}" ${f === a.frequency ? "selected" : ""}>
                ${esc(FREQ_LABEL[f] || f)}${f === a.default_frequency ? " · default" : ""}
              </option>`).join("")}
          </select>
          <span class="meta auto-last">${esc(since(a.last_ran_at || a.last_run))}</span>
          <span style="flex:1"></span>
          <button class="btn auto-run" ${runnable ? "" : "disabled"} ${a.running ? "hidden" : ""}
            title="${runnable ? "run this pass once, now" : "needs an LLM provider"}">run now</button>
          <span class="meta auto-status" ${a.running ? "" : "hidden"}>● running…</span>
        </div>
        <p class="meta" style="margin:var(--s1) 0 0">${esc(a.detail)}</p>
      </div>`;
    }).join("");
    bind(rows);
  };

  let polling = false;
  const poll = async () => {
    if (polling) return;
    polling = true;
    try {
      // Keep refreshing while any pass is running; stop once all settle.
      // eslint-disable-next-line no-constant-condition
      while (true) {
        await new Promise((r) => setTimeout(r, 2500));
        if (!list.isConnected) return; // navigated away
        const fresh = await api("/v1/settings/automations");
        paint(fresh.automations || []);
        if (!(fresh.automations || []).some((a) => a.running)) break;
      }
    } catch { /* transient — a later action will re-poll */ }
    finally { polling = false; }
  };

  const bind = (rows) => {
    list.querySelectorAll(".card").forEach((card) => {
      const key = card.dataset.key;
      const enabled = card.querySelector(".auto-enabled");
      const freq = card.querySelector(".auto-freq");
      const tier = card.querySelector(".auto-tier");
      const runBtn = card.querySelector(".auto-run");
      const status = card.querySelector(".auto-status");

      const save = async (body) => {
        try {
          const updated = await apiPost("/v1/settings/automations", { key, ...body });
          enabled.checked = updated.enabled;
          freq.value = updated.frequency;
          freq.disabled = !updated.enabled;
          if (tier) { tier.value = updated.tier; tier.disabled = !updated.enabled; }
          card.style.opacity = updated.enabled ? "" : "0.55";
        } catch (err) {
          card.querySelector(".auto-last").textContent = `save failed: ${err.message}`;
        }
      };
      enabled.addEventListener("change", () => save({ enabled: enabled.checked }));
      freq.addEventListener("change", () => save({ frequency: freq.value }));
      if (tier) tier.addEventListener("change", () => save({ tier: tier.value }));

      if (runBtn) runBtn.addEventListener("click", async () => {
        runBtn.disabled = true;
        try {
          await apiPost(`/v1/settings/automations/${encodeURIComponent(key)}/run`, {});
          // Optimistic: flip to running immediately, then let polling confirm.
          runBtn.hidden = true;
          status.hidden = false;
          poll();
        } catch (err) {
          runBtn.disabled = false;
          card.querySelector(".auto-last").textContent =
            err.message && /already running/i.test(err.message)
              ? "already running" : `couldn't start: ${err.message}`;
        }
      });
    });
  };

  paint(data.automations || []);
  // If a scheduled run is already in flight when the page opens, watch it.
  if ((data.automations || []).some((a) => a.running)) poll();
}
