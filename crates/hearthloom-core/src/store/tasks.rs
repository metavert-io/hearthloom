//! The task engine: the PRESENT tense of the productivity layer. Tasks
//! are mutable operational rows (the intents precedent — daemon-written,
//! mutation-logged, never the record itself); every completion MINTS an
//! append-only episode (source "desk" → derivation=declared). A workout
//! completion mints kind=workout instead, derives `metric_points` rows
//! (rebuildable numeric series for Pulse), and links any same-day Apple
//! Watch workout it overlaps — manual detail and sensor data reconcile
//! instead of double-counting.
//!
//! Task kinds (codi's hashtag polymorphism, made explicit):
//!   plain      — a description
//!   checklist  — payload: {"items": [{"text", "done"}]}
//!   workout    — payload: {"exercises": [{category: cardio|strength|
//!                flexibility, name, minutes?, zone?, reps?, weight?,
//!                seconds?}]}
//!   form       — payload: {"values": {key: value}} against a
//!                task_templates row (template.note_on_complete offers
//!                the task→research→note loop)

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::paths::Home;
use crate::time::now_utc;

/// One derived series point: (day, metric, value, unit).
pub type MetricPoint = (String, String, f64, Option<String>);

pub const TASK_KINDS: &[&str] = &["plain", "checklist", "workout", "form"];
pub const TASK_FREQUENCIES: &[&str] = &["one-time", "daily", "weekly", "monthly"];

/// codi's hashtag capture, kept: pull #tags out of a description,
/// returning (cleaned text, lowercase tags in order of appearance).
pub fn extract_hashtags(text: &str) -> (String, Vec<String>) {
    let mut tags = Vec::new();
    let mut clean = Vec::new();
    for word in text.split_whitespace() {
        if let Some(tag) = word.strip_prefix('#').filter(|t| {
            t.len() >= 2 && t.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        }) {
            let tag = tag.to_lowercase();
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        } else {
            clean.push(word);
        }
    }
    (clean.join(" "), tags)
}

