//! The network layer (networked brains, phase 1): what a connected
//! friend's brain may read of THIS one, governed by a grant.
//!
//! INVARIANTS (the brain_fs discipline, extended):
//! - 'private' is NEVER grantable — filtered out at grant-write AND
//!   re-excluded in every query, belt and suspenders. Vault state is
//!   irrelevant on this surface.
//! - Access control is deterministic at retrieval: the grant's sphere
//!   list is bound into the SQL before any content is read. Nothing
//!   ungranted ever reaches a response (synthesis can't leak what
//!   retrieval never returned).
//! - Revocation takes effect on the very next request (the grant row is
//!   read per call — no cache).
//! - Every access is recorded in the mutations log: transparency is a
//!   feature; "what has Ada's brain asked me?" is a query.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// Spheres a grant may cover. 'private' is structurally absent.
pub const GRANTABLE_SPHERES: &[&str] = &["work", "personal", "family", "health"];

#[derive(Debug, Clone, Serialize)]
pub struct GrantRow {
    pub friend: String,
    pub spheres: Vec<String>,
    pub detail: String, // excerpts | full
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    /// v46: the raw per-friend sharing override (None = inherit all) —
    /// the matrix editor round-trips this; resolution happens in
    /// sharing.rs::effective_grant.
    pub sharing: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkHit {
    pub id: String,
    pub day: String,
    pub kind: String,
    pub scope: String,
    pub title: String,
    pub excerpt: String,
}

impl Store {
    /// Create or replace the grant for a friend. Silently drops any
    /// non-grantable sphere (private can never be granted, even by a
    /// hand-crafted request).
    pub fn set_grant(
        &self,
        actor: &Actor,
        friend: &str,
        spheres: &[String],
        detail: &str,
        expires_at: Option<&str>,
    ) -> Result<GrantRow> {
        if friend == "__default__" {
            return Err(anyhow!("that name is reserved for the sharing template"));
        }
        if friend.starts_with(super::groups::GROUP_PREFIX) {
            return Err(anyhow!("a connection's name can't start with \"group:\""));
        }
        let spheres: Vec<String> = spheres
            .iter()
            .filter(|s| GRANTABLE_SPHERES.contains(&s.as_str()))
            .cloned()
            .collect();
        if spheres.is_empty() {
            return Err(anyhow!(
                "a grant needs at least one sphere ({})",
                GRANTABLE_SPHERES.join(", ")
            ));
        }
        if !matches!(detail, "excerpts" | "full") {
            return Err(anyhow!("detail must be excerpts|full"));
        }
        let conn = self.lock();
        // v46: the explicit spheres/detail freeze into the sharing
        // descriptor too (the resolver's single source of truth) — an
        // invite grants episodes with exactly what was asked; richer
        // class overrides go through set_friend_sharing.
        let frozen = json!({
            "classes": {"episodes": "on"},
            "spheres": spheres,
            "detail": detail,
        });
        conn.execute(
            "INSERT INTO grants (friend, spheres, detail, created_at, expires_at, revoked_at, sharing, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?4)
             ON CONFLICT(friend) DO UPDATE SET
                 spheres = excluded.spheres, detail = excluded.detail,
                 expires_at = excluded.expires_at, revoked_at = NULL,
                 sharing = excluded.sharing, updated_at = excluded.created_at",
            rusqlite::params![
                friend,
                json!(spheres).to_string(),
                detail,
                now_utc(),
                expires_at,
                frozen.to_string()
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "grant_set",
            &format!("grant_set:{friend}:{}", now_utc()),
            &json!({"friend": friend, "spheres": spheres, "detail": detail,
                    "expires_at": expires_at}),
        )?;
        drop(conn);
        self.grant(friend)?
            .ok_or_else(|| anyhow!("grant vanished after write"))
    }

    pub fn grant(&self, friend: &str) -> Result<Option<GrantRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT friend, spheres, detail, created_at, expires_at, revoked_at, sharing
             FROM grants WHERE friend = ?1",
            [friend],
            row_to_grant,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_grants(&self) -> Result<Vec<GrantRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            // '__default__' is the sharing TEMPLATE row (v46), not a friend
            "SELECT friend, spheres, detail, created_at, expires_at, revoked_at, sharing
             FROM grants WHERE friend != '__default__' ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_grant)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Permanently remove a REVOKED grant row (the struck-through stubs
    /// pile up; revoke is reversible, delete is the tidy). Refuses an
    /// active grant — you revoke first. Item shares to that friend go too.
    pub fn delete_grant(&self, actor: &Actor, friend: &str) -> Result<bool> {
        let conn = self.lock();
        let active: bool = conn
            .query_row(
                "SELECT revoked_at IS NULL FROM grants WHERE friend = ?1 AND friend != '__default__'",
                [friend],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if active {
            return Err(anyhow!("revoke {friend:?} before deleting the grant"));
        }
        let removed = conn.execute("DELETE FROM grants WHERE friend = ?1", [friend])?;
        conn.execute("DELETE FROM shared_items WHERE friend = ?1", [friend])?;
        if removed > 0 {
            record_mutation(
                &conn,
                actor,
                "grant_deleted",
                &format!("grant_deleted:{friend}:{}", now_utc()),
                &json!({"friend": friend}),
            )?;
        }
        Ok(removed > 0)
    }

    pub fn revoke_grant(&self, actor: &Actor, friend: &str) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE grants SET revoked_at = ?2, updated_at = ?2
             WHERE friend = ?1 AND revoked_at IS NULL",
            rusqlite::params![friend, now_utc()],
        )?;
        if changed == 0 {
            return Err(anyhow!("no active grant for {friend:?}"));
        }
        record_mutation(
            &conn,
            actor,
            "grant_revoked",
            &format!("grant_revoked:{friend}:{}", now_utc()),
            &json!({"friend": friend}),
        )?;
        Ok(())
    }

