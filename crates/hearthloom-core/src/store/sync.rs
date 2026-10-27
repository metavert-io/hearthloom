//! Hybrid peer sync, the export half (deployment modes doctrine,
//! 2026-07-11). Law #1 is the sync protocol: only EPISODES move —
//! append-only, deterministic ids (hash of source+source_ref), so
//! replication is idempotent in both directions and echo-back is a
//! no-op. Each node rebuilds its own derived state.
//!
//! A user's OWN peers are FULL REPLICAS — private included (Jon's
//! 2026-07-11 reversal: a privacy relabel must take effect on every
//! node; both nodes vault-lock it). The boundary where private can
//! never go is the FRIEND surface (store/network.rs). Scope changes
//! replicate via scope_changed_at LWW exchange.
//!
//! v1 simplifications, recorded in the design log: edges don't sync
//! (bi-temporal merge needs its own design; the graph skeleton travels
//! as entity refs and each side re-links), identity-attribute claims
//! stay home, and a synced episode's write-actor on the receiving node
//! is `peer:<name>` (original actors remain in the origin's record).

use anyhow::Result;
use serde_json::json;

use super::{Actor, Store};
use crate::ingest::{EntityRef, EpisodeDraft};
use crate::paths::Home;
use crate::time::{now_utc, EventTime, TimeUncertainty};

