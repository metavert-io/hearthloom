//! API keys (PATs): scoped bearer tokens so third-party integrations
//! (HearthFS first) don't ride on naked loopback endpoints. Only the
//! SHA-256 of a secret is stored — the plaintext exists exactly once, in
//! the create response. Revocation is a timestamp, never a delete (the
//! mutations log stays coherent); verification refuses revoked and
//! expired keys and stamps last_used_at.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{record_mutation, Actor, Store, LIFE_AREAS};
use crate::ids;
use crate::time::now_utc;

pub const API_KEY_SCOPES: &[&str] = &[
    "read", "write", "mcp", "peer", "friend", "notify", "ingest", "admin",
];

#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyRow {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: String,
    pub created_by: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
    /// Present on a SCOPED key: it reaches only what these grants name
    /// (default-deny), instead of the blanket access an ordinary key has.
    pub grants: Option<KeyGrants>,
}

/// How far a scoped key reaches into one class of data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    #[default]
    None,
    Read,
    Write,
}

impl Access {
    pub fn can_read(self) -> bool {
        self != Access::None
    }
    pub fn can_write(self) -> bool {
        self == Access::Write
    }
}

/// What a scoped key may do. Default-deny: a class left out is `none`.
/// `memory: write` means PROPOSE (quarantine) — nothing a key writes
/// enters the record without the human's yes. `capture` is the
/// write-only door the phone's lock-screen capture uses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct KeyGrants {
    #[serde(default)]
    pub tasks: Access,
    #[serde(default)]
    pub notes: Access,
    #[serde(default)]
    pub memory: Access,
    #[serde(default)]
    pub ask: bool,
    #[serde(default)]
    pub capture: bool,
    /// Non-GET requests allowed per rolling hour (None = no cap).
    #[serde(default)]
    pub writes_per_hour: Option<u32>,
    /// Ask runs allowed per day — each one spends model tokens.
    #[serde(default)]
    pub asks_per_day: Option<u32>,
    /// Life areas this key's reads are limited to (a subset of
    /// `LIFE_AREAS`). None = every area except private.
    #[serde(default)]
    pub areas: Option<Vec<String>>,
    /// One of the owner's OWN devices (the phone's lock-screen capture, a
    /// Shortcut on their Watch): what it writes is recorded as the owner's
    /// own words, fully trusted, instead of `agent:key:<name>`. Only the
    /// owner sets it, at minting (key management needs the admin); it
    /// widens nothing this key can reach.
    #[serde(default)]
    pub own_device: bool,
}

/// The middleware's answer for one request by a scoped key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantVerdict {
    /// Not on any scoped key's route table.
    Denied,
    /// Allowed; `reads` = it returns record content (so it is refused
    /// while the vault is open — a scoped key never sees private data).
    Allowed { reads: bool, is_ask: bool },
}

impl KeyGrants {
    pub fn grants_nothing(&self) -> bool {
        self.tasks == Access::None
            && self.notes == Access::None
            && self.memory == Access::None
            && !self.ask
            && !self.capture
    }

    /// Refuse a grant set that can't be honoured: an unknown or empty
    /// area list, or an area limit combined with Ask (Ask's tools read
    /// through raw SQL and a thread pool the area filter doesn't cover).
    pub fn validate(&self) -> Result<()> {
        if self.own_device && !self.capture && !self.tasks.can_write() && !self.notes.can_write() {
            return Err(anyhow!(
                "an own-device key needs something to write: capture, tasks or notes"
            ));
        }
        let Some(areas) = &self.areas else {
            return Ok(());
        };
        if areas.is_empty() {
            return Err(anyhow!("an area-limited key needs at least one area"));
        }
        for area in areas {
            if !LIFE_AREAS.contains(&area.as_str()) {
                return Err(anyhow!(
                    "unknown area {area:?} (valid: {})",
                    LIFE_AREAS.join(", ")
                ));
            }
        }
        if self.ask {
            return Err(anyhow!(
                "an area-limited key can't use Ask — drop the area limit or the Ask grant"
            ));
        }
        Ok(())
    }

