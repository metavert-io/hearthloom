//! Opportunities: confirmed `seeking` propositions matched against newly
//! ingested episodes by the idle scanner (embedding + FTS — local, no
//! LLM on the match path). Hits wait at Tend: KEEP mints a
//! kind=opportunity episode (source consolidation → derivation=inferred,
//! with the evidence episode and seeking proposition cited); DISMISS
//! retires the row. Display joins episodes AND self_model for the vault
//! guard — a private evidence episode or private-sphere seeking hides
//! the opportunity entirely while locked.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::embeddings::{bytes_to_floats, cosine_similarity};
use crate::ids;
use crate::paths::Home;
use crate::time::now_utc;

/// A new episode's chunk must sit at least this close to a confirmed
/// seeking proposition. Higher than the intent semantic leg — an
/// opportunity interrupts the human at Tend, so precision wins.
const OPPORTUNITY_THRESHOLD: f32 = 0.62;
/// Chunk rows examined per scan (memory/latency cap under the lock).
const SCAN_CHUNK_CAP: usize = 4000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpportunityRow {
    pub id: String,
    pub seeking_id: String,
    pub seeking_text: String,
    pub episode_id: String,
    pub episode_title: String,
    pub episode_time: Option<String>,
    pub score: f64,
    pub reason: Option<String>,
    pub status: String,
    pub found_at: String,
}

impl Store {
    /// Consolidation watermarks (opportunities, merge candidates).
    pub fn consolidation_state(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM consolidation_state WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
    }

    pub fn set_consolidation_state(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO consolidation_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Hours since a consolidation_state key holding an RFC 3339
    /// timestamp was stamped (None when the key is absent). SQLite's
    /// julianday parses the stored form; the automation frequency gates
    /// (☰ → Settings) read cadence through this.
    pub fn state_age_hours(&self, key: &str) -> Result<Option<f64>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT (julianday('now') - julianday(value)) * 24.0
             FROM consolidation_state WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
    }

