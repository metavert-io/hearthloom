//! Agents (v55; the design log 2026-10-17). Ask may now ACT: when the user
//! asks for something to be done, Ask proposes a task and hands it to an
//! agent harness (Claude Code, Hermes, OpenClaw — later an internal one).
//! Hearthloom is the memory and the conscience; the harness is the hands.
//!
//! This module holds the four pieces of state and the one pure rule:
//! - `agents` — configured bridges. Node-local: an address and a secret
//!   belong to one machine (the secret itself lives in .env.sealed; only
//!   its NAME is stored here).
//! - `governance` — the autonomy policy: row `global` plus one optional
//!   override row per agent. Per kind of action: auto / ask / never.
//!   Default: ask for everything (Jon, 2026-10-17: "default ask first").
//! - `agent_tasks` — what Ask handed off, and how it went.
//! - `agent_approvals` — every decision: the dispatch itself, and each
//!   tool call a harness routes back for permission.
//! - `decide` — THE RULE. Pure; everything that gates an action calls it.
//!
//! Visibility: configuration and the activity log are vault-gated in the
//! HTTP layer (the Agents page). A task born from a sensitive ask (vault
//! open) is `sensitive`: its details and approvals stay behind the vault.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// The kinds of action a policy governs. `other` is anything a harness
/// asks about that maps to none of the rest — it is never auto unless the
/// user set it so.
pub const ACTION_CLASSES: &[&str] = &[
    "read",         // read the record, local files, web search
    "write_record", // add or change tasks / notes in Hearthloom
    "communicate",  // email, messages, posts — anything sent to a person
    "schedule",     // calendar events, invitations
    "spend",        // purchases, payments, transfers
    "files",        // write or delete files, run commands
    "web",          // fetch or browse pages, submit forms
    "friends",      // query or write to other people's Hearthlooms
    "other",
];

/// Kinds of action that can carry data OUT of the record or change
/// something — every class but `read`. A calendar invite reaches its
/// guests and a note can plant the next instruction, so `schedule` and
/// `write_record` belong here too. After a task has read content the user
/// didn't write, these are the path an injected instruction takes; the
/// taint rule in `decide` asks about them, and about reads as well.
pub const TAINT_SENSITIVE: &[&str] = &[
    "communicate",
    "spend",
    "files",
    "web",
    "friends",
    "schedule",
    "write_record",
    "other",
];

pub const HARNESSES: &[&str] = &["claude_code", "hermes", "openclaw", "internal"];

pub const MODES: &[&str] = &["auto", "ask", "never"];

/// Named starting points for the policy matrix.
pub const PRESETS: &[(&str, &[(&str, &str)])] = &[
    ("cautious", &[]),
    (
        "assistant",
        &[("read", "auto"), ("web", "auto"), ("write_record", "auto")],
    ),
    (
        "delegate",
        &[
            ("read", "auto"),
            ("web", "auto"),
            ("write_record", "auto"),
            ("communicate", "auto"),
            ("schedule", "auto"),
            ("friends", "auto"),
        ],
    ),
];

/// A resolved policy: one mode per class, plus the two rules on top.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub modes: std::collections::BTreeMap<String, String>,
    /// The taint rule. Overridable (Jon, 2026-10-17, decision C) — the UI
    /// warns when it is off.
    pub taint_guard: bool,
    pub tasks_per_day: u32,
    pub actions_per_task: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            modes: ACTION_CLASSES
                .iter()
                .map(|c| (c.to_string(), "ask".to_string()))
                .collect(),
            taint_guard: true,
            tasks_per_day: 25,
            actions_per_task: 60,
        }
    }
}

impl Policy {
    pub fn mode(&self, class: &str) -> &str {
        self.modes.get(class).map(String::as_str).unwrap_or("ask")
    }
    /// A preset applied over the all-ask default.
    pub fn preset(name: &str) -> Option<Policy> {
        let (_, entries) = PRESETS.iter().find(|(n, _)| *n == name)?;
        let mut policy = Policy::default();
        for (class, mode) in entries.iter() {
            policy.modes.insert(class.to_string(), mode.to_string());
        }
        Some(policy)
    }
}

/// What may happen with one action.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "decision", content = "reason", rename_all = "lowercase")]
pub enum Decision {
    Auto,
    Ask(String),
    Never(String),
}

/// THE RULE. `class` unknown → treated as `other`. Never wins; then the
/// taint rule: once a task has read content the user didn't write, EVERY
/// action needs a human, whatever its class, mode or preset. The classes
/// are the model's own claim about the task, and a read can fetch the
/// next instruction or carry what it found into the step after it. Only
/// the owner's own switch (`taint_guard`, decision C) turns this off.
pub fn decide(policy: &Policy, class: &str, tainted: Option<&str>) -> Decision {
    let class = if ACTION_CLASSES.contains(&class) {
        class
    } else {
        "other"
    };
    let mode = policy.mode(class);
    if mode == "never" {
        return Decision::Never(format!(
            "your agent settings don't allow \"{class}\" actions"
        ));
    }
    if let Some(why) = tainted.filter(|_| policy.taint_guard) {
        return Decision::Ask(if TAINT_SENSITIVE.contains(&class) {
            format!("this task read {why}, so \"{class}\" actions need your OK even where they're set to automatic")
        } else {
            format!("this task read {why}, so even \"{class}\" actions need your OK: what it reads next could carry those instructions further")
        });
    }
    match mode {
        "auto" => Decision::Auto,
        _ => Decision::Ask(format!("\"{class}\" actions are set to ask first")),
    }
}