    /// THE SCOPED-KEY ROUTE TABLE (default-deny, the plugin ceiling's
    /// twin). Adding a route a scoped key should reach means adding it
    /// HERE; everything else answers 403.
    pub fn permits(&self, method: &str, path: &str) -> GrantVerdict {
        let get = method.eq_ignore_ascii_case("GET");
        let under = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
        let allow = |ok: bool, reads: bool| {
            if ok {
                GrantVerdict::Allowed {
                    reads,
                    is_ask: false,
                }
            } else {
                GrantVerdict::Denied
            }
        };
        if get && (path == "/health" || path == "/v1/version") {
            return allow(true, false);
        }
        // Reads no guard filters by scope (purged-task text, per-origin
        // counts, the vault's own listing) stay off every scoped key.
        if get
            && [
                "/v1/tasks/deleted",
                "/v1/notes/vaulted",
                "/v1/notes/origins",
            ]
            .contains(&path)
        {
            return GrantVerdict::Denied;
        }
        if under("/v1/tasks") {
            return if get {
                allow(self.tasks.can_read(), true)
            } else {
                allow(self.tasks.can_write(), false)
            };
        }
        if under("/v1/notes") {
            return if get {
                allow(self.notes.can_read(), true)
            } else {
                allow(self.notes.can_write(), false)
            };
        }
        if get
            && [
                "/v1/search",
                "/v1/recall",
                "/v1/traverse",
                "/v1/entity",
                "/v1/episode",
                "/v1/timeline",
            ]
            .iter()
            .any(|p| under(p))
        {
            return allow(self.memory.can_read(), true);
        }
        if !get && path == "/v1/propose" {
            return allow(self.memory.can_write(), false);
        }
        if path == "/mcp" {
            // Tool-level checks (and the vault rule for its read tools)
            // live in mcp.rs — the endpoint itself needs any data grant.
            let any = self.tasks != Access::None
                || self.notes != Access::None
                || self.memory != Access::None;
            return allow(any, false);
        }
        if path == "/v1/query" || path == "/v1/query/stream" {
            return if self.ask && self.areas.is_none() {
                GrantVerdict::Allowed {
                    reads: true,
                    is_ask: true,
                }
            } else {
                GrantVerdict::Denied
            };
        }
        if !get && path == "/v1/ingest/capture" {
            return allow(self.capture, false);
        }
        GrantVerdict::Denied
    }
}

/// What a verified token grants.
#[derive(Debug, Clone)]
pub struct ApiKeyClaims {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub grants: Option<KeyGrants>,
}

impl ApiKeyClaims {
    /// The actor every write by this key is recorded under — fixed by
    /// the server, never taken from the request, so the activity list
    /// and Undo can trust it.
    pub fn agent_actor(&self) -> String {
        format!("agent:key:{}", self.name)
    }
}

