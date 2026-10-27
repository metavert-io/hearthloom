//! Relationships: how the human relates to the people and orgs that
//! matter — Orrery's heart, rebuilt on real history. Kinds are
//! PREDICTED at idle from episode evidence (origin=inferred →
//! status=proposed) and confirmed by the human, or declared outright;
//! confirmation never launders origin. Endings set t_invalid — history,
//! not deletion.
//!
//! The directory tiers people by lived evidence, not by list-keeping:
//!   inner    — declared importance, or a confirmed close kind
//!   active   — episode evidence in the last 90 days
//!   drifting — evidence in the year but silent for 90+ days
//! Everyone else stays behind search (zero gardening: the tiers derive
//! themselves).

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::graph::resolve_merged;
use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::time::now_utc;

pub const RELATIONSHIP_KINDS: &[&str] = &[
    "partner",
    "parent",
    "child",
    "family",
    "friend",
    "colleague",
    "collaborator",
    "client",
    "investor",
    "advisor",
    "contact",
    "acquaintance",
    "service",
    "me",
];

/// Kinds that make someone inner-circle once confirmed. A FORMER
/// partner/spouse is the same kind with status=ended (declared via the
/// "was…" group in the UI) — bi-temporal history, not a separate kind.
/// `contact` is the professional catch-all (your accountant, a
/// client) — pair it with the freeform note qualifier, never a new kind.
const CLOSE_KINDS: &[&str] = &[
    "partner",
    "parent",
    "child",
    "friend",
    "family",
    "collaborator",
    "advisor",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationshipRow {
    pub id: String,
    pub entity_id: String,
    pub kind: String,
    pub origin: String,
    pub status: String,
    pub evidence: Vec<String>,
    pub note: Option<String>,
    pub t_valid: Option<String>,
    pub t_invalid: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn relationship_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RelationshipRow> {
    let evidence: Option<String> = row.get("evidence")?;
    Ok(RelationshipRow {
        id: row.get("id")?,
        entity_id: row.get("entity_id")?,
        kind: row.get("kind")?,
        origin: row.get("origin")?,
        status: row.get("status")?,
        evidence: evidence
            .and_then(|e| serde_json::from_str(&e).ok())
            .unwrap_or_default(),
        note: row.get("note")?,
        t_valid: row.get("t_valid")?,
        t_invalid: row.get("t_invalid")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const REL_COLUMNS: &str = "id, entity_id, kind, origin, status, evidence, note, \
     t_valid, t_invalid, created_at, updated_at";

/// One directory entry: an entity plus everything the view needs.
#[derive(Debug, Clone, Serialize)]
pub struct DirectoryRow {
    pub entity_id: String,
    pub name: String,
    pub entity_type: String,
    pub declared_importance: i64,
    pub org: Option<String>,
    /// Best identifying attribute (email first, else phone) — "which
    /// Maria is this?" disambiguation for same-named people.
    pub handle: Option<String>,
    pub last_evidence: Option<String>,
    pub evidence_year: i64,
    /// 12 monthly evidence counts, oldest first (the sparkline).
    pub months: Vec<i64>,
    pub relationships: Vec<RelationshipRow>,
    /// Names of active goals this person keeps appearing around.
    pub goals: Vec<String>,
    /// Cited by an episode that matched a confirmed `seeking`.
    pub seeking_match: bool,
    /// Notes @-mentioning this entity (the paper trail).
    pub notes: i64,
    pub tier: String, // self | inner | active | drifting
}

impl Store {
    /// Declare or propose a relationship. Declared → confirmed on the
    /// spot; inferred → proposed for the human. Re-proposing an existing
    /// (entity, kind) is a no-op returning the existing id — rejected
    /// verdicts stay rejected (the merge-queue discipline).
    #[allow(clippy::too_many_arguments)]
    pub fn add_relationship(
        &self,
        actor: &Actor,
        entity_id: &str,
        kind: &str,
        origin: &str,
        evidence: &[String],
        note: Option<&str>,
        t_valid: Option<&str>,
    ) -> Result<String> {
        if !RELATIONSHIP_KINDS.contains(&kind) {
            return Err(anyhow!("unknown relationship kind {kind:?}"));
        }
        if !matches!(origin, "declared" | "inferred") {
            return Err(anyhow!("origin must be declared|inferred"));
        }
        if origin == "inferred" && evidence.is_empty() {
            return Err(anyhow!("inferred relationships require evidence"));
        }
        let conn = self.lock();
        let entity_id = resolve_merged(&conn, entity_id)?;
        if let Some(existing) = conn
            .query_row(
                "SELECT id FROM relationships WHERE entity_id = ?1 AND kind = ?2",
                [&entity_id, kind],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(existing);
        }
        let id = ids::relationship_id();
        let now = now_utc();
        let (status, t_valid_final) = if origin == "declared" {
            ("confirmed", t_valid.map(String::from).or(Some(now.clone())))
        } else {
            ("proposed", t_valid.map(String::from))
        };
        conn.execute(
            "INSERT INTO relationships (id, entity_id, kind, origin, status,
                 evidence, note, t_valid, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
            rusqlite::params![
                id,
                entity_id,
                kind,
                origin,
                status,
                serde_json::to_string(evidence)?,
                note,
                t_valid_final,
                now,
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "relationship_added",
            &format!("relationship_added:{id}"),
            &json!({"id": id, "entity": entity_id, "kind": kind, "origin": origin}),
        )?;
        Ok(id)
    }

    /// confirm | reject (proposed) | end (confirmed → t_invalid now).
    pub fn resolve_relationship(&self, actor: &Actor, id: &str, action: &str) -> Result<()> {
        let now = now_utc();
        let conn = self.lock();
        let changed = match action {
            "confirm" => conn.execute(
                "UPDATE relationships SET status = 'confirmed',
                     t_valid = COALESCE(t_valid, ?1), updated_at = ?1
                 WHERE id = ?2 AND status = 'proposed'",
                rusqlite::params![now, id],
            )?,
            "reject" => conn.execute(
                "UPDATE relationships SET status = 'rejected', updated_at = ?1
                 WHERE id = ?2 AND status = 'proposed'",
                rusqlite::params![now, id],
            )?,
            "end" => conn.execute(
                "UPDATE relationships SET status = 'ended', t_invalid = ?1, updated_at = ?1
                 WHERE id = ?2 AND status = 'confirmed'",
                rusqlite::params![now, id],
            )?,
            other => return Err(anyhow!("action must be confirm|reject|end, got {other:?}")),
        };
        if changed == 0 {
            return Err(anyhow!("no relationship {id} eligible for {action}"));
        }
        record_mutation(
            &conn,
            actor,
            "relationship_resolved",
            &format!("relationship_resolved:{id}:{action}"),
            &json!({"id": id, "action": action}),
        )?;
        Ok(())
    }

    /// Relationships for one entity (merge-group aware). Vault-guarded
    /// via the entity's scope.
    pub fn relationships_for(&self, entity_id: &str) -> Result<Vec<RelationshipRow>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let winner = resolve_merged(&conn, entity_id)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM relationships r
             JOIN entities e ON e.id = r.entity_id
             WHERE r.entity_id = ?1{guard}
             ORDER BY r.status, r.kind",
            REL_COLUMNS
                .split(", ")
                .map(|c| format!("r.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let rows = stmt.query_map([&winner], relationship_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Pending predictions count (for surfacing, not the Tend badge —
    /// the directory itself is the confirmation surface).
    pub fn pending_relationships(&self) -> Result<i64> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM relationships r
                 JOIN entities e ON e.id = r.entity_id
                 WHERE r.status = 'proposed'{guard}"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Persons with recent evidence but NO relationship row yet — the
    /// prediction pass's feed, best-evidenced first. Non-private only
    /// (this feeds a cloud LLM).
    pub fn persons_needing_prediction(&self, limit: usize) -> Result<Vec<(String, String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT en.id, en.name, COUNT(DISTINCT ee.episode_id) AS n
             FROM entities en
             JOIN episode_entities ee ON ee.entity_id = en.id
             JOIN episodes ep ON ep.id = ee.episode_id
             WHERE en.type = 'Person' AND en.merged_into IS NULL
               AND en.scope != 'private' AND ep.scope != 'private'
               AND ep.superseded_by IS NULL
               AND substr(COALESCE(ep.event_time, ep.ingest_time), 1, 10)
                   >= date('now', '-365 days')
               AND en.id NOT IN (SELECT entity_id FROM relationships)
             GROUP BY en.id
             HAVING n >= 8
             ORDER BY n DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Evidence brief for the predictor: sources/kinds breakdown, org
    /// edges, and sample titles for one person. Explicit non-private.
    pub fn person_evidence_brief(&self, entity_id: &str) -> Result<(Vec<String>, Vec<String>)> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ep.source || '/' || ep.kind, COUNT(*) FROM episode_entities ee
             JOIN episodes ep ON ep.id = ee.episode_id
             WHERE ee.entity_id = ?1 AND ep.scope != 'private' AND ep.superseded_by IS NULL
             GROUP BY 1 ORDER BY 2 DESC LIMIT 6",
        )?;
        let mix: Vec<String> = stmt
            .query_map([entity_id], |row| {
                Ok(format!(
                    "{} ×{}",
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT ep.id || ' ' || substr(COALESCE(ep.event_time, ep.ingest_time), 1, 10)
                    || ' ' || ep.title
             FROM episode_entities ee
             JOIN episodes ep ON ep.id = ee.episode_id
             WHERE ee.entity_id = ?1 AND ep.scope != 'private' AND ep.superseded_by IS NULL
             ORDER BY COALESCE(ep.event_time, ep.ingest_time) DESC LIMIT 8",
        )?;
        let samples: Vec<String> = stmt
            .query_map([entity_id], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok((mix, samples))
    }

    /// Detect the record's OWNER — self-evident, never declared: the
    /// Person present across ≥3 distinct sources holding a dominant
    /// share of all episodes (nobody else can be the recipient of your
    /// mail AND the author of your commits AND the holder of your
    /// photos). Deterministic — no LLM. Proposes kind='me' once; the
    /// human confirms. Returns true if a proposal was made.
    pub fn propose_me(&self, actor: &Actor) -> Result<bool> {
        {
            let conn = self.lock();
            let existing: i64 = conn.query_row(
                "SELECT COUNT(*) FROM relationships WHERE kind = 'me'",
                [],
                |row| row.get(0),
            )?;
            if existing > 0 {
                return Ok(false);
            }
        }
        let candidate: Option<(String, i64, i64, i64)> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT en.id, COUNT(DISTINCT ee.episode_id) AS n,
                        COUNT(DISTINCT ep.source) AS sources,
                        (SELECT COUNT(*) FROM episodes WHERE superseded_by IS NULL) AS total
                 FROM entities en
                 JOIN episode_entities ee ON ee.entity_id = en.id
                 JOIN episodes ep ON ep.id = ee.episode_id AND ep.superseded_by IS NULL
                 WHERE en.type = 'Person' AND en.merged_into IS NULL
                 GROUP BY en.id ORDER BY n DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
        };
        let Some((id, n, sources, total)) = candidate else {
            return Ok(false);
        };
        if sources < 3 || total == 0 || (n as f64) / (total as f64) < 0.25 {
            return Ok(false);
        }
        let evidence: Vec<String> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT DISTINCT episode_id FROM episode_entities
                 WHERE entity_id = ?1 AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL) LIMIT 3",
            )?;
            let rows = stmt.query_map([&id], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        self.add_relationship(actor, &id, "me", "inferred", &evidence, None, None)?;
        Ok(true)
    }

    /// Inner-circle people whose relationship brief is missing or stale
    /// (>60 days) — the brief writer's feed. Non-private (cloud pass).
    /// The owner's name as the record knows it: the entity holding the
    /// `me` relationship (confirmed or inferred by the me detector). None
    /// until the record has worked out who "you" are.
    pub fn owner_name(&self) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT en.name FROM relationships r JOIN entities en ON en.id = r.entity_id
             WHERE r.kind = 'me' AND en.merged_into IS NULL
             ORDER BY (r.status = 'confirmed') DESC, r.updated_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn persons_needing_brief(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT en.id, en.name FROM entities en
             WHERE en.merged_into IS NULL AND en.scope != 'private'
               AND en.type IN ('Person', 'Org')
               AND (en.declared_importance > 0 OR en.id IN (
                    SELECT entity_id FROM relationships
                    WHERE status = 'confirmed'
                      AND kind IN ('friend', 'family', 'collaborator', 'advisor')))
               AND en.id NOT IN (SELECT entity_id FROM relationships WHERE kind = 'me')
               AND NOT EXISTS ( /* all-episodes: a retired brief still counts as written */
                    SELECT 1 FROM episodes b
                    WHERE b.kind = 'relationship_brief'
                      AND b.source_ref LIKE 'brief:' || en.id || ':%'
                      AND b.ingest_time >= datetime('now', '-60 days'))
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The latest relationship brief for an entity (id + body).
    /// Vault-guarded like every read.
    pub fn latest_brief(&self, entity_id: &str) -> Result<Option<(String, String)>> {
        let guard = self.scope_guard("scope");
        let id: Option<String> = {
            let conn = self.lock();
            conn.query_row(
                &format!(
                    "SELECT id FROM episodes
                     WHERE kind = 'relationship_brief' AND superseded_by IS NULL
                       AND source_ref LIKE 'brief:' || ?1 || ':%'{guard}
                     ORDER BY ingest_time DESC LIMIT 1"
                ),
                [entity_id],
                |row| row.get(0),
            )
            .optional()?
        };
        match id {
            Some(id) => Ok(self.episode_body(&id)?.map(|body| (id, body))),
            None => Ok(None),
        }
    }

    /// The directory: tiered people/orgs with evidence, relationships,
    /// goal gravity and seeking matches. Vault-guarded throughout.
    /// `entity_type` narrows to Person or Org — orgs have real evidence
    /// but people's citation volumes crowd them out of shared tier caps,
    /// so the org view gets its own tiers.
    pub fn relationship_directory(
        &self,
        limit_per_tier: usize,
        entity_type: Option<&str>,
    ) -> Result<Vec<DirectoryRow>> {
        let guard = self.scope_guard("en.scope");
        let episode_guard = self.scope_guard("ep.scope");
        let conn = self.lock();

        // Evidence aggregate over the last year. Drive from the DATED
        // episodes (idx_episodes_event_time, ~13k rows) INTO the link
        // table, not entities-first — the planner's choice walks every
        // episode_entities row of every person (200k+ at mail scale,
        // ~2s); CROSS JOIN pins the cheap order (~0.2s warm).
        let mut stmt = conn.prepare(&format!(
            "WITH recent AS MATERIALIZED (
                 SELECT ep.id, substr(COALESCE(ep.event_time, ep.ingest_time), 1, 10) AS day
                 FROM episodes ep
                 WHERE (ep.event_time >= datetime('now', '-365 days')
                        OR (ep.event_time IS NULL
                            AND ep.ingest_time >= datetime('now', '-365 days')))
                   AND ep.superseded_by IS NULL{episode_guard}
             )
             SELECT en.id, en.name, en.type, en.declared_importance,
                    MAX(r.day) AS last_day,
                    COUNT(DISTINCT ee.episode_id) AS n,
                    GROUP_CONCAT(DISTINCT substr(r.day, 1, 7))
             FROM recent r
             CROSS JOIN episode_entities ee ON ee.episode_id = r.id
             CROSS JOIN entities en ON en.id = ee.entity_id
             WHERE en.type IN ({types}) AND en.merged_into IS NULL{guard}
             GROUP BY en.id",
            types = match entity_type {
                Some("Person") => "'Person'",
                Some("Org") => "'Org', 'Group'",
                _ => "'Person', 'Org'",
            },
        ))?;
        type Agg = (
            String,
            String,
            String,
            i64,
            Option<String>,
            i64,
            Option<String>,
        );
        let aggregates: Vec<Agg> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;

        // Relationship rows keyed by entity.
        let mut rel_map: std::collections::HashMap<String, Vec<RelationshipRow>> =
            std::collections::HashMap::new();
        {
            // Ended relationships stay visible ("was partner") — the
            // bi-temporal record showing, not a deletion.
            let mut stmt = conn.prepare(&format!(
                "SELECT {REL_COLUMNS} FROM relationships
                 WHERE status IN ('proposed', 'confirmed', 'ended')
                 ORDER BY status"
            ))?;
            let rows = stmt.query_map([], relationship_from_row)?;
            for row in rows {
                let row = row?;
                rel_map.entry(row.entity_id.clone()).or_default().push(row);
            }
        }

        // Goal gravity: persons repeatedly cited by an active goal's
        // catches (≥3 appearances).
        let mut goal_map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT ee.entity_id, i.name, COUNT(*) AS n
                 FROM intent_matches m
                 JOIN intents i ON i.id = m.intent_id AND i.status = 'active'
                 JOIN episode_entities ee ON ee.episode_id = m.episode_id
                 WHERE m.episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)
                 GROUP BY ee.entity_id, i.id HAVING n >= 3",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (entity, goal) = row?;
                goal_map.entry(entity).or_default().push(goal);
            }
        }

        // Seeking matches: persons cited on opportunity evidence.
        let seeking_set: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT ee.entity_id FROM opportunities o
                 JOIN episode_entities ee ON ee.episode_id = o.episode_id
                 WHERE o.status IN ('pending', 'kept') AND o.episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)",
            )?;
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        // Notes mentioning each entity (the @[Name] link table).
        let note_map: std::collections::HashMap<String, i64> = {
            let mut stmt = conn.prepare(
                "SELECT m.entity_id, COUNT(DISTINCT m.note_id)
                 FROM note_mentions m
                 JOIN notes_index n ON n.id = m.note_id
                 WHERE n.status != 'deleted'
                 GROUP BY m.entity_id",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        // Identifying handle (email > phone): the "which Maria?"
        // disambiguator. One grouped pass over the indexed attributes.
        let attr_guard = self.scope_guard("COALESCE(scope, '')");
        let handle_map: std::collections::HashMap<String, String> = {
            let mut stmt = conn.prepare(&format!(
                "SELECT entity_id,
                        COALESCE(MIN(CASE WHEN kind = 'email' THEN value END),
                                 MIN(value))
                 FROM entity_attributes
                 WHERE kind IN ('email', 'phone'){attr_guard}
                 GROUP BY entity_id"
            ))?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        // Org affiliation: strongest current works_at/member_of edge.
        let mut org_map: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT ed.src, org.name FROM edges ed
                 JOIN entities org ON org.id = ed.dst AND org.type = 'Org'
                 WHERE ed.t_expired IS NULL AND ed.t_invalid IS NULL
                   AND org.scope != 'private'
                 ORDER BY ed.strength",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (src, org) = row?; // strongest last wins the slot
                org_map.insert(src, org);
            }
        }

        let today = now_utc()[..10].to_string();
        let cutoff_90 = chrono::Utc::now() - chrono::Duration::days(90);
        let cutoff_90 = cutoff_90.format("%Y-%m-%d").to_string();
        let mut rows: Vec<DirectoryRow> = aggregates
            .into_iter()
            .map(
                |(id, name, entity_type, importance, last_day, n, months_raw)| {
                    let relationships = rel_map.remove(&id).unwrap_or_default();
                    let close_confirmed = relationships
                        .iter()
                        .any(|r| r.status == "confirmed" && CLOSE_KINDS.contains(&r.kind.as_str()));
                    let active = last_day
                        .as_deref()
                        .map(|d| d >= cutoff_90.as_str())
                        .unwrap_or(false);
                    let is_me = relationships.iter().any(|r| r.kind == "me");
                    let tier = if is_me {
                        "self" // the record's owner — a header card, not a tier
                    } else if importance > 0 || close_confirmed {
                        "inner"
                    } else if active {
                        "active"
                    } else {
                        "drifting"
                    };
                    // sparkline: 12 monthly buckets from the concat months
                    let mut months = vec![0i64; 12];
                    if let Some(raw) = &months_raw {
                        for month in raw.split(',') {
                            if let Ok(diff) = month_diff(month, &today) {
                                if diff < 12 {
                                    months[11 - diff as usize] += 1;
                                }
                            }
                        }
                    }
                    DirectoryRow {
                        notes: note_map.get(&id).copied().unwrap_or(0),
                        seeking_match: seeking_set.contains(&id),
                        goals: goal_map.get(&id).cloned().unwrap_or_default(),
                        org: org_map.get(&id).cloned(),
                        handle: handle_map.get(&id).cloned(),
                        entity_id: id,
                        name,
                        entity_type,
                        declared_importance: importance,
                        last_evidence: last_day,
                        evidence_year: n,
                        months,
                        relationships,
                        tier: tier.into(),
                    }
                },
            )
            .collect();

        // Tier order, then evidence; cap per tier.
        let tier_rank = |t: &str| match t {
            "self" => 0,
            "inner" => 1,
            "active" => 2,
            _ => 3,
        };
        rows.sort_by(|a, b| {
            tier_rank(&a.tier)
                .cmp(&tier_rank(&b.tier))
                .then(b.declared_importance.cmp(&a.declared_importance))
                .then(b.evidence_year.cmp(&a.evidence_year))
        });
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        rows.retain(|row| {
            let seen = counts.entry(row.tier.clone()).or_default();
            *seen += 1;
            *seen <= limit_per_tier
        });
        Ok(rows)
    }
}

/// Whole months between "YYYY-MM…" and "YYYY-MM-DD" (today).
fn month_diff(month: &str, today: &str) -> Result<i64> {
    let ym = |s: &str| -> Result<i64> {
        let y: i64 = s.get(0..4).unwrap_or("0").parse()?;
        let m: i64 = s.get(5..7).unwrap_or("1").parse()?;
        Ok(y * 12 + m)
    };
    Ok((ym(today)? - ym(month)?).max(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EntityRef, EpisodeDraft, IngestBatch};
    use crate::paths::Home;
    use crate::time::EventTime;

    fn actor() -> Actor {
        Actor::new("human:test")
    }

    #[test]
    fn test_relationship_lifecycle_and_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        // Ada shows up in recent episodes.
        let day = crate::time::now_utc()[..10].to_string();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![EpisodeDraft {
                        source: "test".into(),
                        source_ref: "r1".into(),
                        kind: "note".into(),
                        title: "call with Ada".into(),
                        body: "…".into(),
                        event_time: EventTime::parse_rfc3339(
                            &format!("{day}T10:00:00Z"),
                            crate::time::TimeUncertainty::Exact,
                        )
                        .unwrap(),
                        trust: "self".into(),
                        scope: Some("work".into()),
                        entities: vec![EntityRef {
                            entity_type: "Person".into(),
                            name: "Ada".into(),
                            attributes: vec![],
                            role: "author".into(),
                        }],
                        edges: vec![],
                    }],
                },
            )
            .unwrap();
        let ada = store.entity_by_name("Ada").unwrap().unwrap();

        // Predicted (inferred) → proposed; confirming keeps origin.
        let rel = store
            .add_relationship(
                &actor(),
                &ada.id,
                "collaborator",
                "inferred",
                &["ep_x".into()],
                None,
                None,
            )
            .unwrap();
        // duplicate proposal is a no-op
        assert_eq!(
            store
                .add_relationship(
                    &actor(),
                    &ada.id,
                    "collaborator",
                    "inferred",
                    &["ep_y".into()],
                    None,
                    None
                )
                .unwrap(),
            rel
        );
        store
            .resolve_relationship(&actor(), &rel, "confirm")
            .unwrap();
        let rels = store.relationships_for(&ada.id).unwrap();
        assert_eq!(rels[0].status, "confirmed");
        assert_eq!(rels[0].origin, "inferred"); // never laundered

        // Confirmed close kind → inner tier in the directory.
        let directory = store.relationship_directory(50, None).unwrap();
        let row = directory.iter().find(|r| r.name == "Ada").unwrap();
        assert_eq!(row.tier, "inner");
        assert_eq!(row.evidence_year, 1);
        assert!(row.months.iter().sum::<i64>() >= 1);

        // Ending is history, not deletion.
        store.resolve_relationship(&actor(), &rel, "end").unwrap();
        let rels = store.relationships_for(&ada.id).unwrap();
        assert_eq!(rels[0].status, "ended");
        assert!(rels[0].t_invalid.is_some());
    }
}