fn b64(bytes: &[u8]) -> String {
    // minimal base64 (no dep in core): standard alphabet, padded
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let clean: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The tables the operational mirror replicates (multi-master per-row
/// merge — see `apply_operational`). Shared by `export_operational`,
/// `apply_operational`,
/// and `operational_count` so the progress denominator can never
/// drift from what actually ships.
const OPERATIONAL_TABLES: &[&str] = &[
    "notes_index",
    "tasks",
    // v46: item shares mirror between a user's own peers so a share
    // flagged on either node is served by the always-on one (shares
    // name the friend; they bite wherever a grant by that name lives).
    // GRANTS deliberately do NOT mirror: a grant pairs with the friend
    // KEY minted on that node (api_keys are node-local), and the table's
    // PK is `friend` not `id`, so the mirror's LWW gate can't protect it.
    "shared_items",
    "task_templates",
    "intents",
    "goal_projects",
    "self_model",
    "privacy_rules",
    "standing_queries",
    // v37: the rename audit trail rides the mirror; the receiver
    // REPLAYS it against its own entities by (type, from_name) —
    // entity ids differ across peers (the relationships_by_name
    // precedent). Append-only, so replay is idempotent.
    "entity_renames",
];

/// The journaled operational kinds: (entity_kind, table, LWW timestamp
/// expression — not every table has updated_at). Consulted by the
/// journal-sync tombstone leg (`apply_journal`) and by the mirror's
/// resurrection gate in `apply_operational`; a kind absent here has its
/// entry stored but no row effect applied (wire-compat with a newer
/// peer that journals kinds this build doesn't know).
const JOURNALED_KINDS: &[(&str, &str, &str)] = &[
    ("task", "tasks", "COALESCE(updated_at, created_at, '')"),
    ("shared_item", "shared_items", "COALESCE(created_at, '')"),
    (
        "note",
        "notes_index",
        "COALESCE(updated_at, created_at, '')",
    ),
    ("intent", "intents", "COALESCE(created_at, '')"),
    ("privacy_rule", "privacy_rules", "COALESCE(created_at, '')"),
    (
        "self_model",
        "self_model",
        "COALESCE(updated_at, created_at, '')",
    ),
];

fn journaled_kind(kind: &str) -> Option<(&'static str, &'static str)> {
    JOURNALED_KINDS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .map(|(_, table, ts)| (*table, *ts))
}

fn journaled_table(table: &str) -> Option<&'static str> {
    JOURNALED_KINDS
        .iter()
        .find(|(_, t, _)| *t == table)
        .map(|(k, _, _)| *k)
}

/// One page of the export feed: ingest-ready drafts plus the cursor to
/// ask for next (the LAST row's rowid; hand it back as `after`).
#[derive(Debug)]
pub struct SyncPage {
    pub episodes: Vec<EpisodeDraft>,
    pub next_cursor: i64,
    /// True when there was nothing past `after`.
    pub drained: bool,
}

impl Store {
    /// Episodes after `after` (an episodes.rowid cursor), as drafts a
    /// peer can POST straight into /v1/ingest. Private is excluded at
    /// the SQL layer, always. Bodies are re-read from raw/ outside the
    /// store lock.
    pub fn sync_export(&self, home: &Home, after: i64, limit: usize) -> Result<SyncPage> {
        struct Row {
            rowid: i64,
            id: String,
            source: String,
            source_ref: String,
            kind: String,
            title: String,
            event_time: Option<String>,
            tz_offset_minutes: Option<i64>,
            time_uncertainty: String,
            trust: String,
            scope: String,
            content_ref: Option<String>,
        }
        let rows: Vec<Row> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                // actor NOT LIKE 'peer:%': never re-export what a peer
                // sent us — kills echo traffic in a two-node pairing.
                // (Transitive multi-node sync would revisit this; v1 is
                // exactly two nodes by doctrine.)
                // Since 2026-07-11 (Jon): a user's OWN peers are full
                // replicas — private included, vault-locked on both
                // sides. Only the FRIEND surface excludes private.
                "SELECT rowid, id, source, source_ref, kind, title, event_time,
                        tz_offset_minutes, time_uncertainty, trust, scope, content_ref
                 FROM episodes /* all-episodes: peer sync carries every episode; a retirement travels as its own event */
                 WHERE rowid > ?1 AND actor NOT LIKE 'peer:%'
                 ORDER BY rowid LIMIT ?2",
            )?;
            let mapped = stmt.query_map(rusqlite::params![after, limit as i64], |row| {
                Ok(Row {
                    rowid: row.get(0)?,
                    id: row.get(1)?,
                    source: row.get(2)?,
                    source_ref: row.get(3)?,
                    kind: row.get(4)?,
                    title: row.get(5)?,
                    event_time: row.get(6)?,
                    tz_offset_minutes: row.get(7)?,
                    time_uncertainty: row.get(8)?,
                    trust: row.get(9)?,
                    scope: row.get(10)?,
                    content_ref: row.get(11)?,
                })
            })?;
            mapped.collect::<std::result::Result<Vec<_>, _>>()?
        };
        // entity refs per episode — a second short lock, then file reads
        // happen with no lock held at all.
        let mut episodes = Vec::with_capacity(rows.len());
        let mut next_cursor = after;
        for row in rows {
            next_cursor = row.rowid;
            let entities: Vec<EntityRef> = {
                let conn = self.lock();
                let mut stmt = conn.prepare(
                    "SELECT e.type, e.name, ee.role
                     FROM episode_entities ee JOIN entities e ON e.id = ee.entity_id /* all-episodes: peer sync carries every episode; a retirement travels as its own event */
                     WHERE ee.episode_id = ?1 AND e.scope != 'private' LIMIT 50",
                )?;
                let refs = stmt.query_map([&row.id], |r| {
                    Ok(EntityRef {
                        entity_type: r.get(0)?,
                        name: r.get(1)?,
                        attributes: vec![],
                        role: r.get(2)?,
                    })
                })?;
                refs.collect::<std::result::Result<Vec<_>, _>>()?
            };
            let body = row
                .content_ref
                .as_deref()
                .map(|content_ref| home.root().join(content_ref))
                .and_then(|path| std::fs::read_to_string(path).ok())
                .unwrap_or_default();
            episodes.push(EpisodeDraft {
                source: row.source,
                source_ref: row.source_ref,
                kind: row.kind,
                title: row.title,
                body,
                event_time: EventTime {
                    utc: row.event_time,
                    tz_offset_minutes: row.tz_offset_minutes.map(|m| m as i32),
                    uncertainty: TimeUncertainty::parse(&row.time_uncertainty)
                        .unwrap_or(TimeUncertainty::Unknown),
                },
                trust: row.trust,
                scope: Some(row.scope),
                entities,
                edges: vec![],
            });
        }
        Ok(SyncPage {
            drained: episodes.is_empty(),
            episodes,
            next_cursor,
        })
    }

    /// Scope changes stamped after `since` (RFC3339), for LWW exchange
    /// between a user's own peers. Bounded: only rows whose scope was
    /// ever corrected carry a stamp.
    /// The relay cursor after `scope_changes_since(since)` (v57).
    pub fn scope_changes_cursor(&self, since: &str) -> Result<String> {
        let conn = self.lock();
        super::page_cursor(&conn, "scope_synced_at",
            "FROM episodes WHERE scope_changed_at IS NOT NULL AND scope_synced_at > ?1 /* all-episodes: peer sync carries every episode; a retirement travels as its own event */", since, 5000)
    }

    pub fn scope_changes_since(&self, since: &str) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, scope, scope_changed_at FROM episodes /* all-episodes: peer sync carries every episode; a retirement travels as its own event */
             WHERE scope_changed_at IS NOT NULL AND scope_synced_at > ?1
             ORDER BY scope_synced_at LIMIT 5000",
        )?;
        let rows = stmt.query_map([since], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Apply a peer's scope changes: last writer wins per episode; a
    /// change is applied only when strictly newer than our own stamp.
    pub fn apply_scope_changes(
        &self,
        actor: &Actor,
        changes: &[(String, String, String)],
    ) -> Result<usize> {
        let conn = self.lock();
        let mut applied = 0usize;
        for (id, scope, ts) in changes {
            let changed = conn.execute(
                "UPDATE episodes SET scope = ?2, scope_changed_at = ?3
                 WHERE id = ?1 AND COALESCE(scope_changed_at, '') < ?3 AND scope != ?2",
                rusqlite::params![id, scope, ts],
            )?;
            if changed > 0 {
                applied += 1;
                super::record_mutation(
                    &conn,
                    actor,
                    "scope_synced",
                    &format!("scope_synced:{id}:{ts}"),
                    &serde_json::json!({"episode": id, "scope": scope, "ts": ts}),
                )?;
            }
        }
        Ok(applied)
    }

    /// Full operational-state mirror (multi-master: both peers export and
    /// merge — see `apply_operational` for the LWW reconciliation): the
    /// tables that make the remote's dashboard and
    /// Ask whole — notes (with file bodies), tasks, templates, goals,
    /// self-model, privacy rules, standing queries. Verbatim rows (ids
    /// included) so intra-table references stay consistent;
    /// relationships travel by entity NAME because entity ids are
    /// minted per-node. Mentions indexes are rebuilt remotely later
    /// (v2) — the one known gap.
    pub fn export_operational(&self, home: &Home) -> Result<serde_json::Value> {
        let conn = self.lock();
        let mut out = serde_json::Map::new();
        for table in OPERATIONAL_TABLES {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table}"))?;
            let cols: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
            let rows = stmt.query_map([], |row| {
                let mut obj = serde_json::Map::new();
                for (i, col) in cols.iter().enumerate() {
                    let v: rusqlite::types::Value = row.get(i)?;
                    obj.insert(
                        col.clone(),
                        match v {
                            rusqlite::types::Value::Null => serde_json::Value::Null,
                            rusqlite::types::Value::Integer(n) => serde_json::json!(n),
                            rusqlite::types::Value::Real(f) => serde_json::json!(f),
                            rusqlite::types::Value::Text(t) => serde_json::json!(t),
                            rusqlite::types::Value::Blob(b) => {
                                serde_json::json!(format!("__b64__{}", b64(&b)))
                            }
                        },
                    );
                }
                Ok(serde_json::Value::Object(obj))
            })?;
            out.insert(
                table.to_string(),
                serde_json::Value::Array(rows.collect::<std::result::Result<_, _>>()?),
            );
        }
        // relationships by NAME (entity ids are per-node)
        let rels: Vec<serde_json::Value> = {
            let mut stmt = conn.prepare(
                "SELECT e.name, e.type, r.id, r.kind, r.origin, r.status, r.note,
                        r.t_valid, r.t_invalid, r.created_at, r.updated_at
                 FROM relationships r JOIN entities e ON e.id = r.entity_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(json!({
                    "name": row.get::<_, String>(0)?, "type": row.get::<_, String>(1)?,
                    "id": row.get::<_, String>(2)?, "kind": row.get::<_, String>(3)?,
                    "origin": row.get::<_, String>(4)?, "status": row.get::<_, String>(5)?,
                    "note": row.get::<_, Option<String>>(6)?,
                    "t_valid": row.get::<_, Option<String>>(7)?,
                    "t_invalid": row.get::<_, Option<String>>(8)?,
                    "created_at": row.get::<_, String>(9)?,
                    "updated_at": row.get::<_, String>(10)?,
                }))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        out.insert(
            "relationships_by_name".into(),
            serde_json::Value::Array(rels),
        );
        drop(conn);
        // note FILE bodies ride along, keyed by notes_index slug/path
        let notes_dir = home.root().join("notes");
        let mut files = serde_json::Map::new();
        if let Ok(entries) = std::fs::read_dir(&notes_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().map(|e| e == "md").unwrap_or(false) {
                    if let (Some(name), Ok(body)) = (
                        entry.file_name().to_str().map(String::from),
                        std::fs::read_to_string(entry.path()),
                    ) {
                        files.insert(name, serde_json::json!(body));
                    }
                }
            }
        }
        out.insert("note_files".into(), serde_json::Value::Object(files));
        Ok(serde_json::Value::Object(out))
    }

    /// Apply the mirror: a MULTI-MASTER per-row merge inside one
    /// transaction, then write note files. Each sender row is upserted by
    /// `id` under last-writer-wins (by updated_at, else created_at); rows
    /// present only on the receiver are kept (no DELETE). Both nodes are
    /// authoritative — a task created/completed on either side reconciles
    /// with the other rather than clobbering it. (Hard deletes don't
    /// propagate — soft-delete via status is the tombstone; a future rev
    /// can add real tombstones for purge.)
    pub fn apply_operational(&self, home: &Home, state: &serde_json::Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for (table, rows) in state.as_object().into_iter().flatten() {
            // `llm` is the Phase-5 key-sync envelope (provider + sealed key +
            // clear tier models), handled by the daemon's /v1/sync/operational
            // receiver — never a table to replace-all.
            if table == "note_files" || table == "relationships_by_name" || table == "llm" {
                continue;
            }
            if !table.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            // Wire-compat: a table this build doesn't know (a newer peer
            // during the dual-deploy window) is skipped, never a failed
            // mirror — feature-detect-and-degrade per op.
            let known: bool = tx
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table.as_str()],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !known {
                continue;
            }
            let Some(rows) = rows.as_array() else {
                continue;
            };
            // Multi-master MERGE (never replace-all): both nodes are
            // authoritative, so upsert each sender row by its `id` primary
            // key under last-writer-wins, and NEVER delete — rows that exist
            // only on the receiver survive. A task created/completed on
            // either node reconciles instead of clobbering the other's.
            for row in rows {
                let Some(obj) = row.as_object() else { continue };
                // LWW gate: if our local row is strictly newer (by updated_at,
                // else created_at), keep it and skip the sender's stale copy.
                // ts_col comes from the row's own keys, so it always names a
                // real column of `table` (which is already validated
                // alphanumeric above) — no injection surface.
                let ts_col = if obj.contains_key("updated_at") {
                    Some("updated_at")
                } else if obj.contains_key("created_at") {
                    Some("created_at")
                } else {
                    None
                };
                // Resurrection gate (v43 Phase 3): a local delete-tombstone
                // in the journal at-or-after the sender's stamp means this
                // row was purged HERE more recently than the sender's copy —
                // skip it, or this snapshot mirror would re-insert what the
                // journal tombstone leg just removed. A sender copy NEWER
                // than the tombstone (edited after the purge elsewhere)
                // still applies — multi-master, both directions.
                if let (Some(id), Some(kind)) = (
                    obj.get("id").and_then(|v| v.as_str()),
                    journaled_table(table),
                ) {
                    let sender_ts = ts_col
                        .and_then(|c| obj.get(c).and_then(|v| v.as_str()))
                        .unwrap_or("");
                    let tomb: Option<String> = tx
                        .query_row(
                            "SELECT ts FROM op_journal
                             WHERE entity_kind = ?1 AND entity_id = ?2 AND op = 'delete'
                             ORDER BY ts DESC LIMIT 1",
                            [kind, id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if tomb.is_some_and(|t| t.as_str() >= sender_ts) {
                        continue;
                    }
                }
                if let (Some(id), Some(ts_col)) = (obj.get("id").and_then(|v| v.as_str()), ts_col) {
                    let sender_ts = obj.get(ts_col).and_then(|v| v.as_str()).unwrap_or("");
                    // get as Option<String> so a NULL timestamp value (not just
                    // a missing row) resolves to None rather than erroring —
                    // a local row with no timestamp has no basis to win.
                    let local_ts: Option<String> = tx
                        .query_row(
                            &format!("SELECT {ts_col} FROM {table} WHERE id = ?1"),
                            [id],
                            |r| r.get::<_, Option<String>>(0),
                        )
                        .optional()?
                        .flatten();
                    if let Some(local) = local_ts {
                        if local.as_str() > sender_ts {
                            continue; // our copy is newer — keep it
                        }
                    }
                }
                let cols: Vec<&String> = obj.keys().collect();
                let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
                let sql = format!(
                    "INSERT OR REPLACE INTO {table} ({}) VALUES ({})",
                    cols.iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    placeholders.join(", ")
                );
                let params: Vec<rusqlite::types::Value> = cols
                    .iter()
                    .map(|c| match &obj[c.as_str()] {
                        serde_json::Value::Null => rusqlite::types::Value::Null,
                        serde_json::Value::Number(n) if n.is_i64() => {
                            rusqlite::types::Value::Integer(n.as_i64().unwrap())
                        }
                        serde_json::Value::Number(n) => {
                            rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))
                        }
                        serde_json::Value::Bool(b) => rusqlite::types::Value::Integer(*b as i64),
                        serde_json::Value::String(t) if t.starts_with("__b64__") => {
                            rusqlite::types::Value::Blob(
                                b64_decode(&t["__b64__".len()..]).unwrap_or_default(),
                            )
                        }
                        other => rusqlite::types::Value::Text(
                            other
                                .as_str()
                                .map(String::from)
                                .unwrap_or_else(|| other.to_string()),
                        ),
                    })
                    .collect();
                // The same version we already hold: no write, not counted.
                // Re-applying unchanged rows made every mirror pull report
                // thousands of "changes" and touch the bus, which scheduled
                // the next sync — 0.5.68's ten-second loop on the laptop.
                let same_sql = format!(
                    "SELECT 1 FROM {table} WHERE {} LIMIT 1",
                    cols.iter()
                        .enumerate()
                        .map(|(i, c)| if c.as_str() == "id" {
                            format!("id = ?{}", i + 1)
                        } else {
                            format!("{c} IS ?{}", i + 1)
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ")
                );
                if tx
                    .query_row(&same_sql, rusqlite::params_from_iter(params.iter()), |_| {
                        Ok(())
                    })
                    .optional()?
                    .is_some()
                {
                    continue;
                }
                tx.execute(&sql, rusqlite::params_from_iter(params))?;
                applied += 1;
            }
        }
        // relationships: resolve entity by (name, type), create Person/Org
        // shells when missing (episodes usually carried them already)
        if let Some(rels) = state
            .get("relationships_by_name")
            .and_then(|v| v.as_array())
        {
            // Multi-master merge (NOT replace-all — the 2026-07-27 fix): the
            // old `DELETE FROM relationships` here wiped any relationship the
            // receiver had that the sender lacked. We now merge per logical
            // key (entity + kind + t_valid) under LWW, so receiver-only
            // relationships survive. Relationship ids are per-node random, so
            // the key can't be the id.
            for r in rels {
                let (Some(name), Some(etype), Some(id), Some(kind), Some(origin), Some(status)) = (
                    r.get("name").and_then(|v| v.as_str()),
                    r.get("type").and_then(|v| v.as_str()),
                    r.get("id").and_then(|v| v.as_str()),
                    r.get("kind").and_then(|v| v.as_str()),
                    r.get("origin").and_then(|v| v.as_str()),
                    r.get("status").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let entity_id: Option<String> = tx
                    .query_row(
                        "SELECT id FROM entities WHERE name = ?1 AND type = ?2
                         AND merged_into IS NULL LIMIT 1",
                        [name, etype],
                        |row| row.get(0),
                    )
                    .optional()?;
                let entity_id = match entity_id {
                    Some(id) => id,
                    None => {
                        let new_id = crate::ids::entity_id();
                        tx.execute(
                            "INSERT INTO entities (id, type, name, scope, created_at)
                             VALUES (?1, ?2, ?3, 'personal', ?4)",
                            rusqlite::params![new_id, etype, name, now_utc()],
                        )?;
                        new_id
                    }
                };
                // created_at/updated_at are NOT NULL in the schema —
                // carry the sender's stamps, fall back to now for an
                // older sender that didn't export them (this omission
                // 500'd every mirror cycle before the progress surface
                // exposed it, 2026-07-13).
                let now = now_utc();
                // LWW per logical key: keep our copy if it is strictly newer,
                // else replace exactly that (entity,kind,t_valid) relationship.
                let tvalid = r.get("t_valid").and_then(|v| v.as_str());
                let sender_ts = r.get("updated_at").and_then(|v| v.as_str()).unwrap_or(&now);
                let local_ts: Option<String> = tx
                    .query_row(
                        "SELECT updated_at FROM relationships
                         WHERE entity_id=?1 AND kind=?2 AND IFNULL(t_valid,'')=IFNULL(?3,'')
                         ORDER BY updated_at DESC LIMIT 1",
                        rusqlite::params![entity_id, kind, tvalid],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten();
                if let Some(local) = &local_ts {
                    if local.as_str() > sender_ts {
                        continue; // our relationship is newer — keep it
                    }
                }
                // Already exactly this (ids are per node, so not compared):
                // no rewrite, not counted (the 0.5.68 sync-loop fix).
                let same = tx
                    .query_row(
                        "SELECT 1 FROM relationships
                         WHERE entity_id=?1 AND kind=?2 AND IFNULL(t_valid,'')=IFNULL(?3,'')
                           AND origin IS ?4 AND status IS ?5 AND note IS ?6
                           AND t_invalid IS ?7 AND updated_at IS ?8 LIMIT 1",
                        rusqlite::params![
                            entity_id,
                            kind,
                            tvalid,
                            origin,
                            status,
                            r.get("note").and_then(|v| v.as_str()),
                            r.get("t_invalid").and_then(|v| v.as_str()),
                            sender_ts
                        ],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if same {
                    continue;
                }
                tx.execute(
                    "DELETE FROM relationships
                     WHERE entity_id=?1 AND kind=?2 AND IFNULL(t_valid,'')=IFNULL(?3,'')",
                    rusqlite::params![entity_id, kind, tvalid],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO relationships
                     (id, entity_id, kind, origin, status, note, t_valid, t_invalid,
                      created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        id,
                        entity_id,
                        kind,
                        origin,
                        status,
                        r.get("note").and_then(|v| v.as_str()),
                        r.get("t_valid").and_then(|v| v.as_str()),
                        r.get("t_invalid").and_then(|v| v.as_str()),
                        r.get("created_at").and_then(|v| v.as_str()).unwrap_or(&now),
                        r.get("updated_at").and_then(|v| v.as_str()).unwrap_or(&now),
                    ],
                )?;
                applied += 1;
            }
        }
        // Replay the rename trail (v37, item 14) against THIS node's
        // entities. Oldest first so chains (A→B→C) land in order; a
        // rename whose from_name no longer exists (already replayed, or
        // this node never minted the entity) is a no-op; a taken
        // to_name is left alone — merge review handles real collisions.
        {
            let renames: Vec<(String, String, String)> = {
                let mut stmt = tx.prepare(
                    "SELECT entity_type, from_name, to_name FROM entity_renames ORDER BY at, id",
                )?;
                let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            for (etype, from, to) in renames {
                tx.execute(
                    "UPDATE entities SET name = ?3
                     WHERE merged_into IS NULL AND type = ?1 AND name = ?2
                       AND NOT EXISTS (
                           SELECT 1 FROM entities
                           WHERE merged_into IS NULL AND type = ?1
                             AND lower(name) = lower(?3))",
                    rusqlite::params![etype, from, to],
                )?;
            }
        }
        tx.commit()?;
        drop(conn);
        if let Some(files) = state.get("note_files").and_then(|v| v.as_object()) {
            let dir = home.root().join("notes");
            std::fs::create_dir_all(&dir).ok();
            for (name, body) in files {
                if name.contains('/') || name.contains("..") {
                    continue;
                }
                if let Some(body) = body.as_str() {
                    let _ = std::fs::write(dir.join(name), body);
                }
            }
        }
        Ok(applied)
    }

    /// One page of the operational-journal feed (v43 Phase 3): entries
    /// after `after` (an op_journal.seq cursor), oldest first. Entries
    /// received FROM a peer never re-export (actor `peer:%` — the
    /// episodes echo guard, same rule), so a two-node pairing converges
    /// with zero ping-pong. Post-images ship verbatim (peers are full
    /// replicas — the friend surface never sees this endpoint).
    /// `requester` (multi-node M5): when a HUB serves a spoke, it
    /// relays entries it received from OTHER nodes and skips only the
    /// ones that came from the asking node (`peer:<its key name>`).
    /// None — a spoke pushing to its one hub — skips every received entry.
    pub fn journal_export(&self, after: i64, limit: usize) -> Result<serde_json::Value> {
        self.journal_export_for(after, limit, None)
    }

    pub fn journal_export_for(
        &self,
        after: i64,
        limit: usize,
        requester: Option<&str>,
    ) -> Result<serde_json::Value> {
        let conn = self.lock();
        let echo_guard = match requester {
            Some(_) => "actor != ?3",
            None => "actor NOT LIKE 'peer:%'",
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT seq, entry_id, entity_kind, entity_id, op, post_image, actor, ts
             FROM op_journal WHERE seq > ?1 AND {echo_guard}
             ORDER BY seq LIMIT ?2"
        ))?;
        let requester_actor = requester.map(|r| format!("peer:{r}")).unwrap_or_default();
        let map_row = |row: &rusqlite::Row| {
            Ok((
                row.get::<_, i64>(0)?,
                json!({
                    "entry_id": row.get::<_, String>(1)?,
                    "entity_kind": row.get::<_, String>(2)?,
                    "entity_id": row.get::<_, String>(3)?,
                    "op": row.get::<_, String>(4)?,
                    "post_image": row.get::<_, Option<String>>(5)?,
                    "actor": row.get::<_, String>(6)?,
                    "ts": row.get::<_, String>(7)?,
                }),
            ))
        };
        let rows: Vec<rusqlite::Result<(i64, serde_json::Value)>> = if requester.is_some() {
            stmt.query_map(
                rusqlite::params![after, limit as i64, requester_actor],
                map_row,
            )?
            .collect()
        } else {
            stmt.query_map(rusqlite::params![after, limit as i64], map_row)?
                .collect()
        };
        let mut next = after;
        let mut entries = Vec::new();
        for row in rows {
            let (seq, entry) = row?;
            next = seq;
            entries.push(entry);
        }
        Ok(json!({
            "drained": entries.is_empty(),
            "entries": entries,
            "next": next,
        }))
    }

    /// Apply a peer's journal page: idempotent replay keyed by
    /// `entry_id` (INSERT OR IGNORE — a known entry is a no-op), actor
    /// rewritten to `peer:<name>` on the receiving node (the episodes
    /// precedent; also what keeps the entry out of our own export).
    ///
    /// A `delete` entry is THE one sync leg that removes rows across
    /// nodes, so it is LWW-gated: the row goes only when the tombstone
    /// is at-or-after the local row's stamp (ties → the delete wins —
    /// same-second create+purge must still propagate; the content stays
    /// restorable from the journal). A newer local edit survives an
    /// older tombstone, and the next mirror cycle re-seeds the sender.
    pub fn apply_journal(&self, actor: &Actor, batch: &serde_json::Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for entry in batch
            .get("entries")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(entry_id), Some(kind), Some(id), Some(op), Some(ts)) = (
                entry.get("entry_id").and_then(|v| v.as_str()),
                entry.get("entity_kind").and_then(|v| v.as_str()),
                entry.get("entity_id").and_then(|v| v.as_str()),
                entry.get("op").and_then(|v| v.as_str()),
                entry.get("ts").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO op_journal
                     (entry_id, entity_kind, entity_id, op, post_image, actor, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    entry_id,
                    kind,
                    id,
                    op,
                    entry.get("post_image").and_then(|v| v.as_str()),
                    actor.actor,
                    ts
                ],
            )?;
            if inserted == 0 {
                continue; // already replayed — idempotent
            }
            applied += 1;
            if op == "delete" {
                if let Some((table, ts_expr)) = journaled_kind(kind) {
                    let removed = tx.execute(
                        &format!("DELETE FROM {table} WHERE id = ?1 AND {ts_expr} <= ?2"),
                        rusqlite::params![id, ts],
                    )?;
                    if removed > 0 {
                        // same side-index cleanup the local purge paths do
                        match table {
                            "tasks" => {
                                tx.execute("DELETE FROM task_mentions WHERE task_id = ?1", [id])?;
                            }
                            "intents" => {
                                tx.execute(
                                    "DELETE FROM intent_matches WHERE intent_id = ?1",
                                    [id],
                                )?;
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        tx.commit()?;
        Ok(applied)
    }

    /// The highest episodes.rowid — "caught up" means watermark == this.
    pub fn sync_head(&self) -> Result<i64> {
        let conn = self.lock();
        conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM episodes /* all-episodes: peer sync carries every episode; a retirement travels as its own event */", [], |row| {
            row.get(0)
        })
        .map_err(Into::into)
    }

    /// The highest rowid this node will ever EXPORT — peer-received
    /// rows never re-export (the echo guard in `sync_export`), so the
    /// honest denominator for push progress excludes them. On a node
    /// whose newest rows arrived FROM the peer, `sync_head` overstates
    /// the total and the bar would never reach 100%.
    pub fn sync_export_head(&self) -> Result<i64> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM episodes WHERE actor NOT LIKE 'peer:%' /* all-episodes: peer sync carries every episode; a retirement travels as its own event */",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// (done, total) for the scope-change exchange relative to the push
    /// watermark `since` (an RFC3339 stamp): total = every episode whose
    /// scope was ever corrected (only those carry a stamp), done = the
    /// ones at or before the watermark, i.e. already offered to the
    /// peer. The empty watermark (never synced) counts zero done.
    pub fn scope_change_counts(&self, since: &str) -> Result<(i64, i64)> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COALESCE(SUM(scope_changed_at <= ?1), 0), COUNT(*)
             FROM episodes WHERE scope_changed_at IS NOT NULL /* all-episodes: peer sync carries every episode; a retirement travels as its own event */",
            [since],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(Into::into)
    }

    /// Item count of the operational mirror: rows across the mirrored
    /// tables + relationships + note file bodies. The mirror is a
    /// replace-all snapshot per cycle, so progress is all-or-nothing —
    /// this is its weight in the combined replication percentage.
    pub fn operational_count(&self, home: &Home) -> Result<i64> {
        let mut total = 0i64;
        {
            let conn = self.lock();
            for table in OPERATIONAL_TABLES {
                total += conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })?;
            }
            total += conn.query_row("SELECT COUNT(*) FROM relationships", [], |row| {
                row.get::<_, i64>(0)
            })?;
        }
        if let Ok(entries) = std::fs::read_dir(home.root().join("notes")) {
            total += entries
                .flatten()
                .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                .count() as i64;
        }
        Ok(total)
    }

    /// Register (or update) THE peer — v1 is a single always-on
    /// counterpart. Reuses the sources registry (kind='peer', no schema
    /// change); the bearer key lives in ~/.hearthloom/.env, never here.
    pub fn set_peer(&self, actor: &Actor, name: &str, url: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM sources WHERE kind = 'peer'", [])?;
        conn.execute(
            "INSERT INTO sources (name, kind, adapter, config, capabilities, first_seen)
             VALUES (?1, 'peer', 'peer', ?2, '[]', ?3)",
            rusqlite::params![
                name,
                json!({"url": url.trim_end_matches('/')}).to_string(),
                crate::time::now_utc(),
            ],
        )?;
        super::record_mutation(
            &conn,
            actor,
            "peer_paired",
            &format!("peer_paired:{name}"),
            &json!({"name": name, "url": url}),
        )?;
        Ok(())
    }

    /// The configured peer, if any: (name, url).
    pub fn peer(&self) -> Result<Option<(String, String)>> {
        let conn = self.lock();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT name, config FROM sources WHERE kind = 'peer' LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(name, config)| {
            serde_json::from_str::<serde_json::Value>(&config)
                .ok()
                .and_then(|c| c.get("url").and_then(|u| u.as_str()).map(String::from))
                .map(|url| (name, url))
        }))
    }

    pub fn remove_peer(&self, actor: &Actor) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM sources WHERE kind = 'peer'", [])?;
        // Reset every peer watermark: a future re-pair must re-offer
        // everything (stale cursors against a FRESH node silently pushed
        // nothing — 2026-07-18). Idempotent against a duplicate node.
        conn.execute(
            "DELETE FROM consolidation_state WHERE key IN (
                 'peer_push_cursor','peer_pull_cursor','peer_scope_push_ts',
                 'peer_scope_pull_ts','peer_mirror_ts','peer_inbound',
                 'peer_telemetry_push_ts','peer_telemetry_pull_ts',
                 'peer_notif_push_ts','peer_notif_pull_ts',
                 'peer_verdict_push_ts','peer_verdict_pull_ts',
                 'peer_journal_push_seq','peer_journal_pull_seq',
                 'peer_photo_push_seq','peer_photo_pull_seq',
                 'peer_sharing_push_ts','peer_sharing_pull_ts',
                 'peer_bus_cursor','peer_first_sync_notified','peer_fail_streak')",
            [],
        )?;
        super::record_mutation(&conn, actor, "peer_unpaired", "peer_unpaired", &json!({}))?;
        Ok(())
    }
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::IngestBatch;

    fn draft(source_ref: &str, title: &str, scope: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "test".into(),
            source_ref: source_ref.into(),
            kind: "note".into(),
            title: title.into(),
            body: format!("body of {title}"),
            event_time: EventTime::parse_rfc3339("2026-07-01T10:00:00Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: "self".into(),
            scope: Some(scope.into()),
            entities: vec![EntityRef {
                entity_type: "Person".into(),
                name: "Ada".into(),
                attributes: vec![],
                role: "mentioned".into(),
            }],
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
                        draft("a", "public one", "personal"),
                        draft("b", "the secret", "private"),
                        draft("c", "public two", "work"),
                    ],
                },
            )
            .unwrap();
        (store, home, tmp)
    }

    #[test]
    fn test_export_is_full_replica_and_pages() {
        let (store, home, _tmp) = seeded();
        // A user's OWN peers replicate everything, private included
        // (2026-07-11 reversal) — the friend surface is where private
        // can never go.
        let page = store.sync_export(&home, 0, 10).unwrap();
        assert_eq!(page.episodes.len(), 3);
        assert!(page.episodes.iter().any(|e| e.title == "the secret"));
        assert!(page.episodes[0].body.contains("body of"));
        assert_eq!(page.episodes[0].entities[0].name, "Ada");
        // paging: ask again from the cursor → drained
        let more = store.sync_export(&home, page.next_cursor, 10).unwrap();
        assert!(more.drained);
    }

    #[test]
    fn test_roundtrip_is_idempotent() {
        let (store, home, _tmp) = seeded();
        let (peer_store, peer_home, _tmp2) = {
            let tmp = tempfile::tempdir().unwrap();
            let home = Home::at(tmp.path().to_path_buf());
            home.init().unwrap();
            (Store::open(&home.db_path()).unwrap(), home, tmp)
        };
        let page = store.sync_export(&home, 0, 10).unwrap();
        let batch = IngestBatch {
            actor: "peer:test".into(),
            session_id: None,
            episodes: page.episodes,
        };
        let first = peer_store.apply_batch(&peer_home, &batch).unwrap();
        assert_eq!(first.episodes_new, 3); // full replica: private included
                                           // peer-received rows never re-export — echo traffic is zero
        let echo = peer_store.sync_export(&peer_home, 0, 10).unwrap();
        assert!(echo.drained);
        // and even a forced re-apply of the same batch is a pure no-op
        let back = store.apply_batch(&home, &batch).unwrap();
        assert_eq!(back.episodes_new, 0);
        assert_eq!(back.episodes_existing, 3);
    }

    #[test]
    fn test_scope_lww_exchange() {
        let (store, home, _tmp) = seeded();
        let (peer, peer_home, _tmp2) = {
            let t = tempfile::tempdir().unwrap();
            let h = Home::at(t.path().to_path_buf());
            h.init().unwrap();
            (Store::open(&h.db_path()).unwrap(), h, t)
        };
        let page = store.sync_export(&home, 0, 10).unwrap();
        peer.apply_batch(
            &peer_home,
            &crate::ingest::IngestBatch {
                actor: "peer:t".into(),
                session_id: None,
                episodes: page.episodes,
            },
        )
        .unwrap();
        // relabel on origin → change flows to peer
        let ep = {
            let conn = store.lock();
            conn.query_row("SELECT id FROM episodes WHERE scope='work'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
        };
        store
            .apply_scope_classification(&Actor::new("human:t"), &ep, Some("family"))
            .unwrap();
        let changes = store.scope_changes_since("").unwrap();
        assert_eq!(changes.len(), 1);
        let applied = peer
            .apply_scope_changes(&Actor::new("peer:t"), &changes)
            .unwrap();
        assert_eq!(applied, 1);
        // replaying is a no-op (LWW strictness)
        assert_eq!(
            peer.apply_scope_changes(&Actor::new("peer:t"), &changes)
                .unwrap(),
            0
        );
    }

    #[test]
    fn test_sync_export_head_excludes_peer_rows() {
        let (store, home, _tmp) = seeded();
        assert_eq!(
            store.sync_export_head().unwrap(),
            store.sync_head().unwrap()
        );
        // a row received FROM a peer raises sync_head but not the
        // export head — it will never be pushed back out.
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "peer:cloud".into(),
                    session_id: None,
                    episodes: vec![draft("d", "from the peer", "personal")],
                },
            )
            .unwrap();
        assert_eq!(
            store.sync_head().unwrap(),
            store.sync_export_head().unwrap() + 1
        );
        // and the export cursor drains exactly at the export head
        let page = store.sync_export(&home, 0, 10).unwrap();
        assert_eq!(page.next_cursor, store.sync_export_head().unwrap());
    }

    #[test]
    fn test_scope_change_counts_track_watermark() {
        let (store, _home, _tmp) = seeded();
        // nothing ever relabeled → an empty category
        assert_eq!(store.scope_change_counts("").unwrap(), (0, 0));
        let ep = {
            let conn = store.lock();
            conn.query_row("SELECT id FROM episodes WHERE scope='work'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
        };
        store
            .apply_scope_classification(&Actor::new("human:t"), &ep, Some("family"))
            .unwrap();
        // stamped but not yet offered (empty watermark)
        assert_eq!(store.scope_change_counts("").unwrap(), (0, 1));
        // after the push watermark advances to the stamp → done
        let ts = store.scope_changes_since("").unwrap()[0].2.clone();
        assert_eq!(store.scope_change_counts(&ts).unwrap(), (1, 1));
    }

    #[test]
    fn test_operational_count_matches_the_mirror() {
        let (store, home, _tmp) = seeded();
        let base = store.operational_count(&home).unwrap();
        store
            .add_task(
                &Actor::new("human:t"),
                "plain",
                "water the garden",
                None,
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        std::fs::create_dir_all(home.root().join("notes")).unwrap();
        std::fs::write(home.root().join("notes/test.md"), "# hi").unwrap();
        assert_eq!(store.operational_count(&home).unwrap(), base + 2);
    }

    #[test]
    fn test_operational_mirror_roundtrip_includes_relationships() {
        // The mirror must APPLY, not just export — a NOT NULL column
        // missing from the relationships insert 500'd every live cycle
        // until 2026-07-13. Roundtrip the whole payload.
        let (store, home, _tmp) = seeded();
        let actor = Actor::new("human:t");
        let ada: String = {
            let conn = store.lock();
            conn.query_row("SELECT id FROM entities WHERE name='Ada'", [], |r| r.get(0))
                .unwrap()
        };
        store
            .add_relationship(
                &actor,
                &ada,
                "friend",
                "declared",
                &[],
                Some("met at work"),
                None,
            )
            .unwrap();
        store
            .add_task(
                &actor,
                "plain",
                "call Ada",
                None,
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        let exported = store.export_operational(&home).unwrap();
        let (peer, peer_home, _tmp2) = {
            let t = tempfile::tempdir().unwrap();
            let h = Home::at(t.path().to_path_buf());
            h.init().unwrap();
            (Store::open(&h.db_path()).unwrap(), h, t)
        };
        let applied = peer.apply_operational(&peer_home, &exported).unwrap();
        assert!(
            applied >= 2,
            "task + relationship should apply, got {applied}"
        );
        // The same payload again changes nothing — and must SAY nothing
        // changed: a non-zero count touches the bus and schedules another
        // sync (0.5.68 looped every ~10s on 2,953 "applied" rows).
        assert_eq!(peer.apply_operational(&peer_home, &exported).unwrap(), 0);
        // …and the sender re-applying its own state back is also a no-op.
        assert_eq!(
            store
                .apply_operational(&home, &peer.export_operational(&peer_home).unwrap())
                .unwrap(),
            0
        );
        let (kind, created): (String, String) = {
            let conn = peer.lock();
            conn.query_row(
                "SELECT kind, created_at FROM relationships LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(kind, "friend");
        assert!(!created.is_empty());
    }

    #[test]
    fn test_operational_merge_is_multimaster_lww() {
        // Regression for the 2026-07-27 task-loss incident: the operational
        // mirror used to DELETE FROM + replace-all, so a task created or
        // completed on one node was clobbered by the other's snapshot. The
        // merge must now (a) KEEP receiver-only rows (union, never delete),
        // (b) let a NEWER row win by updated_at, (c) let an OLDER row lose.
        let (a, a_home, _ta) = seeded();
        let (b, b_home, _tb) = seeded();
        let actor = Actor::new("human:t");
        let mk = |s: &Store, d: &str| {
            s.add_task(
                &actor,
                "plain",
                d,
                None,
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap()
        };
        // a shared row (same id on both, seeded by a first merge)
        let s = mk(&a, "shared");
        b.apply_operational(&b_home, &a.export_operational(&a_home).unwrap())
            .unwrap();
        // divergent local-only rows
        let za = mk(&a, "only-A");
        let zb = mk(&b, "only-B");
        // diverge the shared row: A's copy older, B's copy newer
        let raw = |s: &Store, id: &str, desc: &str, ts: &str| {
            let c = s.lock();
            c.execute(
                "UPDATE tasks SET description=?1, updated_at=?2 WHERE id=?3",
                rusqlite::params![desc, ts, id],
            )
            .unwrap();
        };
        raw(&a, &s, "from-A", "2026-06-01T00:00:00Z");
        raw(&b, &s, "from-B", "2026-12-01T00:00:00Z");
        let desc = |s: &Store, id: &str| -> String {
            let c = s.lock();
            c.query_row("SELECT description FROM tasks WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        // merge A -> B
        b.apply_operational(&b_home, &a.export_operational(&a_home).unwrap())
            .unwrap();
        assert_eq!(
            desc(&b, &zb),
            "only-B",
            "receiver-only task must survive the merge (union, no DELETE)"
        );
        assert_eq!(desc(&b, &za), "only-A", "sender-only task must arrive");
        assert_eq!(
            desc(&b, &s),
            "from-B",
            "newer local row must win LWW over an older sender row"
        );
        // reverse merge B -> A: A's older shared copy is superseded, A gains only-B
        a.apply_operational(&a_home, &b.export_operational(&b_home).unwrap())
            .unwrap();
        assert_eq!(desc(&a, &s), "from-B", "newer sender row must win LWW");
        assert_eq!(
            desc(&a, &zb),
            "only-B",
            "reverse merge brings the peer's task"
        );
        assert_eq!(
            desc(&a, &za),
            "only-A",
            "own task survives the reverse merge"
        );
    }

    #[test]
    fn test_merge_absent_local_row_applies() {
        // The LWW gate reads the local timestamp as an Option (the 2026-07-27
        // hotfix) so an absent row — or a legacy NULL ts — resolves to None
        // and the sender's row applies rather than erroring the mirror.
        let (a, a_home, _ta) = seeded();
        let (b, b_home, _tb) = seeded();
        let actor = Actor::new("human:t");
        let id = a
            .add_task(
                &actor,
                "plain",
                "x",
                None,
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        b.apply_operational(&b_home, &a.export_operational(&a_home).unwrap())
            .unwrap();
        {
            let c = b.lock();
            c.execute("DELETE FROM tasks WHERE id=?1", [&id]).unwrap();
        }
        {
            let c = a.lock();
            c.execute(
                "UPDATE tasks SET description='newer', updated_at='2030-01-01T00:00:00Z' WHERE id=?1",
                [&id],
            )
            .unwrap();
        }
        b.apply_operational(&b_home, &a.export_operational(&a_home).unwrap())
            .unwrap();
        let d: String = {
            let c = b.lock();
            c.query_row("SELECT description FROM tasks WHERE id=?1", [&id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(
            d, "newer",
            "sender row must apply when the local row is absent"
        );
    }

    #[test]
    fn test_task_journal_restore() {
        // A purged task (content and all) must be restorable from the
        // operational journal — the recovery path the incident lacked.
        let (s, _h, _t) = seeded();
        let actor = Actor::new("human:t");
        let payload = serde_json::json!({"items":[{"text":"socks","done":false},{"text":"tent","done":false}]});
        let id = s
            .add_task(
                &actor,
                "checklist",
                "packing list",
                Some(&payload),
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        s.update_task(&actor, &id, None, None, Some("archived"), None, None)
            .unwrap();
        s.purge_task(&actor, &id).unwrap();
        assert!(s.task(&id).unwrap().is_none(), "task should be purged");
        s.restore_task(&actor, &id).unwrap();
        let t = s.task(&id).unwrap().expect("task restored from journal");
        assert_eq!(t.description, "packing list");
        let items = t
            .payload
            .and_then(|p| p.get("items").and_then(|i| i.as_array()).map(|a| a.len()));
        assert_eq!(
            items,
            Some(2),
            "checklist items restored from the journal post-image"
        );
    }

    #[test]
    fn test_journal_sync_propagates_purge() {
        // Phase 3: a purge on node A must finally remove the row on node B
        // (before this, the operational merge was a pure union and purged
        // tasks lived forever on the other node). Replay is idempotent and
        // peer-received entries never re-export (echo guard).
        let (a, a_home, _ta) = seeded();
        let (b, b_home, _tb) = seeded();
        let actor = Actor::new("human:t");
        let id = a
            .add_task(
                &actor,
                "plain",
                "doomed",
                None,
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        b.apply_operational(&b_home, &a.export_operational(&a_home).unwrap())
            .unwrap();
        assert!(b.task(&id).unwrap().is_some(), "task mirrored to B");
        a.update_task(&actor, &id, None, None, Some("archived"), None, None)
            .unwrap();
        a.purge_task(&actor, &id).unwrap();
        let feed = a.journal_export(0, 500).unwrap();
        assert!(!feed["drained"].as_bool().unwrap());
        let peer = Actor::new("peer:a");
        let applied = b.apply_journal(&peer, &feed).unwrap();
        assert!(applied >= 1);
        assert!(
            b.task(&id).unwrap().is_none(),
            "purge on A must propagate to B"
        );
        // idempotent: replaying the same page changes nothing
        assert_eq!(b.apply_journal(&peer, &feed).unwrap(), 0);
        // echo guard: everything B holds journal-wise came from the peer,
        // so B's own export is empty — no ping-pong.
        assert!(b.journal_export(0, 500).unwrap()["drained"]
            .as_bool()
            .unwrap());
        // and the mirror can't resurrect it: B's copy predates the
        // tombstone, so applying B's snapshot on A leaves the row gone.
        a.apply_operational(&a_home, &b.export_operational(&b_home).unwrap())
            .unwrap();
        assert!(
            a.task(&id).unwrap().is_none(),
            "mirror must not resurrect a tombstoned row"
        );
    }

    #[test]
    fn test_newer_edit_survives_older_tombstone() {
        // The LWW guard on the one deleting leg: an edit on B stamped AFTER
        // A's purge must survive the tombstone — and the next mirror cycle
        // re-seeds A with the surviving row (multi-master, both directions).
        let (a, a_home, _ta) = seeded();
        let (b, b_home, _tb) = seeded();
        let actor = Actor::new("human:t");
        let id = a
            .add_task(
                &actor,
                "plain",
                "contested",
                None,
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        b.apply_operational(&b_home, &a.export_operational(&a_home).unwrap())
            .unwrap();
        a.update_task(&actor, &id, None, None, Some("archived"), None, None)
            .unwrap();
        a.purge_task(&actor, &id).unwrap();
        {
            let c = b.lock();
            c.execute(
                "UPDATE tasks SET description='edited-later', updated_at='2030-01-01T00:00:00Z'
                 WHERE id=?1",
                [&id],
            )
            .unwrap();
        }
        b.apply_journal(&Actor::new("peer:a"), &a.journal_export(0, 500).unwrap())
            .unwrap();
        let survivor = b.task(&id).unwrap();
        assert_eq!(
            survivor.map(|t| t.description).as_deref(),
            Some("edited-later"),
            "a newer edit on B must survive A's older tombstone"
        );
        // B's snapshot re-seeds A: the sender copy is newer than A's
        // tombstone, so the resurrection gate lets it through.
        a.apply_operational(&a_home, &b.export_operational(&b_home).unwrap())
            .unwrap();
        assert!(
            a.task(&id).unwrap().is_some(),
            "newer surviving row must return to A"
        );
    }

    #[test]
    fn test_peer_registry_roundtrip() {
        let (store, _home, _tmp) = seeded();
        let actor = Actor::new("human:test");
        assert!(store.peer().unwrap().is_none());
        store
            .set_peer(&actor, "attic", "https://brain.example.com/")
            .unwrap();
        let (name, url) = store.peer().unwrap().unwrap();
        assert_eq!(name, "attic");
        assert_eq!(url, "https://brain.example.com"); // trailing slash trimmed
        store.remove_peer(&actor).unwrap();
        assert!(store.peer().unwrap().is_none());
    }

    #[test]
    fn test_rename_trail_replays_on_the_receiver() {
        let actor = Actor::new("human:test".to_string());
        // Sender: mints a Project and renames it (v37 trail row).
        let (sender, sender_home, _t1) = seeded();
        sender.declare_project(&actor, "Orrery").unwrap();
        let proj = sender.entity_by_name("Orrery").unwrap().unwrap();
        sender
            .rename_entity(&actor, &proj.id, "Hearthloom")
            .unwrap();
        // Receiver: knows the entity under its OLD name, with a
        // DIFFERENT id (peers mint ids independently).
        let (receiver, receiver_home, _t2) = seeded();
        receiver.declare_project(&actor, "Orrery").unwrap();
        let state = sender.export_operational(&sender_home).unwrap();
        receiver.apply_operational(&receiver_home, &state).unwrap();
        // The replay renamed the receiver's own row instead of leaving
        // a stale "Orrery" to diverge from the sender.
        assert!(receiver.entity_by_name("Hearthloom").unwrap().is_some());
        assert!(receiver.entity_by_name("Orrery").unwrap().is_none());
        // Idempotent: applying the same mirror again changes nothing.
        receiver.apply_operational(&receiver_home, &state).unwrap();
        assert!(receiver.entity_by_name("Hearthloom").unwrap().is_some());
    }
}