fn hash_of(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl Store {
    /// Mint a key. Returns the row AND the plaintext secret — the only
    /// time it is ever available.
    pub fn create_api_key(
        &self,
        actor: &Actor,
        name: &str,
        scopes: &[String],
        expires_at: Option<&str>,
    ) -> Result<(ApiKeyRow, String)> {
        self.create_api_key_scoped(actor, name, scopes, expires_at, None)
    }

    /// Mint a key, optionally SCOPED by `grants` (then `scopes` may be
    /// empty — the grants are the whole of what it can do).
    pub fn create_api_key_scoped(
        &self,
        actor: &Actor,
        name: &str,
        scopes: &[String],
        expires_at: Option<&str>,
        grants: Option<&KeyGrants>,
    ) -> Result<(ApiKeyRow, String)> {
        if let Some(g) = grants {
            if g.grants_nothing() {
                return Err(anyhow!("a scoped key needs at least one grant"));
            }
            if !scopes.is_empty() {
                return Err(anyhow!(
                    "a scoped key takes grants, not scopes — choose one"
                ));
            }
            g.validate()?;
        }
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("a key needs a name (what will hold it?)"));
        }
        if scopes.is_empty() && grants.is_none() {
            return Err(anyhow!("a key needs at least one scope"));
        }
        for scope in scopes {
            if !API_KEY_SCOPES.contains(&scope.as_str()) {
                return Err(anyhow!(
                    "unknown scope {scope:?} (valid: {})",
                    API_KEY_SCOPES.join(", ")
                ));
            }
        }
        let id = ids::api_key_id();
        // Two UUIDs = 256 bits of secret; `hl_` marks it recognizably
        // ours in configs and secret scanners.
        let secret = format!(
            "hl_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let now = now_utc();
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO api_keys (id, name, key_hash, scopes, created_at, created_by, expires_at, grants)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    id,
                    name,
                    hash_of(&secret),
                    serde_json::json!(scopes).to_string(),
                    now,
                    actor.actor,
                    expires_at,
                    grants.map(|g| serde_json::json!(g).to_string()),
                ],
            )?;
            record_mutation(
                &conn,
                actor,
                "api_key_created",
                &format!("api_key_created:{id}"),
                &serde_json::json!({"id": id, "name": name, "scopes": scopes}),
            )?;
        }
        let row = self
            .api_key(&id)?
            .ok_or_else(|| anyhow!("key vanished after insert"))?;
        Ok((row, secret))
    }

    pub fn api_key(&self, id: &str) -> Result<Option<ApiKeyRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, name, scopes, created_at, created_by, expires_at, last_used_at, revoked_at, grants
             FROM api_keys WHERE id = ?1",
            [id],
            row_to_key,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_api_keys(&self) -> Result<Vec<ApiKeyRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, scopes, created_at, created_by, expires_at, last_used_at, revoked_at, grants
             FROM api_keys ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_key)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Revoke = stamp, never delete.
    pub fn revoke_api_key(&self, actor: &Actor, id: &str) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE api_keys SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
            rusqlite::params![id, now_utc()],
        )?;
        if changed == 0 {
            return Err(anyhow!("no active key {id:?}"));
        }
        record_mutation(
            &conn,
            actor,
            "api_key_revoked",
            &format!("api_key_revoked:{id}"),
            &serde_json::json!({"id": id}),
        )?;
        Ok(())
    }

    /// Validate a presented token. None = unknown/revoked/expired (the
    /// caller answers 401 without distinguishing — don't oracle).
    pub fn verify_api_key(&self, token: &str) -> Result<Option<ApiKeyClaims>> {
        let hash = hash_of(token);
        let now = now_utc();
        let conn = self.lock();
        let found: Option<(String, String, String, Option<String>)> = conn
            .query_row(
                "SELECT id, name, scopes, grants FROM api_keys
                 WHERE key_hash = ?1
                   AND revoked_at IS NULL
                   AND (expires_at IS NULL OR expires_at > ?2)",
                rusqlite::params![hash, now],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((id, name, scopes, grants)) = found else {
            return Ok(None);
        };
        conn.execute(
            "UPDATE api_keys SET last_used_at = ?2 WHERE id = ?1",
            rusqlite::params![id, now],
        )?;
        Ok(Some(ApiKeyClaims {
            id,
            name,
            scopes: serde_json::from_str(&scopes).unwrap_or_default(),
            // A grants column that fails to parse must NOT fall back to
            // an unscoped key: deny-all is the safe reading.
            grants: grants.map(|raw| serde_json::from_str(&raw).unwrap_or_default()),
        }))
    }

    /* —— access requests: an agent asks, the human approves —— */

    /// An agent asks for a scoped key. Returns (request id, poll secret);
    /// nothing is granted until a human approves, and the request lapses
    /// after `REQUEST_WINDOW_SECS`.
    pub fn key_request_create(
        &self,
        name: &str,
        grants: &KeyGrants,
        ttl_secs: Option<i64>,
    ) -> Result<(String, String)> {
        let name = name.trim();
        if name.is_empty() || name.len() > 80 {
            return Err(anyhow!("a request needs a short name (who is asking?)"));
        }
        if grants.grants_nothing() {
            return Err(anyhow!("a request must ask for at least one grant"));
        }
        grants.validate()?;
        let id = format!("kr_{}", uuid::Uuid::new_v4().simple());
        let poll = format!("krs_{}", uuid::Uuid::new_v4().simple());
        let conn = self.lock();
        let pending: i64 = conn.query_row(
            "SELECT COUNT(*) FROM key_requests WHERE status = 'pending' AND created_at > ?1",
            [request_cutoff()],
            |row| row.get(0),
        )?;
        if pending >= 5 {
            return Err(anyhow!("too many access requests are already waiting"));
        }
        conn.execute(
            "INSERT INTO key_requests (id, name, grants, ttl_secs, poll_hash, created_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending')",
            rusqlite::params![
                id,
                name,
                serde_json::json!(grants).to_string(),
                ttl_secs,
                hash_of(&poll),
                now_utc()
            ],
        )?;
        Ok((id, poll))
    }

    /// Requests still inside their approval window.
    pub fn key_requests_pending(&self) -> Result<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, grants, ttl_secs, created_at FROM key_requests
             WHERE status = 'pending' AND created_at > ?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map([request_cutoff()], |row| {
                let grants: String = row.get(2)?;
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "grants": serde_json::from_str::<serde_json::Value>(&grants).unwrap_or_default(),
                    "ttl_secs": row.get::<_, Option<i64>>(3)?,
                    "created_at": row.get::<_, String>(4)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Approve (mint the key, held once for the requester to collect) or
    /// deny. False = no such live request.
    pub fn key_request_decide(&self, actor: &Actor, id: &str, approve: bool) -> Result<bool> {
        let found: Option<(String, String, Option<i64>)> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT name, grants, ttl_secs FROM key_requests
                 WHERE id = ?1 AND status = 'pending' AND created_at > ?2",
                rusqlite::params![id, request_cutoff()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
        };
        let Some((name, grants, ttl)) = found else {
            return Ok(false);
        };
        let issued = if approve {
            let grants: KeyGrants = serde_json::from_str(&grants)?;
            let expires = ttl.map(|secs| {
                (chrono::Utc::now() + chrono::Duration::seconds(secs.max(60)))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string()
            });
            let (_, secret) =
                self.create_api_key_scoped(actor, &name, &[], expires.as_deref(), Some(&grants))?;
            Some(secret)
        } else {
            None
        };
        let conn = self.lock();
        conn.execute(
            "UPDATE key_requests SET status = ?2, issued_secret = ?3, decided_at = ?4 WHERE id = ?1",
            rusqlite::params![id, if approve { "approved" } else { "denied" }, issued, now_utc()],
        )?;
        Ok(true)
    }

    /// The requester's poll. Returns (status, secret). The secret is
    /// handed over exactly ONCE and then erased from the row.
    pub fn key_request_poll(
        &self,
        id: &str,
        poll_secret: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        let conn = self.lock();
        let found: Option<(String, Option<String>, String)> = conn
            .query_row(
                "SELECT status, issued_secret, created_at FROM key_requests WHERE id = ?1 AND poll_hash = ?2",
                rusqlite::params![id, hash_of(poll_secret)],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((status, secret, created)) = found else {
            return Ok(None);
        };
        if status == "pending" && created <= request_cutoff() {
            return Ok(Some(("expired".into(), None)));
        }
        if secret.is_some() {
            conn.execute(
                "UPDATE key_requests SET issued_secret = NULL WHERE id = ?1",
                [id],
            )?;
        }
        Ok(Some((status, secret)))
    }

    /// What a key's agent actor has written (operational journal), newest
    /// first — the activity list behind Undo.
    pub fn key_activity(&self, agent_actor: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT entry_id, entity_kind, entity_id, op, post_image, ts FROM op_journal
             WHERE actor = ?1 ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![agent_actor, limit as i64], |row| {
                let image: Option<String> = row.get(4)?;
                let image: serde_json::Value = image
                    .and_then(|raw| serde_json::from_str(&raw).ok())
                    .unwrap_or_default();
                let label = ["description", "title", "name", "text"]
                    .iter()
                    .find_map(|k| image.get(*k).and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .chars()
                    .take(140)
                    .collect::<String>();
                Ok(serde_json::json!({
                    "entry_id": row.get::<_, Option<String>>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "entity_id": row.get::<_, String>(2)?,
                    "op": row.get::<_, String>(3)?,
                    "label": label,
                    "ts": row.get::<_, String>(5)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// How long an access request waits for a human before it lapses.
pub const REQUEST_WINDOW_SECS: i64 = 600;

fn request_cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(REQUEST_WINDOW_SECS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn row_to_key(row: &rusqlite::Row) -> rusqlite::Result<ApiKeyRow> {
    let scopes: String = row.get(2)?;
    Ok(ApiKeyRow {
        id: row.get(0)?,
        name: row.get(1)?,
        scopes: serde_json::from_str(&scopes).unwrap_or_default(),
        created_at: row.get(3)?,
        created_by: row.get(4)?,
        expires_at: row.get(5)?,
        last_used_at: row.get(6)?,
        revoked_at: row.get(7)?,
        grants: row
            .get::<_, Option<String>>(8)?
            .map(|raw| serde_json::from_str(&raw).unwrap_or_default()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn test_store() -> (Store, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.db")).unwrap();
        (store, tmp)
    }

    #[test]
    fn test_key_lifecycle() {
        let (store, _tmp) = test_store();
        let actor = Actor::new("human:test");
        let (row, secret) = store
            .create_api_key(&actor, "hearthfs", &["read".into()], None)
            .unwrap();
        assert!(secret.starts_with("hl_"));
        assert_eq!(row.scopes, vec!["read"]);

        // verify grants; a wrong token doesn't
        let claims = store.verify_api_key(&secret).unwrap().unwrap();
        assert_eq!(claims.id, row.id);
        assert!(store.verify_api_key("hl_wrong").unwrap().is_none());
        // last_used stamped
        assert!(store
            .api_key(&row.id)
            .unwrap()
            .unwrap()
            .last_used_at
            .is_some());

        // revoke kills it; list still shows it (history, not deletion)
        store.revoke_api_key(&actor, &row.id).unwrap();
        assert!(store.verify_api_key(&secret).unwrap().is_none());
        assert_eq!(store.list_api_keys().unwrap().len(), 1);
        assert!(store.revoke_api_key(&actor, &row.id).is_err());
    }

    #[test]
    fn test_expired_key_refused() {
        let (store, _tmp) = test_store();
        let actor = Actor::new("human:test");
        let (_, secret) = store
            .create_api_key(
                &actor,
                "old",
                &["read".into()],
                Some("2020-01-01T00:00:00Z"),
            )
            .unwrap();
        assert!(store.verify_api_key(&secret).unwrap().is_none());
    }

    #[test]
    fn test_bad_scope_refused() {
        let (store, _tmp) = test_store();
        let actor = Actor::new("human:test");
        assert!(store
            .create_api_key(&actor, "x", &["root".into()], None)
            .is_err());
    }

    #[test]
    fn test_scoped_key_route_table_is_default_deny() {
        let g = KeyGrants {
            tasks: Access::Write,
            notes: Access::Read,
            capture: true,
            ..Default::default()
        };
        let ok = |m: &str, p: &str| matches!(g.permits(m, p), GrantVerdict::Allowed { .. });
        assert!(
            ok("GET", "/v1/tasks") && ok("POST", "/v1/tasks") && ok("DELETE", "/v1/tasks/tk_1")
        );
        assert!(
            ok("GET", "/v1/notes/n1") && !ok("POST", "/v1/notes"),
            "notes are read-only here"
        );
        assert!(ok("POST", "/v1/ingest/capture") && ok("GET", "/health") && ok("POST", "/mcp"));
        // Everything not on the table is denied — including what an
        // ordinary key reaches by default.
        for (m, p) in [
            ("GET", "/v1/search"),
            ("POST", "/v1/query"),
            ("GET", "/v1/keys"),
            ("POST", "/v1/vault"),
            ("GET", "/v1/export/start"),
            ("POST", "/v1/sql"),
            ("GET", "/v1/llm/usage"),
            ("GET", "/v1/tasksX"),
            ("POST", "/v1/ingest/health"),
        ] {
            assert!(!ok(m, p), "{m} {p} must be denied");
        }
        // Reads are marked so the vault rule can refuse them; writes aren't.
        assert_eq!(
            g.permits("GET", "/v1/tasks"),
            GrantVerdict::Allowed {
                reads: true,
                is_ask: false
            }
        );
        assert_eq!(
            g.permits("POST", "/v1/tasks"),
            GrantVerdict::Allowed {
                reads: false,
                is_ask: false
            }
        );
        let ask = KeyGrants {
            ask: true,
            ..Default::default()
        };
        assert_eq!(
            ask.permits("POST", "/v1/query"),
            GrantVerdict::Allowed {
                reads: true,
                is_ask: true
            }
        );
        assert!(KeyGrants::default().grants_nothing());
        // Unguarded reads are off the table whatever the grants.
        let all = KeyGrants {
            tasks: Access::Write,
            notes: Access::Write,
            ..Default::default()
        };
        for p in [
            "/v1/tasks/deleted",
            "/v1/notes/vaulted",
            "/v1/notes/origins",
        ] {
            assert_eq!(all.permits("GET", p), GrantVerdict::Denied, "{p}");
        }
    }

    #[test]
    fn test_own_device_needs_something_to_write() {
        let own = |g: KeyGrants| KeyGrants {
            own_device: true,
            ..g
        };
        assert!(own(KeyGrants {
            capture: true,
            ..Default::default()
        })
        .validate()
        .is_ok());
        assert!(own(KeyGrants {
            notes: Access::Write,
            ..Default::default()
        })
        .validate()
        .is_ok());
        assert!(own(KeyGrants {
            memory: Access::Read,
            ..Default::default()
        })
        .validate()
        .is_err());
        // Keys minted before the flag existed read as not-own-device.
        let old: KeyGrants = serde_json::from_str(r#"{"capture": true}"#).unwrap();
        assert!(!old.own_device);
    }

    #[test]
    fn test_area_limited_grants_validate() {
        let areas = |a: &[&str]| Some(a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let ok = KeyGrants {
            memory: Access::Read,
            areas: areas(&["work"]),
            ..Default::default()
        };
        assert!(ok.validate().is_ok());
        for bad in [areas(&[]), areas(&["private"]), areas(&["work", "nope"])] {
            let g = KeyGrants {
                memory: Access::Read,
                areas: bad,
                ..Default::default()
            };
            assert!(g.validate().is_err());
        }
        // Ask and an area limit don't combine — at mint, at request, and
        // (for a row written some other way) at the route table.
        let ask = KeyGrants {
            ask: true,
            areas: areas(&["work"]),
            ..Default::default()
        };
        assert!(ask.validate().is_err());
        assert_eq!(ask.permits("POST", "/v1/query"), GrantVerdict::Denied);
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");
        assert!(store
            .create_api_key_scoped(&actor, "a", &[], None, Some(&ask))
            .is_err());
        assert!(store.key_request_create("a", &ask, None).is_err());
        // The limit survives the round trip through the grants column.
        let (_, secret) = store
            .create_api_key_scoped(&actor, "work-agent", &[], None, Some(&ok))
            .unwrap();
        let claims = store.verify_api_key(&secret).unwrap().unwrap();
        assert_eq!(claims.grants.unwrap().areas, areas(&["work"]));
    }

    #[test]
    fn test_scoped_key_mint_verify_and_request_flow() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");
        let grants = KeyGrants {
            tasks: Access::Write,
            ..Default::default()
        };
        // Grants and scopes don't mix; an empty grant set is refused.
        assert!(store
            .create_api_key_scoped(&actor, "x", &["read".into()], None, Some(&grants))
            .is_err());
        assert!(store
            .create_api_key_scoped(&actor, "x", &[], None, Some(&KeyGrants::default()))
            .is_err());
        let (row, secret) = store
            .create_api_key_scoped(&actor, "agent-a", &[], None, Some(&grants))
            .unwrap();
        assert_eq!(row.grants.as_ref(), Some(&grants));
        let claims = store.verify_api_key(&secret).unwrap().unwrap();
        assert_eq!(claims.grants, Some(grants.clone()));
        assert_eq!(claims.agent_actor(), "agent:key:agent-a");
        // An ordinary key stays unscoped.
        let (_, plain) = store
            .create_api_key(&actor, "plain", &["read".into()], None)
            .unwrap();
        assert!(store
            .verify_api_key(&plain)
            .unwrap()
            .unwrap()
            .grants
            .is_none());

        // Request → nothing until approved → secret handed over once.
        let (id, poll) = store
            .key_request_create("claude-code", &grants, Some(3600))
            .unwrap();
        assert_eq!(store.key_requests_pending().unwrap().len(), 1);
        assert_eq!(
            store.key_request_poll(&id, &poll).unwrap(),
            Some(("pending".into(), None))
        );
        assert_eq!(store.key_request_poll(&id, "wrong").unwrap(), None);
        assert!(store.key_request_decide(&actor, &id, true).unwrap());
        let (status, issued) = store.key_request_poll(&id, &poll).unwrap().unwrap();
        assert_eq!(status, "approved");
        let issued = issued.expect("secret on first poll");
        assert!(store
            .verify_api_key(&issued)
            .unwrap()
            .unwrap()
            .grants
            .is_some());
        assert_eq!(
            store.key_request_poll(&id, &poll).unwrap(),
            Some(("approved".into(), None)),
            "only once"
        );
        assert!(
            !store.key_request_decide(&actor, &id, true).unwrap(),
            "already decided"
        );
        // Denied requests mint nothing.
        let (id2, poll2) = store.key_request_create("other", &grants, None).unwrap();
        assert!(store.key_request_decide(&actor, &id2, false).unwrap());
        assert_eq!(
            store.key_request_poll(&id2, &poll2).unwrap(),
            Some(("denied".into(), None))
        );
    }
}
