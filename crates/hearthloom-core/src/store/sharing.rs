//! Granular friend sharing (v46, the design log 2026-07-28): what a
//! connected friend may read of THIS record, per content CLASS, plus
//! explicit per-item shares that punch through the categorical matrix.
//!
//! The model (CAN / MAY / WILL-ENFORCE):
//! - A reserved grant row `__default__` holds the sharing TEMPLATE every
//!   connection inherits. Per-friend rows override per key (tri-state:
//!   "on" / "off" / absent = inherit). Changing the template instantly
//!   changes every non-overridden connection — live inheritance, not
//!   copy-on-connect.
//! - `shared_items` is the union leg: an explicitly shared task/note/
//!   relationship is readable by that friend even when its class is
//!   categorically off. Explicit shares always serve FULL detail (you
//!   chose this item for this person), survive task completion, and may
//!   carry `collaborate` (checklist toggles — the ONE friend write) and
//!   `expires_at`.
//! - WILL-ENFORCE stays at read time: 'private' is never grantable,
//!   never item-shareable, and re-excluded in every query — a privacy
//!   relabel silently retracts an existing share. Vault state is
//!   irrelevant on this surface (private never leaves regardless).
//! - Every write is journaled (op_journal — the OPERATIONAL DURABILITY
//!   invariant) and every read surface records a network access.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use super::network::GRANTABLE_SPHERES;
use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// The content classes a grant may cover categorically.
pub const SHARE_CLASSES: &[&str] = &["episodes", "tasks", "notes", "relationships", "profile"];

/// Item kinds that can be explicitly shared.
pub const SHAREABLE_KINDS: &[&str] = &["task", "note", "relationship"];

/// The reserved grants row holding the inherited template.
pub const DEFAULT_FRIEND: &str = "__default__";

/// A friend's fully resolved permissions: template + per-friend
/// overrides collapsed into plain values. This is what every network
/// read consults — and what /v1/network/offer advertises (MAY).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EffectiveGrant {
    pub friend: String,
    pub classes: Vec<String>,
    pub spheres: Vec<String>,
    pub detail: String, // excerpts | full
    pub completed_tasks: bool,
    pub evidence_relationships: bool,
}

impl EffectiveGrant {
    pub fn has_class(&self, class: &str) -> bool {
        self.classes.iter().any(|c| c == class)
    }
}

/// Built-in floor when neither template nor override says anything.
fn builtin_class(class: &str) -> bool {
    matches!(class, "episodes" | "profile")
}

fn resolve(friend: &str, template: &Value, over: &Value) -> EffectiveGrant {
    resolve_with(friend, template, over, true)
}

/// A group's grant: exactly what was set on the group — no template, no
/// built-in floor (see store/groups.rs for why).
pub(crate) fn resolve_group(key: &str, sharing: &Value) -> EffectiveGrant {
    resolve_with(key, &json!({}), sharing, false)
}

fn resolve_with(friend: &str, template: &Value, over: &Value, floor: bool) -> EffectiveGrant {
    let pick_class = |class: &str| -> bool {
        for source in [over, template] {
            if let Some(v) = source
                .get("classes")
                .and_then(|c| c.get(class))
                .and_then(|v| v.as_str())
            {
                return v == "on";
            }
        }
        floor && builtin_class(class)
    };
    let pick_spheres = || -> Vec<String> {
        for source in [over, template] {
            if let Some(list) = source.get("spheres").and_then(|s| s.as_array()) {
                return list
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| GRANTABLE_SPHERES.contains(s))
                    .map(String::from)
                    .collect();
            }
        }
        Vec::new()
    };
    let pick_str = |key: &str, default: &str| -> String {
        for source in [over, template] {
            if let Some(v) = source.get(key).and_then(|v| v.as_str()) {
                return v.to_string();
            }
        }
        default.to_string()
    };
    let pick_bool = |key: &str| -> bool {
        for source in [over, template] {
            if let Some(v) = source.get(key).and_then(|v| v.as_bool()) {
                return v;
            }
        }
        false
    };
    EffectiveGrant {
        friend: friend.to_string(),
        classes: SHARE_CLASSES
            .iter()
            .filter(|c| pick_class(c))
            .map(|c| c.to_string())
            .collect(),
        spheres: pick_spheres(),
        detail: pick_str("detail", "excerpts"),
        completed_tasks: pick_bool("completed_tasks"),
        evidence_relationships: pick_bool("evidence_relationships"),
    }
}

/// SQL: `column` is one of `spheres` (false when there are none). The
/// values come from GRANTABLE_SPHERES via `FriendAccess::spheres`; they are
/// quoted here all the same.
fn scope_clause(column: &str, spheres: &[String]) -> String {
    if spheres.is_empty() {
        return "0".into();
    }
    let list = spheres
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(",");
    format!("({column} IN ({list}))")
}

