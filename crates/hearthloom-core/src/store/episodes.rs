//! Episode rows: append-only inserts and reads.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::Store;

/// One episode awaiting entity extraction: (id, source, title, scope,
/// excerpt). The `source` lets the caller group by source and apply each
/// source's configured extraction tier (Phase 3b).
pub type PendingExtraction = (String, String, String, String, String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeRow {
    pub id: String,
    pub source: String,
    pub source_ref: String,
    pub kind: String,
    pub title: String,
    pub actor: String,
    pub event_time: Option<String>,
    pub tz_offset_minutes: Option<i32>,
    pub time_uncertainty: String,
    pub ingest_time: String,
    pub trust: String,
    pub scope: String,
    pub content_ref: Option<String>,
    pub content_hash: String,
    /// How this entered the record: `observed` (absorbed from a data
    /// flow), `declared` (a human stated it), `inferred` (a conclusion
    /// Hearthloom reached by layering information — must cite evidence).
    pub derivation: String,
}

/// The source→derivation mapping (the epistemic ledger's ground rule;
/// rationale in the design log). Arithmetic aggregates like health_day
/// stay `observed` — `inferred` is reserved for semantic layering. Writers
/// minting episodes outside apply_batch set the field explicitly instead.
/// A function of the draft alone (source + trust), so a peer applying the
/// same synced draft derives the same value.
pub fn derivation_for(source: &str, trust: &str) -> &'static str {
    match source {
        // 'ask' = a kept Ask answer (kind=finding): a conclusion the
        // RLM reached by layering evidence — inferred, citations kept.
        "consolidation" | "ask" => "inferred",
        // desk/notes are the live productivity layer: first-person
        // statements when the owner wrote them (trust self). A note an
        // agent or an importer wrote is something the record OBSERVED
        // arriving, never the human's declaration.
        "desk" | "notes" if trust == "self" => "declared",
        // codi is the notes layer's one-time imported ancestor.
        "codi" => "declared",
        _ => "observed",
    }
}

pub(crate) fn episode_from_row(row: &Row<'_>) -> rusqlite::Result<EpisodeRow> {
    Ok(EpisodeRow {
        id: row.get("id")?,
        source: row.get("source")?,
        source_ref: row.get("source_ref")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        actor: row.get("actor")?,
        event_time: row.get("event_time")?,
        tz_offset_minutes: row.get("tz_offset_minutes")?,
        time_uncertainty: row.get("time_uncertainty")?,
        ingest_time: row.get("ingest_time")?,
        trust: row.get("trust")?,
        scope: row.get("scope")?,
        content_ref: row.get("content_ref")?,
        content_hash: row.get("content_hash")?,
        derivation: row.get("derivation")?,
    })
}

pub(crate) const EPISODE_COLUMNS: &str = "id, source, source_ref, kind, title, actor, \
     event_time, tz_offset_minutes, time_uncertainty, ingest_time, trust, scope, \
     content_ref, content_hash, derivation";

/// Insert an episode row + FTS row + chunk rows inside an open transaction.
/// Returns false if (source, source_ref) already exists.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_episode(
    conn: &Connection,
    row: &EpisodeRow,
    session_id: Option<&str>,
    body: &str,
) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO episodes
         (id, source, source_ref, kind, title, actor, session_id, event_time,
          tz_offset_minutes, time_uncertainty, ingest_time, trust, scope,
          content_ref, content_hash, derivation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        rusqlite::params![
            row.id,
            row.source,
            row.source_ref,
            row.kind,
            row.title,
            row.actor,
            session_id,
            row.event_time,
            row.tz_offset_minutes,
            row.time_uncertainty,
            row.ingest_time,
            row.trust,
            row.scope,
            row.content_ref,
            row.content_hash,
            row.derivation,
        ],
    )?;
    if changed == 0 {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO episode_fts (title, body, episode_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![row.title, body, row.id],
    )?;
    conn.execute(
        "UPDATE episodes SET fts_rowid = ?1 WHERE id = ?2",
        rusqlite::params![conn.last_insert_rowid(), row.id],
    )?;
    for (ord, chunk) in chunk_text(body, CHUNK_BYTES).into_iter().enumerate() {
        conn.execute(
            "INSERT INTO chunks (id, episode_id, ord, text) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![format!("{}_{}", row.id, ord), row.id, ord as i64, chunk],
        )?;
    }
    Ok(true)
}

/// Target chunk size (bytes) for embedding-ready chunks (Phase 1 fills the
/// embedding column; same fusion-weight discipline as HearthFS).
const CHUNK_BYTES: usize = 4000;

