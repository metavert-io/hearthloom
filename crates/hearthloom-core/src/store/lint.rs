//! The record lint (2026-07-12): a report-only idle sweep that surfaces
//! CONTRADICTIONS (LLM over embedding-shortlisted note pairs),
//! SUPERSEDED claims, ORPHAN entities and COVERAGE GAPS as Tend cards.
//! Findings never touch the record — a human resolves each one as
//! `fixed` or `not_an_issue`. Deterministic finding ids make the sweep
//! idempotent: a finding once resolved (either way) never resurfaces
//! unless its underlying evidence set changes.
//!
//! Cloud-pass rule: every input query here excludes `scope='private'`
//! unconditionally, so finding bodies never carry private content and
//! the Tend read needs no vault guard.

use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

#[derive(Debug, Clone, Serialize)]
pub struct LintFinding {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub refs: Vec<String>,
    pub created_at: String,
}

/// A candidate pair for the contradiction/superseded LLM check: two
/// resonant notes (ids, titles, paths, updated stamps, similarity).
/// One living-topic-page candidate: (entity_id, name, citations,
/// existing loom note id, existing note path).
pub type TopicCandidate = (String, String, i64, Option<String>, Option<String>);

#[derive(Debug, Clone)]
pub struct LintPair {
    pub id_a: String,
    pub title_a: String,
    pub path_a: String,
    pub updated_a: Option<String>,
    pub id_b: String,
    pub title_b: String,
    pub path_b: String,
    pub updated_b: Option<String>,
    pub score: f64,
}

fn finding_id(kind: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b":");
    hasher.update(key.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("lint_{}", &digest[..24])
}

/// What a weekly review reads: (episode id, display line) pairs, fresh
/// note titles, and the window's most-cited entities.
#[derive(Debug, Clone, Default)]
pub struct ReviewInputs {
    pub episodes: Vec<(String, String)>,
    pub notes: Vec<String>,
    pub entities: Vec<String>,
}

