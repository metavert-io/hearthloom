//! Chunk embeddings: backfill bookkeeping and vector search.
//!
//! Two engines behind one `vector_search`: the sqlite-vec `vec_chunks`
//! ANN index (v49, rowid = chunks.rowid, cosine) once it covers every
//! embedded chunk, and the original brute-force cosine scan over the
//! BLOB column until then (and forever for non-384-dim vectors — tests
//! use tiny ones). New embeddings are dual-written; the consolidation
//! cycle backfills history in small batches. `chunks.embedding` stays
//! the durable copy — the index is derived and rebuildable.

use anyhow::Result;
use rusqlite::OptionalExtension;
use std::sync::atomic::Ordering;

use super::Store;
use crate::embeddings::{bytes_to_floats, cosine_similarity, floats_to_bytes};

/// The embedder's output width. Only vectors of this dimension enter the
/// ANN index (its vec0 column is declared float[384]); anything else is
/// served by the brute-force path.
pub const VEC_DIM: usize = 384;

/// Best-chunk match for an episode.
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub episode_id: String,
    pub similarity: f32,
    pub chunk_text: String,
}

impl Store {
    /// Chunks awaiting embeddings, oldest episodes first.
    pub fn chunks_needing_embedding(&self, limit: usize) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, text FROM chunks WHERE embedding IS NULL LIMIT ?1 /* all-episodes: index upkeep; vector_search filters retired chunks */",
        )?;
        let rows = stmt.query_map([limit as i64], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn set_chunk_embedding(&self, chunk_id: &str, embedding: &[f32]) -> Result<()> {
        let conn = self.lock();
        let blob = floats_to_bytes(embedding);
        conn.execute(
            "UPDATE chunks SET embedding = ?1 WHERE id = ?2",
            rusqlite::params![blob, chunk_id],
        )?;
        // Dual-write into the ANN index (delete-then-insert: vec0 has no
        // upsert). Skipped for off-dimension vectors — those never leave
        // the brute-force path.
        if embedding.len() == VEC_DIM {
            let rowid: Option<i64> = conn
                .query_row("SELECT rowid FROM chunks WHERE id = ?1 /* all-episodes: index upkeep; vector_search filters retired chunks */", [chunk_id], |r| r.get(0))
                .optional()?;
            if let Some(rowid) = rowid {
                conn.execute("DELETE FROM vec_chunks WHERE rowid = ?1", [rowid])?;
                conn.execute(
                    "INSERT INTO vec_chunks(rowid, embedding) VALUES (?1, ?2)",
                    rusqlite::params![rowid, blob],
                )?;
            }
        }
        Ok(())
    }

    /// How many chunks have embeddings vs total.
    pub fn embedding_coverage(&self) -> Result<(i64, i64)> {
        let conn = self.lock();
        let embedded: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE embedding IS NOT NULL /* all-episodes: index upkeep; vector_search filters retired chunks */",
            [],
            |row| row.get(0),
        )?;
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM chunks /* all-episodes: index upkeep; vector_search filters retired chunks */", [], |row| row.get(0))?;
        Ok((embedded, total))
    }

    /// One ANN-index backfill batch: embedded chunks the index misses,
    /// oldest rowids first. Returns rows indexed (< `limit` ⇒ converged).
    /// Naturally resumable — only missing rows are ever selected.
    pub fn vec_index_backfill_batch(&self, limit: usize) -> Result<usize> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT c.rowid, c.embedding FROM chunks c /* all-episodes: index upkeep; vector_search filters retired chunks */
             WHERE c.embedding IS NOT NULL
               AND length(c.embedding) = ?1
               AND c.rowid NOT IN (SELECT rowid FROM vec_chunks)
             LIMIT ?2",
        )?;
        let rows: Vec<(i64, Vec<u8>)> = stmt
            .query_map(rusqlite::params![(VEC_DIM * 4) as i64, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let tx = conn.unchecked_transaction()?;
        for (rowid, blob) in &rows {
            tx.execute(
                "INSERT INTO vec_chunks(rowid, embedding) VALUES (?1, ?2)",
                rusqlite::params![rowid, blob],
            )?;
        }
        tx.commit()?;
        Ok(rows.len())
    }

    /// Does the ANN index cover every embedded 384-dim chunk? Sticky once
    /// true — dual-write keeps parity and chunks are never deleted — so
    /// the coverage scan runs only until the first convergence.
    pub fn vec_index_ready(&self) -> Result<bool> {
        if self.vec_ready.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let missing: i64 = {
            let conn = self.lock();
            conn.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM chunks c /* all-episodes: index upkeep; vector_search filters retired chunks */
                    WHERE c.embedding IS NOT NULL
                      AND length(c.embedding) = ?1
                      AND c.rowid NOT IN (SELECT rowid FROM vec_chunks))",
                [(VEC_DIM * 4) as i64],
                |r| r.get(0),
            )?
        };
        let ready = missing == 0;
        if ready {
            self.vec_ready.store(true, Ordering::Relaxed);
        }
        Ok(ready)
    }

    /// Vector search; episodes scored by their best chunk. ANN once the
    /// index covers the embedded chunks, brute-force otherwise.
    pub fn vector_search(&self, query: &[f32], limit: usize) -> Result<Vec<VectorHit>> {
        if query.len() == VEC_DIM && self.vec_index_ready()? {
            return self.vector_search_ann(query, limit);
        }
        self.vector_search_brute(query, limit)
    }

    /// KNN over vec_chunks. Oversampled: several chunks per episode plus
    /// scope/supersession filtering both shrink the deduped set. Cosine
    /// distance ⇒ similarity = 1 − distance, matching the brute-force
    /// scores on L2-normalized embeddings.
    fn vector_search_ann(&self, query: &[f32], limit: usize) -> Result<Vec<VectorHit>> {
        let k = (limit.max(1) * 8).min(1024);
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT c.episode_id, c.text, knn.distance
             FROM (SELECT rowid, distance FROM vec_chunks
                   WHERE embedding MATCH ?1 AND k = ?2
                   ORDER BY distance) knn
             JOIN chunks c ON c.rowid = knn.rowid
             JOIN episodes e ON e.id = c.episode_id
             WHERE e.superseded_by IS NULL{guard}
             ORDER BY knn.distance"
        ))?;
        let rows: Vec<(String, String, f64)> = stmt
            .query_map(rusqlite::params![floats_to_bytes(query), k as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        drop(conn);

        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for (episode_id, chunk_text, distance) in rows {
            if seen.insert(episode_id.clone()) {
                hits.push(VectorHit {
                    episode_id,
                    similarity: 1.0 - distance as f32,
                    chunk_text,
                });
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    /// Brute-force cosine over the BLOB column (the pre-v49 engine; kept
    /// as the fallback and for off-dimension vectors).
    fn vector_search_brute(&self, query: &[f32], limit: usize) -> Result<Vec<VectorHit>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT c.episode_id, c.text, c.embedding FROM chunks c
             JOIN episodes e ON e.id = c.episode_id
             WHERE c.embedding IS NOT NULL AND e.superseded_by IS NULL{guard}"
        ))?;
        let rows: Vec<(String, String, Vec<u8>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        drop(conn);

        let mut best: std::collections::HashMap<String, (f32, String)> =
            std::collections::HashMap::new();
        for (episode_id, text, blob) in rows {
            let similarity = cosine_similarity(query, &bytes_to_floats(&blob));
            let entry = best.entry(episode_id).or_insert((f32::MIN, String::new()));
            if similarity > entry.0 {
                *entry = (similarity, text);
            }
        }
        let mut hits: Vec<VectorHit> = best
            .into_iter()
            .map(|(episode_id, (similarity, chunk_text))| VectorHit {
                episode_id,
                similarity,
                chunk_text,
            })
            .collect();
        hits.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        hits.truncate(limit);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::episodes::{insert_episode, EpisodeRow};
    use crate::time::now_utc;

    fn seed(store: &Store, id: &str, body: &str) {
        seed_scoped(store, id, body, "work");
    }

    fn seed_scoped(store: &Store, id: &str, body: &str, scope: &str) {
        let conn = store.lock();
        insert_episode(
            &conn,
            &EpisodeRow {
                id: id.into(),
                source: "test".into(),
                source_ref: id.into(),
                kind: "note".into(),
                title: id.into(),
                actor: "human:test".into(),
                event_time: None,
                tz_offset_minutes: None,
                time_uncertainty: "unknown".into(),
                ingest_time: now_utc(),
                trust: "self".into(),
                scope: scope.into(),
                derivation: "observed".into(),
                content_ref: None,
                content_hash: "h".into(),
            },
            None,
            body,
        )
        .unwrap();
    }

    /// Unit vector with 1.0 in one slot — 384-dim so it enters the index.
    fn axis(i: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; VEC_DIM];
        v[i] = 1.0;
        v
    }

    #[test]
    fn test_backfill_and_search() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "ep_a", "alpha text");
        seed(&store, "ep_b", "beta text");

        let pending = store.chunks_needing_embedding(10).unwrap();
        assert_eq!(pending.len(), 2);
        store
            .set_chunk_embedding(&pending[0].0, &[1.0, 0.0])
            .unwrap();
        store
            .set_chunk_embedding(&pending[1].0, &[0.0, 1.0])
            .unwrap();
        assert_eq!(store.embedding_coverage().unwrap(), (2, 2));
        assert!(store.chunks_needing_embedding(10).unwrap().is_empty());

        let hits = store.vector_search(&[0.9, 0.1], 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].similarity > hits[1].similarity);
    }

    #[test]
    fn test_ann_index_dual_write_and_knn() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "ep_a", "alpha text");
        seed(&store, "ep_b", "beta text");
        seed_scoped(&store, "ep_p", "private text", "private");

        let pending = store.chunks_needing_embedding(10).unwrap();
        assert_eq!(pending.len(), 3);
        for (chunk_id, _) in &pending {
            let ep = chunk_id.rsplit_once('_').unwrap().0;
            let axis_i = match ep {
                "ep_a" => 0,
                "ep_b" => 1,
                _ => 2,
            };
            store.set_chunk_embedding(chunk_id, &axis(axis_i)).unwrap();
        }
        // Dual-write covered everything — no backfill needed, ANN ready.
        assert_eq!(store.vec_index_backfill_batch(100).unwrap(), 0);
        assert!(store.vec_index_ready().unwrap());

        // Query near ep_a's axis: ANN path, best-chunk-per-episode,
        // private hidden while the vault is locked.
        let mut q = axis(0);
        q[1] = 0.2;
        let hits = store.vector_search(&q, 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].episode_id, "ep_a");
        assert!(hits[0].similarity > hits[1].similarity);
        assert!(hits[0].similarity > 0.9);
        assert!(!hits.iter().any(|h| h.episode_id == "ep_p"));

        // Unlocked, the private episode surfaces.
        store.vault_unlock(5);
        let hits = store.vector_search(&axis(2), 10).unwrap();
        assert_eq!(hits[0].episode_id, "ep_p");
        store.vault_lock();
    }

    #[test]
    fn test_ann_backfill_converges() {
        let store = Store::open_in_memory().unwrap();
        seed(&store, "ep_a", "alpha text");
        seed(&store, "ep_b", "beta text");
        let pending = store.chunks_needing_embedding(10).unwrap();
        // Write embeddings directly to the BLOB column (simulating a
        // pre-v49 store) so the index starts behind.
        {
            let conn = store.lock();
            for (i, (chunk_id, _)) in pending.iter().enumerate() {
                conn.execute(
                    "UPDATE chunks SET embedding = ?1 WHERE id = ?2",
                    rusqlite::params![floats_to_bytes(&axis(i)), chunk_id],
                )
                .unwrap();
            }
        }
        assert!(!store.vec_index_ready().unwrap());

        assert_eq!(store.vec_index_backfill_batch(1).unwrap(), 1);
        assert_eq!(store.vec_index_backfill_batch(10).unwrap(), 1);
        assert_eq!(store.vec_index_backfill_batch(10).unwrap(), 0);
        assert!(store.vec_index_ready().unwrap());

        let hits = store.vector_search(&axis(0), 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].episode_id, "ep_a");
    }
}