/// Split text into line-aligned chunks of roughly `max_bytes`.
fn chunk_text(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.is_empty() {
        return vec![];
    }
    if text.len() <= max_bytes {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let remaining = &text[start..];
        if remaining.len() <= max_bytes {
            chunks.push(remaining);
            break;
        }
        // Cut at the last newline within the window, else at a char
        // boundary (the window end itself must land on one before slicing).
        let mut window_end = (start + max_bytes).min(text.len());
        while !text.is_char_boundary(window_end) {
            window_end -= 1;
        }
        let window = &text[start..window_end];
        let cut = match window.rfind('\n') {
            Some(pos) if pos > 0 => start + pos + 1,
            _ => window_end,
        };
        chunks.push(&text[start..cut]);
        start = cut;
    }
    chunks
}

/// Episode counts along the four composition axes (see
/// `Store::episode_composition`).
#[derive(Debug, Default, Clone)]
pub struct EpisodeComposition {
    pub total: i64,
    pub sources: Vec<(String, i64)>,
    pub kinds: Vec<(String, i64)>,
    pub derivations: Vec<(String, i64)>,
    pub scopes: Vec<(String, i64)>,
}

impl Store {
    /// Episodes awaiting idle-time scope classification: coarse
    /// account-level labels that an LLM can refine per episode. Currently
    /// the AI-conversation sources (one account, mixed contexts).
    pub fn episodes_needing_scope_classification(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.title, e.scope, COALESCE(substr(f.body, 1, 400), '')
             FROM episodes e
             LEFT JOIN episode_fts f ON f.rowid = e.fts_rowid
             WHERE e.scope_classified = 0 AND e.source LIKE 'chatlog-%'
               AND e.scope != 'private' -- never reclassify or upload a private mark
               AND e.superseded_by IS NULL
             ORDER BY COALESCE(e.event_time, e.ingest_time) DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Record a classification verdict: mark the episode judged, and
    /// relabel its scope if the verdict differs (mutation-logged).
    pub fn apply_scope_classification(
        &self,
        actor: &super::Actor,
        episode_id: &str,
        new_scope: Option<&str>,
    ) -> Result<bool> {
        let conn = self.lock();
        conn.execute(
            "UPDATE episodes SET scope_classified = 1 WHERE id = ?1",
            [episode_id],
        )?;
        let Some(scope) = new_scope else {
            return Ok(false);
        };
        let changed = conn.execute(
            "UPDATE episodes SET scope = ?1, scope_changed_at = ?3 WHERE id = ?2 AND scope != ?1",
            rusqlite::params![scope, episode_id, crate::time::now_utc()],
        )?;
        if changed > 0 {
            super::record_mutation(
                &conn,
                actor,
                "scope_classified",
                &format!("scope_classified:{episode_id}"),
                &serde_json::json!({"episode": episode_id, "scope": scope}),
            )?;
        }
        Ok(changed > 0)
    }

    /// Prose episodes awaiting entity extraction: conversations, mail,
    /// messages, and articles, where entities hide in text rather than
    /// structure. `sources` is the caller-resolved allowlist (Phase 3b:
    /// per-source extraction tier config, `extract.<source>.tier`, with a
    /// Disabled option — a source omitted here is simply not extracted).
    /// Returns (id, source, title, scope, excerpt) so the caller can group
    /// Extraction backlog per source — the cards' "interpreting…"
    /// phase indicator and time forecast.
    pub fn pending_extraction_counts(&self, sources: &[&str]) -> Result<Vec<(String, i64)>> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let placeholders = vec!["?"; sources.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT source, COUNT(*) FROM episodes
             WHERE extracted = 0 AND scope != 'private' AND source IN ({placeholders})
               AND superseded_by IS NULL
             GROUP BY source",
        ))?;
        let params: Vec<&dyn rusqlite::ToSql> =
            sources.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(&params[..], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// pending episodes by source and apply each source's tier. Private
    /// content never reaches the LLM regardless of source config.
    pub fn episodes_needing_extraction(
        &self,
        sources: &[&str],
        limit: usize,
    ) -> Result<Vec<PendingExtraction>> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let placeholders = vec!["?"; sources.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT e.id, e.source, e.title, e.scope, COALESCE(substr(f.body, 1, 1200), '')
             FROM episodes e
             LEFT JOIN episode_fts f ON f.rowid = e.fts_rowid
             WHERE e.extracted = 0
               AND e.scope != 'private' -- private content never reaches the LLM
               AND e.superseded_by IS NULL
               AND e.source IN ({placeholders})
             ORDER BY COALESCE(e.event_time, e.ingest_time) DESC
             LIMIT ?{}",
            sources.len() + 1
        ))?;
        let mut params: Vec<&dyn rusqlite::ToSql> =
            sources.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let lim = limit as i64;
        params.push(&lim);
        let rows = stmt.query_map(params.as_slice(), |row| {
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

    /// Record mined entities for an episode: resolve-or-create each and
    /// cite the episode (role `mentioned`), then mark it extracted.
    /// Returns how many entity links were added.
    pub fn apply_extraction(
        &self,
        actor: &super::Actor,
        episode_id: &str,
        scope: &str,
        entities: &[(String, String)], // (type, name)
    ) -> Result<usize> {
        const TYPES: &[&str] = &["Person", "Org", "Project", "Place", "Event"];
        let conn = self.lock();
        // A superseded or retired episode links nothing (D.12): a verdict
        // arriving from the peer must not bring a retired memory's
        // entities back. The verdict itself is still kept by the caller.
        let live: bool = conn
            .query_row(
                "SELECT superseded_by IS NULL FROM episodes WHERE id = ?1",
                [episode_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if !live {
            conn.execute(
                "UPDATE episodes SET extracted = 1 WHERE id = ?1",
                [episode_id],
            )?;
            return Ok(0);
        }
        let mut linked = 0;
        for (entity_type, name) in entities.iter().take(8) {
            if !TYPES.contains(&entity_type.as_str()) || name.trim().len() < 2 {
                continue;
            }
            let (id, _, _) =
                super::graph::upsert_entity(&conn, actor, entity_type, name.trim(), scope, &[])?;
            let added = conn.execute(
                "INSERT OR IGNORE INTO episode_entities (episode_id, entity_id, role)
                 VALUES (?1, ?2, 'mentioned')",
                [episode_id, &id],
            )?;
            if added > 0 {
                conn.execute(
                    "UPDATE entities SET hidden_by = NULL WHERE id = ?1 AND hidden_by IS NOT NULL",
                    [&id],
                )?;
            }
            linked += added;
        }
        conn.execute(
            "UPDATE episodes SET extracted = 1 WHERE id = ?1",
            [episode_id],
        )?;
        if linked > 0 {
            super::record_mutation(
                &conn,
                actor,
                "entities_extracted",
                &format!("entities_extracted:{episode_id}"),
                &serde_json::json!({"episode": episode_id, "linked": linked}),
            )?;
        }
        Ok(linked)
    }

    /// Does any episode from `source` carry a private mark? Internal
    /// gate for surfaces whose backing rows carry no scope of their own
    /// (e.g. the health sidecar's vquery): if yes, the whole surface
    /// locks with the vault rather than leak through the side door.
    pub fn source_has_private(&self, source: &str) -> Result<bool> {
        let conn = self.lock();
        let exists: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM episodes /* all-episodes: a vault gate counts every private row */
             WHERE source = ?1 AND scope = 'private')",
            [source],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// Episode counts per source (dashboard/status).
    /// Vault-guarded composition counts for the analytics view.
    pub fn episode_counts_by(&self, column: &str) -> Result<Vec<(String, i64)>> {
        // Column is caller-chosen from a fixed set — never user input.
        if !matches!(column, "kind" | "derivation" | "scope" | "trust") {
            return Err(anyhow::anyhow!("unsupported composition column {column:?}"));
        }
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {column}, COUNT(*) FROM episodes WHERE superseded_by IS NULL{guard}
             GROUP BY {column} ORDER BY 2 DESC"
        ))?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The record's composition by source, kind, derivation and scope
    /// in ONE table scan. The dashboard used to ask for these as four
    /// separate GROUP BYs (plus the total) — five walks of a multi-GB
    /// table under the store lock, 26s on a cold page cache, which froze
    /// every screen on 2026-10-06. Vault-guarded like the single-column
    /// forms; each list is sorted by count, descending.
    pub fn episode_composition(&self) -> Result<EpisodeComposition> {
        let guard = self.scope_guard("scope");
        let rows: Vec<(String, String, String, String, i64)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(&format!(
                "SELECT source, kind, derivation, scope, COUNT(*) FROM episodes
                 WHERE superseded_by IS NULL{guard} GROUP BY source, kind, derivation, scope"
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
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let mut out = EpisodeComposition::default();
        let mut maps: [std::collections::HashMap<String, i64>; 4] = Default::default();
        for (source, kind, derivation, scope, n) in rows {
            out.total += n;
            for (map, key) in maps.iter_mut().zip([source, kind, derivation, scope]) {
                *map.entry(key).or_insert(0) += n;
            }
        }
        let sorted = |map: std::collections::HashMap<String, i64>| {
            let mut list: Vec<(String, i64)> = map.into_iter().collect();
            list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            list
        };
        let [sources, kinds, derivations, scopes] = maps;
        out.sources = sorted(sources);
        out.kinds = sorted(kinds);
        out.derivations = sorted(derivations);
        out.scopes = sorted(scopes);
        Ok(out)
    }

    pub fn episode_counts_by_source(&self) -> Result<Vec<(String, i64)>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT source, COUNT(*) FROM episodes WHERE superseded_by IS NULL{guard}
             GROUP BY source ORDER BY 2 DESC"
        ))?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Full text body of an episode (from the FTS shadow; the raw file on
    /// disk is the canonical copy).
    pub fn episode_body(&self, id: &str) -> Result<Option<String>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        conn.query_row(
            &format!(
                "SELECT f.body FROM episodes e
                 JOIN episode_fts f ON f.rowid = e.fts_rowid
                 WHERE e.id = ?1 AND e.superseded_by IS NULL{guard}"
            ),
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// The most recent LIVE daily digest — the one BRAIN.md and the
    /// dashboard show. A rewrite (after a retirement) keeps its day, so
    /// newest-by-day never lets an old day's rewrite pose as today's.
    pub fn latest_summary(&self) -> Result<Option<EpisodeRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!(
                "SELECT {EPISODE_COLUMNS} FROM episodes
                 WHERE source = 'consolidation' AND kind = 'summary' AND superseded_by IS NULL
                 ORDER BY COALESCE(event_time, ingest_time) DESC, ingest_time DESC LIMIT 1"
            ),
            [],
            episode_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The most recent daily digest the pass WROTE — retired or not,
    /// rewrites excluded — for "is today's digest done?".
    pub fn latest_digest_written(&self) -> Result<Option<EpisodeRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!(
                "SELECT {EPISODE_COLUMNS} FROM episodes /* all-episodes: a retired digest still counts as written */
                 WHERE source = 'consolidation' AND kind = 'summary' AND source_ref NOT LIKE '%@%'
                 ORDER BY ingest_time DESC LIMIT 1"
            ),
            [],
            episode_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// One LIVE episode (retired and replaced ones read as absent).
    pub fn episode(&self, id: &str) -> Result<Option<EpisodeRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {EPISODE_COLUMNS} FROM episodes WHERE id = ?1 AND superseded_by IS NULL{guard}"),
            [id],
            episode_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// One episode whatever its lifecycle — the forensic detail view
    /// (Memories shows retired and replaced rows, labelled): the row, its
    /// `superseded_by` (`retired:<id>` when retired) and its body.
    /// Vault-guarded.
    #[allow(clippy::type_complexity)]
    pub fn episode_forensic(
        &self,
        id: &str,
    ) -> Result<Option<(EpisodeRow, Option<String>, Option<String>)>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let columns = EPISODE_COLUMNS
            .split(", ")
            .map(|c| format!("e.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        conn.query_row(
            &format!(
                "SELECT {columns}, e.superseded_by, f.body FROM episodes e /* all-episodes: the forensic detail */
                 LEFT JOIN episode_fts f ON f.rowid = e.fts_rowid
                 WHERE e.id = ?1{guard}"
            ),
            [id],
            |row| Ok((episode_from_row(row)?, row.get(15)?, row.get(16)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Most recent episodes by event time (unknown-time episodes sort by
    /// ingest time via COALESCE). Feeds BRAIN.md and the daily digest —
    /// both persisted artifacts — so private is excluded even when the
    /// vault is open.
    pub fn recent_episodes(&self, limit: usize) -> Result<Vec<EpisodeRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EPISODE_COLUMNS} FROM episodes
             WHERE scope != 'private' AND superseded_by IS NULL
             ORDER BY COALESCE(event_time, ingest_time) DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], episode_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_text() {
        assert!(chunk_text("", 10).is_empty());
        assert_eq!(chunk_text("short", 10), vec!["short"]);

        let long = "line one\nline two\nline three\n";
        let chunks = chunk_text(long, 12);
        assert!(chunks.len() > 1);
        assert_eq!(chunks.concat(), long);

        // No-newline text still splits on char boundaries.
        let solid = "aaaaaaaaaaaaaaaaaaaaaaaa";
        let chunks = chunk_text(solid, 10);
        assert_eq!(chunks.concat(), solid);

        // Multibyte chars straddling the window edge must not panic.
        let boxy = "├".repeat(50); // 3 bytes each
        let chunks = chunk_text(&boxy, 10);
        assert_eq!(chunks.concat(), boxy);
        let mixed = format!("{}\n{}", "a".repeat(9), "├".repeat(20));
        let chunks = chunk_text(&mixed, 10);
        assert_eq!(chunks.concat(), mixed);
    }

    #[test]
    fn test_declared_means_the_owner_wrote_it() {
        assert_eq!(derivation_for("notes", "self"), "declared");
        assert_eq!(derivation_for("desk", "self"), "declared");
        // A note an agent or an importer wrote was observed arriving.
        assert_eq!(derivation_for("notes", "agent"), "observed");
        assert_eq!(derivation_for("notes", "imported"), "observed");
        assert_eq!(derivation_for("codi", "imported"), "declared");
        assert_eq!(derivation_for("consolidation", "self"), "inferred");
        assert_eq!(derivation_for("mbox", "self"), "observed");
    }
}
