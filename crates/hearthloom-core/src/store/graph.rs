//! The knowledge graph: entities, identifying attributes, bi-temporal
//! edges, reversible merges, and traversal.

use anyhow::{anyhow, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::time::now_utc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRow {
    pub id: String,
    pub entity_type: String,
    pub name: String,
    pub scope: String,
    pub created_at: String,
    /// Human-declared importance (0 = unset, 1..3). A tier, not a score:
    /// ranking surfaces order by declared tier first, evidence second —
    /// the sky should show your mother, not your busiest mailing list.
    pub declared_importance: i64,
}

/// One hop discovered by `traverse`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraverseHop {
    pub depth: usize,
    pub from: EntityRow,
    pub relation: String,
    /// `out` if from=src, `in` if from=dst.
    pub direction: String,
    pub to: EntityRow,
    pub t_valid: Option<String>,
    pub t_invalid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DossierEdge {
    pub relation: String,
    pub direction: String,
    pub other: EntityRow,
    pub t_valid: Option<String>,
    pub t_invalid: Option<String>,
    pub provenance: Vec<String>,
}

/// Everything known about one entity, with cited episodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityDossier {
    pub entity: EntityRow,
    pub attributes: Vec<(String, String)>,
    pub edges: Vec<DossierEdge>,
    pub episode_count: i64,
    /// Recent citing episodes: (episode_id, role, title, event_time).
    pub recent_episodes: Vec<(String, String, String, Option<String>)>,
}

/// (src, relation, dst, t_valid, t_invalid) as read from the edges table.
type EdgeTuple = (String, String, String, Option<String>, Option<String>, f64);
/// EdgeTuple plus the leading edge id.
type EdgeTupleWithId = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// (month axis, per-entity monthly evidence counts).
pub type RibbonData = (Vec<String>, Vec<(EntityRow, Vec<i64>)>);

/// What happened when an identity attribute was claimed for an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClaimOutcome {
    /// The attribute was unowned and is now attached.
    Attached { entity_id: String },
    /// The attribute already resolved to this entity; nothing to do.
    AlreadyYours { entity_id: String },
    /// Another entity owned it and was merged into this one (reversibly).
    MergedFrom {
        entity_id: String,
        merged_entity: EntityRow,
        merge_id: String,
    },
}

/// A pending "is this the same entity?" question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: String,
    pub entity_a: EntityRow,
    pub entity_b: EntityRow,
    /// Identifying attributes (email, handle…) per side — the evidence a
    /// human needs to answer "same or different?" without guessing.
    pub attributes_a: Vec<(String, String)>,
    pub attributes_b: Vec<(String, String)>,
    pub episodes_a: i64,
    pub episodes_b: i64,
    pub confidence: f64,
    pub reason: String,
    pub created_at: String,
}

/// A per-RECORD source id (one value per occurrence), NOT an identifying
/// attribute like an email/handle. These must not count as a conflict when
/// deciding whether two same-name entities are duplicates.
pub(crate) fn is_record_id_attr(kind: &str) -> bool {
    kind == "ics-uid"
}

