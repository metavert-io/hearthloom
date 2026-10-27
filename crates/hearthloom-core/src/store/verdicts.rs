//! The extraction-verdict ledger (v39, design locked 2026-07-17):
//! per-episode LLM extraction results — the unrepeatable decision —
//! tier-stamped and superseded in place. Links REGENERATE from verdicts
//! (resolve-or-create by name, the rename-replay precedent), so the
//! ledger is the replication channel extraction never had, the diff
//! base for tier re-runs, and the single-spend suppressor across peers.

use anyhow::Result;
use serde_json::{json, Value};

use super::{Actor, Store};
use crate::time::now_utc;

/// low < medium < high; unknown ranks lowest so anything supersedes it.
pub fn tier_rank(tier: &str) -> i32 {
    match tier.trim().to_lowercase().as_str() {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

impl Store {
    /// Record a verdict and make the episode's `mentioned` links match
    /// it: links the PREVIOUS verdict created that the new one doesn't
    /// repeat are removed (supersession, not accretion), new ones apply
    /// through the normal resolve-or-create path. Marks extracted.
    pub fn record_verdict(
        &self,
        actor: &Actor,
        episode_id: &str,
        scope: &str,
        entities: &[(String, String)],
        tier: &str,
        model: &str,
    ) -> Result<usize> {
        // diff: what the old verdict claimed that the new one dropped
        let old: Vec<(String, String)> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT entities FROM extraction_verdicts WHERE episode_id = ?1",
                [episode_id],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
        };
        {
            let conn = self.lock();
            for (entity_type, name) in &old {
                if entities
                    .iter()
                    .any(|(t, n)| t == entity_type && n.eq_ignore_ascii_case(name))
                {
                    continue;
                }
                conn.execute(
                    "DELETE FROM episode_entities
                     WHERE episode_id = ?1 AND role = 'mentioned'
                       AND entity_id IN (SELECT id FROM entities
                                         WHERE type = ?2 AND name = ?3 COLLATE NOCASE)",
                    rusqlite::params![episode_id, entity_type, name],
                )?;
            }
        }
        let linked = self.apply_extraction(actor, episode_id, scope, entities)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO extraction_verdicts (episode_id, entities, tier, model, ts)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(episode_id) DO UPDATE SET
                 entities = excluded.entities, tier = excluded.tier,
                 model = excluded.model, ts = excluded.ts",
            rusqlite::params![
                episode_id,
                serde_json::to_string(entities)?,
                tier,
                model,
                now_utc(),
            ],
        )?;
        Ok(linked)
    }

    /// The exchange feed: verdicts newer than the watermark. The sender
    /// excludes private-scoped episodes INDEPENDENTLY (extraction never
    /// sees private, but the wire enforces it on its own).
    pub fn verdicts_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT v.episode_id, v.entities, v.tier, v.model, v.ts
             FROM extraction_verdicts v JOIN episodes e ON e.id = v.episode_id /* all-episodes: the verdict ledger replicates whole; linking skips retired episodes */
             WHERE v.synced_at > ?1 AND e.scope != 'private'
             ORDER BY v.synced_at LIMIT ?2",
        )?;
        let rows: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                let entities: String = row.get(1)?;
                Ok(json!({
                    "episode_id": row.get::<_, String>(0)?,
                    "entities": serde_json::from_str::<Value>(&entities).unwrap_or(json!([])),
                    "tier": row.get::<_, String>(2)?,
                    "model": row.get::<_, String>(3)?,
                    "ts": row.get::<_, String>(4)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(&conn, "v.synced_at",
            "FROM extraction_verdicts v JOIN episodes e ON e.id = v.episode_id WHERE v.synced_at > ?1 AND e.scope != 'private' /* all-episodes: the verdict ledger replicates whole; linking skips retired episodes */",
            since, limit)?;
        Ok(json!({"verdicts": rows, "next": next}))
    }

    /// Apply a counterpart's verdicts: unknown episodes skip; an
    /// existing verdict at a STRICTLY higher tier wins over the
    /// incoming one; otherwise the incoming verdict lands through
    /// record_verdict — which marks the episode extracted, so whichever
    /// node interprets first, the other stops spending.
    pub fn apply_verdicts(&self, actor: &Actor, batch: &Value) -> Result<usize> {
        let rows = batch
            .get("verdicts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut applied = 0usize;
        for row in rows {
            let Some(episode_id) = row.get("episode_id").and_then(|v| v.as_str()) else {
                continue;
            };
            let incoming_tier = row.get("tier").and_then(|v| v.as_str()).unwrap_or("low");
            let (scope, ours): (Option<String>, Option<String>) = {
                let conn = self.lock();
                let scope = conn
                    .query_row("SELECT scope FROM episodes WHERE id = ?1 /* all-episodes: the verdict ledger replicates whole; linking skips retired episodes */", [episode_id], |r| {
                        r.get(0)
                    })
                    .ok();
                let ours = conn
                    .query_row(
                        "SELECT tier FROM extraction_verdicts WHERE episode_id = ?1",
                        [episode_id],
                        |r| r.get(0),
                    )
                    .ok();
                (scope, ours)
            };
            let Some(scope) = scope else { continue }; // episode not here (yet)
            if let Some(ours) = ours {
                if tier_rank(&ours) > tier_rank(incoming_tier) {
                    continue; // we already hold a stronger interpretation
                }
            }
            let entities: Vec<(String, String)> = row
                .get("entities")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|pair| {
                            let arr = pair.as_array()?;
                            Some((
                                arr.first()?.as_str()?.to_string(),
                                arr.get(1)?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.record_verdict(
                actor,
                episode_id,
                &scope,
                &entities,
                incoming_tier,
                row.get("model").and_then(|v| v.as_str()).unwrap_or(""),
            )?;
            applied += 1;
        }
        Ok(applied)
    }

    /// Tier re-run: re-queue a source's episodes whose verdict came from
    /// a WEAKER tier (and any that failed) — the drain loop re-interprets
    /// them and record_verdict supersedes. Returns how many re-queued.
    pub fn requeue_below_tier(&self, source: &str, tier: &str) -> Result<usize> {
        let rank = tier_rank(tier);
        let conn = self.lock();
        let ranked: Vec<String> = ["", "low", "medium", "high"]
            .iter()
            .filter(|t| tier_rank(t) < rank)
            .map(|t| t.to_string())
            .collect();
        let placeholders = vec!["?"; ranked.len()].join(",");
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&source];
        for t in &ranked {
            params.push(t);
        }
        let requeued = conn.execute(
            &format!(
                "UPDATE episodes SET extracted = 0
                 WHERE source = ?1 AND scope != 'private'
                   AND (extracted = 2 OR id IN (
                       SELECT episode_id FROM extraction_verdicts WHERE tier IN ({placeholders})))"
            ),
            &params[..],
        )?;
        Ok(requeued)
    }

    /// Poison-batch escape: mark episodes extraction-failed (2) so one
    /// bad batch never blocks the queue; requeue_below_tier retries them.
    pub fn mark_extraction_failed(&self, episode_ids: &[String]) -> Result<()> {
        let conn = self.lock();
        for id in episode_ids {
            conn.execute("UPDATE episodes SET extracted = 2 WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// Failed-extraction (2) counts per source — the Integrations cards
    /// report these instead of letting poison-batch episodes silently
    /// vanish from "N to go" (they leave the pending pool but were
    /// never interpreted).
    pub fn failed_extraction_counts(&self, sources: &[&str]) -> Result<Vec<(String, i64)>> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let placeholders = vec!["?"; sources.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT source, COUNT(*) FROM episodes
             WHERE extracted = 2 AND scope != 'private' AND source IN ({placeholders})
               AND superseded_by IS NULL
             GROUP BY source",
        ))?;
        let params: Vec<&dyn rusqlite::ToSql> =
            sources.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(&params[..], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Put one source's failed (2) episodes back in the pending pool —
    /// the card's "try again" action.
    pub fn requeue_failed_extraction(&self, source: &str) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE episodes SET extracted = 0
             WHERE source = ?1 AND scope != 'private' AND extracted = 2",
            [source],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Home;
    use crate::{EpisodeDraft, EventTime, IngestBatch, TimeUncertainty};

    fn seeded() -> (Store, Home, tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let batch = IngestBatch {
            actor: "adapter:test".into(),
            session_id: None,
            episodes: vec![EpisodeDraft {
                source: "openai".into(),
                source_ref: "c1".into(),
                kind: "conversation".into(),
                title: "Chat".into(),
                body: "Talked with Alice Chen about Hearthloom.".into(),
                event_time: EventTime {
                    utc: Some("2026-07-01T12:00:00Z".into()),
                    tz_offset_minutes: None,
                    uncertainty: TimeUncertainty::Hour,
                },
                trust: "self".into(),
                scope: Some("personal".into()),
                entities: vec![],
                edges: vec![],
            }],
        };
        store.apply_batch(&home, &batch).unwrap();
        let id: String = {
            let conn = store.lock();
            conn.query_row("SELECT id FROM episodes LIMIT 1", [], |r| r.get(0))
                .unwrap()
        };
        (store, home, tmp, id)
    }

    fn actor() -> Actor {
        Actor::new("agent:extractor")
    }

    fn mentioned(store: &Store, ep: &str) -> Vec<String> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT en.name FROM episode_entities ee JOIN entities en ON en.id = ee.entity_id
                 WHERE ee.episode_id = ?1 AND ee.role = 'mentioned' ORDER BY en.name",
            )
            .unwrap();
        stmt.query_map([ep], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect()
    }

    #[test]
    fn test_verdict_supersession_diffs_links() {
        let (store, _home, _tmp, ep) = seeded();
        let a = actor();
        // the weak model saw two names, one of them wrong
        store
            .record_verdict(
                &a,
                &ep,
                "personal",
                &[
                    ("Person".into(), "Alice Chen".into()),
                    ("Org".into(), "Hearthloom Inc".into()),
                ],
                "low",
                "haiku",
            )
            .unwrap();
        assert_eq!(mentioned(&store, &ep), vec!["Alice Chen", "Hearthloom Inc"]);
        // the strong re-run drops the wrong org, adds a project
        store
            .record_verdict(
                &a,
                &ep,
                "personal",
                &[
                    ("Person".into(), "Alice Chen".into()),
                    ("Project".into(), "Hearthloom".into()),
                ],
                "high",
                "opus",
            )
            .unwrap();
        let names = mentioned(&store, &ep);
        assert!(names.contains(&"Alice Chen".to_string()));
        assert!(names.contains(&"Hearthloom".to_string()));
        assert!(
            !names.contains(&"Hearthloom Inc".to_string()),
            "superseded link must go"
        );
    }

    #[test]
    fn test_exchange_idempotent_and_tier_precedence() {
        let (a_store, _h1, _t1, ep) = seeded();
        let (b_store, _h2, _t2, _) = seeded(); // same deterministic episode id
        let act = actor();
        a_store
            .record_verdict(
                &act,
                &ep,
                "personal",
                &[("Person".into(), "Alice Chen".into())],
                "medium",
                "sonnet",
            )
            .unwrap();
        let batch = a_store.verdicts_since("", 100).unwrap();
        assert_eq!(b_store.apply_verdicts(&act, &batch).unwrap(), 1);
        // applying marks extracted → the receiver stops spending
        let extracted: i64 = {
            let conn = b_store.lock();
            conn.query_row("SELECT extracted FROM episodes WHERE id = ?1", [&ep], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(extracted, 1);
        // re-apply = no-op-ish, never duplicates links
        b_store.apply_verdicts(&act, &batch).unwrap();
        assert_eq!(mentioned(&b_store, &ep), vec!["Alice Chen"]);
        // a locally-held HIGH verdict resists an incoming MEDIUM one
        b_store
            .record_verdict(
                &act,
                &ep,
                "personal",
                &[("Person".into(), "Alice C".into())],
                "high",
                "opus",
            )
            .unwrap();
        b_store.apply_verdicts(&act, &batch).unwrap();
        assert_eq!(
            mentioned(&b_store, &ep),
            vec!["Alice C"],
            "stronger tier must win"
        );
    }

    #[test]
    fn test_requeue_below_tier_and_failed() {
        let (store, _home, _tmp, ep) = seeded();
        let a = actor();
        store
            .record_verdict(
                &a,
                &ep,
                "personal",
                &[("Person".into(), "Alice Chen".into())],
                "low",
                "haiku",
            )
            .unwrap();
        // low verdict → requeue at high
        assert_eq!(store.requeue_below_tier("openai", "high").unwrap(), 1);
        // failed episodes requeue too
        store
            .mark_extraction_failed(std::slice::from_ref(&ep))
            .unwrap();
        assert_eq!(store.requeue_below_tier("openai", "high").unwrap(), 1);
        // already-high verdicts don't requeue
        store
            .record_verdict(
                &a,
                &ep,
                "personal",
                &[("Person".into(), "Alice Chen".into())],
                "high",
                "opus",
            )
            .unwrap();
        assert_eq!(store.requeue_below_tier("openai", "high").unwrap(), 0);
    }

    #[test]
    fn test_private_never_rides_the_wire() {
        let (store, _home, _tmp, ep) = seeded();
        let a = actor();
        store
            .record_verdict(
                &a,
                &ep,
                "personal",
                &[("Person".into(), "Alice Chen".into())],
                "low",
                "h",
            )
            .unwrap();
        {
            let conn = store.lock();
            conn.execute("UPDATE episodes SET scope = 'private' WHERE id = ?1", [&ep])
                .unwrap();
        }
        let batch = store.verdicts_since("", 100).unwrap();
        assert_eq!(batch.get("verdicts").unwrap().as_array().unwrap().len(), 0);
    }
}