    /// The live grant for a friend, or a clear refusal.
    pub(crate) fn live_grant(&self, friend: &str) -> Result<GrantRow> {
        let grant = self
            .grant(friend)?
            .ok_or_else(|| anyhow!("no grant exists for {friend:?}"))?;
        if grant.revoked_at.is_some() {
            return Err(anyhow!("the grant for {friend:?} was revoked"));
        }
        if let Some(exp) = &grant.expires_at {
            if exp.as_str() < now_utc().as_str() {
                return Err(anyhow!("the grant for {friend:?} has expired"));
            }
        }
        Ok(grant)
    }

    /// FTS search under a friend's grant. Sphere list is bound into the
    /// SQL; private re-excluded regardless of what the row says.
    pub fn network_search(
        &self,
        friend: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<NetworkHit>> {
        // v46: the effective grant folds in the sharing matrix — the
        // episodes class must be on (legacy grants froze it on, so
        // nothing observable changed at the migration).
        // v54: grant by grant (store/groups.rs) — the life areas where
        // one grant, direct or through a group, covers memories.
        let access = self.access(friend)?;
        if !access.any(|g| g.has_class("episodes")) {
            return Err(anyhow!("the grant for {friend:?} doesn't cover memories"));
        }
        let granted = access.spheres(|g| g.has_class("episodes"));
        if granted.is_empty() {
            return Ok(Vec::new());
        }
        let spheres: Vec<&str> = granted
            .iter()
            .map(String::as_str)
            .filter(|s| *s != "private")
            .collect();
        let placeholders = spheres
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT e.id, substr(COALESCE(e.event_time, e.ingest_time), 1, 10),
                    e.kind, e.scope, e.title,
                    snippet(episode_fts, 1, '', '', '…', 24)
             FROM episode_fts f
             JOIN episodes e ON e.fts_rowid = f.rowid
             WHERE episode_fts MATCH ?1
               AND e.scope != 'private' AND e.superseded_by IS NULL
               AND e.scope IN ({placeholders})
             ORDER BY bm25(episode_fts) LIMIT ?2"
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![
            Box::new(super::search::sanitize_fts_query(query)),
            Box::new(limit as i64),
        ];
        for sphere in &spheres {
            params.push(Box::new(sphere.to_string()));
        }
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |row| {
                Ok(NetworkHit {
                    id: row.get(0)?,
                    day: row.get(1)?,
                    kind: row.get(2)?,
                    scope: row.get(3)?,
                    title: row.get(4)?,
                    excerpt: row.get(5)?,
                })
            },
        )?;
        let hits = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        record_mutation(
            &conn,
            &Actor::new(format!("friend:{friend}")),
            "network_access",
            &format!("network_access:{friend}:{}", crate::ids::entity_id()),
            &json!({"friend": friend, "op": "search", "query": query,
                    "hits": hits.len()}),
        )?;
        Ok(hits)
    }