/// Validate a sharing descriptor from the wire (unknown keys ignored —
/// wire discipline; known keys strictly checked; private never passes).
pub(crate) fn validate_sharing(sharing: &Value) -> Result<Value> {
    let mut out = serde_json::Map::new();
    if let Some(classes) = sharing.get("classes") {
        let obj = classes
            .as_object()
            .ok_or_else(|| anyhow!("sharing.classes must be an object"))?;
        let mut clean = serde_json::Map::new();
        for (k, v) in obj {
            if !SHARE_CLASSES.contains(&k.as_str()) {
                continue; // a newer peer's class this build doesn't know
            }
            match v.as_str() {
                Some("on") | Some("off") => {
                    clean.insert(k.clone(), v.clone());
                }
                _ => return Err(anyhow!("class {k:?} must be \"on\" or \"off\"")),
            }
        }
        out.insert("classes".into(), Value::Object(clean));
    }
    if let Some(spheres) = sharing.get("spheres") {
        let list = spheres
            .as_array()
            .ok_or_else(|| anyhow!("sharing.spheres must be a list"))?;
        let clean: Vec<Value> = list
            .iter()
            .filter(|v| {
                v.as_str()
                    .map(|s| GRANTABLE_SPHERES.contains(&s))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        out.insert("spheres".into(), Value::Array(clean));
    }
    if let Some(detail) = sharing.get("detail").and_then(|v| v.as_str()) {
        if !matches!(detail, "excerpts" | "full") {
            return Err(anyhow!("detail must be excerpts|full"));
        }
        out.insert("detail".into(), json!(detail));
    }
    for flag in ["completed_tasks", "evidence_relationships"] {
        if let Some(v) = sharing.get(flag) {
            let b = v
                .as_bool()
                .ok_or_else(|| anyhow!("{flag} must be true or false"))?;
            out.insert(flag.into(), json!(b));
        }
    }
    Ok(Value::Object(out))
}

impl Store {
    /* ————— the matrix: template + per-friend overrides ————— */

    /// The inherited sharing template (the `__default__` row).
    pub fn default_sharing(&self) -> Result<Value> {
        let conn = self.lock();
        let raw: Option<Option<String>> = conn
            .query_row(
                "SELECT sharing FROM grants WHERE friend = ?1",
                [DEFAULT_FRIEND],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({"classes": {"episodes": "on", "profile": "on"}})))
    }

    /// Update the template every connection inherits.
    pub fn set_default_sharing(&self, actor: &Actor, sharing: &Value) -> Result<Value> {
        let clean = validate_sharing(sharing)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO grants (friend, spheres, detail, created_at, sharing, updated_at)
             VALUES (?1, '[]', 'excerpts', ?2, ?3, ?2)
             ON CONFLICT(friend) DO UPDATE SET sharing = excluded.sharing,
                 updated_at = excluded.created_at",
            rusqlite::params![DEFAULT_FRIEND, now_utc(), clean.to_string()],
        )?;
        record_mutation(
            &conn,
            actor,
            "sharing_default_set",
            &format!("sharing_default_set:{}", now_utc()),
            &clean,
        )?;
        Ok(clean)
    }

    /// Per-friend sharing override (tri-state; absent keys inherit).
    pub fn set_friend_sharing(
        &self,
        actor: &Actor,
        friend: &str,
        sharing: &Value,
    ) -> Result<Value> {
        if friend == DEFAULT_FRIEND {
            return Err(anyhow!("use the default-sharing surface for the template"));
        }
        if friend.starts_with(super::groups::GROUP_PREFIX) {
            return Err(anyhow!("a group's sharing is set on the group"));
        }
        let clean = validate_sharing(sharing)?;
        let conn = self.lock();
        let now = now_utc();
        let changed = conn.execute(
            "UPDATE grants SET sharing = ?2, revoked_at = NULL, updated_at = ?3
             WHERE friend = ?1",
            rusqlite::params![friend, clean.to_string(), now],
        )?;
        if changed == 0 {
            // no grant row yet (connection invited elsewhere): create one
            // that inherits everything except these overrides.
            conn.execute(
                "INSERT INTO grants (friend, spheres, detail, created_at, sharing, updated_at)
                 VALUES (?1, '[]', 'excerpts', ?2, ?3, ?2)",
                rusqlite::params![friend, now, clean.to_string()],
            )?;
        }
        record_mutation(
            &conn,
            actor,
            "sharing_set",
            &format!("sharing_set:{friend}:{}", now_utc()),
            &json!({"friend": friend, "sharing": clean}),
        )?;
        Ok(clean)
    }

    /// The raw override JSON for a friend (None = fully inherited).
    pub fn friend_sharing(&self, friend: &str) -> Result<Option<Value>> {
        let conn = self.lock();
        let raw: Option<Option<String>> = conn
            .query_row(
                "SELECT sharing FROM grants WHERE friend = ?1",
                [friend],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.flatten().and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// The fully resolved live permissions for a friend: liveness
    /// (exists / not revoked / not expired) then template ∪ overrides.
    pub fn effective_grant(&self, friend: &str) -> Result<EffectiveGrant> {
        if friend == DEFAULT_FRIEND {
            return Err(anyhow!("no such friend"));
        }
        self.live_grant(friend)?; // liveness gate (revoked/expired refuse)
        let template = self.default_sharing()?;
        let over = self.friend_sharing(friend)?.unwrap_or_else(|| json!({}));
        Ok(resolve(friend, &template, &over))
    }

    /* ————— explicit item shares ————— */

    /// Share one task/note/relationship with one friend. Refuses
    /// private items and unknown kinds; upserts (re-sharing updates
    /// collaborate/expiry). Journaled — durable and peer-mirrored.
    pub fn share_item(
        &self,
        actor: &Actor,
        kind: &str,
        item_id: &str,
        friend: &str,
        collaborate: bool,
        expires_at: Option<&str>,
    ) -> Result<Value> {
        if !SHAREABLE_KINDS.contains(&kind) {
            return Err(anyhow!("kind must be one of {SHAREABLE_KINDS:?}"));
        }
        if friend == DEFAULT_FRIEND {
            return Err(anyhow!("share to a named connection"));
        }
        if let Some(group_id) = super::groups::group_id_of(friend) {
            if self.group(group_id)?.is_none() {
                return Err(anyhow!("no such group"));
            }
        }
        let scope = self.item_scope(kind, item_id)?;
        if scope.as_deref() == Some("private") {
            return Err(anyhow!(
                "private items can never be shared — change its scope first"
            ));
        }
        if scope.is_none() {
            return Err(anyhow!("no {kind} {item_id:?}"));
        }
        let conn = self.lock();
        let id = format!("sh_{}", crate::ids::entity_id().trim_start_matches("en_"));
        conn.execute(
            "INSERT INTO shared_items
                 (id, kind, item_id, friend, collaborate, expires_at, created_at, actor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(kind, item_id, friend) DO UPDATE SET
                 collaborate = excluded.collaborate,
                 expires_at = excluded.expires_at,
                 created_at = excluded.created_at",
            rusqlite::params![
                id,
                kind,
                item_id,
                friend,
                collaborate as i64,
                expires_at,
                now_utc(),
                actor.actor
            ],
        )?;
        let row_id: String = conn.query_row(
            "SELECT id FROM shared_items WHERE kind = ?1 AND item_id = ?2 AND friend = ?3",
            rusqlite::params![kind, item_id, friend],
            |row| row.get(0),
        )?;
        super::journal_row(
            &conn,
            actor,
            "shared_item",
            "shared_items",
            &row_id,
            "update",
        )?;
        record_mutation(
            &conn,
            actor,
            "item_shared",
            &format!("item_shared:{kind}:{item_id}:{friend}"),
            &json!({"kind": kind, "item": item_id, "friend": friend,
                    "collaborate": collaborate, "expires_at": expires_at}),
        )?;
        Ok(
            json!({"id": row_id, "kind": kind, "item_id": item_id, "friend": friend,
                  "collaborate": collaborate}),
        )
    }

    /// Un-share: takes effect on the friend's very next request.
    pub fn unshare_item(
        &self,
        actor: &Actor,
        kind: &str,
        item_id: &str,
        friend: &str,
    ) -> Result<bool> {
        let conn = self.lock();
        let row_id: Option<String> = conn
            .query_row(
                "SELECT id FROM shared_items WHERE kind = ?1 AND item_id = ?2 AND friend = ?3",
                rusqlite::params![kind, item_id, friend],
                |row| row.get(0),
            )
            .optional()?;
        let Some(row_id) = row_id else {
            return Ok(false);
        };
        // tombstone BEFORE the row is gone (delete propagates via journal sync)
        super::journal_row(
            &conn,
            actor,
            "shared_item",
            "shared_items",
            &row_id,
            "delete",
        )?;
        conn.execute("DELETE FROM shared_items WHERE id = ?1", [&row_id])?;
        record_mutation(
            &conn,
            actor,
            "item_unshared",
            &format!("item_unshared:{kind}:{item_id}:{friend}"),
            &json!({"kind": kind, "item": item_id, "friend": friend}),
        )?;
        Ok(true)
    }

    /// Every share touching an item — the badge row on the record.
    pub fn item_shares(&self, kind: &str, item_id: &str) -> Result<Vec<Value>> {
        let rows = self.item_shares_raw(kind, item_id)?;
        self.label_shares(rows)
    }

    /// Give each share row a `label` a person can read: the friend's
    /// name, or "<group name> (group)" when the target is a group.
    fn label_shares(&self, mut rows: Vec<Value>) -> Result<Vec<Value>> {
        let groups = self.groups()?;
        for row in rows.iter_mut() {
            let target = row["friend"].as_str().unwrap_or("").to_string();
            let label = match super::groups::group_id_of(&target) {
                Some(id) => groups
                    .iter()
                    .find(|g| g.id == id)
                    .map(|g| format!("{} (group)", g.name))
                    .unwrap_or_else(|| "a deleted group".into()),
                None => target,
            };
            row["label"] = json!(label);
        }
        Ok(rows)
    }

    fn item_shares_raw(&self, kind: &str, item_id: &str) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT friend, collaborate, expires_at, created_at FROM shared_items
             WHERE kind = ?1 AND item_id = ?2 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([kind, item_id], |row| {
            Ok(json!({
                "friend": row.get::<_, String>(0)?,
                "collaborate": row.get::<_, i64>(1)? != 0,
                "expires_at": row.get::<_, Option<String>>(2)?,
                "shared_at": row.get::<_, String>(3)?,
            }))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Everything explicitly shared to one friend (the owner-side panel).
    pub fn shares_to_friend(&self, friend: &str) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT kind, item_id, collaborate, expires_at, created_at FROM shared_items
             WHERE friend = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([friend], |row| {
            Ok(json!({
                "kind": row.get::<_, String>(0)?,
                "item_id": row.get::<_, String>(1)?,
                "collaborate": row.get::<_, i64>(2)? != 0,
                "expires_at": row.get::<_, Option<String>>(3)?,
                "shared_at": row.get::<_, String>(4)?,
            }))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// An item's scope (None = item doesn't exist). Relationships take
    /// their entity's scope.
    fn item_scope(&self, kind: &str, item_id: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let sql = match kind {
            "task" => "SELECT scope FROM tasks WHERE id = ?1",
            "note" => "SELECT scope FROM notes_index WHERE id = ?1",
            "relationship" => {
                "SELECT e.scope FROM relationships r JOIN entities e ON e.id = r.entity_id
                 WHERE r.id = ?1"
            }
            _ => return Ok(None),
        };
        conn.query_row(sql, [item_id], |row| row.get(0))
            .optional()
            .map_err(Into::into)
    }

    /* ————— the network reads (WILL-ENFORCE) ————— */

    /// Tasks visible to a friend: (class `tasks` AND sphere granted) ∪
    /// explicitly shared. `done` status categorically requires the
    /// completed_tasks flag; an explicitly shared task's lifecycle is
    /// always visible (you shared THAT item). Excerpts omit payload;
    /// explicit shares always serve full (collaboration needs items).
    pub fn network_tasks(&self, friend: &str, status: &str, limit: usize) -> Result<Vec<Value>> {
        let access = self.access(friend)?;
        if !matches!(status, "open" | "backlog" | "done") {
            return Err(anyhow!("status must be open|backlog|done"));
        }
        // Grant by grant: a life area counts only where ONE grant gives
        // both the tasks class and (for done tasks) the completed flag.
        let covers =
            |g: &EffectiveGrant| g.has_class("tasks") && (status != "done" || g.completed_tasks);
        let cat_clause = scope_clause("t.scope", &access.spheres(covers));
        let full_spheres = access.spheres(|g| covers(g) && g.detail == "full");
        let target = access.share_target("s.friend", "?1");
        let now = now_utc();
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT t.id, t.kind, t.description, t.status, t.remind_at, t.tags, t.scope,
                    t.updated_at, t.completed_at, t.payload,
                    (SELECT COUNT(*) FROM shared_items s
                     WHERE s.kind='task' AND s.item_id = t.id AND {target}
                       AND (s.expires_at IS NULL OR s.expires_at > ?3)) AS shared,
                    (SELECT MAX(s.collaborate) FROM shared_items s
                     WHERE s.kind='task' AND s.item_id = t.id AND {target}
                       AND (s.expires_at IS NULL OR s.expires_at > ?3)) AS collab
             FROM tasks t
             WHERE t.status = ?2 AND t.scope != 'private'
               AND ({cat_clause} OR EXISTS (
                    SELECT 1 FROM shared_items s
                    WHERE s.kind='task' AND s.item_id = t.id AND {target}
                      AND (s.expires_at IS NULL OR s.expires_at > ?3)))
             ORDER BY t.updated_at DESC LIMIT ?4",
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![friend, status, now, limit as i64],
            |row| {
                let shared = row.get::<_, i64>(10)? > 0;
                let payload: Option<String> = row.get(9)?;
                let scope: String = row.get(6)?;
                let full = full_spheres.contains(&scope);
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "description": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "remind_at": row.get::<_, Option<String>>(4)?,
                    "tags": row.get::<_, Option<String>>(5)?
                        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                        .unwrap_or_else(|| json!([])),
                    "scope": scope,
                    "updated_at": row.get::<_, String>(7)?,
                    "completed_at": row.get::<_, Option<String>>(8)?,
                    // explicit shares always full; categorical honors detail
                    "payload": if shared || full {
                        payload.and_then(|p| serde_json::from_str::<Value>(&p).ok())
                    } else { None },
                    "shared": shared,
                    "collaborate": row.get::<_, Option<i64>>(11)?.unwrap_or(0) != 0,
                }))
            },
        )?;
        let out: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        drop(conn); // the access log re-takes the lock — NOT reentrant
        self.log_network_access(friend, "tasks", &json!({"status": status, "n": out.len()}))?;
        Ok(out)
    }

    /// Relationships visible to a friend: declared rows under the class
    /// grant (evidence-tier people too when that flag is on) ∪ explicit
    /// shares. Entity scope guards every row; private never.
    pub fn network_relationships(&self, friend: &str, limit: usize) -> Result<Vec<Value>> {
        let access = self.access(friend)?;
        let now = now_utc();
        let mut out: Vec<Value> = Vec::new();
        let target = access.share_target("s.friend", "?1");
        {
            let conn = self.lock();
            // declared rows: categorical (class on + sphere) OR item-shared
            let cat_clause = format!(
                "(r.origin = 'declared' AND {})",
                scope_clause("e.scope", &access.spheres(|g| g.has_class("relationships")))
            );
            let mut stmt = conn.prepare(&format!(
                "SELECT r.id, e.name, e.type, r.kind, r.note, r.status, r.t_valid, r.t_invalid,
                        (SELECT COUNT(*) FROM shared_items s
                         WHERE s.kind='relationship' AND s.item_id = r.id AND {target}
                           AND (s.expires_at IS NULL OR s.expires_at > ?2)) AS shared
                 FROM relationships r JOIN entities e ON e.id = r.entity_id
                 WHERE e.scope != 'private'
                   AND ({cat_clause} OR EXISTS (
                        SELECT 1 FROM shared_items s
                        WHERE s.kind='relationship' AND s.item_id = r.id AND {target}
                          AND (s.expires_at IS NULL OR s.expires_at > ?2)))
                 ORDER BY r.updated_at DESC LIMIT ?3",
            ))?;
            let rows = stmt.query_map(rusqlite::params![friend, now, limit as i64], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "type": row.get::<_, String>(2)?,
                    "kind": row.get::<_, String>(3)?,
                    "note": row.get::<_, Option<String>>(4)?,
                    "status": row.get::<_, String>(5)?,
                    "t_valid": row.get::<_, Option<String>>(6)?,
                    "t_invalid": row.get::<_, Option<String>>(7)?,
                    "shared": row.get::<_, i64>(8)? > 0,
                    "tier": Value::Null,
                }))
            })?;
            out.extend(rows.collect::<std::result::Result<Vec<_>, _>>()?);
        }
        // evidence-tier people (owner opted in): most-cited Persons in
        // granted spheres — names + citation counts, no relationship rows.
        let evidence_spheres =
            access.spheres(|g| g.has_class("relationships") && g.evidence_relationships);
        if !evidence_spheres.is_empty() {
            for (entity, citations) in self.top_entities(limit.min(100))? {
                if entity.entity_type == "Person"
                    && evidence_spheres.contains(&entity.scope)
                    && !out.iter().any(|r| r["name"] == json!(entity.name))
                {
                    out.push(json!({
                        "id": Value::Null,
                        "name": entity.name,
                        "type": "Person",
                        "kind": Value::Null,
                        "note": Value::Null,
                        "status": "evidence",
                        "shared": false,
                        "tier": "evidence",
                        "citations": citations,
                    }));
                }
            }
        }
        self.log_network_access(friend, "relationships", &json!({"n": out.len()}))?;
        Ok(out)
    }

    /// Notes visible to a friend (metadata only — bodies go through
    /// `network_note_allowed` + the file read in the daemon).
    pub fn network_notes(&self, friend: &str, limit: usize) -> Result<Vec<Value>> {
        let access = self.access(friend)?;
        let cat_clause = scope_clause("n.scope", &access.spheres(|g| g.has_class("notes")));
        let target = access.share_target("s.friend", "?1");
        let now = now_utc();
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT n.id, n.title, n.tags, n.scope, n.updated_at,
                    (SELECT COUNT(*) FROM shared_items s
                     WHERE s.kind='note' AND s.item_id = n.id AND {target}
                       AND (s.expires_at IS NULL OR s.expires_at > ?2)) AS shared
             FROM notes_index n
             WHERE n.scope != 'private' AND n.status = 'active'
               AND ({cat_clause} OR EXISTS (
                    SELECT 1 FROM shared_items s
                    WHERE s.kind='note' AND s.item_id = n.id AND {target}
                      AND (s.expires_at IS NULL OR s.expires_at > ?2)))
             ORDER BY n.updated_at DESC LIMIT ?3",
        ))?;
        let rows = stmt.query_map(rusqlite::params![friend, now, limit as i64], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "tags": row.get::<_, Option<String>>(2)?
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .unwrap_or_else(|| json!([])),
                "scope": row.get::<_, String>(3)?,
                "updated_at": row.get::<_, Option<String>>(4)?,
                "shared": row.get::<_, i64>(5)? > 0,
            }))
        })?;
        let out: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        drop(conn); // the access log re-takes the lock — NOT reentrant
        self.log_network_access(friend, "notes", &json!({"n": out.len()}))?;
        Ok(out)
    }

    /// May this friend read this note's BODY? Explicit share always;
    /// categorical needs class + sphere + full detail (the episode-fetch
    /// precedent: excerpts grants never serve whole bodies).
    pub fn network_note_allowed(&self, friend: &str, note_id: &str) -> Result<bool> {
        let access = self.access(friend)?;
        let scope = self.item_scope("note", note_id)?;
        let Some(scope) = scope else { return Ok(false) };
        if scope == "private" {
            return Ok(false);
        }
        let now = now_utc();
        let target = access.share_target("friend", "?2");
        let conn = self.lock();
        let shared: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM shared_items
                 WHERE kind='note' AND item_id = ?1 AND {target}
                   AND (expires_at IS NULL OR expires_at > ?3)"
            ),
            rusqlite::params![note_id, friend, now],
            |row| row.get(0),
        )?;
        if shared > 0 {
            return Ok(true);
        }
        Ok(access
            .spheres(|g| g.has_class("notes") && g.detail == "full")
            .contains(&scope))
    }

    /// The explicit-shares bundle for a friend's Desk: shared tasks with
    /// payloads, shared note metadata, shared relationship rows, plus a
    /// cursor (max created_at) for cheap change polling.
    pub fn network_shared_bundle(&self, friend: &str) -> Result<Value> {
        // liveness + groups — explicit shares need no class
        let target = self.access(friend)?.share_target("s.friend", "?1");
        let tasks = self.shared_only(friend, "task", &target)?;
        let notes = self.shared_only(friend, "note", &target)?;
        let relationships = self.shared_only(friend, "relationship", &target)?;
        let cursor = self.network_shared_cursor(friend)?;
        self.log_network_access(
            friend,
            "shared",
            &json!({"tasks": tasks.len(), "notes": notes.len(), "relationships": relationships.len()}),
        )?;
        Ok(json!({
            "tasks": tasks, "notes": notes, "relationships": relationships,
            "cursor": cursor,
        }))
    }

    /// `target` is `FriendAccess::share_target("s.friend", "?1")`. An item
    /// shared both directly and through a group is ONE row (GROUP BY),
    /// collaborative if either share is.
    fn shared_only(&self, friend: &str, kind: &str, target: &str) -> Result<Vec<Value>> {
        let now = now_utc();
        let conn = self.lock();
        let (sql, cols): (String, usize) = match kind {
            "task" => (
                format!(
                    "SELECT t.id, t.kind, t.description, t.status, t.remind_at, t.tags,
                            t.updated_at, t.completed_at, t.payload, MAX(s.collaborate)
                     FROM shared_items s JOIN tasks t ON t.id = s.item_id
                     WHERE {target} AND s.kind = 'task' AND t.scope != 'private'
                       AND (s.expires_at IS NULL OR s.expires_at > ?2)
                     GROUP BY t.id ORDER BY t.updated_at DESC"
                ),
                10,
            ),
            "note" => (
                format!(
                    "SELECT n.id, n.title, n.tags, n.scope, n.updated_at, 0
                     FROM shared_items s JOIN notes_index n ON n.id = s.item_id
                     WHERE {target} AND s.kind = 'note' AND n.scope != 'private'
                       AND n.status = 'active'
                       AND (s.expires_at IS NULL OR s.expires_at > ?2)
                     GROUP BY n.id ORDER BY n.updated_at DESC"
                ),
                6,
            ),
            _ => (
                format!(
                    "SELECT r.id, e.name, e.type, r.kind, r.note, r.status
                     FROM shared_items s JOIN relationships r ON r.id = s.item_id
                     JOIN entities e ON e.id = r.entity_id
                     WHERE {target} AND s.kind = 'relationship' AND e.scope != 'private'
                       AND (s.expires_at IS NULL OR s.expires_at > ?2)
                     GROUP BY r.id ORDER BY r.updated_at DESC"
                ),
                6,
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![friend, now], |row| {
            Ok(match cols {
                10 => json!({
                    "id": row.get::<_, String>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "description": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "remind_at": row.get::<_, Option<String>>(4)?,
                    "tags": row.get::<_, Option<String>>(5)?
                        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                        .unwrap_or_else(|| json!([])),
                    "updated_at": row.get::<_, String>(6)?,
                    "completed_at": row.get::<_, Option<String>>(7)?,
                    "payload": row.get::<_, Option<String>>(8)?
                        .and_then(|p| serde_json::from_str::<Value>(&p).ok()),
                    "collaborate": row.get::<_, i64>(9)? != 0,
                }),
                6 if kind == "note" => json!({
                    "id": row.get::<_, String>(0)?,
                    "title": row.get::<_, String>(1)?,
                    "tags": row.get::<_, Option<String>>(2)?
                        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                        .unwrap_or_else(|| json!([])),
                    "scope": row.get::<_, String>(3)?,
                    "updated_at": row.get::<_, Option<String>>(4)?,
                }),
                _ => json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "type": row.get::<_, String>(2)?,
                    "kind": row.get::<_, String>(3)?,
                    "note": row.get::<_, Option<String>>(4)?,
                    "status": row.get::<_, String>(5)?,
                }),
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// The ONE friend write: toggle a checklist item on a task that was
    /// explicitly shared with `collaborate`. Owner's node stays
    /// authoritative; the write lands as actor `friend:<name>`, is
    /// journaled, and verifies the item TEXT so a stale index never
    /// ticks the wrong line.
    pub fn friend_toggle_checklist(
        &self,
        friend: &str,
        task_id: &str,
        index: usize,
        text: &str,
        done: bool,
    ) -> Result<Value> {
        // liveness; a collaborate share to one of their groups counts too
        let target = self.access(friend)?.share_target("friend", "?2");
        let actor = Actor::new(format!("friend:{friend}"));
        let now = now_utc();
        let conn = self.lock();
        let collab: i64 = conn
            .query_row(
                &format!(
                    "SELECT MAX(collaborate) FROM shared_items
                     WHERE kind='task' AND item_id = ?1 AND {target}
                       AND (expires_at IS NULL OR expires_at > ?3)"
                ),
                rusqlite::params![task_id, friend, now],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
            .unwrap_or(0);
        if collab == 0 {
            return Err(anyhow!("that task isn't shared for collaboration"));
        }
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM tasks WHERE id = ?1 AND kind = 'checklist'
                 AND scope != 'private'",
                [task_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(payload) = payload else {
            return Err(anyhow!("no shared checklist {task_id:?}"));
        };
        let mut parsed: Value = serde_json::from_str(&payload)?;
        let items = parsed
            .get_mut("items")
            .and_then(|i| i.as_array_mut())
            .ok_or_else(|| anyhow!("that task has no checklist items"))?;
        let item = items
            .get_mut(index)
            .ok_or_else(|| anyhow!("the list changed — refresh and try again"))?;
        if item.get("text").and_then(|t| t.as_str()) != Some(text) {
            return Err(anyhow!("the list changed — refresh and try again"));
        }
        item["done"] = json!(done);
        let done_count = items.iter().filter(|i| i["done"] == json!(true)).count();
        let total = items.len();
        let serialized = parsed.to_string();
        conn.execute(
            "UPDATE tasks SET payload = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![task_id, serialized, now],
        )?;
        super::journal_row(&conn, &actor, "task", "tasks", task_id, "update")?;
        record_mutation(
            &conn,
            &actor,
            "friend_toggle",
            &format!("friend_toggle:{task_id}:{index}:{now}"),
            &json!({"task": task_id, "index": index, "done": done, "friend": friend}),
        )?;
        Ok(json!({"id": task_id, "done": done_count, "total": total}))
    }

    /// The whole sharing posture in one compact object — the owner's
    /// Ask introspects this ("what am I sharing with Sam?"): the
    /// default template, every friend's RESOLVED effective grant, and
    /// their explicit item shares.
    pub fn sharing_overview(&self) -> Result<Value> {
        let template = self.default_sharing()?;
        let mut friends = Vec::new();
        for grant in self.list_grants()? {
            let effective = match self.effective_grant(&grant.friend) {
                Ok(e) => serde_json::to_value(&e)?,
                Err(err) => json!({"inactive": err.to_string()}),
            };
            let shares = self.shares_to_friend(&grant.friend)?;
            friends.push(json!({
                "friend": grant.friend,
                "effective": effective,
                "explicit_shares": shares,
            }));
        }
        // v54: groups — what each grants its members on top of the above.
        let groups: Vec<Value> = self
            .groups()?
            .into_iter()
            .map(|g| {
                let key = g.key();
                Ok(json!({
                    "group": g.name,
                    "members": g.members,
                    "grants": super::sharing::resolve_group(&key, &g.sharing),
                    "explicit_shares": self.shares_to_friend(&key)?,
                }))
            })
            .collect::<Result<_>>()?;
        Ok(json!({"default_template": template, "friends": friends, "groups": groups}))
    }

    /// Every explicit share, newest first — one fetch paints every
    /// badge on the Desk/Notes/Relationships surfaces.
    pub fn all_shares(&self) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT kind, item_id, friend, collaborate, expires_at, created_at
             FROM shared_items ORDER BY created_at DESC LIMIT 500",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "kind": row.get::<_, String>(0)?,
                "item_id": row.get::<_, String>(1)?,
                "friend": row.get::<_, String>(2)?,
                "collaborate": row.get::<_, i64>(3)? != 0,
                "expires_at": row.get::<_, Option<String>>(4)?,
                "shared_at": row.get::<_, String>(5)?,
            }))
        })?;
        let rows: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        drop(conn); // labelling re-takes the lock — NOT reentrant
        self.label_shares(rows)
    }

    /// The max shared_items stamp for a friend — cheap change polling.
    pub fn network_shared_cursor(&self, friend: &str) -> Result<Option<String>> {
        let target = self.access(friend)?.share_target("s.friend", "?1");
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT MAX(s.created_at) FROM shared_items s WHERE {target}"),
            [friend],
            |row| row.get(0),
        )
        .optional()
        .map(Option::flatten)
        .map_err(Into::into)
    }

    /// Public wrapper for daemon-side surfaces that assemble their own
    /// responses (the note-body fetch) but must still log the access.
    pub fn log_network_access_pub(
        &self,
        friend: &str,
        surface: &str,
        detail: &Value,
    ) -> Result<()> {
        self.log_network_access(friend, surface, detail)
    }

    /// The grants/shares peer feed (v47): grant rows (template included)
    /// stamped after `since`, plus shared_items after `since` — a user's
    /// own peers converge on ONE sharing posture, editable anywhere.
    /// Friend KEYS never travel (api_keys are node-local; enforcement
    /// happens on the node the friend actually queries).
    pub fn sharing_sync_export(&self, since: &str) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT friend, spheres, detail, created_at, expires_at, revoked_at, sharing, updated_at
             FROM grants WHERE synced_at > ?1
             ORDER BY synced_at LIMIT 500",
        )?;
        let grants: Vec<Value> = stmt
            .query_map([since], |row| {
                Ok(json!({
                    "friend": row.get::<_, String>(0)?,
                    "spheres": row.get::<_, String>(1)?,
                    "detail": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, String>(3)?,
                    "expires_at": row.get::<_, Option<String>>(4)?,
                    "revoked_at": row.get::<_, Option<String>>(5)?,
                    "sharing": row.get::<_, Option<String>>(6)?,
                    "updated_at": row.get::<_, Option<String>>(7)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT id, kind, item_id, friend, collaborate, expires_at, created_at, actor
             FROM shared_items WHERE synced_at > ?1 ORDER BY synced_at LIMIT 500",
        )?;
        let shares: Vec<Value> = stmt
            .query_map([since], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "item_id": row.get::<_, String>(2)?,
                    "friend": row.get::<_, String>(3)?,
                    "collaborate": row.get::<_, i64>(4)?,
                    "expires_at": row.get::<_, Option<String>>(5)?,
                    "created_at": row.get::<_, String>(6)?,
                    "actor": row.get::<_, String>(7)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        // v54: friend groups ride the same leg (additive — an older peer
        // ignores the field), whole and outside the cursor.
        let groups = Store::groups_sync_export(&conn)?;
        drop(stmt);
        let grants_end = super::page_cursor(
            &conn,
            "synced_at",
            "FROM grants WHERE synced_at > ?1",
            since,
            500,
        )?;
        let shares_end = super::page_cursor(
            &conn,
            "synced_at",
            "FROM shared_items WHERE synced_at > ?1",
            since,
            500,
        )?;
        let next = match (grants.len() >= 500, shares.len() >= 500) {
            (true, false) => grants_end,
            (false, true) => shares_end,
            (true, true) => grants_end.min(shares_end),
            (false, false) => grants_end.max(shares_end),
        };
        Ok(json!({"grants": grants, "shares": shares, "groups": groups, "next": next}))
    }

    /// Apply a peer's grants/shares page. Grants merge per friend under
    /// LWW by updated_at; shared_items upsert by id (a local journal
    /// delete-tombstone newer than the row blocks resurrection — the
    /// apply_operational gate's rule, applied here too).
    pub fn sharing_sync_apply(&self, batch: &Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for g in batch
            .get("grants")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let Some(friend) = g.get("friend").and_then(|v| v.as_str()) else {
                continue;
            };
            let sender_ts = g.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
            let local_ts: Option<String> = tx
                .query_row(
                    "SELECT COALESCE(updated_at, created_at) FROM grants WHERE friend = ?1",
                    [friend],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            if let Some(local) = local_ts {
                if local.as_str() >= sender_ts {
                    continue; // ours is newer (or same) — keep it
                }
            }
            tx.execute(
                "INSERT INTO grants (friend, spheres, detail, created_at, expires_at,
                     revoked_at, sharing, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(friend) DO UPDATE SET spheres=excluded.spheres,
                     detail=excluded.detail, expires_at=excluded.expires_at,
                     revoked_at=excluded.revoked_at, sharing=excluded.sharing,
                     updated_at=excluded.updated_at",
                rusqlite::params![
                    friend,
                    g.get("spheres").and_then(|v| v.as_str()).unwrap_or("[]"),
                    g.get("detail")
                        .and_then(|v| v.as_str())
                        .unwrap_or("excerpts"),
                    g.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
                    g.get("expires_at").and_then(|v| v.as_str()),
                    g.get("revoked_at").and_then(|v| v.as_str()),
                    g.get("sharing").and_then(|v| v.as_str()),
                    sender_ts,
                ],
            )?;
            applied += 1;
        }
        for s in batch
            .get("shares")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(kind), Some(item_id), Some(friend)) = (
                s.get("id").and_then(|v| v.as_str()),
                s.get("kind").and_then(|v| v.as_str()),
                s.get("item_id").and_then(|v| v.as_str()),
                s.get("friend").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let sender_ts = s.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
            // a local delete-tombstone at-or-after the sender's stamp wins
            let tomb: Option<String> = tx
                .query_row(
                    "SELECT ts FROM op_journal
                     WHERE entity_kind = 'shared_item' AND entity_id = ?1 AND op = 'delete'
                     ORDER BY ts DESC LIMIT 1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if tomb.is_some_and(|t| t.as_str() >= sender_ts) {
                continue;
            }
            applied += tx.execute(
                "INSERT INTO shared_items (id, kind, item_id, friend, collaborate,
                     expires_at, created_at, actor)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(kind, item_id, friend) DO UPDATE SET
                     collaborate=excluded.collaborate, expires_at=excluded.expires_at,
                     created_at=excluded.created_at
                 WHERE excluded.created_at > shared_items.created_at",
                rusqlite::params![
                    id,
                    kind,
                    item_id,
                    friend,
                    s.get("collaborate").and_then(|v| v.as_i64()).unwrap_or(0),
                    s.get("expires_at").and_then(|v| v.as_str()),
                    sender_ts,
                    s.get("actor").and_then(|v| v.as_str()).unwrap_or("peer"),
                ],
            )?;
        }
        applied += Store::groups_sync_apply(&tx, batch)?;
        tx.commit()?;
        Ok(applied)
    }

    fn log_network_access(&self, friend: &str, surface: &str, detail: &Value) -> Result<()> {
        let conn = self.lock();
        record_mutation(
            &conn,
            &Actor::new(format!("friend:{friend}")),
            "network_access",
            &format!("network_access:{friend}:{}", crate::ids::entity_id()),
            &json!({"friend": friend, "surface": surface, "detail": detail}),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Home;

    fn seeded() -> (Store, Home, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        (store, home, tmp)
    }

    fn actor() -> Actor {
        Actor::new("human:test")
    }

    fn grant_for(store: &Store, friend: &str) {
        store
            .set_grant(&actor(), friend, &["work".into()], "excerpts", None)
            .unwrap();
    }

    fn task(store: &Store, desc: &str, scope: &str) -> String {
        store
            .add_task(
                &actor(),
                "plain",
                desc,
                None,
                None,
                "one-time",
                scope,
                None,
                None,
                &[],
            )
            .unwrap()
    }

    #[test]
    fn test_default_template_inherits_and_overrides() {
        let (store, _h, _t) = seeded();
        grant_for(&store, "ada"); // legacy-style grant: frozen episodes-only
                                  // a fresh-style friend inheriting everything
        store
            .set_friend_sharing(&actor(), "bea", &json!({}))
            .unwrap();
        // template turns tasks ON for everyone (spheres too)
        store
            .set_default_sharing(
                &actor(),
                &json!({"classes": {"episodes": "on", "tasks": "on"},
                        "spheres": ["work"], "detail": "excerpts"}),
            )
            .unwrap();
        let bea = store.effective_grant("bea").unwrap();
        assert!(
            bea.has_class("tasks"),
            "inheriting friend follows the template"
        );
        assert_eq!(bea.spheres, vec!["work"]);
        // ada's invite froze episodes/spheres/detail EXPLICITLY, but the
        // classes it never mentioned still inherit — flipping the default
        // reaches every connection (the point of the template); her own
        // spheres stay her own.
        let ada = store.effective_grant("ada").unwrap();
        assert!(
            ada.has_class("tasks"),
            "unmentioned classes inherit the template"
        );
        assert_eq!(
            ada.spheres,
            vec!["work"],
            "frozen spheres stay the row's own"
        );
        assert_eq!(ada.detail, "excerpts", "frozen detail stays the row's own");
        // bea revokes tasks specifically — override beats template
        store
            .set_friend_sharing(&actor(), "bea", &json!({"classes": {"tasks": "off"}}))
            .unwrap();
        assert!(!store.effective_grant("bea").unwrap().has_class("tasks"));
        // and the template flipping OFF doesn't kill an explicit ON
        store
            .set_friend_sharing(&actor(), "bea", &json!({"classes": {"notes": "on"}}))
            .unwrap();
        assert!(store.effective_grant("bea").unwrap().has_class("notes"));
    }

    #[test]
    fn test_item_share_punches_through_and_unshare_revokes() {
        let (store, _h, _t) = seeded();
        grant_for(&store, "ada"); // episodes-only: NO tasks class
        let id = task(&store, "pack the car", "personal");
        // categorically invisible (class off AND sphere personal not granted)
        assert!(store.network_tasks("ada", "open", 50).unwrap().is_empty());
        store
            .share_item(&actor(), "task", &id, "ada", false, None)
            .unwrap();
        let visible = store.network_tasks("ada", "open", 50).unwrap();
        assert_eq!(visible.len(), 1, "explicit share must punch through");
        assert_eq!(visible[0]["shared"], json!(true));
        assert!(store.unshare_item(&actor(), "task", &id, "ada").unwrap());
        assert!(
            store.network_tasks("ada", "open", 50).unwrap().is_empty(),
            "unshare takes effect immediately"
        );
    }

    #[test]
    fn test_private_never_shareable_and_relabel_retracts() {
        let (store, _h, _t) = seeded();
        grant_for(&store, "ada");
        let id = task(&store, "secret plans", "private");
        assert!(
            store
                .share_item(&actor(), "task", &id, "ada", false, None)
                .is_err(),
            "private items must refuse sharing"
        );
        // share a personal task, then relabel it private — retracted at read
        let id2 = task(&store, "was personal", "personal");
        store
            .share_item(&actor(), "task", &id2, "ada", false, None)
            .unwrap();
        assert_eq!(store.network_tasks("ada", "open", 50).unwrap().len(), 1);
        {
            let conn = store.lock();
            conn.execute("UPDATE tasks SET scope='private' WHERE id=?1", [&id2])
                .unwrap();
        }
        assert!(
            store.network_tasks("ada", "open", 50).unwrap().is_empty(),
            "a privacy relabel must silently retract the share"
        );
    }

    #[test]
    fn test_completed_tasks_gate_and_shared_lifecycle() {
        let (store, home, _t) = seeded();
        store
            .set_friend_sharing(
                &actor(),
                "ada",
                &json!({"classes": {"tasks": "on"}, "spheres": ["personal"]}),
            )
            .unwrap();
        let id = task(&store, "walk", "personal");
        store.complete_task(&actor(), &home, &id, None).unwrap();
        // categorical done requires the flag
        assert!(store.network_tasks("ada", "done", 50).unwrap().is_empty());
        store
            .set_friend_sharing(
                &actor(),
                "ada",
                &json!({"classes": {"tasks": "on"}, "spheres": ["personal"],
                        "completed_tasks": true}),
            )
            .unwrap();
        assert_eq!(store.network_tasks("ada", "done", 50).unwrap().len(), 1);
    }

    #[test]
    fn test_collaborative_toggle_gated_and_text_checked() {
        let (store, _h, _t) = seeded();
        grant_for(&store, "ada");
        let payload = json!({"items": [{"text": "tent", "done": false}]});
        let id = store
            .add_task(
                &actor(),
                "checklist",
                "camping",
                Some(&payload),
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        store
            .share_item(&actor(), "task", &id, "ada", false, None)
            .unwrap();
        assert!(
            store
                .friend_toggle_checklist("ada", &id, 0, "tent", true)
                .is_err(),
            "collaborate=false must refuse the toggle"
        );
        store
            .share_item(&actor(), "task", &id, "ada", true, None)
            .unwrap();
        assert!(
            store
                .friend_toggle_checklist("ada", &id, 0, "sleeping bag", true)
                .is_err(),
            "text mismatch must refuse (stale index)"
        );
        let out = store
            .friend_toggle_checklist("ada", &id, 0, "tent", true)
            .unwrap();
        assert_eq!(out["done"], json!(1));
        let t = store.task(&id).unwrap().unwrap();
        assert_eq!(t.payload.unwrap()["items"][0]["done"], json!(true));
    }

    #[test]
    fn test_shared_bundle_and_note_body_gate() {
        let (store, home, _t) = seeded();
        grant_for(&store, "ada");
        let note = store
            .create_note(
                &actor(),
                &home,
                "Trip plan",
                "# pack\ntent",
                &[],
                "personal",
                None,
            )
            .unwrap();
        assert!(
            !store.network_note_allowed("ada", &note).unwrap(),
            "excerpts grant + no share = no body"
        );
        store
            .share_item(&actor(), "note", &note, "ada", false, None)
            .unwrap();
        assert!(store.network_note_allowed("ada", &note).unwrap());
        let bundle = store.network_shared_bundle("ada").unwrap();
        assert_eq!(bundle["notes"].as_array().unwrap().len(), 1);
        assert!(bundle["cursor"].as_str().is_some());
    }

    /* ————— friend groups (v54) ————— */

    fn sharing(classes: &[&str], spheres: &[&str], detail: &str) -> Value {
        let classes: serde_json::Map<String, Value> = classes
            .iter()
            .map(|c| (c.to_string(), json!("on")))
            .collect();
        json!({"classes": classes, "spheres": spheres, "detail": detail})
    }

    fn descriptions(tasks: &[Value]) -> Vec<String> {
        let mut out: Vec<String> = tasks
            .iter()
            .map(|t| t["description"].as_str().unwrap().to_string())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn test_group_grant_is_a_union_never_a_cross_product() {
        let (store, _h, _t) = seeded();
        grant_for(&store, "ada"); // direct: memories in `work`, excerpts
        grant_for(&store, "bob");
        task(&store, "work task", "work");
        task(&store, "family task", "family");
        task(&store, "health task", "health");
        task(&store, "hidden", "private");

        let group = store.create_group(&actor(), "Family").unwrap();
        store
            .set_group_members(&actor(), &group.id, &["ada".into()])
            .unwrap();
        // An empty group grants nothing — no template, no floor.
        assert!(store.network_tasks("ada", "open", 50).unwrap().is_empty());

        store
            .set_group_sharing(
                &actor(),
                &group.id,
                &sharing(&["tasks"], &["family", "private"], "full"),
            )
            .unwrap();
        // ada: tasks come from the group, and only in the group's area.
        // Her direct grant covers `work` but not tasks — the two must not
        // combine into "tasks in work".
        let seen = store.network_tasks("ada", "open", 50).unwrap();
        assert_eq!(descriptions(&seen), ["family task"]);
        // bob is not a member.
        assert!(store.network_tasks("bob", "open", 50).unwrap().is_empty());

        let access = store.access("ada").unwrap();
        assert_eq!(
            access.spheres(|g| g.has_class("tasks")),
            ["family"],
            "private is never granted"
        );
        assert_eq!(access.spheres(|g| g.has_class("episodes")), ["work"]);
        assert!(access.spheres(|g| g.has_class("notes")).is_empty());
        // Full bodies of memories: the group's `full` does not upgrade
        // the direct grant's excerpts.
        assert!(access
            .spheres(|g| g.has_class("episodes") && g.detail == "full")
            .is_empty());
        // The advertised ceiling is wider than what is enforced — by design.
        let ceiling = access.ceiling();
        assert!(ceiling.has_class("tasks") && ceiling.spheres.contains(&"work".to_string()));

        // A second group adds its own area; done tasks need THAT grant's flag.
        let other = store.create_group(&actor(), "Clinic").unwrap();
        store
            .set_group_members(&actor(), &other.id, &["ada".into(), "bob".into()])
            .unwrap();
        let mut health = sharing(&["tasks"], &["health"], "excerpts");
        health["completed_tasks"] = json!(true);
        store
            .set_group_sharing(&actor(), &other.id, &health)
            .unwrap();
        assert_eq!(
            descriptions(&store.network_tasks("ada", "open", 50).unwrap()),
            ["family task", "health task"]
        );
        assert_eq!(
            descriptions(&store.network_tasks("bob", "open", 50).unwrap()),
            ["health task"]
        );
        let done_spheres = store
            .access("ada")
            .unwrap()
            .spheres(|g| g.has_class("tasks") && g.completed_tasks);
        assert_eq!(
            done_spheres,
            ["health"],
            "the family group never offered done tasks"
        );

        // Leaving the roster ends it on the next read.
        store.set_group_members(&actor(), &group.id, &[]).unwrap();
        assert_eq!(
            descriptions(&store.network_tasks("ada", "open", 50).unwrap()),
            ["health task"]
        );
    }

    #[test]
    fn test_group_never_undoes_a_direct_off_or_revives_a_dead_connection() {
        let (store, _h, _t) = seeded();
        grant_for(&store, "ada");
        task(&store, "work task", "work");
        // The template gives everyone tasks; ada is switched off by hand.
        store
            .set_default_sharing(&actor(), &json!({"classes": {"tasks": "on"}}))
            .unwrap();
        store
            .set_friend_sharing(
                &actor(),
                "ada",
                &json!({"classes": {"episodes": "on", "tasks": "off"}, "spheres": ["work"]}),
            )
            .unwrap();
        assert!(store.network_tasks("ada", "open", 50).unwrap().is_empty());
        // Membership in a group that says nothing about tasks changes nothing.
        let group = store.create_group(&actor(), "Team").unwrap();
        store
            .set_group_members(&actor(), &group.id, &["ada".into()])
            .unwrap();
        store
            .set_group_sharing(&actor(), &group.id, &json!({"spheres": ["work"]}))
            .unwrap();
        assert!(
            store.network_tasks("ada", "open", 50).unwrap().is_empty(),
            "a group does not inherit the template"
        );

        // A revoked connection gets nothing through a group.
        store
            .set_group_sharing(&actor(), &group.id, &sharing(&["tasks"], &["work"], "full"))
            .unwrap();
        assert_eq!(store.network_tasks("ada", "open", 50).unwrap().len(), 1);
        store.revoke_grant(&actor(), "ada").unwrap();
        assert!(store.network_tasks("ada", "open", 50).is_err());
        assert!(store.network_groups("ada").is_err());
        assert!(store.network_shared_bundle("ada").is_err());
    }

    #[test]
    fn test_group_item_shares_and_delete() {
        let (store, home, _t) = seeded();
        grant_for(&store, "ada");
        grant_for(&store, "bob");
        let group = store.create_group(&actor(), "Trip").unwrap();
        store
            .set_group_members(&actor(), &group.id, &["ada".into()])
            .unwrap();
        let key = group.key();

        let list = store
            .add_task(
                &actor(),
                "checklist",
                "packing",
                Some(&json!({"items": [{"text": "tent", "done": false}]})),
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        let note = store
            .create_note(&actor(), &home, "Route", "# day one", &[], "personal", None)
            .unwrap();
        let secret = task(&store, "secret", "private");
        assert!(store
            .share_item(&actor(), "task", &secret, &key, false, None)
            .is_err());
        assert!(
            store
                .share_item(&actor(), "task", &list, "group:gr_00", false, None)
                .is_err(),
            "an unknown group is refused"
        );

        store
            .share_item(&actor(), "task", &list, &key, true, None)
            .unwrap();
        store
            .share_item(&actor(), "note", &note, &key, false, None)
            .unwrap();
        // Also shared directly, without collaboration: still ONE row, and
        // the group's collaborate flag carries.
        store
            .share_item(&actor(), "task", &list, "ada", false, None)
            .unwrap();
        let bundle = store.network_shared_bundle("ada").unwrap();
        assert_eq!(bundle["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(bundle["tasks"][0]["collaborate"], true);
        assert_eq!(bundle["notes"].as_array().unwrap().len(), 1);
        assert!(bundle["cursor"].as_str().is_some());
        assert!(store.network_note_allowed("ada", &note).unwrap());
        assert_eq!(store.network_tasks("ada", "open", 50).unwrap().len(), 1);
        assert_eq!(store.network_notes("ada", 50).unwrap().len(), 1);
        store
            .friend_toggle_checklist("ada", &list, 0, "tent", true)
            .unwrap();

        // bob is connected but not in the group.
        let bobs = store.network_shared_bundle("bob").unwrap();
        assert!(bobs["tasks"].as_array().unwrap().is_empty());
        assert!(!store.network_note_allowed("bob", &note).unwrap());
        assert!(store
            .friend_toggle_checklist("bob", &list, 0, "tent", false)
            .is_err());

        // Deleting the group un-shares what was shared to it; the direct
        // share to ada stands, without collaboration.
        assert!(store.delete_group(&actor(), &group.id).unwrap());
        assert!(store.group(&group.id).unwrap().is_none());
        assert!(store.groups_of("ada").unwrap().is_empty());
        let after = store.network_shared_bundle("ada").unwrap();
        assert_eq!(after["tasks"].as_array().unwrap().len(), 1);
        assert_eq!(after["tasks"][0]["collaborate"], false);
        assert!(after["notes"].as_array().unwrap().is_empty());
        assert!(!store.network_note_allowed("ada", &note).unwrap());
        assert!(store
            .friend_toggle_checklist("ada", &list, 0, "tent", false)
            .is_err());
        assert!(store
            .all_shares()
            .unwrap()
            .iter()
            .all(|s| s["friend"] != json!(key)));
    }

    #[test]
    fn test_group_roster_rules_and_member_view() {
        let (store, _h, _t) = seeded();
        grant_for(&store, "ada");
        grant_for(&store, "bob");
        let group = store.create_group(&actor(), "  Book   club ").unwrap();
        assert_eq!(group.name, "Book club");
        assert!(
            store.create_group(&actor(), "book CLUB").is_err(),
            "names are unique"
        );
        assert!(store.create_group(&actor(), "   ").is_err());
        assert!(
            store
                .set_group_members(&actor(), &group.id, &["nobody".into()])
                .is_err(),
            "only connections can be members"
        );
        assert!(store
            .set_group_members(&actor(), &group.id, &["__default__".into()])
            .is_err());
        assert!(
            store
                .set_grant(&actor(), "group:gr_abc", &["work".into()], "excerpts", None)
                .is_err(),
            "no connection may be named like a group"
        );
        assert!(store
            .set_friend_sharing(&actor(), &group.key(), &json!({}))
            .is_err());
        assert!(
            store.effective_grant(&group.key()).is_err(),
            "a group is not a caller"
        );

        let roster = store
            .set_group_members(
                &actor(),
                &group.id,
                &["ada".into(), "bob".into(), "ada".into()],
            )
            .unwrap();
        assert_eq!(roster.members, ["ada", "bob"]);
        let renamed = store.rename_group(&actor(), &group.id, "Readers").unwrap();
        assert_eq!(renamed.name, "Readers");

        // What a member's node is told.
        let told = store.network_groups("ada").unwrap();
        assert_eq!(told.len(), 1);
        assert_eq!(told[0]["name"], "Readers");
        assert_eq!(told[0]["members"], json!(["ada", "bob"]));
        store
            .set_group_members(&actor(), &group.id, &["bob".into()])
            .unwrap();
        assert!(store.network_groups("ada").unwrap().is_empty());

        // The member side keeps the answer whole, replacing the last one.
        let member = seeded().0;
        member.set_peer_groups("jon", &told).unwrap();
        member
            .set_peer_groups(
                "mia",
                &[json!({"id": "gr_1", "name": "Choir", "members": ["me"]})],
            )
            .unwrap();
        assert_eq!(member.peer_groups().unwrap().len(), 2);
        member.set_peer_groups("jon", &[]).unwrap();
        let left = member.peer_groups().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(
            (left[0]["owner"].as_str(), left[0]["name"].as_str()),
            (Some("mia"), Some("Choir"))
        );
    }

    #[test]
    fn test_groups_sync_between_own_nodes() {
        let (a, _ha, _ta) = seeded();
        let (b, _hb, _tb) = seeded();
        for store in [&a, &b] {
            grant_for(store, "ada");
        }
        let group = a.create_group(&actor(), "Family").unwrap();
        a.set_group_members(&actor(), &group.id, &["ada".into()])
            .unwrap();
        a.set_group_sharing(
            &actor(),
            &group.id,
            &sharing(&["tasks"], &["family"], "full"),
        )
        .unwrap();
        let page = a.sharing_sync_export("").unwrap();
        assert_eq!(page["groups"].as_array().unwrap().len(), 1);
        b.sharing_sync_apply(&page).unwrap();
        b.sharing_sync_apply(&page).unwrap(); // replay is a no-op
        let got = b.group(&group.id).unwrap().unwrap();
        assert_eq!(
            (got.name.as_str(), got.members.clone()),
            ("Family", vec!["ada".to_string()])
        );
        task(&b, "family task", "family");
        assert_eq!(
            b.network_tasks("ada", "open", 50).unwrap().len(),
            1,
            "enforced on the other node"
        );

        // An older row never overwrites a newer one.
        let mut stale = page.clone();
        stale["groups"][0]["name"] = json!("Old name");
        stale["groups"][0]["updated_at"] = json!("2000-01-01T00:00:00Z");
        b.sharing_sync_apply(&stale).unwrap();
        assert_eq!(b.group(&group.id).unwrap().unwrap().name, "Family");

        // Deletion travels as a newer row with deleted_at.
        let cursor = page["next"].as_str().unwrap().to_string();
        a.delete_group(&actor(), &group.id).unwrap();
        let next = a.sharing_sync_export(&cursor).unwrap();
        assert!(next["groups"][0]["deleted_at"].as_str().is_some());
        assert!(
            next["next"].as_str().unwrap() <= crate::time::now_utc().as_str(),
            "group stamps never push the shared cursor into the future"
        );
        b.sharing_sync_apply(&next).unwrap();
        assert!(b.group(&group.id).unwrap().is_none());
        assert!(b.network_tasks("ada", "open", 50).unwrap().is_empty());

        // A peer that predates groups sends no `groups` field at all.
        let mut old_peer = page.clone();
        old_peer.as_object_mut().unwrap().remove("groups");
        b.sharing_sync_apply(&old_peer).unwrap();
    }
}