impl Store {
    /// Record a finding. Returns false when this exact finding (same
    /// kind + evidence key) already exists, open or resolved.
    pub fn add_lint_finding(
        &self,
        actor: &Actor,
        kind: &str,
        key: &str,
        title: &str,
        body: &str,
        refs: &[String],
    ) -> Result<bool> {
        if !matches!(kind, "contradiction" | "superseded" | "orphan" | "gap") {
            return Err(anyhow!("unknown lint kind {kind:?}"));
        }
        let id = finding_id(kind, key);
        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO lint_findings (id, kind, title, body, refs, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                id,
                kind,
                title,
                body,
                serde_json::to_string(refs)?,
                now_utc()
            ],
        )?;
        if inserted > 0 {
            record_mutation(
                &conn,
                actor,
                "lint_finding",
                &format!("lint_finding:{id}"),
                &json!({"id": id, "kind": kind, "title": title}),
            )?;
        }
        Ok(inserted > 0)
    }

    /// Open findings, newest first (the Tend section).
    pub fn lint_pending(&self, limit: usize) -> Result<Vec<LintFinding>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, kind, title, body, refs, created_at FROM lint_findings
             WHERE resolution IS NULL
             ORDER BY created_at DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], |row| {
            let refs: String = row.get(4)?;
            Ok(LintFinding {
                id: row.get(0)?,
                kind: row.get(1)?,
                title: row.get(2)?,
                body: row.get(3)?,
                refs: serde_json::from_str(&refs).unwrap_or_default(),
                created_at: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn lint_pending_count(&self) -> Result<i64> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM lint_findings WHERE resolution IS NULL",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Human resolution: fixed | not_an_issue. Never deletes.
    pub fn resolve_lint_finding(&self, actor: &Actor, id: &str, resolution: &str) -> Result<()> {
        if !matches!(resolution, "fixed" | "not_an_issue") {
            return Err(anyhow!("resolution must be fixed|not_an_issue"));
        }
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE lint_findings SET resolution = ?1, resolved_at = ?2
             WHERE id = ?3 AND resolution IS NULL",
            rusqlite::params![resolution, now_utc(), id],
        )?;
        if changed == 0 {
            return Err(anyhow!("no open finding {id}"));
        }
        record_mutation(
            &conn,
            actor,
            "lint_resolved",
            &format!("lint_resolved:{id}"),
            &json!({"id": id, "resolution": resolution}),
        )?;
        Ok(())
    }

    /// Embedding-shortlisted note pairs for the LLM check — the
    /// note_neighbors table is the shortlist (private excluded HARD:
    /// these bodies go to a cloud provider).
    pub fn lint_pair_candidates(&self, limit: usize) -> Result<Vec<LintPair>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT a.id, a.title, a.path, a.updated_at,
                    b.id, b.title, b.path, b.updated_at, r.score
             FROM note_neighbors r
             JOIN notes_index a ON a.id = r.note_id
                 AND a.status = 'active' AND a.scope != 'private'
             JOIN notes_index b ON b.id = r.neighbor_note_id
                 AND b.status = 'active' AND b.scope != 'private'
             WHERE r.note_id < r.neighbor_note_id
             ORDER BY r.score DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(LintPair {
                id_a: row.get(0)?,
                title_a: row.get(1)?,
                path_a: row.get(2)?,
                updated_a: row.get(3)?,
                id_b: row.get(4)?,
                title_b: row.get(5)?,
                path_b: row.get(6)?,
                updated_b: row.get(7)?,
                score: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Orphan entities: no episode evidence, no live edges, never
    /// declared. Reported, never auto-deleted (zero-gardening still
    /// means the HUMAN decides).
    pub fn lint_orphan_entities(&self, limit: usize) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT e.id, e.name, e.type FROM entities e
             WHERE e.merged_into IS NULL AND e.declared_importance = 0
               AND e.scope != 'private' AND e.hidden_by IS NULL
               AND NOT EXISTS (SELECT 1 FROM episode_entities ee
                               WHERE ee.entity_id = e.id
                                 AND ee.episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL))
               AND NOT EXISTS (SELECT 1 FROM edges ed
                               WHERE (ed.src = e.id OR ed.dst = e.id)
                                 AND ed.t_expired IS NULL)
             ORDER BY e.name LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Living-topic-page candidates: heavily cited entities (90 days,
    /// never private) whose loom hub note is missing or >30 days stale,
    /// with no proposal already waiting in quarantine. Returns
    /// (entity_id, name, citations, existing_note_id, existing_path).
    pub fn topic_page_candidates(&self, limit: usize) -> Result<Vec<TopicCandidate>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT en.id, en.name, COUNT(*) AS n,
                    (SELECT ni.id FROM notes_index ni
                     WHERE ni.origin = 'loom' AND ni.origin_ref = 'topic:' || en.id
                       AND ni.status != 'deleted' LIMIT 1),
                    (SELECT ni.path FROM notes_index ni
                     WHERE ni.origin = 'loom' AND ni.origin_ref = 'topic:' || en.id
                       AND ni.status != 'deleted' LIMIT 1)
             FROM episode_entities ee
             JOIN episodes e ON e.id = ee.episode_id AND e.scope != 'private'
                 AND e.superseded_by IS NULL
                 AND e.ingest_time >= datetime('now', '-90 days')
             JOIN entities en ON en.id = ee.entity_id
                 AND en.merged_into IS NULL AND en.scope != 'private'
             WHERE NOT EXISTS (
                     SELECT 1 FROM notes_index ni
                     WHERE ni.origin = 'loom' AND ni.origin_ref = 'topic:' || en.id
                       AND ni.status != 'deleted'
                       AND ni.updated_at >= datetime('now', '-30 days'))
               AND NOT EXISTS (
                     SELECT 1 FROM quarantine q
                     WHERE q.status = 'pending' AND q.kind = 'topic_page'
                       AND q.content LIKE '%topic:' || en.id || '%')
             GROUP BY en.id
             HAVING n >= 15
             ORDER BY n DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Does an episode with this (source, source_ref) already exist?
    /// (The weekly review checks before spending its LLM call.)
    pub fn episode_exists(&self, source: &str, source_ref: &str) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM episodes /* all-episodes: existence, retired or not */
             WHERE source = ?1 AND source_ref = ?2",
            [source, source_ref],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// Inputs for the weekly review: recent episode lines, fresh note
    /// titles, and the week's most-cited entities. Private excluded
    /// everywhere (this feeds a cloud LLM).
    pub fn weekly_review_inputs(&self, days: i64) -> Result<ReviewInputs> {
        let to = crate::time::now_utc();
        let from = (chrono::Utc::now() - chrono::Duration::days(days))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        self.weekly_review_inputs_between(&from, &to)
    }

    /// The same inputs for any window of arrivals [from, to] — a rewrite
    /// after a retirement re-reads the window the review was written for.
    pub fn weekly_review_inputs_between(&self, from: &str, to: &str) -> Result<ReviewInputs> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, substr(COALESCE(event_time, ingest_time), 1, 10)
                    || ' [' || kind || '] ' || title
             FROM episodes
             WHERE scope != 'private' AND superseded_by IS NULL
               AND ingest_time >= ?1 AND ingest_time <= ?2
               AND kind NOT IN ('health_day', 'chapter')
               AND title != ''
             ORDER BY ingest_time DESC LIMIT 60",
        )?;
        let episodes: Vec<(String, String)> = stmt
            .query_map([from, to], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT title FROM notes_index
             WHERE status != 'deleted' AND scope != 'private'
               AND updated_at >= ?1 AND updated_at <= ?2
             ORDER BY updated_at DESC LIMIT 20",
        )?;
        let notes: Vec<String> = stmt
            .query_map([from, to], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT en.name || ' (' || en.type || ') ×' || COUNT(*)
             FROM episode_entities ee
             JOIN episodes e ON e.id = ee.episode_id AND e.scope != 'private'
                 AND e.superseded_by IS NULL
                 AND e.ingest_time >= ?1 AND e.ingest_time <= ?2
             JOIN entities en ON en.id = ee.entity_id
                 AND en.merged_into IS NULL AND en.scope != 'private'
             GROUP BY en.id ORDER BY COUNT(*) DESC LIMIT 12",
        )?;
        let entities: Vec<String> = stmt
            .query_map([from, to], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(ReviewInputs {
            episodes,
            notes,
            entities,
        })
    }

    /// Coverage gaps: entities the record keeps citing (90 days) that
    /// no note develops — referenced but unwritten-about.
    pub fn lint_coverage_gaps(&self, limit: usize) -> Result<Vec<(String, String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT en.id, en.name, COUNT(*) AS n
             FROM episode_entities ee
             JOIN episodes e ON e.id = ee.episode_id
                 AND e.scope != 'private' AND e.superseded_by IS NULL
                 AND e.ingest_time >= datetime('now', '-90 days')
             JOIN entities en ON en.id = ee.entity_id
                 AND en.merged_into IS NULL AND en.scope != 'private'
                 AND en.type IN ('Project', 'Org', 'Person')
             WHERE NOT EXISTS (SELECT 1 FROM note_mentions m
                               WHERE m.entity_id = en.id)
             GROUP BY en.id
             HAVING n >= 8
             ORDER BY n DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}