    /// Scan episodes ingested since the watermark against every confirmed
    /// seeking proposition — embeddings only (an FTS AND over a seeking
    /// SENTENCE is uselessly strict), and only derivation='observed'
    /// episodes: opportunities come from the world, never from
    /// Hearthloom's own digests/cascades/tasks. Local, no LLM. Returns
    /// new opportunity count.
    ///
    /// Deliberately NOT the v49 ANN index (considered 2026-08-16): a
    /// global KNN over vec_chunks can't restrict to just-ingested
    /// episodes, so new episodes outside a seeking vector's global
    /// top-k would be silently missed. The watermark-bounded scan is
    /// already incremental and cheap (~100ms/cycle measured live) —
    /// its cost scales with NEW chunks, not the substrate.
    pub fn scan_opportunities(&self, actor: &Actor) -> Result<usize> {
        let last_seq: i64 = self
            .consolidation_state("opportunities_last_seq")?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let seeking: Vec<(String, Vec<f32>)> = self
            .confirmed_seeking()?
            .into_iter()
            .filter_map(|(id, _text, embedding)| embedding.map(|e| (id, e)))
            .collect();

        // Advance the watermark even with nothing to seek — declaring a
        // want later must not replay months of backlog into Tend.
        let latest: i64 = {
            let conn = self.lock();
            conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM mutations", [], |row| {
                row.get(0)
            })?
        };
        if seeking.is_empty() {
            self.set_consolidation_state("opportunities_last_seq", &latest.to_string())?;
            return Ok(0);
        }

        // (episode, chunk text, best seeking id, similarity) above floor.
        let mut found: Vec<(String, String, String, f32)> = Vec::new();
        {
            let conn = self.lock();
            let mut stmt = conn.prepare(&format!(
                "SELECT c.episode_id, c.text, c.embedding
                 FROM mutations m
                 JOIN chunks c ON c.episode_id = json_extract(m.payload, '$.id')
                 JOIN episodes e ON e.id = c.episode_id
                 WHERE m.seq > ?1 AND m.kind = 'episode_ingested'
                   AND c.embedding IS NOT NULL
                   AND e.derivation = 'observed' AND e.superseded_by IS NULL
                 LIMIT {SCAN_CHUNK_CAP}"
            ))?;
            let rows = stmt.query_map([last_seq], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            for row in rows {
                let (episode_id, text, blob) = row?;
                let chunk_vec = bytes_to_floats(&blob);
                for (seeking_id, seeking_vec) in &seeking {
                    let similarity = cosine_similarity(seeking_vec, &chunk_vec);
                    if similarity >= OPPORTUNITY_THRESHOLD {
                        found.push((
                            episode_id.clone(),
                            text.clone(),
                            seeking_id.clone(),
                            similarity,
                        ));
                    }
                }
            }
        }

        let mut added = 0usize;
        for (episode_id, text, seeking_id, similarity) in found {
            let excerpt: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let excerpt: String = excerpt.chars().take(120).collect();
            if self.insert_opportunity(
                actor,
                &seeking_id,
                &episode_id,
                similarity as f64,
                &format!("close to: \u{201c}{excerpt}\u{201d}"),
            )? {
                added += 1;
            }
        }
        self.set_consolidation_state("opportunities_last_seq", &latest.to_string())?;
        Ok(added)
    }

    /// Record a match (scanner-side). Idempotent per (seeking, episode).
    pub fn insert_opportunity(
        &self,
        actor: &Actor,
        seeking_id: &str,
        episode_id: &str,
        score: f64,
        reason: &str,
    ) -> Result<bool> {
        let id = ids::opportunity_id();
        let conn = self.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO opportunities
                 (id, seeking_id, episode_id, score, reason, found_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, seeking_id, episode_id, score, reason, now_utc()],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        record_mutation(
            &conn,
            actor,
            "opportunity_found",
            &format!("opportunity_found:{id}"),
            &json!({"id": id, "seeking_id": seeking_id, "episode_id": episode_id, "score": score}),
        )?;
        Ok(true)
    }

    /// Pending opportunities for Tend, best-first, fully vault-guarded.
    pub fn list_opportunities(&self, limit: usize) -> Result<Vec<OpportunityRow>> {
        let episode_guard = self.scope_guard("e.scope");
        let sphere_guard = self.scope_guard("s.sphere");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT o.id, o.seeking_id, s.text, o.episode_id, e.title,
                    e.event_time, o.score, o.reason, o.status, o.found_at
             FROM opportunities o
             JOIN episodes e ON e.id = o.episode_id
             JOIN self_model s ON s.id = o.seeking_id
             WHERE o.status = 'pending' AND e.superseded_by IS NULL{episode_guard}{sphere_guard}
             ORDER BY o.score DESC, o.found_at DESC
             LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(OpportunityRow {
                id: row.get(0)?,
                seeking_id: row.get(1)?,
                seeking_text: row.get(2)?,
                episode_id: row.get(3)?,
                episode_title: row.get(4)?,
                episode_time: row.get(5)?,
                score: row.get(6)?,
                reason: row.get(7)?,
                status: row.get(8)?,
                found_at: row.get(9)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Keep (mint a kind=opportunity episode citing seeking + evidence)
    /// or dismiss. Returns the minted episode id on keep.
    pub fn resolve_opportunity(
        &self,
        actor: &Actor,
        home: &Home,
        id: &str,
        keep: bool,
    ) -> Result<Option<String>> {
        let status = if keep { "kept" } else { "dismissed" };
        // Resolve + gather inside one lock scope; the episode mint below
        // re-takes the lock via apply_batch, so it must run after.
        type MintRow = (String, String, String, String, Option<String>, String);
        let mint: Option<MintRow> = {
            let conn = self.lock();
            let row = conn
                .query_row(
                    "SELECT s.text, e.title, e.scope, o.reason, e.event_time, o.episode_id
                     FROM opportunities o
                     JOIN episodes e ON e.id = o.episode_id
                     JOIN self_model s ON s.id = o.seeking_id
                     WHERE o.id = ?1 AND o.status = 'pending' AND e.superseded_by IS NULL",
                    [id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, String>(5)?,
                        ))
                    },
                )
                .map_err(|_| anyhow!("no pending opportunity {id}"))?;
            let changed = conn.execute(
                "UPDATE opportunities SET status = ?1, resolved_at = ?2
                 WHERE id = ?3 AND status = 'pending'",
                rusqlite::params![status, now_utc(), id],
            )?;
            if changed == 0 {
                return Err(anyhow!("no pending opportunity {id}"));
            }
            record_mutation(
                &conn,
                actor,
                "opportunity_resolved",
                &format!("opportunity_resolved:{id}"),
                &json!({"id": id, "status": status}),
            )?;
            keep.then_some(row)
        };

        let Some((seeking_text, episode_title, scope, reason, event_time, evidence_id)) = mint
        else {
            return Ok(None);
        };
        let draft = crate::ingest::EpisodeDraft {
            source: "consolidation".into(),
            source_ref: format!("opportunity:{id}"),
            kind: "opportunity".into(),
            title: format!("Opportunity: {episode_title}"),
            body: format!("seeking: {seeking_text}\nevidence: {evidence_id}\n\n{reason}"),
            event_time: match event_time {
                Some(t) => crate::time::EventTime {
                    utc: Some(t),
                    tz_offset_minutes: None,
                    uncertainty: crate::time::TimeUncertainty::Day,
                },
                None => crate::time::EventTime::unknown(),
            },
            trust: "agent".into(),
            scope: Some(scope),
            entities: vec![],
            edges: vec![],
        };
        let report = self.apply_batch(
            home,
            &crate::ingest::IngestBatch {
                actor: actor.actor.clone(),
                session_id: actor.session_id.clone(),
                episodes: vec![draft],
            },
        )?;
        let _ = report;
        Ok(Some(ids::episode_id(
            "consolidation",
            &format!("opportunity:{id}"),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::time::EventTime;

    fn actor() -> Actor {
        Actor::new("agent:consolidation")
    }

    #[test]
    fn test_opportunity_flow() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        let seeking = store
            .add_proposition(
                &Actor::new("human:test"),
                "work",
                "seeking",
                "collaborators for a game economy project",
                "declared",
                &[],
                None,
            )
            .unwrap();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![EpisodeDraft {
                        source: "git".into(),
                        source_ref: "r#opp".into(),
                        kind: "commit".into(),
                        title: "met a game economy designer".into(),
                        body: "intro call notes".into(),
                        event_time: EventTime::unknown(),
                        trust: "self".into(),
                        scope: Some("work".into()),
                        entities: vec![],
                        edges: vec![],
                    }],
                },
            )
            .unwrap();
        let episode_id = crate::ids::episode_id("git", "r#opp");

        assert!(store
            .insert_opportunity(&actor(), &seeking, &episode_id, 0.83, "strong overlap")
            .unwrap());
        // Same pair again: idempotent no-op.
        assert!(!store
            .insert_opportunity(&actor(), &seeking, &episode_id, 0.83, "strong overlap")
            .unwrap());

        let pending = store.list_opportunities(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].seeking_text,
            "collaborators for a game economy project"
        );

        // Keep → kind=opportunity episode minted, derivation inferred.
        let minted = store
            .resolve_opportunity(&Actor::new("human:test"), &home, &pending[0].id, true)
            .unwrap()
            .unwrap();
        let episode = store.episode(&minted).unwrap().unwrap();
        assert_eq!(episode.kind, "opportunity");
        assert_eq!(episode.derivation, "inferred");
        assert!(store.list_opportunities(10).unwrap().is_empty());
        // Double-resolve fails cleanly.
        assert!(store
            .resolve_opportunity(&Actor::new("human:test"), &home, &pending[0].id, false)
            .is_err());
    }

    #[test]
    fn test_scan_matches_observed_only_and_respects_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let scanner = actor();

        // Seeking declared with an embedding along the x axis.
        let seeking = store
            .add_proposition(
                &Actor::new("human:test"),
                "work",
                "seeking",
                "collaborators in games",
                "declared",
                &[],
                None,
            )
            .unwrap();
        store
            .set_proposition_embedding(&seeking, &[1.0, 0.0, 0.0])
            .unwrap();

        // One observed episode, one inferred (consolidation) episode.
        let mk = |source: &str, source_ref: &str| EpisodeDraft {
            source: source.into(),
            source_ref: source_ref.into(),
            kind: "note".into(),
            title: format!("{source_ref} title"),
            body: "body".into(),
            event_time: EventTime::unknown(),
            trust: "self".into(),
            scope: Some("work".into()),
            entities: vec![],
            edges: vec![],
        };
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![mk("git", "obs"), mk("consolidation", "inf")],
                },
            )
            .unwrap();
        let observed = crate::ids::episode_id("git", "obs");
        let inferred = crate::ids::episode_id("consolidation", "inf");
        store
            .set_chunk_embedding(&format!("{observed}_0"), &[0.95, 0.05, 0.0])
            .unwrap();
        store
            .set_chunk_embedding(&format!("{inferred}_0"), &[0.95, 0.05, 0.0])
            .unwrap();

        // Only the observed episode fires; the watermark stops replays.
        assert_eq!(store.scan_opportunities(&scanner).unwrap(), 1);
        let pending = store.list_opportunities(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].episode_id, observed);
        assert_eq!(store.scan_opportunities(&scanner).unwrap(), 0);
    }
}