    /// Full body of one episode — only under a `detail: full` grant, and
    /// only if the episode's scope is granted.
    pub fn network_fetch(
        &self,
        home: &crate::paths::Home,
        friend: &str,
        episode_id: &str,
    ) -> Result<String> {
        let access = self.access(friend)?;
        if !access.any(|g| g.has_class("episodes")) {
            return Err(anyhow!("the grant for {friend:?} doesn't cover memories"));
        }
        // Full bodies only in life areas where ONE grant gives both
        // memories and full detail.
        let full_spheres = access.spheres(|g| g.has_class("episodes") && g.detail == "full");
        if !access.any(|g| g.has_class("episodes") && g.detail == "full") {
            return Err(anyhow!(
                "the grant for {friend:?} covers excerpts only, not full bodies"
            ));
        }
        let conn = self.lock();
        let found: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT scope, content_ref FROM episodes WHERE id = ?1 AND superseded_by IS NULL",
                [episode_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((scope, content_ref)) = found else {
            return Err(anyhow!("no such memory"));
        };
        if scope == "private" || !full_spheres.contains(&scope) {
            // Same answer as not-found: don't oracle what exists.
            return Err(anyhow!("no such memory"));
        }
        record_mutation(
            &conn,
            &Actor::new(format!("friend:{friend}")),
            "network_access",
            &format!("network_access:{friend}:{}", crate::ids::entity_id()),
            &json!({"friend": friend, "op": "fetch", "episode": episode_id}),
        )?;
        drop(conn);
        let body = content_ref
            .map(|content_ref| {
                std::fs::read_to_string(home.root().join(content_ref)).unwrap_or_default()
            })
            .unwrap_or_default();
        Ok(body.chars().take(64 * 1024).collect())
    }
}

impl Store {
    /// Declared network state that replicates between MY OWN peered
    /// nodes (same owner, same trust): grants + friend-scoped key rows
    /// (hashes only — the secret never exists server-side anywhere).
    /// This is what lets a key minted on the laptop open the cloud node.
    pub fn export_declared(&self) -> Result<serde_json::Value> {
        let grants = self.list_grants()?;
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, key_hash, scopes, created_at, created_by, expires_at, revoked_at
             FROM api_keys WHERE scopes LIKE '%friend%'",
        )?;
        let keys: Vec<serde_json::Value> = stmt
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?, "name": row.get::<_, String>(1)?,
                    "key_hash": row.get::<_, String>(2)?, "scopes": row.get::<_, String>(3)?,
                    "created_at": row.get::<_, String>(4)?, "created_by": row.get::<_, String>(5)?,
                    "expires_at": row.get::<_, Option<String>>(6)?,
                    "revoked_at": row.get::<_, Option<String>>(7)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(json!({"grants": grants, "friend_keys": keys}))
    }