/// Rebuild a task's @[Name] mention links from its description — the
/// note_mentions pattern: derived data, name-resolved, merge-aware at
/// read time; an unknown name simply stays text.
fn index_task_mentions(conn: &rusqlite::Connection, task_id: &str, text: &str) -> Result<()> {
    conn.execute("DELETE FROM task_mentions WHERE task_id = ?1", [task_id])?;
    for (ty, name) in super::notes::extract_mentions_typed(text) {
        if let Some(entity_id) = super::notes::resolve_mention(conn, ty.as_deref(), &name)? {
            conn.execute(
                "INSERT OR IGNORE INTO task_mentions (task_id, entity_id) VALUES (?1, ?2)",
                [task_id, &entity_id],
            )?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub kind: String,
    pub description: String,
    pub tags: Vec<String>,
    pub payload: Option<Value>,
    pub template_id: Option<String>,
    pub frequency: String,
    pub scope: String,
    pub goal_id: Option<String>,
    pub status: String,
    pub remind_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
    pub linked_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskTemplate {
    pub id: String,
    pub name: String,
    pub fields: Value,
    pub note_on_complete: bool,
    pub created_at: String,
}

/// What a completion produced: the minted episode, the next occurrence
/// (recurring tasks), and whether the UI should offer a linked note.
#[derive(Debug, Clone, Serialize)]
pub struct CompletionOutcome {
    pub episode_id: String,
    pub next_task_id: Option<String>,
    pub offer_note: bool,
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    let payload: Option<String> = row.get("payload")?;
    let tags: Option<String> = row.get("tags")?;
    Ok(TaskRow {
        id: row.get("id")?,
        kind: row.get("kind")?,
        description: row.get("description")?,
        tags: tags
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default(),
        payload: payload.and_then(|p| serde_json::from_str(&p).ok()),
        template_id: row.get("template_id")?,
        frequency: row.get("frequency")?,
        scope: row.get("scope")?,
        goal_id: row.get("goal_id")?,
        status: row.get("status")?,
        remind_at: row.get("remind_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        completed_at: row.get("completed_at")?,
        linked_note: row.get("linked_note")?,
    })
}

const TASK_COLUMNS: &str = "id, kind, description, tags, payload, template_id, frequency, \
     scope, goal_id, status, remind_at, created_at, updated_at, completed_at, linked_note";

/// Advance an RFC 3339 timestamp by one frequency step (recurrence).
fn next_occurrence(remind_at: &str, frequency: &str) -> Option<String> {
    let parsed = chrono::DateTime::parse_from_rfc3339(remind_at).ok()?;
    let next = match frequency {
        "daily" => parsed + chrono::Duration::days(1),
        "weekly" => parsed + chrono::Duration::weeks(1),
        "monthly" => parsed
            .checked_add_months(chrono::Months::new(1))
            .unwrap_or(parsed + chrono::Duration::days(30)),
        _ => return None,
    };
    Some(next.to_rfc3339())
}

/// Human-readable body for the minted episode; the raw payload JSON is
/// appended verbatim so metric_points stays rebuildable from episodes.
fn render_completion_body(task: &TaskRow, template: Option<&super::notes::TemplateDef>) -> String {
    let mut body = format!("task: {}\nkind: {}\n", task.description, task.kind);
    if !task.tags.is_empty() {
        body.push_str(&format!("tags: {}\n", task.tags.join(", ")));
    }
    match (task.kind.as_str(), &task.payload) {
        ("checklist", Some(payload)) => {
            if let Some(items) = payload.get("items").and_then(|v| v.as_array()) {
                for item in items {
                    body.push_str(&format!(
                        "- [{}] {}\n",
                        if item.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                            "x"
                        } else {
                            " "
                        },
                        item.get("text").and_then(|t| t.as_str()).unwrap_or("")
                    ));
                }
            }
        }
        ("workout", Some(payload)) => {
            for exercise in payload
                .get("exercises")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let get = |k: &str| exercise.get(k);
                let name = ["name", "cardioType", "exerciseName", "flexibilityName"]
                    .iter()
                    .find_map(|k| get(k).and_then(|v| v.as_str()))
                    .unwrap_or("exercise");
                let category = get("category").and_then(|v| v.as_str()).unwrap_or("");
                let mut line = format!("- {category}: {name}");
                if let Some(minutes) = get("minutes").and_then(|v| v.as_f64()) {
                    line.push_str(&format!(" {minutes}min"));
                }
                if let Some(zone) = get("zone").and_then(|v| v.as_str()) {
                    line.push_str(&format!(" {zone}"));
                }
                if let Some(distance) = get("distance").and_then(|v| v.as_f64()) {
                    line.push_str(&format!(" {distance}mi"));
                }
                if let (Some(reps), Some(weight)) = (
                    get("reps").and_then(|v| v.as_f64()),
                    get("weight").and_then(|v| v.as_f64()),
                ) {
                    line.push_str(&format!(" {reps}x{weight}lb"));
                }
                if let Some(seconds) = get("seconds").and_then(|v| v.as_f64()) {
                    line.push_str(&format!(" {seconds}s"));
                }
                body.push_str(&line);
                body.push('\n');
            }
        }
        ("form", Some(payload)) => {
            if let Some(name) = template.map(|t| t.name.as_str()) {
                body.push_str(&format!("template: {name}\n"));
            }
            if let Some(values) = payload.get("values").and_then(|v| v.as_object()) {
                for (key, value) in values {
                    body.push_str(&format!("{key}: {value}\n"));
                }
            }
        }
        _ => {}
    }
    if let Some(payload) = &task.payload {
        body.push_str(&format!("\npayload: {payload}\n"));
    }
    body
}

/// Derive numeric series rows from a workout payload.
fn workout_metrics(payload: &Value) -> Vec<(String, f64, &'static str)> {
    let mut out = Vec::new();
    let slug = |s: &str| -> String {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("_")
    };
    for exercise in payload
        .get("exercises")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let get = |k: &str| exercise.get(k);
        let category = get("category").and_then(|v| v.as_str()).unwrap_or("");
        match category {
            "cardio" => {
                let name = ["name", "cardioType"]
                    .iter()
                    .find_map(|k| get(k).and_then(|v| v.as_str()))
                    .unwrap_or("cardio");
                if let Some(minutes) = get("minutes").and_then(|v| v.as_f64()) {
                    out.push((format!("cardio/{}/minutes", slug(name)), minutes, "min"));
                }
                if let Some(distance) = get("distance").and_then(|v| v.as_f64()) {
                    out.push((format!("cardio/{}/distance", slug(name)), distance, "mi"));
                }
            }
            "strength" => {
                let name = ["name", "exerciseName"]
                    .iter()
                    .find_map(|k| get(k).and_then(|v| v.as_str()))
                    .unwrap_or("strength");
                let reps = get("reps").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let weight = get("weight").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if reps > 0.0 && weight > 0.0 {
                    out.push((
                        format!("strength/{}/volume", slug(name)),
                        reps * weight,
                        "lb·reps",
                    ));
                    out.push((format!("strength/{}/weight", slug(name)), weight, "lb"));
                    out.push((format!("strength/{}/reps", slug(name)), reps, "reps"));
                }
            }
            "flexibility" => {
                let name = ["name", "flexibilityName"]
                    .iter()
                    .find_map(|k| get(k).and_then(|v| v.as_str()))
                    .unwrap_or("flexibility");
                if let Some(seconds) = get("seconds").and_then(|v| v.as_f64()) {
                    out.push((
                        format!("flexibility/{}/seconds", slug(name)),
                        seconds,
                        "s",
                    ));
                }
            }
            _ => {}
        }
    }
    // Aggregate volumes are summed per metric key (3 sets of bench = one
    // volume row); UNIQUE(episode_id, metric) makes this necessary.
    let mut merged: std::collections::BTreeMap<String, (f64, &'static str)> =
        std::collections::BTreeMap::new();
    for (metric, value, unit) in out {
        let entry = merged.entry(metric).or_insert((0.0, unit));
        // weight metrics keep the max; everything else sums
        if entry.0 > 0.0 && unit == "lb" {
            entry.0 = entry.0.max(value);
        } else {
            entry.0 += value;
        }
    }
    merged
        .into_iter()
        .map(|(metric, (value, unit))| (metric, value, unit))
        .collect()
}

impl Store {
    /// Create a task. The description is hashtag-aware (codi's flow):
    /// #urgent/#important order the desk, #daily/#weekly/#monthly set
    /// the frequency, #checklist/#workout set the kind, and a tag
    /// matching a template's name binds that form — everything else is
    /// a freeform tag. Explicit arguments win over hashtag hints.
    /// `carry_tags` preserves tags across recurrence spawns (the clean
    /// description no longer carries the original hashtags).
    #[allow(clippy::too_many_arguments)]
    pub fn add_task(
        &self,
        actor: &Actor,
        kind: &str,
        description: &str,
        payload: Option<&Value>,
        template_id: Option<&str>,
        frequency: &str,
        scope: &str,
        goal_id: Option<&str>,
        remind_at: Option<&str>,
        carry_tags: &[String],
    ) -> Result<String> {
        let (clean, mut tags) = extract_hashtags(description.trim());
        for tag in carry_tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        let description = clean.trim();
        if description.is_empty() {
            return Err(anyhow!("task description is required"));
        }
        // Smart tags: explicit args win; hashtags fill the gaps.
        let mut kind = kind.to_string();
        let mut frequency = frequency.to_string();
        let mut template_id = template_id.map(String::from);
        let mut remind_at = remind_at.map(String::from);
        let conn = self.lock();
        for tag in &tags {
            match tag.as_str() {
                "checklist" | "workout" | "form" if kind == "plain" => kind = tag.clone(),
                "daily" | "weekly" | "monthly" if frequency == "one-time" => {
                    frequency = tag.clone()
                }
                "today" | "tomorrow" if remind_at.is_none() => {
                    let day = chrono::Local::now().date_naive()
                        + chrono::Duration::days(if tag == "tomorrow" { 1 } else { 0 });
                    remind_at = Some(format!("{day}T12:00:00Z"));
                }
                _ if template_id.is_none() => {
                    // a tag naming a TEMPLATE NOTE binds its data definition
                    let hit: Option<String> = conn
                        .query_row(
                            "SELECT id FROM notes_index
                             WHERE kind = 'template' AND status = 'active'
                               AND lower(replace(title, ' ', '-')) = ?1",
                            [tag],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if let Some(template) = hit {
                        template_id = Some(template);
                        kind = "form".into();
                    }
                }
                _ => {}
            }
        }
        if !TASK_KINDS.contains(&kind.as_str()) {
            return Err(anyhow!("unknown task kind {kind:?}"));
        }
        if !TASK_FREQUENCIES.contains(&frequency.as_str()) {
            return Err(anyhow!("unknown frequency {frequency:?}"));
        }
        if let Some(template) = &template_id {
            let known: i64 = conn.query_row(
                "SELECT COUNT(*) FROM notes_index WHERE id = ?1 AND kind = 'template'",
                [template],
                |row| row.get(0),
            )?;
            if known == 0 {
                return Err(anyhow!("unknown template {template:?}"));
            }
        }
        let id = ids::task_id();
        let now = now_utc();
        conn.execute(
            "INSERT INTO tasks (id, kind, description, tags, payload, template_id,
                 frequency, scope, goal_id, remind_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            rusqlite::params![
                id,
                kind,
                description,
                serde_json::to_string(&tags)?,
                payload.map(|p| p.to_string()),
                template_id,
                frequency,
                scope,
                goal_id,
                remind_at,
                now,
            ],
        )?;
        index_task_mentions(&conn, &id, description)?;
        record_mutation(
            &conn,
            actor,
            "task_added",
            &format!("task_added:{id}"),
            &json!({"id": id, "kind": kind, "description": description, "tags": tags}),
        )?;
        super::journal_row(&conn, actor, "task", "tasks", &id, "create")?;
        Ok(id)
    }

    /// Open + backlog tasks (and recently completed for the Desk's
    /// satisfaction strip). Vault-guarded.
    pub fn list_tasks(&self, status: Option<&str>, limit: usize) -> Result<Vec<TaskRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        // Archived (and legacy 'deleted') tasks are hidden by default;
        // asking for status=archived opens the drawer.
        let mut sql = format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE 1=1{guard}"
        );
        match status {
            Some("archived") => sql.push_str(" AND status IN ('archived', 'deleted')"),
            Some(_) => sql.push_str(" AND status = ?1"),
            None => sql.push_str(" AND status NOT IN ('archived', 'deleted')"),
        }
        // Completed tasks read as a history — most-recently-finished first.
        if status == Some("done") {
            sql.push_str(" ORDER BY completed_at DESC, updated_at DESC LIMIT ");
        } else {
            sql.push_str(
                " ORDER BY CASE status WHEN 'open' THEN 0 WHEN 'backlog' THEN 1 ELSE 2 END,
                  CASE WHEN tags LIKE '%\"urgent\"%' THEN 0
                       WHEN tags LIKE '%\"important\"%' THEN 1 ELSE 2 END,
                  COALESCE(remind_at, '9999') , created_at DESC LIMIT ",
            );
        }
        sql.push_str(&limit.to_string());
        let mut stmt = conn.prepare(&sql)?;
        let rows = match status {
            Some(s) if s != "archived" => stmt.query_map([s], task_from_row)?,
            _ => stmt.query_map([], task_from_row)?,
        };
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Restore a task from its operational-journal history — re-inserts the
    /// last known post-image (works for an archived, edited-away, or purged
    /// task). The recovery path the 2026-07-27 incident was missing.
    pub fn restore_task(&self, actor: &Actor, id: &str) -> Result<()> {
        let conn = self.lock();
        let post: Option<String> = conn
            .query_row(
                "SELECT post_image FROM op_journal
                 WHERE entity_kind='task' AND entity_id=?1 AND post_image IS NOT NULL
                 ORDER BY ts DESC, seq DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(post) = post else {
            return Err(anyhow!("no journal history for task {id}"));
        };
        let obj: Value = serde_json::from_str(&post)?;
        let Some(map) = obj.as_object() else {
            return Err(anyhow!("bad journal post-image for {id}"));
        };
        let mut map = map.clone();
        // A tombstone whose post-image is status=deleted would restore an
        // INVISIBLE row (soft-deleted-then-purged path) — a restore's whole
        // point is that the task comes back where the user can see it.
        if map.get("status").and_then(|s| s.as_str()) == Some("deleted") {
            map.insert("status".into(), Value::from("open"));
        }
        let cols: Vec<&String> = map.keys().collect();
        let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT OR REPLACE INTO tasks ({}) VALUES ({})",
            cols.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
            placeholders.join(", ")
        );
        let params: Vec<rusqlite::types::Value> =
            cols.iter().map(|c| super::json_to_sql(&map[c.as_str()])).collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))?;
        if let Some(desc) = map.get("description").and_then(|v| v.as_str()) {
            index_task_mentions(&conn, id, desc)?;
        }
        super::journal_row(&conn, actor, "task", "tasks", id, "update")?;
        Ok(())
    }

    /// Tasks that were purged (gone from `tasks`) but recoverable from a
    /// delete-tombstone in the journal — the "recently deleted" list.
    pub fn deleted_tasks(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT entity_id, post_image, ts FROM op_journal
             WHERE entity_kind='task' AND op='delete'
               AND entity_id NOT IN (SELECT id FROM tasks)
             GROUP BY entity_id
             ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            let id: String = r.get(0)?;
            let post: Option<String> = r.get(1)?;
            let ts: String = r.get(2)?;
            let desc = post
                .as_deref()
                .and_then(|p| serde_json::from_str::<Value>(p).ok())
                .and_then(|o| o.get("description").and_then(|d| d.as_str()).map(String::from))
                .unwrap_or_default();
            Ok((id, desc, ts))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Permanently remove an ARCHIVED task row. Tasks are operational
    /// state, not the record — history already minted as episodes stays
    /// untouched; the purge itself is mutation-logged.
    pub fn purge_task(&self, actor: &Actor, id: &str) -> Result<()> {
        let conn = self.lock();
        // Capture the content as a delete-tombstone in the journal BEFORE the
        // row is gone, so even a purged task remains restorable.
        let purgeable = super::row_as_json(&conn, "tasks", id)?
            .and_then(|r| r.get("status").and_then(|s| s.as_str()).map(String::from))
            .map(|s| s == "archived" || s == "deleted")
            .unwrap_or(false);
        if !purgeable {
            return Err(anyhow!("no archived task {id} (archive before purging)"));
        }
        super::journal_row(&conn, actor, "task", "tasks", id, "delete")?;
        conn.execute(
            "DELETE FROM tasks WHERE id = ?1 AND status IN ('archived', 'deleted')",
            [id],
        )?;
        conn.execute("DELETE FROM task_mentions WHERE task_id = ?1", [id])?;
        record_mutation(
            &conn,
            actor,
            "task_purged",
            &format!("task_purged:{id}"),
            &json!({"id": id}),
        )?;
        Ok(())
    }

    /// Tasks @-mentioning an entity ("Send report to @[Sam]"), open
    /// first, newest within status. Merge-group aware, vault-guarded —
    /// the dossier's "on your desk about them" section.
    pub fn tasks_mentioning(&self, entity_id: &str, limit: usize) -> Result<Vec<TaskRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let winner = super::graph::resolve_merged(&conn, entity_id)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks
             WHERE status NOT IN ('archived', 'deleted'){guard}
               AND id IN (SELECT task_id FROM task_mentions WHERE entity_id = ?1)
             ORDER BY CASE status WHEN 'open' THEN 0 WHEN 'backlog' THEN 1 ELSE 2 END,
                      updated_at DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([&winner], task_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn task(&self, id: &str) -> Result<Option<TaskRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1{guard}"),
            [id],
            task_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Edit description/payload/remind/backlog-status/goal. Completion
    /// has its own path; deletion is soft.
    #[allow(clippy::too_many_arguments)]
    pub fn update_task(
        &self,
        actor: &Actor,
        id: &str,
        description: Option<&str>,
        payload: Option<&Value>,
        status: Option<&str>,
        remind_at: Option<Option<&str>>,
        goal_id: Option<Option<&str>>,
    ) -> Result<()> {
        // "deleted" is accepted as a legacy alias for archived — a
        // task leaves the desk softly; perma-delete is its own path.
        let status = status.map(|s| if s == "deleted" { "archived" } else { s });
        if let Some(status) = status {
            if !matches!(status, "open" | "backlog" | "archived") {
                return Err(anyhow!(
                    "status must be open|backlog|archived (completion has its own path)"
                ));
            }
        }
        let conn = self.lock();
        let now = now_utc();
        let mut changed = 0;
        if let Some(raw) = description.map(str::trim).filter(|d| !d.is_empty()) {
            // Edits stay hashtag-aware: "#urgent fix the fence" retags.
            let (clean, new_tags) = extract_hashtags(raw);
            let clean = if clean.trim().is_empty() { raw } else { clean.trim() };
            changed += conn.execute(
                "UPDATE tasks SET description = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![clean, now, id],
            )?;
            index_task_mentions(&conn, id, clean)?;
            if !new_tags.is_empty() {
                let existing: Option<String> = conn.query_row(
                    "SELECT tags FROM tasks WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )?;
                let mut tags: Vec<String> = existing
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default();
                for tag in new_tags {
                    if !tags.contains(&tag) {
                        tags.push(tag);
                    }
                }
                conn.execute(
                    "UPDATE tasks SET tags = ?1 WHERE id = ?2",
                    rusqlite::params![serde_json::to_string(&tags)?, id],
                )?;
            }
        }
        if let Some(payload) = payload {
            changed += conn.execute(
                "UPDATE tasks SET payload = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![payload.to_string(), now, id],
            )?;
        }
        if let Some(status) = status {
            changed += conn.execute(
                "UPDATE tasks SET status = ?1, updated_at = ?2
                 WHERE id = ?3 AND status != 'done'",
                rusqlite::params![status, now, id],
            )?;
        }
        if let Some(remind_at) = remind_at {
            changed += conn.execute(
                "UPDATE tasks SET remind_at = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![remind_at, now, id],
            )?;
        }
        if let Some(goal_id) = goal_id {
            changed += conn.execute(
                "UPDATE tasks SET goal_id = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![goal_id, now, id],
            )?;
        }
        if changed == 0 {
            return Err(anyhow!("no such task (or nothing to change): {id}"));
        }
        record_mutation(
            &conn,
            actor,
            "task_updated",
            &format!("task_updated:{id}:{now}"),
            &json!({"id": id}),
        )?;
        super::journal_row(&conn, actor, "task", "tasks", id, "update")?;
        Ok(())
    }

    /// Complete a task: mint the episode (kind task_done, or workout with
    /// derived metric_points + Apple Health reconciliation), spawn the
    /// next occurrence for recurring tasks, report whether to offer a
    /// linked note (the task → research → note loop).
    pub fn complete_task(
        &self,
        actor: &Actor,
        home: &Home,
        id: &str,
        final_payload: Option<&Value>,
    ) -> Result<CompletionOutcome> {
        let now = now_utc();
        // Phase 1 under the lock: flip status, gather what the mint needs.
        let (task, template): (TaskRow, Option<(String, String, String)>) = {
            let conn = self.lock();
            if let Some(payload) = final_payload {
                conn.execute(
                    "UPDATE tasks SET payload = ?1 WHERE id = ?2",
                    rusqlite::params![payload.to_string(), id],
                )?;
            }
            let changed = conn.execute(
                "UPDATE tasks SET status = 'done', completed_at = ?1, updated_at = ?1
                 WHERE id = ?2 AND status IN ('open', 'backlog')",
                rusqlite::params![now, id],
            )?;
            if changed == 0 {
                return Err(anyhow!("no open task {id}"));
            }
            let task = conn.query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
                [id],
                task_from_row,
            )?;
            // template = a template NOTE (id, title, file path) — read
            // the row here, parse the file after the lock drops.
            let template_row: Option<(String, String, String)> = match &task.template_id {
                Some(template_id) => conn
                    .query_row(
                        "SELECT id, title, path FROM notes_index
                         WHERE id = ?1 AND kind = 'template'",
                        [template_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?,
                None => None,
            };
            record_mutation(
                &conn,
                actor,
                "task_completed",
                &format!("task_completed:{id}:{now}"),
                &json!({"id": id, "kind": task.kind}),
            )?;
            // completion mints an episode (durable), but also journal the
            // done-state so the operational history is complete
            super::journal_row(&conn, actor, "task", "tasks", id, "update")?;
            (task, template_row)
        };
        let template = template.map(|(tid, title, path)| {
            let body = home.read_note(&path).unwrap_or_default();
            super::notes::parse_template(&tid, &title, &body)
        });

        // Phase 2, lock released: mint the episode via the one write path.
        let episode_kind = if task.kind == "workout" { "workout" } else { "task_done" };
        let body = render_completion_body(&task, template.as_ref());
        let draft = crate::ingest::EpisodeDraft {
            source: "desk".into(),
            source_ref: format!("{episode_kind}:{id}:{now}"),
            kind: episode_kind.into(),
            title: task.description.clone(),
            body,
            event_time: crate::time::EventTime {
                utc: Some(now.clone()),
                tz_offset_minutes: None,
                uncertainty: crate::time::TimeUncertainty::Minute,
            },
            trust: "self".into(),
            scope: Some(task.scope.clone()),
            entities: vec![],
            edges: vec![],
        };
        self.apply_batch(
            home,
            &crate::ingest::IngestBatch {
                actor: actor.actor.clone(),
                session_id: actor.session_id.clone(),
                episodes: vec![draft],
            },
        )?;
        let episode_id = ids::episode_id("desk", &format!("{episode_kind}:{id}:{now}"));

        // Phase 3: workout numerics + reconciliation with the Watch.
        if task.kind == "workout" {
            if let Some(payload) = &task.payload {
                let day = &now[..10];
                let conn = self.lock();
                for (metric, value, unit) in workout_metrics(payload) {
                    conn.execute(
                        "INSERT OR IGNORE INTO metric_points
                             (episode_id, day, metric, value, unit)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![episode_id, day, metric, value, unit],
                    )?;
                }
                // Same-day Apple Health workout → link, don't double-count:
                // the manual record carries sets/reps the Watch can't see;
                // the Watch carries heart data the human didn't type.
                let watch: Option<String> = conn
                    .query_row(
                        "SELECT id FROM episodes
                         WHERE source = 'apple-health' AND kind = 'workout' AND superseded_by IS NULL
                           AND substr(COALESCE(event_time, ingest_time), 1, 10) = ?1
                         ORDER BY event_time DESC LIMIT 1",
                        [day],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(watch_id) = watch {
                    record_mutation(
                        &conn,
                        actor,
                        "workout_reconciled",
                        &format!("workout_reconciled:{episode_id}:{watch_id}"),
                        &json!({"manual": episode_id, "watch": watch_id}),
                    )?;
                }
            }
        }

        // Phase 4: recurrence — completing a daily spawns tomorrow's.
        let next_task_id = if task.frequency != "one-time" {
            let next_remind = task
                .remind_at
                .as_deref()
                .and_then(|r| next_occurrence(r, &task.frequency))
                .or_else(|| next_occurrence(&now, &task.frequency));
            Some(self.add_task(
                actor,
                &task.kind,
                &task.description,
                // Checklists recur unchecked; forms recur empty.
                match task.kind.as_str() {
                    "checklist" => task.payload.clone().map(|p| reset_checklist(&p)),
                    "form" | "workout" => None,
                    _ => task.payload.clone(),
                }
                .as_ref(),
                task.template_id.as_deref(),
                &task.frequency,
                &task.scope,
                task.goal_id.as_deref(),
                next_remind.as_deref(),
                &task.tags,
            )?)
        } else {
            None
        };

        Ok(CompletionOutcome {
            episode_id,
            next_task_id,
            offer_note: template.as_ref().map(|t| t.note_on_complete).unwrap_or(false),
        })
    }

    /// Un-complete a done task: it returns to the desk as an open item and
    /// its completion timestamp is cleared. The `task_done` episode that
    /// completion minted stays in the record (episodes are append-only) —
    /// the history honestly shows it was finished, then reopened. Only the
    /// operational task state flips back; recurrence spawns are left alone.
    pub fn uncomplete_task(&self, actor: &Actor, id: &str) -> Result<()> {
        let conn = self.lock();
        let now = now_utc();
        let changed = conn.execute(
            "UPDATE tasks SET status = 'open', completed_at = NULL, updated_at = ?1
             WHERE id = ?2 AND status = 'done'",
            rusqlite::params![now, id],
        )?;
        if changed == 0 {
            return Err(anyhow!("no completed task {id}"));
        }
        record_mutation(
            &conn,
            actor,
            "task_reopened",
            &format!("task_reopened:{id}:{now}"),
            &json!({"id": id}),
        )?;
        super::journal_row(&conn, actor, "task", "tasks", id, "update")?;
        Ok(())
    }

    pub fn set_task_note(&self, id: &str, note_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE tasks SET linked_note = ?1 WHERE id = ?2",
            [note_id, id],
        )?;
        Ok(())
    }

    /// The latest use of each exercise — the workout logger's memory
    /// (codi's RecentExercises, rebuilt): folded newest-first from done
    /// workout tasks' payloads, then from metric_points (which the
    /// codi-era backfill seeds), so "bench press" comes back with last
    /// time's numbers. Vault-guarded on both legs.
    pub fn recent_exercises(&self) -> Result<Vec<Value>> {
        let guard = self.scope_guard("scope");
        let episode_guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let mut seen: Vec<(String, Value)> = Vec::new(); // (key, exercise)
        let mut remember = |key: String, value: Value| {
            if !seen.iter().any(|(k, _)| *k == key) {
                seen.push((key, value));
            }
        };

        // Leg 1: full-fidelity payloads from completed workout tasks.
        let mut stmt = conn.prepare(&format!(
            "SELECT payload FROM tasks
             WHERE kind = 'workout' AND status = 'done' AND payload IS NOT NULL{guard}
             ORDER BY completed_at DESC LIMIT 60"
        ))?;
        let payloads: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        for payload in payloads {
            let Ok(value) = serde_json::from_str::<Value>(&payload) else { continue };
            for exercise in value
                .get("exercises")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let name = exercise.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let category = exercise.get("category").and_then(|v| v.as_str()).unwrap_or("");
                if !name.is_empty() {
                    remember(format!("{category}/{}", name.to_lowercase()), exercise.clone());
                }
            }
        }

        // Leg 2: metric_points (slugged names, last weight/minutes).
        let mut stmt = conn.prepare(&format!(
            "SELECT p.metric, p.value FROM metric_points p
             JOIN episodes e ON e.id = p.episode_id
             WHERE e.superseded_by IS NULL{episode_guard}
             ORDER BY p.day DESC LIMIT 400"
        ))?;
        let rows: Vec<(String, f64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for (metric, value) in rows {
            let parts: Vec<&str> = metric.split('/').collect();
            let [category, slug, measure] = parts.as_slice() else { continue };
            let name = slug.replace('_', " ");
            let key = format!("{category}/{name}");
            match (*category, *measure) {
                ("strength", "weight") => remember(
                    key,
                    json!({"category": "strength", "name": name, "weight": value}),
                ),
                ("cardio", "minutes") => remember(
                    key,
                    json!({"category": "cardio", "name": name, "minutes": value}),
                ),
                ("flexibility", "seconds") => remember(
                    key,
                    json!({"category": "flexibility", "name": name, "seconds": value}),
                ),
                _ => {}
            }
        }
        Ok(seen.into_iter().map(|(_, v)| v).take(40).collect())
    }

    /// Codi-era workout episodes carry their payload JSON in-body but
    /// never got metric_points (the harvest predates the derivation).
    /// Backfill them — derived data, rebuildable, exactly what the
    /// metric_points table is for. Sources desk|codi only: apple-health
    /// workouts have no payload line and would rescan forever.
    pub fn backfill_workout_metrics(&self, home: &Home, cap: usize) -> Result<usize> {
        let pending: Vec<(String, String)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT id, substr(COALESCE(event_time, ingest_time), 1, 10)
                 FROM episodes
                 WHERE kind = 'workout' AND source IN ('desk', 'codi') AND superseded_by IS NULL
                   AND id NOT IN (SELECT DISTINCT episode_id FROM metric_points)
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map([cap as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut minted = 0usize;
        for (episode_id, day) in pending {
            // content files live under raw/<date>/; the row knows the path
            let body: Option<String> = {
                let conn = self.lock();
                conn.query_row(
                    "SELECT content_ref FROM episodes WHERE id = ?1 /* all-episodes: a row from the live list above */",
                    [&episode_id],
                    |row| row.get::<_, Option<String>>(0),
                )?
            };
            let Some(content_ref) = body else { continue };
            let Ok(text) = std::fs::read_to_string(home.root().join(&content_ref)) else {
                continue;
            };
            let Some(payload_line) = text.lines().find_map(|l| l.strip_prefix("payload: "))
            else {
                continue;
            };
            let Ok(payload) = serde_json::from_str::<Value>(payload_line) else { continue };
            let conn = self.lock();
            for (metric, value, unit) in workout_metrics(&payload) {
                minted += conn.execute(
                    "INSERT OR IGNORE INTO metric_points
                         (episode_id, day, metric, value, unit)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![episode_id, day, metric, value, unit],
                )?;
            }
        }
        Ok(minted)
    }

    /// Open tasks that belong to today: due today, overdue, or carrying
    /// no reminder at all (an unreminded open task IS today's desk).
    /// Vault-guarded — the nav badge must not count private tasks while
    /// locked.
    pub fn open_tasks_today(&self) -> Result<i64> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM tasks
                 WHERE status = 'open'{guard}
                   AND (remind_at IS NULL
                        OR substr(remind_at, 1, 10) <= date('now', 'localtime'))"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Recently completed tasks per day (the Desk satisfaction strip and
    /// the Pulse task-completions metric). Vault-guarded.
    pub fn task_completions_by_day(&self, days: i64) -> Result<Vec<(String, i64)>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT substr(completed_at, 1, 10) AS day, COUNT(*)
             FROM tasks WHERE status = 'done' AND completed_at IS NOT NULL{guard}
               AND julianday(completed_at) >= julianday('now') - ?1
             GROUP BY day ORDER BY day"
        ))?;
        let rows = stmt.query_map([days], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Derived numeric series (Pulse's native leg). `metric_like` is a
    /// SQL LIKE pattern (e.g. `strength/%/volume`); vault-guarded via
    /// the episodes join.
    pub fn metric_series(
        &self,
        metric_like: &str,
        from: &str,
        to: &str,
        limit: usize,
    ) -> Result<Vec<MetricPoint>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT p.day, p.metric, p.value, p.unit
             FROM metric_points p JOIN episodes e ON e.id = p.episode_id
             WHERE p.metric LIKE ?1 AND p.day BETWEEN ?2 AND ?3 AND e.superseded_by IS NULL{guard}
             ORDER BY p.day LIMIT {limit}"
        ))?;
        let rows = stmt.query_map(rusqlite::params![metric_like, from, to], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Distinct metric keys present (Pulse's specimen drawer).
    pub fn metric_names(&self) -> Result<Vec<String>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT p.metric FROM metric_points p
             JOIN episodes e ON e.id = p.episode_id WHERE e.superseded_by IS NULL{guard}
             ORDER BY p.metric"
        ))?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Upsert one device reminder into the tasks model
    /// (POST /v1/ingest/reminders). The task id is DETERMINISTIC from
    /// the reminder uid (`tk_rem_<sha256 prefix>`), so a re-sync — or
    /// the same phone posting to the other peer — upserts the same row
    /// instead of duplicating; the operational mirror then LWWs the
    /// copies together. Journaled like every operational write (the
    /// v43 durability law). Returns "new" | "updated" | "unchanged".
    ///
    /// Completion flows ONE way: a completed reminder marks the task
    /// done, but an open reminder never re-opens a task the human
    /// completed on the Desk (their gesture wins until the phone's next
    /// state catches up).
    pub fn reminder_upsert(
        &self,
        actor: &Actor,
        uid: &str,
        title: &str,
        due: Option<&str>,
        completed: bool,
        list: Option<&str>,
    ) -> Result<&'static str> {
        let title = title.trim();
        if title.is_empty() || uid.trim().is_empty() {
            return Err(anyhow!("reminder uid and title are required"));
        }
        let id = format!("tk_rem_{}", &ids::content_hash(uid.as_bytes())[..24]);
        let mut tags = vec!["reminder".to_string()];
        if let Some(l) = list {
            let slug = l.trim().to_lowercase().replace(' ', "-");
            if !slug.is_empty() {
                tags.push(slug);
            }
        }
        let now = now_utc();
        let conn = self.lock();
        let existing: Option<(String, Option<String>, String)> = conn
            .query_row(
                "SELECT description, remind_at, status FROM tasks WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let outcome = match existing {
            None => {
                conn.execute(
                    "INSERT INTO tasks (id, kind, description, tags, payload, template_id,
                         frequency, scope, goal_id, remind_at, status, completed_at,
                         created_at, updated_at)
                     VALUES (?1, 'plain', ?2, ?3, NULL, NULL, 'one-time', 'personal',
                             NULL, ?4, ?5, ?6, ?7, ?7)",
                    rusqlite::params![
                        id, title, serde_json::to_string(&tags)?, due,
                        if completed { "done" } else { "open" },
                        if completed { Some(now.as_str()) } else { None },
                        now
                    ],
                )?;
                record_mutation(
                    &conn,
                    actor,
                    "task_added",
                    &format!("task_added:{id}"),
                    &json!({"id": id, "kind": "plain", "description": title, "tags": tags}),
                )?;
                super::journal_row(&conn, actor, "task", "tasks", &id, "create")?;
                "new"
            }
            Some((desc, remind_at, status)) => {
                let want_done = completed && status != "done";
                let changed = desc != title || remind_at.as_deref() != due || want_done;
                if !changed {
                    return Ok("unchanged");
                }
                conn.execute(
                    "UPDATE tasks SET description = ?2, remind_at = ?3,
                         status = CASE WHEN ?4 THEN 'done' ELSE status END,
                         completed_at = CASE WHEN ?4 THEN ?5 ELSE completed_at END,
                         updated_at = ?5
                     WHERE id = ?1",
                    rusqlite::params![id, title, due, want_done, now],
                )?;
                super::journal_row(&conn, actor, "task", "tasks", &id, "update")?;
                "updated"
            }
        };
        Ok(outcome)
    }
}

/// A recurring checklist starts its next life unchecked.
fn reset_checklist(payload: &Value) -> Value {
    let mut next = payload.clone();
    if let Some(items) = next.get_mut("items").and_then(|v| v.as_array_mut()) {
        for item in items {
            if let Some(obj) = item.as_object_mut() {
                obj.insert("done".into(), Value::Bool(false));
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> Actor {
        Actor::new("human:test")
    }

    fn setup() -> (tempfile::TempDir, Home, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        (tmp, home, store)
    }

    #[test]
    fn test_task_mentions_index_and_query() {
        let (_tmp, _home, store) = setup();
        // A known person to mention…
        let conn = store.lock();
        conn.execute(
            "INSERT INTO entities (id, type, name, scope, created_at)
             VALUES ('en_ada', 'Person', 'Ada Lovelace', 'work', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        drop(conn);

        let id = store
            .add_task(&actor(), "plain", "Send report to @[Ada Lovelace] by Friday",
                None, None, "one-time", "personal", None, None, &[])
            .unwrap();
        let hits = store.tasks_mentioning("en_ada", 10).unwrap();
        assert_eq!(hits.len(), 1, "picked mention links the task to the entity");
        assert_eq!(hits[0].id, id);

        // Bare @text never links; editing away the mention unlinks.
        store
            .update_task(&actor(), &id, Some("email @ada instead"), None, None, None, None)
            .unwrap();
        assert!(store.tasks_mentioning("en_ada", 10).unwrap().is_empty());
    }

    #[test]
    fn test_task_lifecycle_and_recurrence() {
        let (_tmp, home, store) = setup();

        let id = store
            .add_task(
                &actor(),
                "checklist",
                "morning routine",
                Some(&json!({"items": [{"text": "stretch", "done": false}]})),
                None,
                "daily",
                "personal",
                None,
                Some("2026-07-09T07:00:00+00:00"),
                &[],
            )
            .unwrap();
        assert_eq!(store.list_tasks(Some("open"), 50).unwrap().len(), 1);

        let outcome = store
            .complete_task(
                &actor(),
                &home,
                &id,
                Some(&json!({"items": [{"text": "stretch", "done": true}]})),
            )
            .unwrap();
        // The record: a declared task_done episode with the checklist.
        let episode = store.episode(&outcome.episode_id).unwrap().unwrap();
        assert_eq!(episode.kind, "task_done");
        assert_eq!(episode.derivation, "declared");
        assert!(store
            .episode_body(&episode.id)
            .unwrap()
            .unwrap()
            .contains("[x] stretch"));
        // Recurrence: tomorrow's copy exists, unchecked, reminder advanced.
        let next = store.task(&outcome.next_task_id.unwrap()).unwrap().unwrap();
        assert_eq!(next.status, "open");
        assert!(next.remind_at.unwrap().starts_with("2026-07-10"));
        assert_eq!(
            next.payload.unwrap()["items"][0]["done"],
            Value::Bool(false)
        );
        // Double-complete fails; the original is done.
        assert!(store.complete_task(&actor(), &home, &id, None).is_err());
    }

    #[test]
    fn test_workout_completion_derives_metrics() {
        let (_tmp, home, store) = setup();
        let id = store
            .add_task(
                &actor(),
                "workout",
                "push day",
                Some(&json!({"exercises": [
                    {"category": "strength", "name": "bench press", "reps": 10, "weight": 135},
                    {"category": "strength", "name": "bench press", "reps": 8, "weight": 155},
                    {"category": "cardio", "name": "treadmill", "minutes": 20, "zone": "zone2"}
                ]})),
                None,
                "one-time",
                "health",
                None,
                None,
                &[],
            )
            .unwrap();
        let outcome = store.complete_task(&actor(), &home, &id, None).unwrap();
        let episode = store.episode(&outcome.episode_id).unwrap().unwrap();
        assert_eq!(episode.kind, "workout");

        let conn = store.lock();
        let volume: f64 = conn
            .query_row(
                "SELECT value FROM metric_points
                 WHERE episode_id = ?1 AND metric = 'strength/bench_press/volume'",
                [&outcome.episode_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(volume, 10.0 * 135.0 + 8.0 * 155.0);
        let max_weight: f64 = conn
            .query_row(
                "SELECT value FROM metric_points
                 WHERE episode_id = ?1 AND metric = 'strength/bench_press/weight'",
                [&outcome.episode_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(max_weight, 155.0);
        let cardio: f64 = conn
            .query_row(
                "SELECT value FROM metric_points
                 WHERE episode_id = ?1 AND metric = 'cardio/treadmill/minutes'",
                [&outcome.episode_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cardio, 20.0);
    }

    #[test]
    fn test_form_template_offers_note() {
        let (_tmp, home, store) = setup();
        let template = store
            .create_note_kind(
                &actor(), &home, "research reading",
                "---template\nfield: source | Source | text\nnote-on-complete: true\n---\n## Learned\n",
                &[], "personal", None, "hearthloom", None, "template",
            )
            .unwrap();
        let id = store
            .add_task(
                &actor(),
                "form",
                "read the attention paper",
                Some(&json!({"values": {"source": "arxiv"}})),
                Some(&template),
                "one-time",
                "work",
                None,
                None,
                &[],
            )
            .unwrap();
        let outcome = store.complete_task(&actor(), &home, &id, None).unwrap();
        assert!(outcome.offer_note);
        assert!(outcome.next_task_id.is_none());
    }

    #[test]
    fn test_tasks_vault_guarded() {
        let (_tmp, _home, store) = setup();
        store
            .add_task(
                &actor(), "plain", "secret errand", None, None, "one-time",
                "private", None, None, &[],
            )
            .unwrap();
        store
            .add_task(
                &actor(), "plain", "public errand", None, None, "one-time",
                "personal", None, None, &[],
            )
            .unwrap();
        assert_eq!(store.list_tasks(None, 50).unwrap().len(), 1);
        store.vault_unlock(5);
        assert_eq!(store.list_tasks(None, 50).unwrap().len(), 2);
        store.vault_lock();
        // The raw-sql handle shadows too.
        let (_, rows) = store
            .read_query("SELECT description FROM tasks", 100)
            .unwrap();
        assert_eq!(rows.len(), 1);
    }
}