fn entity_attributes(conn: &Connection, id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn
        .prepare("SELECT kind, value FROM entity_attributes WHERE entity_id = ?1 ORDER BY kind")?;
    let rows = stmt.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn entity_episode_count(conn: &Connection, id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(DISTINCT episode_id) FROM episode_entities
         WHERE entity_id = ?1 AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)",
        [id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// One row of the reversible merge log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeRecord {
    pub id: String,
    pub winner: EntityRow,
    pub loser_name: String,
    pub confidence: f64,
    pub reason: String,
    pub actor: String,
    pub merged_at: String,
    pub reverted_at: Option<String>,
}

/// Follow the merged_into chain to the surviving entity id.
pub(crate) fn resolve_merged(conn: &Connection, id: &str) -> Result<String> {
    let mut current = id.to_string();
    for _ in 0..32 {
        let next: Option<String> = conn.query_row(
            "SELECT merged_into FROM entities WHERE id = ?1",
            [&current],
            |row| row.get(0),
        )?;
        match next {
            Some(winner) => current = winner,
            None => return Ok(current),
        }
    }
    Err(anyhow!("merge chain too deep at {id}"))
}

/// All entity ids that resolve to `id` (itself plus everything merged
/// into it, transitively). Group-aware reads use this so a merged
/// identity's history reads as one entity.
pub(crate) fn merge_group(conn: &Connection, id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE grp(id) AS (
             SELECT ?1
             UNION
             SELECT e.id FROM entities e JOIN grp ON e.merged_into = grp.id
         )
         SELECT id FROM grp",
    )?;
    let ids = stmt
        .query_map([id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(ids)
}

/// `?,?,?` placeholder list for an IN clause.
fn placeholders(n: usize) -> String {
    vec!["?"; n.max(1)].join(",")
}

pub(crate) fn entity_row(conn: &Connection, id: &str) -> Result<Option<EntityRow>> {
    conn.query_row(
        "SELECT id, type, name, scope, created_at, declared_importance
         FROM entities WHERE id = ?1",
        [id],
        |row| {
            Ok(EntityRow {
                id: row.get(0)?,
                entity_type: row.get(1)?,
                name: row.get(2)?,
                scope: row.get(3)?,
                created_at: row.get(4)?,
                declared_importance: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Resolve-or-create an entity inside an open transaction.
/// Order: identifying attribute match → exact (type, name) match → create.
/// Returns (id, created, review_queued).
pub(crate) fn upsert_entity(
    conn: &Connection,
    actor: &Actor,
    entity_type: &str,
    name: &str,
    scope: &str,
    attributes: &[(String, String)],
) -> Result<(String, bool, bool)> {
    // 1. Attribute-based identity (email, phone, handle, repo-url…).
    for (kind, value) in attributes {
        let hit: Option<String> = conn
            .query_row(
                "SELECT entity_id FROM entity_attributes WHERE kind = ?1 AND value = ?2",
                [kind.as_str(), value.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = hit {
            let id = resolve_merged(conn, &id)?;
            attach_attributes(conn, &id, attributes)?;
            return Ok((id, false, false));
        }
    }

    // 2. Exact (type, name) match among unmerged entities.
    let hit: Option<String> = conn
        .query_row(
            "SELECT id FROM entities
             WHERE type = ?1 AND name = ?2 AND merged_into IS NULL LIMIT 1",
            [entity_type, name],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = hit {
        attach_attributes(conn, &id, attributes)?;
        return Ok((id, false, false));
    }

    // 3. Fresh entity. Bias to under-merge: a same-type case-insensitive
    // name collision goes to the review queue instead of auto-merging.
    let id = ids::entity_id();
    conn.execute(
        "INSERT INTO entities (id, type, name, scope, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, entity_type, name, scope, now_utc()],
    )?;
    attach_attributes(conn, &id, attributes)?;
    record_mutation(
        conn,
        actor,
        "entity_created",
        &format!("entity_created:{id}"),
        &json!({"id": id, "type": entity_type, "name": name, "scope": scope}),
    )?;

    let similar: Option<String> = conn
        .query_row(
            "SELECT id FROM entities
             WHERE type = ?1 AND id != ?2 AND merged_into IS NULL
               AND lower(name) = lower(?3) LIMIT 1",
            rusqlite::params![entity_type, id, name],
            |row| row.get(0),
        )
        .optional()?;
    let mut queued = false;
    if let Some(other) = similar {
        let changed = conn.execute(
            "INSERT OR IGNORE INTO merge_review_queue
             (id, entity_a, entity_b, confidence, reason, created_at)
             VALUES (?1, ?2, ?3, 0.5, 'same name, no shared identifying attribute', ?4)",
            rusqlite::params![ids::review_id(), other, id, now_utc()],
        )?;
        queued = changed > 0;
    }
    Ok((id, true, queued))
}

fn merge_entities_inner(
    conn: &Connection,
    actor: &Actor,
    winner_id: &str,
    loser_id: &str,
    confidence: f64,
    reason: &str,
) -> Result<String> {
    if winner_id == loser_id {
        return Err(anyhow!("cannot merge an entity into itself"));
    }
    if entity_row(conn, winner_id)?.is_none() || entity_row(conn, loser_id)?.is_none() {
        return Err(anyhow!("merge references unknown entity"));
    }
    // Guard against cycles: the winner must not already resolve to the loser.
    if resolve_merged(conn, winner_id)? == loser_id {
        return Err(anyhow!("merge would create a cycle"));
    }
    let merge_id = ids::merge_id();
    conn.execute(
        "UPDATE entities SET merged_into = ?1 WHERE id = ?2",
        [winner_id, loser_id],
    )?;
    conn.execute(
        "INSERT INTO merges (id, winner_id, loser_id, confidence, reason, actor, merged_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            merge_id,
            winner_id,
            loser_id,
            confidence,
            reason,
            actor.actor,
            now_utc()
        ],
    )?;
    record_mutation(
        conn,
        actor,
        "entity_merged",
        &format!("entity_merged:{merge_id}"),
        &json!({"merge_id": merge_id, "winner": winner_id, "loser": loser_id, "reason": reason}),
    )?;
    Ok(merge_id)
}

/// Resolve an entity reference WITHOUT creating anything — used for scope
/// inference before a draft is applied.
pub(crate) fn peek_entity(
    conn: &Connection,
    entity_type: &str,
    name: &str,
    attributes: &[(String, String)],
) -> Result<Option<EntityRow>> {
    for (kind, value) in attributes {
        let hit: Option<String> = conn
            .query_row(
                "SELECT entity_id FROM entity_attributes WHERE kind = ?1 AND value = ?2",
                [kind.as_str(), value.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = hit {
            let id = resolve_merged(conn, &id)?;
            return entity_row(conn, &id);
        }
    }
    let hit: Option<String> = conn
        .query_row(
            "SELECT id FROM entities
             WHERE type = ?1 AND name = ?2 AND merged_into IS NULL LIMIT 1",
            [entity_type, name],
            |row| row.get(0),
        )
        .optional()?;
    match hit {
        Some(id) => entity_row(conn, &id),
        None => Ok(None),
    }
}

/// The scope label an identity attribute carries, if any.
pub(crate) fn attribute_scope(
    conn: &Connection,
    kind: &str,
    value: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT scope FROM entity_attributes WHERE kind = ?1 AND value = ?2",
            [kind, value],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

fn attach_attributes(conn: &Connection, id: &str, attributes: &[(String, String)]) -> Result<()> {
    for (kind, value) in attributes {
        conn.execute(
            "INSERT OR IGNORE INTO entity_attributes (entity_id, kind, value) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, kind, value],
        )?;
    }
    Ok(())
}

/// Assert an edge inside an open transaction. If a current record for
/// (src, relation, dst) exists, extend its provenance and pull t_valid
/// earlier if this episode predates it; otherwise create the edge.
/// Returns true if a new edge row was created.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assert_edge(
    conn: &Connection,
    actor: &Actor,
    src: &str,
    relation: &str,
    dst: &str,
    scope: &str,
    t_valid: Option<&str>,
    episode_id: &str,
) -> Result<bool> {
    let existing: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT id, t_valid FROM edges
             WHERE src = ?1 AND relation = ?2 AND dst = ?3 AND t_expired IS NULL
             LIMIT 1",
            [src, relation, dst],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    if let Some((edge_id, existing_valid)) = existing {
        let fresh = conn.execute(
            "INSERT OR IGNORE INTO edge_provenance (edge_id, episode_id) VALUES (?1, ?2)",
            [&edge_id, episode_id],
        )?;
        if fresh > 0 {
            // Strength = number of distinct supporting episodes. Without
            // this, every edge sits at 1.0 forever and "strongest first"
            // ordering (traverse_top, the constellation) is meaningless.
            conn.execute(
                "UPDATE edges SET strength = strength + 1 WHERE id = ?1",
                [&edge_id],
            )?;
        }
        if let Some(tv) = t_valid {
            if existing_valid.as_deref().map(|e| tv < e).unwrap_or(true) {
                conn.execute(
                    "UPDATE edges SET t_valid = ?1 WHERE id = ?2",
                    [tv, &edge_id],
                )?;
            }
        }
        return Ok(false);
    }

    let edge_id = ids::edge_id();
    conn.execute(
        "INSERT INTO edges (id, src, relation, dst, scope, t_valid, t_created)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![edge_id, src, relation, dst, scope, t_valid, now_utc()],
    )?;
    conn.execute(
        "INSERT INTO edge_provenance (edge_id, episode_id) VALUES (?1, ?2)",
        [&edge_id, episode_id],
    )?;
    record_mutation(
        conn,
        actor,
        "edge_created",
        &format!("edge_created:{edge_id}"),
        &json!({"id": edge_id, "src": src, "relation": relation, "dst": dst}),
    )?;
    Ok(true)
}

impl Store {
    /// Find an unmerged entity by name (case-insensitive), preferring an
    /// exact-case match.
    pub fn entity_by_name(&self, name: &str) -> Result<Option<EntityRow>> {
        let conn = self.lock();
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM entities
                 WHERE merged_into IS NULL AND lower(name) = lower(?1)
                 ORDER BY (name = ?1) DESC, created_at LIMIT 1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        match id {
            Some(id) => entity_row(&conn, &id),
            None => Ok(None),
        }
    }

    /// The retroactive machine-address sweep: unmerged Persons whose
    /// identities are all machine addresses (see crate::machine)
    /// reclassify as `Service` — out of every person surface, episodes
    /// and provenance untouched. Idempotent; mutation-logged; runs each
    /// consolidation so future imports are covered belt-and-suspenders.
    pub fn sweep_machine_persons(&self, actor: &Actor) -> Result<usize> {
        let candidates: Vec<(String, String)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT e.id, e.name FROM entities e
                 WHERE e.type = 'Person' AND e.merged_into IS NULL
                   AND (e.name LIKE '%@%' OR EXISTS (
                        SELECT 1 FROM entity_attributes a
                        WHERE a.entity_id = e.id AND a.kind = 'email'))",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut swept = 0usize;
        for chunk in candidates.chunks(200) {
            {
                let conn = self.lock();
                for (id, name) in chunk {
                    let attrs: Vec<(String, String)> = {
                        let mut stmt = conn.prepare(
                            "SELECT kind, value FROM entity_attributes WHERE entity_id = ?1",
                        )?;
                        let rows = stmt.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
                        rows.collect::<std::result::Result<_, _>>()?
                    };
                    if !crate::machine::is_machine_person(name, &attrs) {
                        continue;
                    }
                    conn.execute("UPDATE entities SET type = 'Service' WHERE id = ?1", [id])?;
                    record_mutation(
                        &conn,
                        actor,
                        "entity_reclassified",
                        &format!("machine_sweep:{id}"),
                        &json!({"id": id, "name": name, "from": "Person", "to": "Service",
                                "reason": "machine address rulebook"}),
                    )?;
                    swept += 1;
                }
            }
            // unfair mutex — let readers breathe between batches
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        // Purge pending merge-reviews that involve a machine Service: a
        // Service is never a person to merge, so these are pure Tend noise.
        // Covers the entities just reclassified AND the backlog from prior
        // sweeps (Jon: donotreply/calendar addresses lingering in Tend
        // after reclassification, 2026-07-13).
        let purged = {
            let conn = self.lock();
            conn.execute(
                "DELETE FROM merge_review_queue
                 WHERE resolution IS NULL
                   AND (entity_a IN (SELECT id FROM entities WHERE type = 'Service')
                     OR entity_b IN (SELECT id FROM entities WHERE type = 'Service'))",
                [],
            )?
        };
        if purged > 0 {
            tracing::info!("machine sweep: purged {purged} Tend reviews involving a Service");
        }
        Ok(swept)
    }

    /// An entity by exact id (unmerged or not) — the HTTP layer's
    /// id-or-name resolution uses this before falling back to names.
    pub fn entity_by_id(&self, id: &str) -> Result<Option<EntityRow>> {
        let conn = self.lock();
        entity_row(&conn, id)
    }

    /// Rename an entity BY ID (item 12) — dependencies stay pointed at
    /// the id, so nothing is re-mapped by name and a same-named
    /// Person/Org is never collapsed in. Appends to the v37 rename
    /// audit trail (item 14) so the peer node reconciles the rename by
    /// (type, from_name) instead of minting a second entity. Returns
    /// (id, from, to).
    pub fn rename_entity(
        &self,
        actor: &Actor,
        id: &str,
        new_name: &str,
    ) -> Result<(String, String, String)> {
        let new_name = new_name.trim();
        if new_name.is_empty() || new_name.len() > 120 {
            return Err(anyhow!("a name must be 1–120 characters"));
        }
        let conn = self.lock();
        let id = resolve_merged(&conn, id)?;
        let (entity_type, from): (String, String) = conn
            .query_row(
                "SELECT type, name FROM entities WHERE id = ?1 AND merged_into IS NULL",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("no entity {id}"))?;
        if from == new_name {
            return Ok((id, from.clone(), from));
        }
        // A live same-type entity already holding the name is a merge
        // decision, not a rename — refuse rather than create ambiguity.
        let taken: Option<String> = conn
            .query_row(
                "SELECT id FROM entities
                 WHERE merged_into IS NULL AND type = ?1
                   AND lower(name) = lower(?2) AND id != ?3",
                rusqlite::params![entity_type, new_name, id],
                |row| row.get(0),
            )
            .optional()?;
        if taken.is_some() {
            return Err(anyhow!(
                "a {entity_type} named \"{new_name}\" already exists — merge them instead of renaming onto it"
            ));
        }
        let now = now_utc();
        conn.execute(
            "UPDATE entities SET name = ?2 WHERE id = ?1",
            rusqlite::params![id, new_name],
        )?;
        conn.execute(
            "INSERT INTO entity_renames (id, entity_id, entity_type, from_name, to_name, actor, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ids::rename_id(),
                id,
                entity_type,
                from,
                new_name,
                actor.actor,
                now
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "entity_rename",
            &format!("rename:{id}:{now}"),
            &json!({"entity_id": id, "type": entity_type, "from": from, "to": new_name}),
        )?;
        Ok((id, from, new_name.to_string()))
    }

    /// The one-line identity card for a candidate list: best identifying
    /// attribute (email first, else phone) + episode count — "which
    /// Maria is this?" before any merge or declaration.
    pub fn entity_contact_card(&self, entity_id: &str) -> Result<(Option<String>, i64)> {
        let attr_guard = self.scope_guard("COALESCE(scope, '')");
        let conn = self.lock();
        let handle: Option<String> = conn
            .query_row(
                &format!(
                    "SELECT COALESCE(MIN(CASE WHEN kind = 'email' THEN value END),
                                     MIN(value))
                     FROM entity_attributes
                     WHERE entity_id = ?1 AND kind IN ('email', 'phone'){attr_guard}"
                ),
                [entity_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let guard = self.scope_guard("ep.scope");
        let episodes: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM episode_entities ee
                 JOIN episodes ep ON ep.id = ee.episode_id AND ep.superseded_by IS NULL
                 WHERE ee.entity_id = ?1{guard}"
            ),
            [entity_id],
            |row| row.get(0),
        )?;
        Ok((handle, episodes))
    }

    /// Person entities created after `since` (merge-candidate scan feed),
    /// oldest first so the watermark can advance monotonically.
    /// EVIDENCE-GATED: a person with zero episode citations can't be
    /// usefully merge-reviewed (nothing to show the human) and bulk
    /// imports would flood Tend — they enter the feed only once real
    /// evidence cites them.
    pub fn persons_created_since(
        &self,
        since: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at FROM entities e
             WHERE type = 'Person' AND merged_into IS NULL AND hidden_by IS NULL AND created_at > ?1
               AND EXISTS (SELECT 1 FROM episode_entities ee WHERE ee.entity_id = e.id
                           AND ee.episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL))
             ORDER BY created_at LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![since, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Unmerged Persons whose name contains `token` — the cheap lexical
    /// prefilter before the embedding comparison.
    /// Email attributes of one entity — the merge scan's email-evidence
    /// leg reads these.
    pub fn entity_emails(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT value FROM entity_attributes WHERE entity_id = ?1 AND kind = 'email'",
        )?;
        let rows = stmt.query_map([id], |row| row.get(0))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Persons whose EMAIL localpart contains `token` — with the email,
    /// so the caller can verify the name↔localpart evidence properly.
    pub fn persons_with_email_like(
        &self,
        token: &str,
        exclude: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        let escaped = token
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.name, a.value FROM entity_attributes a
             JOIN entities e ON e.id = a.entity_id
             WHERE a.kind = 'email' AND e.type = 'Person'
               AND e.merged_into IS NULL AND e.id != ?1
               AND a.value LIKE ?2 ESCAPE '\\'
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![exclude, format!("%{escaped}%"), limit as i64],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    pub fn persons_sharing_token(
        &self,
        token: &str,
        exclude: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let escaped = token
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name FROM entities
             WHERE type = 'Person' AND merged_into IS NULL AND hidden_by IS NULL AND id != ?1
               AND name LIKE ?2 ESCAPE '\\'
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![exclude, format!("%{escaped}%"), limit as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Queue an embedding-suggested same-person pair for the Tend gate.
    /// Pair-order normalized; already-queued (either order) or already-
    /// merged pairs are no-ops. Returns true if freshly queued.
    pub fn queue_merge_candidate(
        &self,
        a: &str,
        b: &str,
        confidence: f64,
        reason: &str,
    ) -> Result<bool> {
        let conn = self.lock();
        if resolve_merged(&conn, a)? == resolve_merged(&conn, b)? {
            return Ok(false);
        }
        let (first, second) = if a <= b { (a, b) } else { (b, a) };
        let seen: i64 = conn.query_row(
            "SELECT COUNT(*) FROM merge_review_queue
             WHERE (entity_a = ?1 AND entity_b = ?2)
                OR (entity_a = ?2 AND entity_b = ?1)",
            [first, second],
            |row| row.get(0),
        )?;
        if seen > 0 {
            return Ok(false);
        }
        let changed = conn.execute(
            "INSERT OR IGNORE INTO merge_review_queue
             (id, entity_a, entity_b, confidence, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                ids::review_id(),
                first,
                second,
                confidence,
                reason,
                now_utc()
            ],
        )?;
        Ok(changed > 0)
    }

    /// Detach one identifying attribute from an entity ("this email is
    /// not them"). The attribute row is deleted — a future ingest can
    /// re-claim it onto whoever actually owns it. Mutation-logged.
    pub fn remove_entity_attribute(
        &self,
        actor: &Actor,
        entity_id: &str,
        kind: &str,
        value: &str,
    ) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "DELETE FROM entity_attributes
             WHERE entity_id = ?1 AND kind = ?2 AND value = ?3",
            [entity_id, kind, value],
        )?;
        if changed > 0 {
            record_mutation(
                &conn,
                actor,
                "attribute_removed",
                &format!("attribute_removed:{entity_id}:{kind}:{value}"),
                &json!({"entity": entity_id, "kind": kind, "value": value}),
            )?;
        }
        Ok(changed > 0)
    }

    /// Declare an entity's importance (0 = unset, 1..3). Merge-group
    /// aware: the mark lands on the surviving entity.
    pub fn set_declared_importance(&self, actor: &Actor, name: &str, level: i64) -> Result<()> {
        if !(0..=3).contains(&level) {
            return Err(anyhow!("importance must be 0..3, got {level}"));
        }
        let target = self
            .entity_by_name(name)?
            .ok_or_else(|| anyhow!("unknown entity: {name}"))?;
        let conn = self.lock();
        let winner = resolve_merged(&conn, &target.id)?;
        conn.execute(
            "UPDATE entities SET declared_importance = ?1 WHERE id = ?2",
            rusqlite::params![level, winner],
        )?;
        record_mutation(
            &conn,
            actor,
            "importance_declared",
            &format!("importance_declared:{winner}:{level}"),
            &json!({"entity": winner, "name": name, "level": level}),
        )?;
        Ok(())
    }

    /// Contradict a fact: set t_invalid. The edge row survives — the
    /// history of what we believed, and when, is part of the graph.
    pub fn invalidate_edge(&self, actor: &Actor, edge_id: &str, t_invalid: &str) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE edges SET t_invalid = ?1 WHERE id = ?2 AND t_expired IS NULL",
            [t_invalid, edge_id],
        )?;
        if changed == 0 {
            return Err(anyhow!("no current edge {edge_id}"));
        }
        record_mutation(
            &conn,
            actor,
            "edge_invalidated",
            &format!("edge_invalidated:{edge_id}:{t_invalid}"),
            &json!({"id": edge_id, "t_invalid": t_invalid}),
        )?;
        Ok(())
    }

    /// Merge loser into winner as a reversible event.
    pub fn merge_entities(
        &self,
        actor: &Actor,
        winner_id: &str,
        loser_id: &str,
        confidence: f64,
        reason: &str,
    ) -> Result<String> {
        let conn = self.lock();
        merge_entities_inner(&conn, actor, winner_id, loser_id, confidence, reason)
    }

    /// Undo a merge: clear the pointer, stamp reverted_at.
    pub fn revert_merge(&self, actor: &Actor, merge_id: &str) -> Result<()> {
        let conn = self.lock();
        let loser: Option<String> = conn
            .query_row(
                "SELECT loser_id FROM merges WHERE id = ?1 AND reverted_at IS NULL",
                [merge_id],
                |row| row.get(0),
            )
            .optional()?;
        let loser = loser.ok_or_else(|| anyhow!("no un-reverted merge {merge_id}"))?;
        conn.execute(
            "UPDATE entities SET merged_into = NULL WHERE id = ?1",
            [&loser],
        )?;
        conn.execute(
            "UPDATE merges SET reverted_at = ?1 WHERE id = ?2",
            [&now_utc(), merge_id],
        )?;
        record_mutation(
            &conn,
            actor,
            "merge_reverted",
            &format!("merge_reverted:{merge_id}"),
            &json!({"merge_id": merge_id}),
        )?;
        Ok(())
    }

    /// Claim an identifying attribute for an entity: "this identity is
    /// also me/it." If the attribute is unowned it attaches (optionally
    /// carrying a scope — e.g. a company email is a `work` identity); if
    /// another entity of the same type owns it, that entity is merged in
    /// (reversibly, confidence 1.0). This is the one-human-many-identities
    /// mechanism.
    /// Remove ONE identity binding (Settings "remove this email"). The
    /// entity keeps existing; only the attribute row goes — the reverse
    /// of a claim's attach step (merges are reverted separately via the
    /// merges surface).
    pub fn unclaim_identity(&self, actor: &Actor, kind: &str, value: &str) -> Result<bool> {
        let conn = self.lock();
        let removed = conn.execute(
            "DELETE FROM entity_attributes WHERE kind = ?1 AND value = ?2",
            rusqlite::params![kind, value],
        )?;
        if removed > 0 {
            super::record_mutation(
                &conn,
                actor,
                "identity_unclaimed",
                &format!("identity_unclaimed:{kind}:{value}"),
                &serde_json::json!({"kind": kind, "value": value}),
            )?;
        }
        Ok(removed > 0)
    }

    pub fn claim_identity(
        &self,
        actor: &Actor,
        entity_name: &str,
        kind: &str,
        value: &str,
        scope: Option<&str>,
    ) -> Result<ClaimOutcome> {
        // Claiming for a name the graph doesn't know yet creates the
        // Person — this is how a fresh brain bootstraps its owner.
        let target = match self.entity_by_name(entity_name)? {
            Some(entity) => entity,
            None => {
                let conn = self.lock();
                let (id, _, _) = upsert_entity(
                    &conn,
                    actor,
                    "Person",
                    entity_name,
                    scope.unwrap_or("personal"),
                    &[],
                )?;
                drop(conn);
                self.entity_by_name(entity_name)?
                    .ok_or_else(|| anyhow!("failed to create {entity_name} ({id})"))?
            }
        };
        let conn = self.lock();

        let owner: Option<String> = conn
            .query_row(
                "SELECT entity_id FROM entity_attributes WHERE kind = ?1 AND value = ?2",
                [kind, value],
                |row| row.get(0),
            )
            .optional()?;

        let outcome = match owner {
            None => {
                conn.execute(
                    "INSERT INTO entity_attributes (entity_id, kind, value, scope)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![target.id, kind, value, scope],
                )?;
                record_mutation(
                    &conn,
                    actor,
                    "identity_claimed",
                    &format!("identity_claimed:{}:{kind}:{value}", target.id),
                    &json!({"entity": target.id, "kind": kind, "value": value, "scope": scope}),
                )?;
                ClaimOutcome::Attached {
                    entity_id: target.id,
                }
            }
            Some(owner_id) => {
                let resolved = resolve_merged(&conn, &owner_id)?;
                if resolved == target.id {
                    // Already ours; still allow labeling the identity's scope.
                    if scope.is_some() {
                        conn.execute(
                            "UPDATE entity_attributes SET scope = ?1
                             WHERE kind = ?2 AND value = ?3",
                            rusqlite::params![scope, kind, value],
                        )?;
                    }
                    ClaimOutcome::AlreadyYours {
                        entity_id: target.id,
                    }
                } else {
                    let other = entity_row(&conn, &resolved)?
                        .ok_or_else(|| anyhow!("dangling attribute owner {resolved}"))?;
                    if other.entity_type != target.entity_type {
                        return Err(anyhow!(
                            "{kind}={value} belongs to {} ({}), not a {}",
                            other.name,
                            other.entity_type,
                            target.entity_type
                        ));
                    }
                    let merge_id = merge_entities_inner(
                        &conn,
                        actor,
                        &target.id,
                        &other.id,
                        1.0,
                        &format!("identity claimed: {kind}={value}"),
                    )?;
                    if scope.is_some() {
                        conn.execute(
                            "UPDATE entity_attributes SET scope = ?1
                             WHERE kind = ?2 AND value = ?3",
                            rusqlite::params![scope, kind, value],
                        )?;
                    }
                    // Any review item pairing these two is now settled.
                    conn.execute(
                        "UPDATE merge_review_queue SET resolution = 'merged'
                         WHERE resolution IS NULL
                           AND ((entity_a = ?1 AND entity_b = ?2)
                             OR (entity_a = ?2 AND entity_b = ?1))",
                        [&target.id, &other.id],
                    )?;
                    ClaimOutcome::MergedFrom {
                        entity_id: target.id,
                        merged_entity: other,
                        merge_id,
                    }
                }
            }
        };
        Ok(outcome)
    }

    /// The scope an identity attribute carries, if labeled.
    pub fn identity_scope(&self, kind: &str, value: &str) -> Result<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT scope FROM entity_attributes WHERE kind = ?1 AND value = ?2",
                [kind, value],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Pending merge-review questions, oldest first.
    pub fn pending_reviews(&self, limit: usize) -> Result<Vec<ReviewItem>> {
        self.reviews_in_state(None, limit)
    }

    /// Re-apply the current precision rules to the EXISTING pending queue
    /// (the rules only gate fresh scans; this cleans the backlog). A pair
    /// whose two multi-token names have CONFLICTING surnames is
    /// affirmative distinctness → auto-resolve 'distinct'. Everything
    /// else stays for the human. Returns how many were cleared.
    pub fn retriage_reviews(&self, actor: &Actor) -> Result<usize> {
        let pending = self.reviews_in_state(None, 5000)?;
        let mut cleared = 0usize;
        for r in pending {
            let a_tok: Vec<String> = r
                .entity_a
                .name
                .split_whitespace()
                .map(|s| s.to_lowercase())
                .collect();
            let b_tok: Vec<String> = r
                .entity_b
                .name
                .split_whitespace()
                .map(|s| s.to_lowercase())
                .collect();
            let surname_conflict =
                a_tok.len() > 1 && b_tok.len() > 1 && a_tok.last() != b_tok.last();
            // never auto-clear a pair that shares an identifying attribute
            // (same email/handle IS strong same-person evidence)
            let shares_attr = r.attributes_a.iter().any(|(_, v)| {
                r.attributes_b
                    .iter()
                    .any(|(_, w)| v.eq_ignore_ascii_case(w))
            });
            if surname_conflict && !shares_attr {
                self.resolve_review(actor, &r.id, "distinct")?;
                cleared += 1;
            }
        }
        Ok(cleared)
    }

    /// The don't-know pile (resolution='unknown') or the pending queue
    /// (None) — same evidence-rich rows either way.
    pub fn reviews_in_state(&self, state: Option<&str>, limit: usize) -> Result<Vec<ReviewItem>> {
        let conn = self.lock();
        let clause = match state {
            Some(_) => "resolution = ?2",
            None => "resolution IS NULL AND ?2 = ?2",
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT id, entity_a, entity_b, confidence, reason, created_at
             FROM merge_review_queue WHERE {clause}
             ORDER BY created_at LIMIT ?1",
        ))?;
        let raw: Vec<(String, String, String, f64, String, String)> = stmt
            .query_map(
                rusqlite::params![limit as i64, state.unwrap_or("x")],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?
            .collect::<std::result::Result<_, _>>()?;
        let hide_private = !self.private_visible();
        let mut items = Vec::new();
        for (id, a, b, confidence, reason, created_at) in raw {
            let (Some(entity_a), Some(entity_b)) = (entity_row(&conn, &a)?, entity_row(&conn, &b)?)
            else {
                continue;
            };
            if hide_private && (entity_a.scope == "private" || entity_b.scope == "private") {
                continue;
            }
            items.push(ReviewItem {
                attributes_a: entity_attributes(&conn, &entity_a.id)?,
                attributes_b: entity_attributes(&conn, &entity_b.id)?,
                episodes_a: entity_episode_count(&conn, &entity_a.id)?,
                episodes_b: entity_episode_count(&conn, &entity_b.id)?,
                id,
                entity_a,
                entity_b,
                confidence,
                reason,
                created_at,
            });
        }
        Ok(items)
    }

    /// Sweep pending reviews whose two names differ ONLY by case and
    /// whose identifying attributes don't CONFLICT (same kind, different
    /// value — e.g. two different emails — stays a human question). Those
    /// aren't questions, they're noise: auto-merge, more-attributed /
    /// better-evidenced side winning, reversibly like every merge.
    pub fn sweep_case_duplicates(&self, actor: &Actor) -> Result<usize> {
        let pending: Vec<(String, String, String)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT q.id, q.entity_a, q.entity_b
                 FROM merge_review_queue q
                 JOIN entities a ON a.id = q.entity_a
                 JOIN entities b ON b.id = q.entity_b
                 WHERE q.resolution IS NULL AND lower(a.name) = lower(b.name)",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut merged = 0usize;
        for (review_id, a, b) in pending {
            let conn = self.lock();
            let attrs_a = entity_attributes(&conn, &a)?;
            let attrs_b = entity_attributes(&conn, &b)?;
            // Conflict: both sides carry the same identifying attribute KIND
            // with different values — could be two people named Ben. A
            // per-RECORD source id (a calendar event's `ics-uid`) is NOT
            // identifying: an Event entity accumulates one per occurrence, so
            // two case-variant events ("Main street dental"/"Main Street Dental") always
            // "conflict" on ics-uid and never auto-merge. Exclude record-ids.
            let identity = |kind: &str| !is_record_id_attr(kind);
            let conflicted =
                attrs_a
                    .iter()
                    .filter(|(k, _)| identity(k))
                    .any(|(kind_a, value_a)| {
                        attrs_b
                            .iter()
                            .filter(|(k, _)| identity(k))
                            .any(|(kind_b, value_b)| kind_a == kind_b && value_a != value_b)
                    });
            if conflicted {
                continue;
            }
            let (Some(row_a), Some(row_b)) = (entity_row(&conn, &a)?, entity_row(&conn, &b)?)
            else {
                continue;
            };
            // A human-DECLARED entity (importance>0 — a starred person, a
            // declared Project) is never silently absorbed: the case
            // sweep must not eat "Hearthloom" (the project you named)
            // into "hearthloom" (the auto-minted repo). Leave it in the
            // Tend queue for an explicit yes/no.
            if row_a.declared_importance > 0 || row_b.declared_importance > 0 {
                continue;
            }
            // A project and the repo it CONTAINS are not duplicates —
            // the containment edge says they're distinct-but-related.
            let related: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM edges
                     WHERE relation = 'contains' AND t_expired IS NULL
                       AND ((src = ?1 AND dst = ?2) OR (src = ?2 AND dst = ?1)))",
                [&a, &b],
                |row| row.get(0),
            )?;
            if related {
                continue;
            }
            // Winner: more identity, then more evidence, then the
            // title-cased display name, then age.
            let count_a = entity_episode_count(&conn, &a)?;
            let count_b = entity_episode_count(&conn, &b)?;
            let a_wins = (
                attrs_a.len(),
                count_a,
                row_a
                    .name
                    .chars()
                    .next()
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false),
                std::cmp::Reverse(row_a.created_at.clone()),
            ) >= (
                attrs_b.len(),
                count_b,
                row_b
                    .name
                    .chars()
                    .next()
                    .map(|c| c.is_uppercase())
                    .unwrap_or(false),
                std::cmp::Reverse(row_b.created_at.clone()),
            );
            let (winner, loser) = if a_wins { (&a, &b) } else { (&b, &a) };
            let winner = resolve_merged(&conn, winner)?;
            let loser = resolve_merged(&conn, loser)?;
            if winner != loser {
                merge_entities_inner(
                    &conn,
                    actor,
                    &winner,
                    &loser,
                    0.95,
                    "case-only name duplicate, no conflicting identity",
                )?;
            }
            conn.execute(
                "UPDATE merge_review_queue SET resolution = 'merged' WHERE id = ?1",
                [&review_id],
            )?;
            record_mutation(
                &conn,
                actor,
                "review_resolved",
                &format!("review_resolved:{review_id}"),
                &json!({"review": review_id, "same": true, "auto": "case-duplicate"}),
            )?;
            drop(conn);
            merged += 1;
        }
        Ok(merged)
    }

    /// Answer a review question: same (merge b into a), distinct,
    /// unknown (parked in the don't-know pile for later), or reopen
    /// (unknown → pending again).
    pub fn resolve_review(
        &self,
        actor: &Actor,
        review_id: &str,
        action: &str,
    ) -> Result<Option<String>> {
        if !matches!(action, "same" | "distinct" | "unknown" | "reopen") {
            return Err(anyhow!("resolution must be same|distinct|unknown|reopen"));
        }
        let same = action == "same";
        let conn = self.lock();
        // same/distinct/unknown act on pending OR parked rows; reopen
        // only makes sense from the parked pile.
        let eligible = if action == "reopen" {
            "resolution = 'unknown'"
        } else {
            "(resolution IS NULL OR resolution = 'unknown')"
        };
        let pair: Option<(String, String)> = conn
            .query_row(
                &format!(
                    "SELECT entity_a, entity_b FROM merge_review_queue
                     WHERE id = ?1 AND {eligible}"
                ),
                [review_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (a, b) = pair.ok_or_else(|| anyhow!("no eligible review {review_id}"))?;

        let merge_id = if same {
            let winner = resolve_merged(&conn, &a)?;
            let loser = resolve_merged(&conn, &b)?;
            if winner == loser {
                None // already merged by an earlier claim
            } else {
                Some(merge_entities_inner(
                    &conn,
                    actor,
                    &winner,
                    &loser,
                    1.0,
                    &format!("review {review_id} resolved: same"),
                )?)
            }
        } else {
            None
        };
        let resolution: Option<&str> = match action {
            "same" => Some("merged"),
            "distinct" => Some("distinct"),
            "unknown" => Some("unknown"),
            _ => None, // reopen → back to pending
        };
        conn.execute(
            "UPDATE merge_review_queue SET resolution = ?1 WHERE id = ?2",
            rusqlite::params![resolution, review_id],
        )?;
        record_mutation(
            &conn,
            actor,
            "review_resolved",
            &format!("review_resolved:{review_id}:{action}"),
            &json!({"review": review_id, "action": action, "merge_id": merge_id}),
        )?;
        Ok(merge_id)
    }

    /// Recent merges (for inspection and reverts), newest first.
    pub fn recent_merges(&self, limit: usize) -> Result<Vec<MergeRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.id, m.winner_id, e.name, m.confidence, m.reason, m.actor,
                    m.merged_at, m.reverted_at
             FROM merges m JOIN entities e ON e.id = m.loser_id
             ORDER BY m.merged_at DESC LIMIT ?1",
        )?;
        type MergeRow = (
            String,
            String,
            String,
            f64,
            String,
            String,
            String,
            Option<String>,
        );
        let raw: Vec<MergeRow> = stmt
            .query_map([limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut records = Vec::new();
        for (id, winner_id, loser_name, confidence, reason, actor, merged_at, reverted_at) in raw {
            let Some(winner) = entity_row(&conn, &winner_id)? else {
                continue;
            };
            records.push(MergeRecord {
                id,
                winner,
                loser_name,
                confidence,
                reason,
                actor,
                merged_at,
                reverted_at,
            });
        }
        Ok(records)
    }

    /// Correct an entity's scope label (scopes are user-overridable
    /// metadata, not history). For Project entities, `cascade` relabels
    /// the episodes citing it as their repo — the "this repo is a personal
    /// project, not client work" fix.
    pub fn set_entity_scope(
        &self,
        actor: &Actor,
        entity_name: &str,
        scope: &str,
        cascade: bool,
    ) -> Result<usize> {
        let entity = self
            .entity_by_name(entity_name)?
            .ok_or_else(|| anyhow!("unknown entity: {entity_name}"))?;
        let keep_vaulted = self.relabel_keeps_private();
        let conn = self.lock();
        let group = merge_group(&conn, &entity.id)?;
        let marks = placeholders(group.len());
        conn.execute(
            &format!("UPDATE entities SET scope = ?1 WHERE id IN ({marks}){keep_vaulted}"),
            rusqlite::params_from_iter(
                std::iter::once(scope.to_string()).chain(group.iter().cloned()),
            ),
        )?;
        let mut relabeled = 0;
        if cascade && entity.entity_type == "Project" {
            relabeled = conn.execute(
                &format!(
                    "UPDATE episodes SET scope = ?1, scope_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id IN (
                         SELECT episode_id FROM episode_entities
                         WHERE role = 'repo' AND entity_id IN ({marks})){keep_vaulted}"
                ),
                rusqlite::params_from_iter(
                    std::iter::once(scope.to_string()).chain(group.iter().cloned()),
                ),
            )?;
            conn.execute(
                &format!(
                    "UPDATE edges SET scope = ?1
                     WHERE t_expired IS NULL
                       AND (src IN ({marks}) OR dst IN ({marks})){keep_vaulted}"
                ),
                rusqlite::params_from_iter(
                    std::iter::once(scope.to_string())
                        .chain(group.iter().cloned())
                        .chain(group.iter().cloned()),
                ),
            )?;
        }
        record_mutation(
            &conn,
            actor,
            "scope_set",
            &format!("scope_set:{}:{scope}:{cascade}", entity.id),
            &json!({
                "entity": entity.id, "name": entity.name, "scope": scope,
                "cascade": cascade, "episodes_relabeled": relabeled,
            }),
        )?;
        Ok(relabeled)
    }

    /// Temporal ribbons: monthly citing-episode counts for the top
    /// entities of the given types — the data behind "what was true when".
    pub fn ribbons(&self, types: &[&str], lanes: usize) -> Result<RibbonData> {
        let top: Vec<EntityRow> = self
            .top_entities(60)?
            .into_iter()
            .map(|(entity, _)| entity)
            .filter(|entity| types.contains(&entity.entity_type.as_str()))
            .take(lanes)
            .collect();

        let episode_guard = self.scope_guard("ep.scope");
        let conn = self.lock();
        // month axis: from earliest to latest episode month
        let (min, max): (Option<String>, Option<String>) = conn.query_row(
            "SELECT substr(MIN(event_time),1,7), substr(MAX(event_time),1,7)
             FROM episodes WHERE event_time IS NOT NULL AND event_time >= '2000'
               AND superseded_by IS NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let (Some(min), Some(max)) = (min, max) else {
            return Ok((vec![], vec![]));
        };
        let mut months = Vec::new();
        let (mut year, mut month): (i32, u32) = (
            min[..4].parse().unwrap_or(2020),
            min[5..7].parse().unwrap_or(1),
        );
        let (end_year, end_month): (i32, u32) = (
            max[..4].parse().unwrap_or(2026),
            max[5..7].parse().unwrap_or(12),
        );
        while (year, month) <= (end_year, end_month) && months.len() < 240 {
            months.push(format!("{year:04}-{month:02}"));
            month += 1;
            if month > 12 {
                month = 1;
                year += 1;
            }
        }
        let index: std::collections::HashMap<&str, usize> = months
            .iter()
            .enumerate()
            .map(|(i, m)| (m.as_str(), i))
            .collect();

        let mut result = Vec::new();
        for entity in top {
            let group = merge_group(&conn, &entity.id)?;
            let marks = placeholders(group.len());
            let mut stmt = conn.prepare(&format!(
                "SELECT substr(ep.event_time,1,7), COUNT(DISTINCT ep.id)
                 FROM episode_entities ee JOIN episodes ep ON ep.id = ee.episode_id
                 WHERE ee.entity_id IN ({marks}) AND ep.event_time IS NOT NULL
                   AND ep.superseded_by IS NULL{episode_guard}
                 GROUP BY 1"
            ))?;
            let rows: Vec<(String, i64)> = stmt
                .query_map(rusqlite::params_from_iter(&group), |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            let mut counts = vec![0i64; months.len()];
            for (m, n) in rows {
                if let Some(&i) = index.get(m.as_str()) {
                    counts[i] = n;
                }
            }
            result.push((entity, counts));
        }
        Ok((months, result))
    }

    /// Every known local repo path (repo-path identifying attributes) —
    /// the auto-refresh worklist.
    pub fn known_repo_paths(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT value FROM entity_attributes WHERE kind = 'repo-path' ORDER BY value",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Relabel every episode from a source (e.g. an entire imported
    /// account's conversations). Scope is correction metadata; the
    /// mutation log keeps the audit trail.
    pub fn set_scope_by_source(&self, actor: &Actor, source: &str, scope: &str) -> Result<usize> {
        let keep_vaulted = self.relabel_keeps_private();
        let conn = self.lock();
        let relabeled = conn.execute(
            &format!(
                "UPDATE episodes SET scope = ?1, scope_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE source = ?2 AND scope != ?1{keep_vaulted}"
            ),
            [scope, source],
        )?;
        if relabeled > 0 {
            record_mutation(
                &conn,
                actor,
                "scope_set_by_source",
                &format!("scope_set_by_source:{source}:{scope}:{relabeled}"),
                &json!({"source": source, "scope": scope, "episodes_relabeled": relabeled}),
            )?;
        }
        Ok(relabeled)
    }

    /// Relabel specific episodes by id (single or bulk — the Browse grid's
    /// "correct scope" / "mark private" action). Content is untouched;
    /// scope is correction metadata with an audit trail.
    pub fn set_episode_scopes(
        &self,
        actor: &Actor,
        episode_ids: &[String],
        scope: &str,
    ) -> Result<usize> {
        let keep_vaulted = self.relabel_keeps_private();
        let conn = self.lock();
        let mut relabeled = 0;
        for id in episode_ids {
            relabeled += conn.execute(
                &format!(
                    "UPDATE episodes SET scope = ?1, scope_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now') WHERE id = ?2 AND scope != ?1{keep_vaulted}"
                ),
                [scope, id],
            )?;
        }
        if relabeled > 0 {
            record_mutation(
                &conn,
                actor,
                "scope_set_by_episode",
                &format!("scope_set_by_episode:{scope}:{}", crate::time::now_utc()),
                &json!({"episodes": episode_ids, "scope": scope, "episodes_relabeled": relabeled}),
            )?;
        }
        Ok(relabeled)
    }

    /// Breadth-first traversal from an entity (by id or name) over current
    /// edges, both directions. `as_of` filters to edges valid at that
    /// instant (bi-temporal read).
    pub fn traverse(
        &self,
        start: &str,
        relation: Option<&str>,
        depth: usize,
        as_of: Option<&str>,
    ) -> Result<Vec<TraverseHop>> {
        self.traverse_top(start, relation, depth, as_of, None)
    }

    /// `traverse` with a significance cap: at every level, each node
    /// contributes at most `top` edges, strongest first — the
    /// constellation's progressive-loading contract. Uncapped traverse
    /// over the post-mail graph (23k entities, fan-out in the
    /// thousands) took 140s and held the store lock throughout; capped
    /// it is milliseconds. `None` = unlimited (small-graph callers).
    pub fn traverse_top(
        &self,
        start: &str,
        relation: Option<&str>,
        depth: usize,
        as_of: Option<&str>,
        top: Option<usize>,
    ) -> Result<Vec<TraverseHop>> {
        let start_row = match self.entity_by_name(start)? {
            Some(row) => row,
            None => {
                let conn = self.lock();
                entity_row(&conn, start)?.ok_or_else(|| anyhow!("unknown entity: {start}"))?
            }
        };
        let hidden = self.scope_hider();
        if hidden(&start_row.scope) {
            return Err(anyhow!("unknown entity: {start}"));
        }
        let edge_guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut hops = Vec::new();
        let mut visited = std::collections::HashSet::new();
        visited.insert(start_row.id.clone());
        let mut frontier = vec![start_row];

        for level in 1..=depth.max(1) {
            let mut next = Vec::new();
            for from in &frontier {
                // Edges attached to any member of this entity's merge
                // group count as its edges.
                let group = merge_group(&conn, &from.id)?;
                let marks = placeholders(group.len());
                // Bare `?` placeholders bind in textual order: group twice
                // (the two IN lists), relation twice, as_of three times.
                // Over-fetch 3× by raw strength, then re-rank with the
                // human's declared importance as the leading tier — a
                // level-3 person beats any mailing list's edge count.
                let cap = top
                    .map(|t| format!(" ORDER BY strength DESC, rowid DESC LIMIT {}", t * 3))
                    .unwrap_or_default();
                let mut stmt = conn.prepare(&format!(
                    "SELECT src, relation, dst, t_valid, t_invalid, strength FROM edges
                     WHERE (src IN ({marks}) OR dst IN ({marks})) AND t_expired IS NULL{edge_guard}
                       AND (? IS NULL OR relation = ?)
                       AND (? IS NULL OR
                            ((t_valid IS NULL OR t_valid <= ?)
                             AND (t_invalid IS NULL OR t_invalid > ?))){cap}"
                ))?;
                let params: Vec<Option<String>> = group
                    .iter()
                    .map(|g| Some(g.clone()))
                    .chain(group.iter().map(|g| Some(g.clone())))
                    .chain(
                        [relation, relation, as_of, as_of, as_of]
                            .iter()
                            .map(|v| v.map(String::from)),
                    )
                    .collect();
                let edges: Vec<EdgeTuple> = stmt
                    .query_map(rusqlite::params_from_iter(params), |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    })?
                    .collect::<std::result::Result<_, _>>()?;
                let mut candidates: Vec<(TraverseHop, String, f64)> = Vec::new();
                for (src, rel, dst, t_valid, t_invalid, strength) in edges {
                    let (raw_other, direction) = if group.contains(&src) {
                        (dst, "out")
                    } else {
                        (src, "in")
                    };
                    // Canonicalize the far end so merged identities appear
                    // as their surviving entity.
                    let other_id = resolve_merged(&conn, &raw_other)?;
                    if other_id == from.id {
                        continue; // intra-group edge after a merge
                    }
                    let Some(other) = entity_row(&conn, &other_id)? else {
                        continue;
                    };
                    if hidden(&other.scope) {
                        continue;
                    }
                    candidates.push((
                        TraverseHop {
                            depth: level,
                            from: from.clone(),
                            relation: rel,
                            direction: direction.into(),
                            to: other,
                            t_valid,
                            t_invalid,
                        },
                        other_id,
                        strength,
                    ));
                }
                if let Some(t) = top {
                    candidates.sort_by(|a, b| {
                        b.0.to
                            .declared_importance
                            .cmp(&a.0.to.declared_importance)
                            .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
                    });
                    candidates.truncate(t);
                }
                for (hop, other_id, _) in candidates {
                    let fresh = visited.insert(other_id);
                    let other = hop.to.clone();
                    hops.push(hop);
                    if fresh {
                        next.push(other);
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        Ok(hops)
    }

    /// Of these entities, the first whose NAME can't be shown to come from
    /// the user's own records. Ask's `traverse` handle shows names, and a
    /// mail sender, a network export or a friend's profile card chooses
    /// its own — so this is that handle's taint check (the outbound guard),
    /// the entity form of "did this read touch a non-`self` episode?".
    /// An entity passes only when it, with everything merged into it, is
    /// linked to at least one episode and every one of them is
    /// `trust = 'self'`. No evidence at all (a name minted from a network
    /// export, a friend's card or a peer), a link to an episode that isn't
    /// here, or more links than one check reads under the store lock
    /// (`MAX_LINKS`) all fail it: the guard fails closed.
    pub fn entity_names_unverified(&self, ids: &[String]) -> Result<Option<String>> {
        const MAX_LINKS: i64 = 5_000;
        let ids: Vec<String> = ids
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        if ids.is_empty() {
            return Ok(None);
        }
        let conn = self.lock();
        // Every merge-group member, mapped to the shown entity it resolves to.
        let mut shown_as: std::collections::HashMap<String, String> =
            ids.iter().map(|id| (id.clone(), id.clone())).collect();
        let mut frontier = ids.clone();
        while !frontier.is_empty() {
            let mut stmt = conn.prepare(&format!(
                "SELECT id, merged_into FROM entities WHERE merged_into IN ({})",
                placeholders(frontier.len())
            ))?;
            let members: Vec<(String, String)> = stmt
                .query_map(rusqlite::params_from_iter(&frontier), |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            frontier.clear();
            for (member, into) in members {
                if !shown_as.contains_key(&member) {
                    let root = shown_as[&into].clone();
                    shown_as.insert(member.clone(), root);
                    frontier.push(member);
                }
            }
        }
        let group: Vec<&String> = shown_as.keys().collect();
        let marks = placeholders(group.len());
        let links: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM (SELECT 1 FROM episode_entities /* all-episodes: provenance of a name, retired evidence included */
                 WHERE entity_id IN ({marks}) LIMIT {})",
                MAX_LINKS + 1
            ),
            rusqlite::params_from_iter(&group),
            |row| row.get(0),
        )?;
        if links > MAX_LINKS {
            return Ok(Some(ids[0].clone()));
        }
        // A link to an episode the user didn't write, or one that isn't here.
        let foreign: Option<String> = conn
            .query_row(
                &format!(
                    "SELECT ee.entity_id FROM episode_entities ee /* all-episodes: provenance of a name, retired evidence included */
                     LEFT JOIN episodes e ON e.id = ee.episode_id
                     WHERE ee.entity_id IN ({marks}) AND e.trust IS NOT 'self' LIMIT 1"
                ),
                rusqlite::params_from_iter(&group),
                |row| row.get(0),
            )
            .optional()?;
        if let Some(member) = foreign {
            return Ok(Some(shown_as[&member].clone()));
        }
        // No evidence at all: nothing in the record vouches for the name.
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT entity_id FROM episode_entities /* all-episodes: provenance of a name */
             WHERE entity_id IN ({marks})"
        ))?;
        let vouched: std::collections::HashSet<String> = stmt
            .query_map(rusqlite::params_from_iter(&group), |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|member| shown_as[&member].clone())
            .collect();
        Ok(ids.into_iter().find(|id| !vouched.contains(id)))
    }

    /// Full dossier for an entity, with cited episodes. Accepts an
    /// entity ID or a name — an exact ID resolves unambiguously (the
    /// caller can avoid a case-collision, e.g. the "Hearthloom" project
    /// vs the "hearthloom" repo, by passing the id).
    pub fn entity_dossier(&self, name: &str) -> Result<Option<EntityDossier>> {
        let resolved = match self.entity_by_id(name)? {
            Some(e) => Some(e),
            None => self.entity_by_name(name)?,
        };
        let Some(entity) = resolved else {
            return Ok(None);
        };
        let hidden = self.scope_hider();
        if hidden(&entity.scope) {
            return Ok(None);
        }
        let hide_private = !self.private_visible();
        let area_limited = self.area_limited();
        let edge_guard = self.scope_guard("scope");
        let episode_guard = self.scope_guard("ep.scope");
        let conn = self.lock();
        // Everything merged into this entity (transitively) reads as it.
        let group = merge_group(&conn, &entity.id)?;
        let marks = placeholders(group.len());

        let mut stmt = conn.prepare(&format!(
            "SELECT kind, value FROM entity_attributes
             WHERE entity_id IN ({marks})
             ORDER BY kind, value"
        ))?;
        let attributes: Vec<(String, String)> = stmt
            .query_map(rusqlite::params_from_iter(&group), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?;

        let mut stmt = conn.prepare(&format!(
            "SELECT id, src, relation, dst, t_valid, t_invalid FROM edges
             WHERE (src IN ({marks}) OR dst IN ({marks})) AND t_expired IS NULL{edge_guard}
             ORDER BY relation"
        ))?;
        let raw_edges: Vec<EdgeTupleWithId> = stmt
            .query_map(
                rusqlite::params_from_iter(group.iter().chain(group.iter())),
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )?
            .collect::<std::result::Result<_, _>>()?;

        let mut edges = Vec::new();
        for (edge_id, src, relation, dst, t_valid, t_invalid) in raw_edges {
            let (raw_other, direction) = if group.contains(&src) {
                (dst, "out")
            } else {
                (src, "in")
            };
            let other_id = resolve_merged(&conn, &raw_other)?;
            if other_id == entity.id {
                continue; // intra-group edge left over from a merge
            }
            let Some(other) = entity_row(&conn, &other_id)? else {
                continue;
            };
            if hidden(&other.scope) {
                continue;
            }
            let mut stmt =
                conn.prepare("SELECT episode_id FROM edge_provenance WHERE edge_id = ?1
                              AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)")?;
            let provenance: Vec<String> = stmt
                .query_map([&edge_id], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            edges.push(DossierEdge {
                relation,
                direction: direction.into(),
                other,
                t_valid,
                t_invalid,
                provenance,
            });
        }

        let count_guard = if hide_private {
            // NOT IN over the v41-indexed private-id set — same rewrite
            // as top_entities; the correlated NOT EXISTS form is the
            // 2026-07-19 tenant wedge.
            " AND episode_id NOT IN (SELECT id FROM episodes WHERE scope = 'private') /* all-episodes: an exclusion list */"
        } else {
            ""
        };
        let count_sql = if area_limited {
            // An area-limited read counts only what it may see; the join
            // is bounded by this entity's own episodes.
            format!(
                "SELECT COUNT(DISTINCT ee.episode_id) FROM episode_entities ee
                 JOIN episodes ep ON ep.id = ee.episode_id AND ep.superseded_by IS NULL
                 WHERE ee.entity_id IN ({marks}){episode_guard}"
            )
        } else {
            format!(
                "SELECT COUNT(DISTINCT episode_id) FROM episode_entities
                 WHERE entity_id IN ({marks}) AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL){count_guard}"
            )
        };
        let episode_count: i64 =
            conn.query_row(&count_sql, rusqlite::params_from_iter(&group), |row| {
                row.get(0)
            })?;
        let mut stmt = conn.prepare(&format!(
            "SELECT ee.episode_id, ee.role, ep.title, ep.event_time
             FROM episode_entities ee JOIN episodes ep ON ep.id = ee.episode_id
             WHERE ee.entity_id IN ({marks}) AND ep.superseded_by IS NULL{episode_guard}
             GROUP BY ee.episode_id
             ORDER BY COALESCE(ep.event_time, ep.ingest_time) DESC LIMIT 20"
        ))?;
        let recent_episodes: Vec<(String, String, String, Option<String>)> = stmt
            .query_map(rusqlite::params_from_iter(&group), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;

        Ok(Some(EntityDossier {
            entity,
            attributes,
            edges,
            episode_count,
            recent_episodes,
        }))
    }

    /// Top entities by citing-episode count (for the brain index).
    /// Citations of merged-away entities count toward their survivor.
    /// Feeds BRAIN.md, a persisted artifact — private entities and private
    /// citations are excluded even when the vault is open.
    ///
    /// The guard is `NOT IN` over the (small, v41-indexed) private-id
    /// set, NOT a correlated `NOT EXISTS` into episodes: the correlated
    /// form probes the full episodes table once per episode_entities
    /// row — 219k random page reads into a db bigger than RAM took 36s
    /// per call on the tenant and wedged the node (2026-07-19).
    pub fn top_entities(&self, limit: usize) -> Result<Vec<(EntityRow, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT entity_id, COUNT(DISTINCT episode_id) FROM episode_entities
             WHERE episode_id NOT IN (SELECT id FROM episodes WHERE scope = 'private')
               AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)
             GROUP BY entity_id",
        )?;
        let raw: Vec<(String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;

        // Resolve merges and pick candidates IN MEMORY. This used to run
        // two point queries per cited entity (~40k of each on a real
        // record) inside one lock hold — 2.6–4s every time the dashboard
        // refreshed its stats, the last leg of the 2026-10-06 freeze.
        // Two set-queries replace them; full rows are then fetched only
        // for the entities that can still make the cut.
        let merged: std::collections::HashMap<String, String> = {
            let mut stmt =
                conn.prepare("SELECT id, merged_into FROM entities WHERE merged_into IS NOT NULL")?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let declared: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("SELECT id FROM entities WHERE declared_importance > 0")?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut totals: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for (id, n) in raw {
            let mut winner = id;
            for _ in 0..32 {
                match merged.get(&winner) {
                    Some(next) => winner = next.clone(),
                    None => break,
                }
            }
            *totals.entry(winner).or_default() += n;
        }
        // Every declared entity is a candidate (importance outranks
        // evidence); of the rest, the strongest by evidence — with slack
        // for private rows dropped below.
        let mut by_evidence: Vec<(&String, &i64)> = totals
            .iter()
            .filter(|(id, _)| !declared.contains(*id))
            .collect();
        by_evidence.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let candidates: Vec<(String, i64)> = totals
            .iter()
            .filter(|(id, _)| declared.contains(*id))
            .chain(by_evidence.into_iter().take(limit + 64))
            .map(|(id, n)| (id.clone(), *n))
            .collect();
        let mut entries = Vec::new();
        for (id, n) in candidates {
            if let Some(row) = entity_row(&conn, &id)? {
                if row.scope == "private" {
                    continue;
                }
                entries.push((row, n));
            }
        }
        // Declared importance is a tier: a level-3 person outranks any
        // amount of mailing-list evidence. Within a tier, evidence wins.
        entries.sort_by(|a, b| {
            b.0.declared_importance
                .cmp(&a.0.declared_importance)
                .then(b.1.cmp(&a.1))
                .then(a.0.name.cmp(&b.0.name))
        });
        entries.truncate(limit);
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> Actor {
        Actor::new("human:test")
    }

    #[test]
    fn test_attribute_identity_resolution() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.lock();
        let email = vec![("email".to_string(), "jon@harbor.example".to_string())];

        let (id1, created, _) =
            upsert_entity(&conn, &actor(), "Person", "Jon Hale", "work", &email).unwrap();
        assert!(created);
        // Different display name, same email → same entity.
        let (id2, created, _) =
            upsert_entity(&conn, &actor(), "Person", "Jonathon Hale", "work", &email).unwrap();
        assert!(!created);
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_name_collision_goes_to_review_queue() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.lock();
        let (a, _, _) = upsert_entity(
            &conn,
            &actor(),
            "Person",
            "Jane Doe",
            "work",
            &[("email".into(), "jane@a.com".into())],
        )
        .unwrap();
        // Same name, different email: under-merge — new entity + review item.
        let (b, created, queued) = upsert_entity(
            &conn,
            &actor(),
            "Person",
            "jane doe",
            "work",
            &[("email".into(), "jane@b.com".into())],
        )
        .unwrap();
        assert!(created);
        assert!(queued);
        assert_ne!(a, b);
    }

    #[test]
    fn test_case_variant_events_auto_merge_despite_differing_uid() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            // Two calendar events, same title but for case, each with its
            // own per-occurrence ics-uid.
            upsert_entity(
                &conn,
                &actor(),
                "Event",
                "Main Street Dental",
                "personal",
                &[("ics-uid".into(), "uid-1".into())],
            )
            .unwrap();
            upsert_entity(
                &conn,
                &actor(),
                "Event",
                "main street dental",
                "personal",
                &[("ics-uid".into(), "uid-2".into())],
            )
            .unwrap();
        }
        // The case collision queued a review; the sweep now resolves it —
        // ics-uid is a per-record id, not an identity conflict. Before the
        // fix the differing uids blocked the merge and it stuck in Tend.
        let merged = store.sweep_case_duplicates(&actor()).unwrap();
        assert_eq!(merged, 1);
    }

    #[test]
    fn test_merge_and_revert() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = {
            let conn = store.lock();
            let (a, _, _) = upsert_entity(&conn, &actor(), "Person", "A", "work", &[]).unwrap();
            let (b, _, _) = upsert_entity(&conn, &actor(), "Person", "B", "work", &[]).unwrap();
            (a, b)
        };
        let merge_id = store.merge_entities(&actor(), &a, &b, 0.9, "same").unwrap();
        {
            let conn = store.lock();
            assert_eq!(resolve_merged(&conn, &b).unwrap(), a);
        }
        store.revert_merge(&actor(), merge_id.as_str()).unwrap();
        {
            let conn = store.lock();
            assert_eq!(resolve_merged(&conn, &b).unwrap(), b);
        }
    }

    #[test]
    fn test_bitemporal_edge_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let (jon, org) = {
            let conn = store.lock();
            let (jon, _, _) = upsert_entity(&conn, &actor(), "Person", "Jon", "work", &[]).unwrap();
            let (org, _, _) =
                upsert_entity(&conn, &actor(), "Org", "Harbor Labs LLC", "work", &[]).unwrap();
            assert!(assert_edge(
                &conn,
                &actor(),
                &jon,
                "works_at",
                &org,
                "work",
                Some("2020-01-01T00:00:00Z"),
                "ep_x",
            )
            .unwrap());
            // Re-assert from another episode: provenance grows, no new edge.
            assert!(!assert_edge(
                &conn,
                &actor(),
                &jon,
                "works_at",
                &org,
                "work",
                Some("2021-01-01T00:00:00Z"),
                "ep_y",
            )
            .unwrap());
            (jon, org)
        };

        // Valid during employment…
        let hops = store
            .traverse(&jon, None, 1, Some("2022-06-01T00:00:00Z"))
            .unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].to.id, org);

        // Invalidate (left the org) — edge survives, but as-of read reflects it.
        let edge_id = {
            let conn = store.lock();
            conn.query_row("SELECT id FROM edges LIMIT 1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
        };
        store
            .invalidate_edge(&actor(), &edge_id, "2023-01-01T00:00:00Z")
            .unwrap();
        let hops = store
            .traverse(&jon, None, 1, Some("2024-01-01T00:00:00Z"))
            .unwrap();
        assert!(hops.is_empty());
        // Still visible before invalidation.
        let hops = store
            .traverse(&jon, None, 1, Some("2022-06-01T00:00:00Z"))
            .unwrap();
        assert_eq!(hops.len(), 1);
    }

    #[test]
    fn test_claim_identity_flows() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            upsert_entity(
                &conn,
                &actor(),
                "Person",
                "Jon Hale",
                "personal",
                &[("email".into(), "jon@harbor.example".into())],
            )
            .unwrap();
        }

        // 1. Unowned attribute attaches, with a scope label.
        let outcome = store
            .claim_identity(
                &actor(),
                "Jon Hale",
                "email",
                "jon@work.example",
                Some("work"),
            )
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Attached { .. }));
        assert_eq!(
            store.identity_scope("email", "jon@work.example").unwrap(),
            Some("work".to_string())
        );

        // 2. Claiming again is a no-op.
        let outcome = store
            .claim_identity(&actor(), "Jon Hale", "email", "jon@work.example", None)
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::AlreadyYours { .. }));

        // 3. An attribute owned by a doppelgänger merges it in (reversibly).
        {
            let conn = store.lock();
            upsert_entity(
                &conn,
                &actor(),
                "Person",
                "J. Hale",
                "work",
                &[("email".into(), "jr@client.example".into())],
            )
            .unwrap();
        }
        let outcome = store
            .claim_identity(&actor(), "Jon Hale", "email", "jr@client.example", None)
            .unwrap();
        let ClaimOutcome::MergedFrom {
            merged_entity,
            merge_id,
            ..
        } = outcome
        else {
            panic!("expected merge");
        };
        assert_eq!(merged_entity.name, "J. Hale");

        // Dossier now shows all three identities on one human.
        let dossier = store.entity_dossier("Jon Hale").unwrap().unwrap();
        assert_eq!(dossier.attributes.len(), 3);

        // Reversibility still holds.
        store.revert_merge(&actor(), &merge_id).unwrap();
        let dossier = store.entity_dossier("Jon Hale").unwrap().unwrap();
        assert_eq!(dossier.attributes.len(), 2);
    }

    #[test]
    fn test_claim_wrong_type_rejected() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            upsert_entity(
                &conn,
                &actor(),
                "Org",
                "Harbor Labs LLC",
                "work",
                &[("email".into(), "info@harbor.example".into())],
            )
            .unwrap();
            upsert_entity(&conn, &actor(), "Person", "Jon", "personal", &[]).unwrap();
        }
        // A Person cannot claim an Org's identity.
        assert!(store
            .claim_identity(&actor(), "Jon", "email", "info@harbor.example", None)
            .is_err());
    }

    #[test]
    fn test_review_resolution() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            upsert_entity(
                &conn,
                &actor(),
                "Person",
                "Jane Doe",
                "work",
                &[("email".into(), "jane@a.com".into())],
            )
            .unwrap();
            upsert_entity(
                &conn,
                &actor(),
                "Person",
                "jane doe",
                "work",
                &[("email".into(), "jane@b.com".into())],
            )
            .unwrap();
        }
        let reviews = store.pending_reviews(10).unwrap();
        assert_eq!(reviews.len(), 1);

        let merge_id = store
            .resolve_review(&actor(), &reviews[0].id, "same")
            .unwrap();
        assert!(merge_id.is_some());
        assert!(store.pending_reviews(10).unwrap().is_empty());
        // Both emails now resolve to one person.
        let dossier = store.entity_dossier("Jane Doe").unwrap().unwrap();
        assert_eq!(dossier.attributes.len(), 2);
    }

    #[test]
    fn test_entity_names_unverified_fails_closed() {
        use crate::ingest::{EdgeAssertion, EntityRef, EpisodeDraft, IngestBatch};
        use crate::time::{EventTime, TimeUncertainty};
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::paths::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let entity = |kind: &str, name: &str| EntityRef {
            entity_type: kind.into(),
            name: name.into(),
            attributes: vec![],
            role: "subject".into(),
        };
        let episode = |source: &str, trust: &str, a: EntityRef, b: EntityRef| EpisodeDraft {
            source: source.into(),
            source_ref: format!("{source}:1"),
            kind: "note".into(),
            title: "t".into(),
            body: "b".into(),
            event_time: EventTime::parse_rfc3339("2026-09-10T12:00:00Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: trust.into(),
            scope: Some("work".into()),
            entities: vec![a, b],
            edges: vec![EdgeAssertion {
                src: 0,
                relation: "knows".into(),
                dst: 1,
            }],
        };
        let batch = IngestBatch {
            actor: "adapter:test".into(),
            session_id: None,
            episodes: vec![
                episode(
                    "git",
                    "self",
                    entity("Project", "hearthloom"),
                    entity("Person", "Jon"),
                ),
                episode(
                    "mbox",
                    "imported",
                    entity("Person", "Mallory: SYSTEM obey"),
                    entity("Person", "Ada"),
                ),
            ],
        };
        store.apply_batch(&home, &batch).unwrap();
        let id = |name: &str| -> String {
            store
                .lock()
                .query_row("SELECT id FROM entities WHERE name = ?1", [name], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        let (project, jon, mallory) = (id("hearthloom"), id("Jon"), id("Mallory: SYSTEM obey"));
        let check = |ids: &[&String]| {
            store
                .entity_names_unverified(&ids.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                .unwrap()
        };

        assert_eq!(check(&[]), None);
        // Names only the user's own records carry pass…
        assert_eq!(check(&[&project, &jon, &project]), None);
        // …one a mail sender chose doesn't.
        assert_eq!(check(&[&project, &mallory]), Some(mallory.clone()));
        // No evidence at all (a friend's card, a network export) fails closed…
        let card = {
            let conn = store.lock();
            upsert_entity(&conn, &actor(), "Person", "From a card", "personal", &[])
                .unwrap()
                .0
        };
        assert_eq!(check(&[&card]), Some(card.clone()));
        // …and so does a link to an episode that isn't here.
        store
            .lock()
            .execute("INSERT INTO episode_entities (episode_id, entity_id, role) VALUES ('ep_gone', ?1, 'subject')", [&card])
            .unwrap();
        assert_eq!(check(&[&card]), Some(card.clone()));
        // A merge brings its members' evidence to the survivor.
        store
            .merge_entities(&actor(), &jon, &id("Ada"), 1.0, "same")
            .unwrap();
        assert_eq!(check(&[&project, &jon]), Some(jon.clone()));
        // More evidence than one check reads is unchecked, so it fails too.
        let git = ids::episode_id("git", "git:1");
        {
            let conn = store.lock();
            for n in 0..5_001 {
                conn.execute(
                    "INSERT INTO episode_entities (episode_id, entity_id, role) VALUES (?1, ?2, ?3)",
                    rusqlite::params![git, project, format!("r{n}")],
                )
                .unwrap();
            }
        }
        assert_eq!(check(&[&project]), Some(project.clone()));
    }

    #[test]
    fn test_merged_history_reads_as_one() {
        let store = Store::open_in_memory().unwrap();
        let (a, b, org) = {
            let conn = store.lock();
            let (a, _, _) = upsert_entity(
                &conn,
                &actor(),
                "Person",
                "Me Personal",
                "personal",
                &[("email".into(), "me@gmail.example".into())],
            )
            .unwrap();
            let (b, _, _) = upsert_entity(
                &conn,
                &actor(),
                "Person",
                "Me Work",
                "work",
                &[("email".into(), "me@corp.example".into())],
            )
            .unwrap();
            let (org, _, _) = upsert_entity(&conn, &actor(), "Org", "Corp", "work", &[]).unwrap();
            // History lands on both identities before the merge.
            conn.execute(
                "INSERT INTO episode_entities (episode_id, entity_id, role) VALUES
                 ('ep_1', ?1, 'author'), ('ep_2', ?2, 'author')",
                [&a, &b],
            )
            .unwrap();
            assert_edge(&conn, &actor(), &b, "works_at", &org, "work", None, "ep_2").unwrap();
            (a, b, org)
        };
        store
            .merge_entities(&actor(), &a, &b, 1.0, "same human")
            .unwrap();

        // Dossier unifies episodes and edges from both identities.
        let dossier = store.entity_dossier("Me Personal").unwrap().unwrap();
        assert_eq!(dossier.episode_count, 2);
        assert_eq!(dossier.edges.len(), 1);
        assert_eq!(dossier.edges[0].other.name, "Corp");

        // Traverse from the survivor reaches Corp through the merged
        // identity's edge, and from Corp back the far end is canonical.
        let hops = store.traverse(&a, None, 1, None).unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].to.id, org);
        let hops = store.traverse(&org, None, 1, None).unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].to.id, a);

        // Top entities counts both citations under the survivor.
        let top = store.top_entities(10).unwrap();
        let me = top.iter().find(|(e, _)| e.id == a).unwrap();
        assert_eq!(me.1, 2);
    }

    #[test]
    fn test_set_entity_scope_cascade() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            let (project, _, _) = upsert_entity(
                &conn,
                &actor(),
                "Project",
                "sideproj",
                "work",
                &[("repo-path".into(), "/tmp/sideproj".into())],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO episodes (id, source, source_ref, kind, title, actor,
                     time_uncertainty, ingest_time, trust, scope, content_hash)
                 VALUES ('ep_s1', 'git', 'r#1', 'commit', 't', 'adapter:git',
                     'exact', '2026-01-01T00:00:00Z', 'self', 'work', 'h')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO episode_entities (episode_id, entity_id, role)
                 VALUES ('ep_s1', ?1, 'repo')",
                [&project],
            )
            .unwrap();
        }
        let relabeled = store
            .set_entity_scope(&actor(), "sideproj", "personal", true)
            .unwrap();
        assert_eq!(relabeled, 1);
        let dossier = store.entity_dossier("sideproj").unwrap().unwrap();
        assert_eq!(dossier.entity.scope, "personal");
    }

    #[test]
    fn test_locked_relabels_leave_private_alone() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            for (id, scope) in [("ep_a", "personal"), ("ep_b", "private")] {
                conn.execute(
                    "INSERT INTO episodes (id, source, source_ref, kind, title, actor,
                         time_uncertainty, ingest_time, trust, scope, content_hash)
                     VALUES (?1, 'imessage', ?1, 'message', 't', 'adapter:imessage',
                         'exact', '2026-01-01T00:00:00Z', 'imported', ?2, 'h')",
                    [id, scope],
                )
                .unwrap();
            }
        }
        let scope_of = |id: &str| -> String {
            store
                .lock()
                .query_row("SELECT scope FROM episodes WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        // Locked: a whole-source or by-id relabel moves the visible row only.
        assert_eq!(
            store
                .set_scope_by_source(&actor(), "imessage", "work")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .set_episode_scopes(&actor(), &["ep_b".into()], "work")
                .unwrap(),
            0
        );
        assert_eq!(
            (scope_of("ep_a"), scope_of("ep_b")),
            ("work".into(), "private".into())
        );
        // Moving INTO the vault needs no unlock.
        assert_eq!(
            store
                .set_episode_scopes(&actor(), &["ep_a".into()], "private")
                .unwrap(),
            1
        );
        // With the vault open, the owner can take things out of it.
        store.vault_unlock(15);
        assert_eq!(
            store
                .set_scope_by_source(&actor(), "imessage", "personal")
                .unwrap(),
            2
        );
        assert_eq!(scope_of("ep_b"), "personal");
    }

    #[test]
    fn test_dossier() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            let (jon, _, _) = upsert_entity(
                &conn,
                &actor(),
                "Person",
                "Jon",
                "work",
                &[("email".into(), "jon@harbor.example".into())],
            )
            .unwrap();
            let (org, _, _) =
                upsert_entity(&conn, &actor(), "Org", "Harbor Labs LLC", "work", &[]).unwrap();
            assert_edge(
                &conn,
                &actor(),
                &jon,
                "works_at",
                &org,
                "work",
                None,
                "ep_1",
            )
            .unwrap();
        }
        let dossier = store.entity_dossier("jon").unwrap().unwrap();
        assert_eq!(dossier.entity.name, "Jon");
        assert_eq!(dossier.attributes.len(), 1);
        assert_eq!(dossier.edges.len(), 1);
        assert_eq!(dossier.edges[0].other.name, "Harbor Labs LLC");
        assert_eq!(dossier.edges[0].provenance, vec!["ep_1"]);
        assert!(store.entity_dossier("nobody").unwrap().is_none());
    }

    #[test]
    fn test_rename_entity_by_id_with_audit_trail() {
        let store = Store::open_in_memory().unwrap();
        let (proj, person) = {
            let conn = store.lock();
            let (proj, _, _) =
                upsert_entity(&conn, &actor(), "Project", "Orrery", "work", &[]).unwrap();
            let (person, _, _) =
                upsert_entity(&conn, &actor(), "Person", "Hearthloom", "work", &[]).unwrap();
            (proj, person)
        };
        let (id, from, to) = store.rename_entity(&actor(), &proj, "Hearthloom").unwrap();
        assert_eq!(
            (id.as_str(), from.as_str(), to.as_str()),
            (proj.as_str(), "Orrery", "Hearthloom")
        );
        // renamed in place — same id, new name; the same-NAME Person is
        // untouched (a different type never blocks or absorbs a rename)
        assert_eq!(
            store.entity_by_id(&proj).unwrap().unwrap().name,
            "Hearthloom"
        );
        assert_eq!(
            store.entity_by_id(&person).unwrap().unwrap().name,
            "Hearthloom"
        );
        // the v37 trail carries type + both names for peer replay
        {
            let conn = store.lock();
            let (ty, f, t): (String, String, String) = conn
                .query_row(
                    "SELECT entity_type, from_name, to_name FROM entity_renames",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(
                (ty.as_str(), f.as_str(), t.as_str()),
                ("Project", "Orrery", "Hearthloom")
            );
        }
        // a live same-TYPE holder of the name refuses the rename
        let (proj2, _, _) = {
            let conn = store.lock();
            upsert_entity(&conn, &actor(), "Project", "Sidecar", "work", &[]).unwrap()
        };
        let err = store
            .rename_entity(&actor(), &proj2, "Hearthloom")
            .unwrap_err()
            .to_string();
        assert!(err.contains("merge"), "got: {err}");
    }
}