    /// Apply a peer's declared state — full-row upserts (last writer
    /// wins per row; both nodes are the same human).
    pub fn apply_declared(&self, declared: &serde_json::Value) -> Result<usize> {
        let conn = self.lock();
        let mut applied = 0usize;
        for g in declared
            .get("grants")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            let (Some(friend), Some(spheres), Some(detail), Some(created)) = (
                g.get("friend").and_then(|v| v.as_str()),
                g.get("spheres"),
                g.get("detail").and_then(|v| v.as_str()),
                g.get("created_at").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            conn.execute(
                "INSERT INTO grants (friend, spheres, detail, created_at, expires_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(friend) DO UPDATE SET spheres=excluded.spheres,
                     detail=excluded.detail, expires_at=excluded.expires_at,
                     revoked_at=excluded.revoked_at",
                rusqlite::params![
                    friend,
                    spheres.to_string().replace('\u{0}', ""),
                    detail,
                    created,
                    g.get("expires_at").and_then(|v| v.as_str()),
                    g.get("revoked_at").and_then(|v| v.as_str()),
                ],
            )?;
            applied += 1;
        }
        for k in declared
            .get("friend_keys")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            let (Some(id), Some(name), Some(hash), Some(scopes), Some(created), Some(by)) = (
                k.get("id").and_then(|v| v.as_str()),
                k.get("name").and_then(|v| v.as_str()),
                k.get("key_hash").and_then(|v| v.as_str()),
                k.get("scopes").and_then(|v| v.as_str()),
                k.get("created_at").and_then(|v| v.as_str()),
                k.get("created_by").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            conn.execute(
                "INSERT INTO api_keys (id, name, key_hash, scopes, created_at, created_by, expires_at, revoked_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET revoked_at=excluded.revoked_at,
                     expires_at=excluded.expires_at",
                rusqlite::params![
                    id, name, hash, scopes, created, by,
                    k.get("expires_at").and_then(|v| v.as_str()),
                    k.get("revoked_at").and_then(|v| v.as_str()),
                ],
            )?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Where friends should reach this brain: the always-on address.
    pub fn network_address(&self) -> Result<Option<String>> {
        self.consolidation_state("network_address")
    }

    pub fn set_network_address(&self, url: &str) -> Result<()> {
        self.set_consolidation_state("network_address", url.trim_end_matches('/'))
    }

    /// Recent network accesses, newest first — the transparency ledger.
    pub fn recent_network_accesses(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, serde_json::Value)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ts, payload FROM mutations WHERE kind = 'network_access'
             ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|r| {
            r.map(|(ts, p)| {
                (
                    ts,
                    serde_json::from_str(&p).unwrap_or(serde_json::Value::Null),
                )
            })
            .map_err(Into::into)
        })
        .collect()
    }
}

/// ————— outbound friend connections (the brains WE query) —————
/// Registry rides the sources table (kind='friend'); the bearer key for
/// each lives in ~/.hearthloom/.env, never here. `entity` optionally
/// binds the connection to the Person in the relationships graph — the
/// dossier and Relationships page render a "connected brain" chip.
impl Store {
    pub fn set_friend(
        &self,
        actor: &Actor,
        name: &str,
        url: &str,
        entity: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sources (name, kind, adapter, config, capabilities, first_seen)
             VALUES (?1, 'friend', 'friend', ?2, '[]', ?3)
             ON CONFLICT(name) DO UPDATE SET config = excluded.config",
            rusqlite::params![
                name,
                json!({"url": url.trim_end_matches('/'), "entity": entity}).to_string(),
                now_utc(),
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "friend_connected",
            &format!("friend_connected:{name}"),
            &json!({"name": name, "url": url, "entity": entity}),
        )?;
        Ok(())
    }

    /// (name, url, entity) triples for every connected friend brain.
    pub fn friends(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT name, config FROM sources WHERE kind = 'friend' ORDER BY name")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, config) = row?;
            if let Ok(c) = serde_json::from_str::<serde_json::Value>(&config) {
                if let Some(url) = c.get("url").and_then(|u| u.as_str()) {
                    out.push((
                        name,
                        url.to_string(),
                        c.get("entity").and_then(|e| e.as_str()).map(String::from),
                    ));
                }
            }
        }
        Ok(out)
    }

    pub fn remove_friend(&self, actor: &Actor, name: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM sources WHERE kind = 'friend' AND name = ?1",
            [name],
        )?;
        record_mutation(
            &conn,
            actor,
            "friend_removed",
            &format!("friend_removed:{name}"),
            &json!({"name": name}),
        )?;
        Ok(())
    }
}

