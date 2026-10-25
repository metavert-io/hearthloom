//! The mountable read surface behind the discovery manifest (`/brain/…`
//! in the daemon): the brain rendered as a small filesystem tree for
//! HearthFS and any manifest-driven client.
//!
//! INVARIANT: every query here HARDCODES `scope != 'private'`. This
//! surface serves the vault-LOCKED view ALWAYS — mount clients cache
//! responses (ApiSource TTL cache, NFS/FSKit kernel caches), so a
//! response produced during an unlock window would outlive the relock
//! and break the zero-leakage promise. Same law as BRAIN.md and the
//! daily digest: private never leaves. Do NOT swap these for the
//! vault-aware readers.
//!
//! Listings are BOUNDED (top entities, recent days, active goals) — a
//! readdir is a view, not an export; search reaches everything else.

use anyhow::Result;
use serde::Serialize;

use super::Store;

#[derive(Debug, Clone, Serialize)]
pub struct BrainFsItem {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrainFsHit {
    pub path: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
}

impl Store {
    /// Top entities for the /entities listing: declared importance first,
    /// then non-private citation volume. Merged-away rows never surface.
    pub fn brain_fs_entities(&self, limit: usize) -> Result<Vec<BrainFsItem>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.name
             FROM entities e
             WHERE e.merged_into IS NULL AND e.hidden_by IS NULL
               AND e.scope != 'private'
             ORDER BY e.declared_importance DESC,
                      (SELECT COUNT(*) FROM episode_entities ee
                        JOIN episodes ep ON ep.id = ee.episode_id
                        WHERE ee.entity_id = e.id AND ep.scope != 'private'
                          AND ep.superseded_by IS NULL) DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(BrainFsItem {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// One entity as a markdown dossier. None if unknown, merged away, or
    /// private-scoped (a private entity does not exist on this surface).
    pub fn brain_fs_entity(&self, id_or_name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let found: Option<(String, String, String)> = conn
            .query_row(
                "SELECT id, name, type FROM entities
                 WHERE (id = ?1 OR name = ?1)
                   AND merged_into IS NULL
                   AND scope != 'private'
                 ORDER BY CASE WHEN id = ?1 THEN 0 ELSE 1 END
                 LIMIT 1",
                [id_or_name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((id, name, entity_type)) = found else {
            return Ok(None);
        };
        let mut out = format!("# {name}\n\n*{entity_type}*\n\n");
        let mut attrs = conn.prepare(
            "SELECT kind, value FROM entity_attributes WHERE entity_id = ?1 LIMIT 20",
        )?;
        let attrs: Vec<(String, String)> = attrs
            .query_map([&id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        if !attrs.is_empty() {
            out.push_str("## Attributes\n");
            for (kind, value) in attrs {
                out.push_str(&format!("- {kind}: {value}\n"));
            }
            out.push('\n');
        }
        let mut eps = conn.prepare(
            "SELECT substr(COALESCE(ep.event_time, ep.ingest_time), 1, 10),
                    ep.kind, ep.title
             FROM episode_entities ee
             JOIN episodes ep ON ep.id = ee.episode_id
             WHERE ee.entity_id = ?1 AND ep.scope != 'private' AND ep.superseded_by IS NULL
             ORDER BY COALESCE(ep.event_time, ep.ingest_time) DESC
             LIMIT 15",
        )?;
        let eps: Vec<(String, String, String)> = eps
            .query_map([&id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        if !eps.is_empty() {
            out.push_str("## Recent evidence\n");
            for (day, kind, title) in eps {
                out.push_str(&format!("- {day} [{kind}] {title}\n"));
            }
        }
        Ok(Some(out))
    }

    /// One day as markdown: non-private episodes, capped.
    pub fn brain_fs_day(&self, date: &str) -> Result<String> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT substr(COALESCE(event_time, ingest_time), 12, 5),
                    kind, source, title
             FROM episodes
             WHERE substr(COALESCE(event_time, ingest_time), 1, 10) = ?1
               AND scope != 'private' AND superseded_by IS NULL
             ORDER BY COALESCE(event_time, ingest_time)
             LIMIT 100",
        )?;
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map([date], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = format!("# {date}\n\n");
        if rows.is_empty() {
            out.push_str("No episodes recorded.\n");
        } else {
            for (time, kind, source, title) in rows {
                out.push_str(&format!("- {time} [{kind}/{source}] {title}\n"));
            }
        }
        Ok(out)
    }

    /// Active goals for /intents.
    pub fn brain_fs_intents(&self) -> Result<Vec<BrainFsItem>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name FROM intents
             WHERE COALESCE(status, 'active') = 'active'
             ORDER BY name LIMIT 50",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(BrainFsItem {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn brain_fs_intent(&self, id_or_name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let found: Option<(String, String, String)> = conn
            .query_row(
                "SELECT name, query, COALESCE(status, 'active') FROM intents
                 WHERE id = ?1 OR name = ?1 LIMIT 1",
                [id_or_name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(found.map(|(name, query, status)| {
            format!("# {name}\n\n*{status}*\n\nWatching for: {query}\n")
        }))
    }

    /// FTS over non-private episodes; hits point at /days/<date>.md so
    /// every result is reachable inside the mounted tree. Joins on
    /// fts_rowid (never episode_id — the quadratic-scan invariant).
    pub fn brain_fs_search(&self, query: &str, limit: usize) -> Result<Vec<BrainFsHit>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.title,
                    snippet(episode_fts, 1, '', '', '…', 20),
                    bm25(episode_fts),
                    substr(COALESCE(e.event_time, e.ingest_time), 1, 10)
             FROM episode_fts f
             JOIN episodes e ON e.fts_rowid = f.rowid
             WHERE episode_fts MATCH ?1
               AND e.scope != 'private' AND e.superseded_by IS NULL
             ORDER BY bm25(episode_fts)
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![super::search::sanitize_fts_query(query), limit as i64],
            |row| {
                let title: String = row.get(0)?;
                let snippet: String = row.get(1)?;
                let bm25: f64 = row.get(2)?;
                let day: String = row.get(3)?;
                Ok(BrainFsHit {
                    path: format!("days/{day}.md"),
                    title,
                    snippet,
                    // bm25 is negative-is-better; flip to a positive score
                    score: -bm25,
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::paths::Home;
    use crate::time::{EventTime, TimeUncertainty};
    use crate::Store;

    fn draft(source_ref: &str, title: &str, body: &str, scope: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "test".into(),
            source_ref: source_ref.into(),
            kind: "note".into(),
            title: title.into(),
            body: body.into(),
            event_time: EventTime::parse_rfc3339(
                "2026-07-01T10:00:00Z",
                TimeUncertainty::Exact,
            )
            .unwrap(),
            trust: "self".into(),
            scope: Some(scope.into()),
            entities: vec![],
            edges: vec![],
        }
    }

    fn seeded() -> (Store, tempfile::TempDir) {
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
                        draft("a", "public plans for the garden", "planting tomatoes", "personal"),
                        draft("b", "private medical detail", "secret tomato diagnosis", "private"),
                    ],
                },
            )
            .unwrap();
        (store, tmp)
    }

    #[test]
    fn test_brain_fs_excludes_private_even_when_unlocked() {
        let (store, _tmp) = seeded();
        // Unlock the vault — this surface must not care.
        store.vault_unlock(5);
        let day = store.brain_fs_day("2026-07-01").unwrap();
        assert!(day.contains("garden"));
        assert!(!day.contains("medical"));
        let hits = store.brain_fs_search("tomato", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].title.contains("garden"));
        assert!(hits[0].path.starts_with("days/2026-07-01"));
    }
}