/// The strictest decision across several classes (a task needing three
/// kinds of action needs approval if any one does). A task that names no
/// class is `other` — never automatic for want of a claim.
pub fn decide_all(policy: &Policy, classes: &[String], tainted: Option<&str>) -> Decision {
    if classes.is_empty() {
        return decide(policy, "other", tainted);
    }
    let mut out = Decision::Auto;
    for class in classes {
        match decide(policy, class, tainted) {
            Decision::Never(r) => return Decision::Never(r),
            Decision::Ask(r) if out == Decision::Auto => out = Decision::Ask(r),
            _ => {}
        }
    }
    out
}

/// Parse a stored or submitted policy. Unknown classes and modes are
/// dropped; missing classes are `ask`; limits are clamped. `overrides`
/// = an agent's row, which only carries the classes it changes.
fn parse_policy(raw: &Value, base: &Policy, overrides_only: bool) -> Policy {
    let mut policy = base.clone();
    if let Some(modes) = raw.get("modes").and_then(|m| m.as_object()) {
        for (class, mode) in modes {
            if ACTION_CLASSES.contains(&class.as_str()) {
                if let Some(mode) = mode.as_str().filter(|m| MODES.contains(m)) {
                    policy.modes.insert(class.clone(), mode.to_string());
                }
            }
        }
    }
    if !overrides_only || raw.get("taint_guard").is_some() {
        if let Some(guard) = raw.get("taint_guard").and_then(|g| g.as_bool()) {
            policy.taint_guard = guard;
        }
    }
    if let Some(n) = raw.get("tasks_per_day").and_then(|n| n.as_u64()) {
        policy.tasks_per_day = n.clamp(1, 1000) as u32;
    }
    if let Some(n) = raw.get("actions_per_task").and_then(|n| n.as_u64()) {
        policy.actions_per_task = n.clamp(1, 10_000) as u32;
    }
    policy
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub harness: String,
    pub config: Value,
    pub secret_name: Option<String>,
    pub key_id: Option<String>,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentIn {
    pub name: String,
    pub harness: String,
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentTaskRow {
    pub id: String,
    pub ask_id: Option<String>,
    pub agent_id: Option<String>,
    pub request: String,
    pub brief: String,
    pub classes: Vec<String>,
    pub tainted: Option<String>,
    pub sensitive: bool,
    pub status: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub node: Option<String>,
    pub actions: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Task lifecycle. `proposed` waits for the user's go (ask-first);
/// `queued` is cleared to run and waits for the node that hosts its agent.
pub const TASK_STATUSES: &[&str] = &[
    "proposed",
    "queued",
    "running",
    "done",
    "failed",
    "cancelled",
    "refused",
];

fn row_to_agent(row: &rusqlite::Row) -> rusqlite::Result<AgentRow> {
    Ok(AgentRow {
        id: row.get(0)?,
        name: row.get(1)?,
        harness: row.get(2)?,
        config: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_else(|_| json!({})),
        secret_name: row.get(4)?,
        key_id: row.get(5)?,
        enabled: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

const AGENT_COLS: &str =
    "id, name, harness, config, secret_name, key_id, enabled, created_at, updated_at";
const TASK_COLS: &str = "id, ask_id, agent_id, request, brief, classes, tainted, sensitive, status,
     result, error, node, actions, created_at, updated_at";

fn row_to_task(row: &rusqlite::Row) -> rusqlite::Result<AgentTaskRow> {
    Ok(AgentTaskRow {
        id: row.get(0)?,
        ask_id: row.get(1)?,
        agent_id: row.get(2)?,
        request: row.get(3)?,
        brief: row.get(4)?,
        classes: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
        tainted: row.get(6)?,
        sensitive: row.get::<_, i64>(7)? != 0,
        status: row.get(8)?,
        result: row.get(9)?,
        error: row.get(10)?,
        node: row.get(11)?,
        actions: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Config keys an agent may carry (non-secret). Anything else is dropped.
const CONFIG_KEYS: &[&str] = &[
    "url",
    "workdir",
    "model",
    "agent_id",
    "allowed_tools",
    "notes",
    "claude_path",
    "timeout_secs",
    "areas",
];

fn clean_config(config: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(obj) = config.as_object() {
        for (k, v) in obj {
            if CONFIG_KEYS.contains(&k.as_str()) && (v.is_string() || v.is_array() || v.is_number())
            {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

impl Store {
    /* ————— agents (node-local configuration) ————— */

    pub fn agents(&self) -> Result<Vec<AgentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLS} FROM agents ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([], row_to_agent)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    pub fn agent(&self, id: &str) -> Result<Option<AgentRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {AGENT_COLS} FROM agents WHERE id = ?1"),
            [id],
            row_to_agent,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Create (id None) or update an agent's non-secret configuration.
    pub fn save_agent(&self, actor: &Actor, id: Option<&str>, input: &AgentIn) -> Result<AgentRow> {
        let name = input.name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.is_empty() || name.chars().count() > 60 {
            return Err(anyhow!("an agent needs a name of up to 60 characters"));
        }
        if !HARNESSES.contains(&input.harness.as_str()) {
            return Err(anyhow!("harness must be one of {HARNESSES:?}"));
        }
        let config = clean_config(&input.config).to_string();
        let now = now_utc();
        let id = match id {
            Some(id) => {
                let conn = self.lock();
                let changed = conn.execute(
                    "UPDATE agents SET name = ?2, harness = ?3, config = ?4,
                         enabled = COALESCE(?5, enabled), updated_at = ?6 WHERE id = ?1",
                    rusqlite::params![
                        id,
                        name,
                        input.harness,
                        config,
                        input.enabled.map(i64::from),
                        now
                    ],
                )?;
                if changed == 0 {
                    return Err(anyhow!("no such agent"));
                }
                id.to_string()
            }
            None => {
                let id = new_id("ag");
                let conn = self.lock();
                conn.execute(
                    "INSERT INTO agents (id, name, harness, config, enabled, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                    rusqlite::params![id, name, input.harness, config, input.enabled.unwrap_or(true) as i64, now],
                )?;
                id
            }
        };
        let conn = self.lock();
        super::journal_row(&conn, actor, "agent", "agents", &id, "update")?;
        record_mutation(
            &conn,
            actor,
            "agent_saved",
            &format!("agent_saved:{id}:{now}"),
            &json!({"id": id, "name": name, "harness": input.harness}),
        )?;
        drop(conn);
        self.agent(&id)?
            .ok_or_else(|| anyhow!("agent vanished after write"))
    }

    /// Record which Hearthloom key the agent reads with, and the NAME of
    /// the sealed secret holding its harness token (never the value).
    pub fn set_agent_credentials(
        &self,
        id: &str,
        key_id: Option<&str>,
        secret_name: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agents SET key_id = COALESCE(?2, key_id), secret_name = COALESCE(?3, secret_name),
                 updated_at = ?4 WHERE id = ?1",
            rusqlite::params![id, key_id, secret_name, now_utc()],
        )?;
        Ok(())
    }

    pub fn delete_agent(&self, actor: &Actor, id: &str) -> Result<Option<AgentRow>> {
        let Some(agent) = self.agent(id)? else {
            return Ok(None);
        };
        let conn = self.lock();
        super::journal_row(&conn, actor, "agent", "agents", id, "delete")?;
        conn.execute("DELETE FROM agents WHERE id = ?1", [id])?;
        conn.execute("DELETE FROM governance WHERE id = ?1", [id])?;
        record_mutation(
            &conn,
            actor,
            "agent_deleted",
            &format!("agent_deleted:{id}:{}", now_utc()),
            &json!({"id": id, "name": agent.name}),
        )?;
        Ok(Some(agent))
    }

    /* ————— governance ————— */

    fn policy_raw(&self, id: &str) -> Result<Option<Value>> {
        let conn = self.lock();
        let raw: Option<String> = conn
            .query_row("SELECT policy FROM governance WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
    }

    /// The global policy (all-ask default when never set).
    pub fn global_policy(&self) -> Result<Policy> {
        Ok(self
            .policy_raw("global")?
            .map(|raw| parse_policy(&raw, &Policy::default(), false))
            .unwrap_or_default())
    }

    /// An agent's own override row, as stored (classes it changes only).
    pub fn agent_policy_overrides(&self, agent_id: &str) -> Result<Value> {
        Ok(self.policy_raw(agent_id)?.unwrap_or_else(|| json!({})))
    }

    /// What actually governs this agent: global, then its overrides.
    pub fn effective_policy(&self, agent_id: Option<&str>) -> Result<Policy> {
        let global = self.global_policy()?;
        match agent_id {
            Some(id) => Ok(match self.policy_raw(id)? {
                Some(raw) => parse_policy(&raw, &global, true),
                None => global,
            }),
            None => Ok(global),
        }
    }

    /// Save the global policy (`id = "global"`) or an agent's overrides.
    /// Returns the cleaned value as stored.
    pub fn set_policy(&self, actor: &Actor, id: &str, raw: &Value) -> Result<Value> {
        if id != "global" && self.agent(id)?.is_none() {
            return Err(anyhow!("no such agent"));
        }
        let stored = if id == "global" {
            serde_json::to_value(parse_policy(raw, &Policy::default(), false))?
        } else {
            // keep only the classes the agent changes, plus explicit rules
            let mut out = serde_json::Map::new();
            if let Some(modes) = raw.get("modes").and_then(|m| m.as_object()) {
                let clean: serde_json::Map<String, Value> = modes
                    .iter()
                    .filter(|(c, m)| {
                        ACTION_CLASSES.contains(&c.as_str())
                            && m.as_str().is_some_and(|m| MODES.contains(&m))
                    })
                    .map(|(c, m)| (c.clone(), m.clone()))
                    .collect();
                out.insert("modes".into(), Value::Object(clean));
            }
            if let Some(g) = raw.get("taint_guard").and_then(|g| g.as_bool()) {
                out.insert("taint_guard".into(), json!(g));
            }
            Value::Object(out)
        };
        let now = now_utc();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO governance (id, policy, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET policy = excluded.policy, updated_at = excluded.updated_at",
            rusqlite::params![id, stored.to_string(), now],
        )?;
        super::journal_row(&conn, actor, "governance", "governance", id, "update")?;
        record_mutation(
            &conn,
            actor,
            "governance_set",
            &format!("governance_set:{id}:{now}"),
            &json!({"id": id, "policy": stored}),
        )?;
        Ok(stored)
    }

    /* ————— tasks ————— */

    #[allow(clippy::too_many_arguments)]
    pub fn create_agent_task(
        &self,
        actor: &Actor,
        ask_id: Option<&str>,
        agent_id: Option<&str>,
        request: &str,
        brief: &str,
        classes: &[String],
        tainted: Option<&str>,
        sensitive: bool,
    ) -> Result<AgentTaskRow> {
        let classes: Vec<String> = classes
            .iter()
            .map(|c| {
                if ACTION_CLASSES.contains(&c.as_str()) {
                    c.clone()
                } else {
                    "other".into()
                }
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let id = new_id("at");
        let now = now_utc();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO agent_tasks (id, ask_id, agent_id, request, brief, classes, tainted,
                 sensitive, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'proposed', ?9, ?9)",
            rusqlite::params![
                id,
                ask_id,
                agent_id,
                request.chars().take(4000).collect::<String>(),
                brief.chars().take(8000).collect::<String>(),
                json!(classes).to_string(),
                tainted,
                sensitive as i64,
                now,
            ],
        )?;
        super::journal_row(&conn, actor, "agent_task", "agent_tasks", &id, "create")?;
        drop(conn);
        self.agent_task(&id)?
            .ok_or_else(|| anyhow!("task vanished after write"))
    }

    pub fn agent_task(&self, id: &str) -> Result<Option<AgentTaskRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {TASK_COLS} FROM agent_tasks WHERE id = ?1"),
            [id],
            row_to_task,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Newest first. `include_sensitive` false hides tasks born of a
    /// vault-open ask (the caller passes the vault state).
    pub fn agent_tasks(&self, limit: usize, include_sensitive: bool) -> Result<Vec<AgentTaskRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLS} FROM agent_tasks WHERE (?2 OR sensitive = 0)
             ORDER BY created_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![limit as i64, include_sensitive],
            row_to_task,
        )?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Tasks cleared to run on an agent hosted by this node.
    pub fn queued_agent_tasks_for(&self, agent_ids: &[String]) -> Result<Vec<AgentTaskRow>> {
        Ok(self
            .agent_tasks(200, true)?
            .into_iter()
            .filter(|t| {
                t.status == "queued" && t.agent_id.as_ref().is_some_and(|a| agent_ids.contains(a))
            })
            .collect())
    }

    /// Move a task along. Only forward moves out of a final state are
    /// refused (a finished task never restarts).
    pub fn set_agent_task_status(
        &self,
        actor: &Actor,
        id: &str,
        status: &str,
        result: Option<&str>,
        error: Option<&str>,
        node: Option<&str>,
    ) -> Result<AgentTaskRow> {
        if !TASK_STATUSES.contains(&status) {
            return Err(anyhow!("unknown task status {status:?}"));
        }
        let task = self
            .agent_task(id)?
            .ok_or_else(|| anyhow!("no such task"))?;
        if ["done", "failed", "cancelled", "refused"].contains(&task.status.as_str())
            && task.status != status
        {
            return Err(anyhow!("that task already ended ({})", task.status));
        }
        let now = now_utc();
        let conn = self.lock();
        conn.execute(
            "UPDATE agent_tasks SET status = ?2, result = COALESCE(?3, result),
                 error = COALESCE(?4, error), updated_at = ?5,
                 started_at = CASE WHEN ?2 = 'running' AND started_at IS NULL THEN ?5 ELSE started_at END,
                 finished_at = CASE WHEN ?2 IN ('done','failed','cancelled','refused') THEN ?5 ELSE finished_at END,
                 node = COALESCE(?6, node)
             WHERE id = ?1",
            rusqlite::params![id, status, result, error, now, node],
        )?;
        super::journal_row(&conn, actor, "agent_task", "agent_tasks", id, "update")?;
        drop(conn);
        self.agent_task(id)?
            .ok_or_else(|| anyhow!("task vanished after write"))
    }

    /// Count one more action against the task's limit; returns the count.
    pub fn bump_agent_task_actions(&self, id: &str) -> Result<i64> {
        let conn = self.lock();
        conn.execute(
            "UPDATE agent_tasks SET actions = actions + 1, updated_at = ?2 WHERE id = ?1",
            rusqlite::params![id, now_utc()],
        )?;
        Ok(
            conn.query_row("SELECT actions FROM agent_tasks WHERE id = ?1", [id], |r| {
                r.get(0)
            })?,
        )
    }

    /// Tasks that started running in the last 24 hours (the daily limit).
    pub fn agent_tasks_started_today(&self) -> Result<i64> {
        let since = (chrono::Utc::now() - chrono::Duration::hours(24))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM agent_tasks WHERE started_at IS NOT NULL AND started_at > ?1",
            [since],
            |r| r.get(0),
        )?)
    }

    /* ————— approvals ————— */

    #[allow(clippy::too_many_arguments)]
    pub fn add_approval(
        &self,
        task_id: &str,
        kind: &str,
        class: Option<&str>,
        summary: &str,
        detail: &Value,
        status: &str,
        decided_by: Option<&str>,
    ) -> Result<String> {
        if !["dispatch", "tool"].contains(&kind) {
            return Err(anyhow!("approval kind must be dispatch|tool"));
        }
        let id = new_id("ap");
        let now = now_utc();
        let decided_at = (status != "pending").then(|| now.clone());
        let conn = self.lock();
        conn.execute(
            "INSERT INTO agent_approvals (id, task_id, kind, class, summary, detail, status,
                 decided_by, decided_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            rusqlite::params![
                id,
                task_id,
                kind,
                class,
                summary.chars().take(500).collect::<String>(),
                detail.to_string().chars().take(8000).collect::<String>(),
                status,
                decided_by,
                decided_at,
                now,
            ],
        )?;
        super::journal_row(
            &conn,
            &Actor::new(decided_by.unwrap_or("system:governance")),
            "agent_approval",
            "agent_approvals",
            &id,
            "create",
        )?;
        Ok(id)
    }

    pub fn approval(&self, id: &str) -> Result<Option<Value>> {
        Ok(self
            .approvals_where("a.id = ?1", &[id], 1)?
            .into_iter()
            .next())
    }

    /// Pending approvals, newest first, each with its task's request.
    pub fn pending_approvals(&self, include_sensitive: bool) -> Result<Vec<Value>> {
        let rows = self.approvals_where("a.status = 'pending'", &[], 100)?;
        Ok(rows
            .into_iter()
            .filter(|r| include_sensitive || r["sensitive"] == json!(false))
            .collect())
    }

    pub fn task_approvals(&self, task_id: &str) -> Result<Vec<Value>> {
        self.approvals_where("a.task_id = ?1", &[task_id], 500)
    }

    fn approvals_where(&self, clause: &str, params: &[&str], limit: usize) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT a.id, a.task_id, a.kind, a.class, a.summary, a.detail, a.status, a.decided_by,
                    a.decided_at, a.created_at, t.request, t.sensitive, t.agent_id
             FROM agent_approvals a LEFT JOIN agent_tasks t ON t.id = a.task_id
             WHERE {clause} ORDER BY a.created_at DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "task_id": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "class": row.get::<_, Option<String>>(3)?,
                "summary": row.get::<_, String>(4)?,
                "detail": row.get::<_, Option<String>>(5)?
                    .and_then(|d| serde_json::from_str::<Value>(&d).ok()),
                "status": row.get::<_, String>(6)?,
                "decided_by": row.get::<_, Option<String>>(7)?,
                "decided_at": row.get::<_, Option<String>>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "request": row.get::<_, Option<String>>(10)?,
                "sensitive": row.get::<_, Option<i64>>(11)?.unwrap_or(0) != 0,
                "agent_id": row.get::<_, Option<String>>(12)?,
            }))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Decide a pending approval. A decided one is never re-decided.
    pub fn decide_approval(&self, id: &str, approve: bool, by: &str) -> Result<Value> {
        let now = now_utc();
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE agent_approvals SET status = ?2, decided_by = ?3, decided_at = ?4, updated_at = ?4
             WHERE id = ?1 AND status = 'pending'",
            rusqlite::params![id, if approve { "approved" } else { "denied" }, by, now],
        )?;
        if changed == 0 {
            return Err(anyhow!(
                "that approval was already decided or doesn't exist"
            ));
        }
        super::journal_row(
            &conn,
            &Actor::new(by),
            "agent_approval",
            "agent_approvals",
            id,
            "update",
        )?;
        drop(conn);
        self.approval(id)?
            .ok_or_else(|| anyhow!("approval vanished"))
    }

    /* ————— the agent directory (v56): which node hosts which agent ————— */

    /// Publish (or tombstone, `removed`) this node's entry for an agent.
    /// Only the hosting node writes an agent's entry.
    pub fn publish_agent(
        &self,
        agent: &AgentRow,
        node_id: &str,
        node_name: &str,
        removed: bool,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO agent_directory (agent_id, node_id, node_name, name, harness, enabled, removed, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(agent_id) DO UPDATE SET node_id = excluded.node_id, node_name = excluded.node_name,
                 name = excluded.name, harness = excluded.harness, enabled = excluded.enabled,
                 removed = excluded.removed, updated_at = excluded.updated_at",
            rusqlite::params![agent.id, node_id, node_name, agent.name, agent.harness,
                agent.enabled as i64, removed as i64, now_utc()],
        )?;
        Ok(())
    }

    /// Every live agent on every node, as AgentRow-shaped entries (config
    /// empty — configuration never leaves its node) plus where it runs.
    pub fn agent_directory(&self) -> Result<Vec<(AgentRow, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT agent_id, name, harness, enabled, node_id, node_name, updated_at
             FROM agent_directory WHERE removed = 0 ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                AgentRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    harness: row.get(2)?,
                    config: json!({}),
                    secret_name: None,
                    key_id: None,
                    enabled: row.get::<_, i64>(3)? != 0,
                    created_at: row.get(6)?,
                    updated_at: row.get(6)?,
                },
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Install an agent arriving from another node under its SAME id (so
    /// its queued tasks follow it). Config only; secrets are the caller's.
    pub fn adopt_agent(&self, actor: &Actor, agent: &AgentRow) -> Result<AgentRow> {
        if !HARNESSES.contains(&agent.harness.as_str()) {
            return Err(anyhow!("unknown harness {:?}", agent.harness));
        }
        let now = now_utc();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO agents (id, name, harness, config, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, harness = excluded.harness,
                 config = excluded.config, enabled = excluded.enabled, updated_at = excluded.updated_at,
                 key_id = NULL",
            rusqlite::params![agent.id, agent.name, agent.harness,
                clean_config(&agent.config).to_string(), agent.enabled as i64, now],
        )?;
        super::journal_row(&conn, actor, "agent", "agents", &agent.id, "update")?;
        record_mutation(
            &conn,
            actor,
            "agent_adopted",
            &format!("agent_adopted:{}:{now}", agent.id),
            &json!({"id": agent.id, "name": agent.name}),
        )?;
        drop(conn);
        self.agent(&agent.id)?
            .ok_or_else(|| anyhow!("agent vanished after adopt"))
    }

    /* ————— nodes (v58) ————— */

    /// Publish this node's own row (called every sync cycle).
    pub fn publish_node(
        &self,
        node_id: &str,
        name: &str,
        kind: &str,
        roles: &Value,
        version: &str,
    ) -> Result<()> {
        let now = now_utc();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO nodes (node_id, name, kind, roles, version, last_seen, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(node_id) DO UPDATE SET name = excluded.name, kind = excluded.kind,
                 roles = excluded.roles, version = excluded.version, last_seen = excluded.last_seen,
                 updated_at = excluded.updated_at",
            rusqlite::params![node_id, name, kind, roles.to_string(), version, now],
        )?;
        Ok(())
    }

    /// Every node this person has, newest contact first.
    pub fn nodes(&self) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT node_id, name, kind, roles, version, last_seen FROM nodes ORDER BY last_seen DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "node_id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "roles": serde_json::from_str::<Value>(&row.get::<_, String>(3)?).unwrap_or_else(|_| json!({})),
                "version": row.get::<_, Option<String>>(4)?,
                "last_seen": row.get::<_, String>(5)?,
            }))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /* ————— peer sync: policy, tasks, approvals, directory (not agents) ————— */

    pub fn agents_sync_export(&self, since: &str) -> Result<Value> {
        let conn = self.lock();
        let dump = |table: &str| -> Result<Vec<Value>> {
            let mut stmt = conn.prepare(&format!(
                "SELECT * FROM {table} WHERE synced_at > ?1 ORDER BY synced_at LIMIT 500"
            ))?;
            let cols: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
            let rows = stmt.query_map([since], |row| {
                let mut obj = serde_json::Map::new();
                for (i, c) in cols.iter().enumerate() {
                    let v: rusqlite::types::Value = row.get(i)?;
                    obj.insert(
                        c.clone(),
                        match v {
                            rusqlite::types::Value::Null => Value::Null,
                            rusqlite::types::Value::Integer(n) => json!(n),
                            rusqlite::types::Value::Real(f) => json!(f),
                            rusqlite::types::Value::Text(t) => json!(t),
                            rusqlite::types::Value::Blob(_) => Value::Null,
                        },
                    );
                }
                Ok(Value::Object(obj))
            })?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(Into::into)
        };
        let governance = dump("governance")?;
        let tasks = dump("agent_tasks")?;
        let approvals = dump("agent_approvals")?;
        let directory = dump("agent_directory")?;
        let nodes = dump("nodes")?;
        // the smallest end among full pages (more to come there), else the largest
        let mut full_ends = Vec::new();
        let mut ends = Vec::new();
        for (table, rows) in [
            ("governance", &governance),
            ("agent_tasks", &tasks),
            ("agent_approvals", &approvals),
            ("agent_directory", &directory),
            ("nodes", &nodes),
        ] {
            let end = super::page_cursor(
                &conn,
                "synced_at",
                &format!("FROM {table} WHERE synced_at > ?1"),
                since,
                500,
            )?;
            if rows.len() >= 500 {
                full_ends.push(end.clone());
            }
            ends.push(end);
        }
        let next = full_ends
            .into_iter()
            .min()
            .or_else(|| ends.into_iter().max())
            .unwrap_or_else(|| since.to_string());
        Ok(
            json!({"governance": governance, "tasks": tasks, "approvals": approvals,
                  "directory": directory, "nodes": nodes, "next": next}),
        )
    }

    /// Last-writer-wins by `updated_at`, per row. A decided approval is
    /// never reopened by a stale pending copy (its stamp is newer).
    pub fn agents_sync_apply(&self, batch: &Value) -> Result<usize> {
        const TABLES: &[(&str, &str, &[&str])] = &[
            ("governance", "governance", &["id", "policy", "updated_at"]),
            (
                "nodes",
                "nodes",
                &[
                    "node_id",
                    "name",
                    "kind",
                    "roles",
                    "version",
                    "last_seen",
                    "updated_at",
                ],
            ),
            (
                "directory",
                "agent_directory",
                &[
                    "agent_id",
                    "node_id",
                    "node_name",
                    "name",
                    "harness",
                    "enabled",
                    "removed",
                    "updated_at",
                ],
            ),
            (
                "tasks",
                "agent_tasks",
                &[
                    "id",
                    "ask_id",
                    "agent_id",
                    "request",
                    "brief",
                    "classes",
                    "tainted",
                    "sensitive",
                    "status",
                    "result",
                    "error",
                    "node",
                    "actions",
                    "created_at",
                    "updated_at",
                    "started_at",
                    "finished_at",
                ],
            ),
            (
                "approvals",
                "agent_approvals",
                &[
                    "id",
                    "task_id",
                    "kind",
                    "class",
                    "summary",
                    "detail",
                    "status",
                    "decided_by",
                    "decided_at",
                    "created_at",
                    "updated_at",
                ],
            ),
        ];
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for (key, table, cols) in TABLES {
            for row in batch
                .get(*key)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let key_col = cols[0]; // `id`, or `agent_id` for the directory
                if row.get(key_col).and_then(|v| v.as_str()).is_none()
                    || row.get("updated_at").and_then(|v| v.as_str()).is_none()
                {
                    continue;
                }
                let values: Vec<rusqlite::types::Value> = cols
                    .iter()
                    .map(|c| match row.get(*c) {
                        Some(Value::String(s)) => rusqlite::types::Value::Text(s.clone()),
                        Some(Value::Number(n)) => {
                            rusqlite::types::Value::Integer(n.as_i64().unwrap_or(0))
                        }
                        Some(Value::Bool(b)) => rusqlite::types::Value::Integer(*b as i64),
                        _ => rusqlite::types::Value::Null,
                    })
                    .collect();
                let placeholders = (1..=cols.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(",");
                let updates = cols
                    .iter()
                    .filter(|c| **c != key_col)
                    .map(|c| format!("{c} = excluded.{c}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                applied += tx.execute(
                    &format!(
                        "INSERT INTO {table} ({}) VALUES ({placeholders})
                         ON CONFLICT({key_col}) DO UPDATE SET {updates}
                         WHERE excluded.updated_at > {table}.updated_at",
                        cols.join(",")
                    ),
                    rusqlite::params_from_iter(values.iter()),
                )?;
            }
        }
        tx.commit()?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }
    fn actor() -> Actor {
        Actor::new("human:test")
    }

    #[test]
    fn test_default_is_ask_first_and_presets_widen_only_what_they_name() {
        let p = Policy::default();
        for class in ACTION_CLASSES {
            assert!(
                matches!(decide(&p, class, None), Decision::Ask(_)),
                "{class}"
            );
        }
        let assistant = Policy::preset("assistant").unwrap();
        assert_eq!(decide(&assistant, "read", None), Decision::Auto);
        assert!(matches!(
            decide(&assistant, "communicate", None),
            Decision::Ask(_)
        ));
        assert!(matches!(
            decide(&assistant, "spend", None),
            Decision::Ask(_)
        ));
        let delegate = Policy::preset("delegate").unwrap();
        assert_eq!(decide(&delegate, "communicate", None), Decision::Auto);
        assert!(
            matches!(decide(&delegate, "spend", None), Decision::Ask(_)),
            "money always asks in presets"
        );
        assert!(matches!(decide(&delegate, "files", None), Decision::Ask(_)));
        assert!(Policy::preset("reckless").is_none());
        // unknown classes are `other`, which asks
        assert!(matches!(
            decide(&delegate, "launch_missiles", None),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn test_taint_rule_and_never() {
        let mut p = Policy::preset("delegate").unwrap();
        let why = Some("imported content (ep_1)");
        // after untrusted reads, data-carrying actions need a human…
        assert!(
            matches!(decide(&p, "communicate", why), Decision::Ask(r) if r.contains("read imported"))
        );
        assert!(matches!(decide(&p, "web", why), Decision::Ask(_)));
        // …and so do calendar invites, record writes and plain reads
        assert!(matches!(decide(&p, "schedule", why), Decision::Ask(_)));
        assert!(matches!(decide(&p, "write_record", why), Decision::Ask(_)));
        assert!(matches!(decide(&p, "read", why), Decision::Ask(r) if r.contains("reads next")));
        // untainted, the preset's automatic classes stay automatic
        assert_eq!(decide(&p, "read", None), Decision::Auto);
        assert_eq!(decide(&p, "schedule", None), Decision::Auto);
        // the guard can be switched off (decision C)
        p.taint_guard = false;
        assert_eq!(decide(&p, "communicate", why), Decision::Auto);
        assert_eq!(decide(&p, "read", why), Decision::Auto);
        // never wins over everything
        p.modes.insert("spend".into(), "never".into());
        assert!(matches!(decide(&p, "spend", None), Decision::Never(_)));
        let classes = vec!["read".to_string(), "spend".to_string()];
        assert!(matches!(decide_all(&p, &classes, None), Decision::Never(_)));
        let mixed = vec!["read".to_string(), "files".to_string()];
        assert!(matches!(decide_all(&p, &mixed, None), Decision::Ask(_)));
    }

    #[test]
    fn test_a_tainted_task_never_runs_on_its_own() {
        let why = Some("imported content (ep_1)");
        let everything_auto = Policy {
            modes: ACTION_CLASSES
                .iter()
                .map(|c| (c.to_string(), "auto".to_string()))
                .collect(),
            ..Policy::default()
        };
        let mut policies: Vec<Policy> = PRESETS
            .iter()
            .map(|(n, _)| Policy::preset(n).unwrap())
            .collect();
        policies.push(everything_auto.clone());
        // Whatever the preset, class or model-chosen class name (unknown
        // ones are `other`), a tainted action asks first…
        for p in &policies {
            for class in ACTION_CLASSES.iter().chain(&["launch_missiles", ""]) {
                assert!(
                    matches!(decide(p, class, why), Decision::Ask(_)),
                    "{class} under {:?}",
                    p.modes
                );
            }
            for needs in [
                vec![],
                vec!["read".to_string()],
                vec!["read".into(), "write_record".into()],
            ] {
                assert!(
                    matches!(decide_all(p, &needs, why), Decision::Ask(_)),
                    "{needs:?}"
                );
            }
        }
        // …except that never stays never.
        let mut p = everything_auto;
        p.modes.insert("spend".into(), "never".into());
        assert!(matches!(decide(&p, "spend", why), Decision::Never(_)));
        assert!(matches!(
            decide_all(&p, &["read".into(), "spend".into()], why),
            Decision::Never(_)
        ));
        // Only `read` is outside the data-carrying list.
        let mut sensitive: Vec<&str> = TAINT_SENSITIVE.to_vec();
        sensitive.push("read");
        sensitive.sort_unstable();
        let mut all = ACTION_CLASSES.to_vec();
        all.sort_unstable();
        assert_eq!(sensitive, all);
    }

    #[test]
    fn test_a_task_that_names_no_class_is_other() {
        let p = Policy::preset("delegate").unwrap();
        assert!(matches!(decide_all(&p, &[], None), Decision::Ask(r) if r.contains("\"other\"")));
        let mut open = p.clone();
        open.modes.insert("other".into(), "never".into());
        assert!(matches!(decide_all(&open, &[], None), Decision::Never(_)));
    }

    #[test]
    fn test_policy_storage_global_and_agent_overrides() {
        let s = store();
        assert_eq!(s.global_policy().unwrap(), Policy::default());
        let stored = s
            .set_policy(
                &actor(),
                "global",
                &json!({
                    "modes": {"read": "auto", "spend": "sometimes", "bogus": "auto"},
                    "taint_guard": true, "tasks_per_day": 999_999
                }),
            )
            .unwrap();
        assert_eq!(stored["modes"]["read"], "auto");
        assert_eq!(
            stored["modes"]["spend"], "ask",
            "an unknown mode is ignored"
        );
        assert!(stored["modes"].get("bogus").is_none());
        assert_eq!(stored["tasks_per_day"], 1000, "limits are clamped");

        let agent = s
            .save_agent(
                &actor(),
                None,
                &AgentIn {
                    name: "Code".into(),
                    harness: "claude_code".into(),
                    config: json!({"workdir": "/tmp/x", "api_key": "LEAK"}),
                    enabled: None,
                },
            )
            .unwrap();
        assert!(
            agent.config.get("api_key").is_none(),
            "secrets never land in config"
        );
        assert!(s.set_policy(&actor(), "ag_nope", &json!({})).is_err());
        s.set_policy(
            &actor(),
            &agent.id,
            &json!({"modes": {"communicate": "never"}}),
        )
        .unwrap();
        let eff = s.effective_policy(Some(&agent.id)).unwrap();
        assert_eq!(eff.mode("read"), "auto", "inherits the global setting");
        assert_eq!(eff.mode("communicate"), "never", "its own override wins");
        assert_eq!(s.effective_policy(None).unwrap().mode("communicate"), "ask");
        assert!(s
            .save_agent(
                &actor(),
                None,
                &AgentIn {
                    name: "x".into(),
                    harness: "skynet".into(),
                    config: json!({}),
                    enabled: None,
                }
            )
            .is_err());
        s.delete_agent(&actor(), &agent.id).unwrap();
        assert!(s.agent(&agent.id).unwrap().is_none());
        assert_eq!(s.agent_policy_overrides(&agent.id).unwrap(), json!({}));
    }

    #[test]
    fn test_task_lifecycle_and_approvals() {
        let s = store();
        let task = s
            .create_agent_task(
                &actor(),
                Some("ask_1"),
                None,
                "email the plumber",
                "Send…",
                &["communicate".into(), "warp".into(), "communicate".into()],
                None,
                false,
            )
            .unwrap();
        assert_eq!(task.status, "proposed");
        assert_eq!(task.classes, vec!["communicate", "other"]);
        let ap = s
            .add_approval(
                &task.id,
                "dispatch",
                None,
                "send an email",
                &json!({}),
                "pending",
                None,
            )
            .unwrap();
        assert_eq!(s.pending_approvals(false).unwrap().len(), 1);
        let decided = s.decide_approval(&ap, true, "human:dashboard").unwrap();
        assert_eq!(decided["status"], "approved");
        assert!(
            s.decide_approval(&ap, false, "human:dashboard").is_err(),
            "decided once"
        );
        s.set_agent_task_status(&actor(), &task.id, "queued", None, None, None)
            .unwrap();
        s.set_agent_task_status(&actor(), &task.id, "running", None, None, None)
            .unwrap();
        assert_eq!(s.agent_tasks_started_today().unwrap(), 1);
        assert_eq!(s.bump_agent_task_actions(&task.id).unwrap(), 1);
        let done = s
            .set_agent_task_status(&actor(), &task.id, "done", Some("sent"), None, None)
            .unwrap();
        assert_eq!(done.result.as_deref(), Some("sent"));
        assert!(
            s.set_agent_task_status(&actor(), &task.id, "running", None, None, None)
                .is_err(),
            "ended stays ended"
        );

        // a sensitive task hides from a locked view, approvals included
        let secret = s
            .create_agent_task(&actor(), None, None, "x", "y", &[], None, true)
            .unwrap();
        s.add_approval(
            &secret.id,
            "dispatch",
            None,
            "z",
            &json!({}),
            "pending",
            None,
        )
        .unwrap();
        assert_eq!(s.agent_tasks(10, false).unwrap().len(), 1);
        assert_eq!(s.agent_tasks(10, true).unwrap().len(), 2);
        assert!(s.pending_approvals(false).unwrap().is_empty());
        assert_eq!(s.pending_approvals(true).unwrap().len(), 1);
    }

    #[test]
    fn test_sync_carries_policy_tasks_and_decisions_not_agents() {
        let a = store();
        let b = store();
        a.save_agent(
            &actor(),
            None,
            &AgentIn {
                name: "Code".into(),
                harness: "claude_code".into(),
                config: json!({}),
                enabled: None,
            },
        )
        .unwrap();
        a.set_policy(&actor(), "global", &json!({"modes": {"read": "auto"}}))
            .unwrap();
        let task = a
            .create_agent_task(
                &actor(),
                None,
                None,
                "r",
                "b",
                &["read".into()],
                None,
                false,
            )
            .unwrap();
        let ap = a
            .add_approval(&task.id, "dispatch", None, "s", &json!({}), "pending", None)
            .unwrap();
        let page = a.agents_sync_export("").unwrap();
        assert!(page.get("agents").is_none(), "agent configs are node-local");
        assert!(b.agents_sync_apply(&page).unwrap() >= 3);
        assert_eq!(b.global_policy().unwrap().mode("read"), "auto");
        assert_eq!(b.agent_task(&task.id).unwrap().unwrap().request, "r");
        // approve on B; the decision reaches A
        std::thread::sleep(std::time::Duration::from_millis(1100));
        b.decide_approval(&ap, true, "human:phone").unwrap();
        a.agents_sync_apply(&b.agents_sync_export("").unwrap())
            .unwrap();
        assert_eq!(a.approval(&ap).unwrap().unwrap()["status"], "approved");
        // the stale pending copy never reopens it
        b.agents_sync_apply(&page).unwrap();
        assert_eq!(b.approval(&ap).unwrap().unwrap()["status"], "approved");

        // the directory: A publishes its agent; B learns where it runs
        let agent = a.agents().unwrap().remove(0);
        a.publish_agent(&agent, "nd_a", "MacBook", false).unwrap();
        b.agents_sync_apply(&a.agents_sync_export("").unwrap())
            .unwrap();
        let dir = b.agent_directory().unwrap();
        assert_eq!(dir.len(), 1);
        assert_eq!(
            (dir[0].0.name.as_str(), dir[0].2.as_str()),
            ("Code", "MacBook")
        );
        assert!(
            b.agents().unwrap().is_empty(),
            "the configuration itself stays on A"
        );
        // moved to B: B adopts it under the same id and republishes
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let adopted = b.adopt_agent(&actor(), &agent).unwrap();
        assert_eq!(adopted.id, agent.id);
        b.publish_agent(&adopted, "nd_b", "Mac mini", false)
            .unwrap();
        a.agents_sync_apply(&b.agents_sync_export("").unwrap())
            .unwrap();
        assert_eq!(a.agent_directory().unwrap()[0].2, "Mac mini");
    }
}