fn row_to_grant(row: &rusqlite::Row) -> rusqlite::Result<GrantRow> {
    let spheres: String = row.get(1)?;
    Ok(GrantRow {
        friend: row.get(0)?,
        spheres: serde_json::from_str(&spheres).unwrap_or_default(),
        detail: row.get(2)?,
        created_at: row.get(3)?,
        expires_at: row.get(4)?,
        revoked_at: row.get(5)?,
        sharing: row
            .get::<_, Option<String>>(6)?
            .and_then(|s| serde_json::from_str(&s).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::paths::Home;
    use crate::time::{EventTime, TimeUncertainty};

    fn draft(source_ref: &str, title: &str, scope: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "test".into(),
            source_ref: source_ref.into(),
            kind: "note".into(),
            title: title.into(),
            body: format!("secret contents of {title}"),
            event_time: EventTime::parse_rfc3339("2026-07-01T10:00:00Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: "self".into(),
            scope: Some(scope.into()),
            entities: vec![],
            edges: vec![],
        }
    }

    fn seeded() -> (Store, Home, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().to_path_buf());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "human:test".into(),
                    session_id: None,
                    episodes: vec![
                        draft("w", "quarterly roadmap review", "work"),
                        draft("p", "weekend cabin plans", "personal"),
                        draft("x", "therapy reflections", "private"),
                    ],
                },
            )
            .unwrap();
        (store, home, tmp)
    }

    #[test]
    fn test_grant_scopes_are_binding() {
        let (store, _home, _tmp) = seeded();
        let actor = Actor::new("human:test");
        store
            .set_grant(&actor, "ada", &["work".into()], "excerpts", None)
            .unwrap();
        // work visible
        let hits = store.network_search("ada", "roadmap", 10).unwrap();
        assert_eq!(hits.len(), 1);
        // personal NOT granted
        assert!(store.network_search("ada", "cabin", 10).unwrap().is_empty());
        // private never, even if smuggled into the sphere list
        assert!(store
            .set_grant(&actor, "eve", &["private".into()], "excerpts", None)
            .is_err());
        store.vault_unlock(5); // must not matter
        assert!(store
            .network_search("ada", "therapy", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_fetch_needs_full_detail_and_granted_scope() {
        let (store, home, _tmp) = seeded();
        let actor = Actor::new("human:test");
        store
            .set_grant(&actor, "ada", &["work".into()], "excerpts", None)
            .unwrap();
        let hit = store.network_search("ada", "roadmap", 10).unwrap()[0].clone();
        assert!(store.network_fetch(&home, "ada", &hit.id).is_err()); // excerpts only
        store
            .set_grant(&actor, "ada", &["work".into()], "full", None)
            .unwrap();
        let body = store.network_fetch(&home, "ada", &hit.id).unwrap();
        assert!(body.contains("secret contents"));
        // an UNGRANTED-scope episode answers "no such memory" even on full
        let personal = store.network_search("ada", "cabin", 10).unwrap();
        assert!(personal.is_empty());
    }

    #[test]
    fn test_revoke_and_expiry_bite_immediately() {
        let (store, _home, _tmp) = seeded();
        let actor = Actor::new("human:test");
        store
            .set_grant(&actor, "ada", &["work".into()], "excerpts", None)
            .unwrap();
        store.revoke_grant(&actor, "ada").unwrap();
        assert!(store.network_search("ada", "roadmap", 10).is_err());
        store
            .set_grant(
                &actor,
                "bea",
                &["work".into()],
                "excerpts",
                Some("2020-01-01T00:00:00Z"),
            )
            .unwrap();
        assert!(store.network_search("bea", "roadmap", 10).is_err());
        assert!(store.network_search("carl", "roadmap", 10).is_err()); // no grant
    }

    #[test]
    fn test_fts_to_episodes_join_uses_the_v45_index() {
        // Regression for the 2026-07-28 tenant wedge: queries that
        // drive FROM episode_fts and probe episodes by fts_rowid
        // (network_search, brain_fs search, cascades) did a FULL episodes
        // scan per FTS match — quadratic on common tokens — because the
        // v12 pointer column was never indexed. The plan must show the
        // v45 index, not a table scan.
        let (store, _home, _tmp) = seeded();
        let plan: Vec<String> = {
            let conn = store.lock();
            let mut stmt = conn
                .prepare(
                    "EXPLAIN QUERY PLAN
                     SELECT e.id FROM episode_fts f
                     JOIN episodes e ON e.fts_rowid = f.rowid
                     WHERE episode_fts MATCH 'hello'",
                )
                .unwrap();
            let rows = stmt.query_map([], |row| row.get::<_, String>(3)).unwrap();
            rows.collect::<std::result::Result<_, _>>().unwrap()
        };
        let episodes_step = plan
            .iter()
            .find(|s| s.contains("episodes") && !s.contains("episode_fts"))
            .expect("plan has an episodes step");
        assert!(
            episodes_step.contains("idx_episodes_fts_rowid"),
            "episodes probe must use the v45 index, got: {plan:?}"
        );
    }
}
