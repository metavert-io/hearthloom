//! FTS5 search over episodes (BM25, AND-then-OR fallback — the HearthFS
//! pattern: natural-language queries return nothing under implicit AND, so
//! retry with informative terms ORed).

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::Store;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub episode_id: String,
    pub title: String,
    pub snippet: String,
    pub rank: f64,
    pub source: String,
    pub kind: String,
    pub event_time: Option<String>,
    pub scope: String,
    pub trust: String,
}

/// Weighted reciprocal-rank fusion constants — the HearthFS retrieval-benchmark finding, shared
/// with HearthFS: a small local embedder is a recall backfill for queries
/// FTS misses, not a ranker; equal weighting measurably degrades precision.
const RRF_K: f64 = 60.0;
const VEC_WEIGHT: f64 = 0.05;

impl Store {
    /// Hybrid search: BM25 + (optionally) vector recall, fused with
    /// weighted RRF. Pass the query embedding when a local embedder is
    /// loaded; without one this is plain FTS.
    pub fn search_hybrid(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let fts = self.search(query, limit.max(20))?;
        let Some(qv) = query_embedding else {
            let mut fts = fts;
            fts.truncate(limit);
            return Ok(fts);
        };
        let vector = self.vector_search(qv, limit.max(20))?;

        let mut fused: Vec<(String, f64)> = Vec::new();
        let mut bump = |episode_id: &str, rank: usize, weight: f64| {
            let score = weight / (RRF_K + rank as f64 + 1.0);
            if let Some(entry) = fused.iter_mut().find(|(id, _)| id == episode_id) {
                entry.1 += score;
            } else {
                fused.push((episode_id.to_string(), score));
            }
        };
        for (rank, hit) in fts.iter().enumerate() {
            bump(&hit.episode_id, rank, 1.0);
        }
        for (rank, hit) in vector.iter().enumerate() {
            bump(&hit.episode_id, rank, VEC_WEIGHT);
        }
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        fused.truncate(limit);

        let mut results = Vec::with_capacity(fused.len());
        for (episode_id, score) in fused {
            if let Some(hit) = fts.iter().find(|h| h.episode_id == episode_id) {
                let mut hit = hit.clone();
                hit.rank = score;
                results.push(hit);
                continue;
            }
            // Vector-only hit: build a SearchHit from the episode row,
            // with the best chunk standing in for the snippet.
            let Some(episode) = self.episode(&episode_id)? else {
                continue;
            };
            let snippet = vector
                .iter()
                .find(|v| v.episode_id == episode_id)
                .map(|v| {
                    let text = v.chunk_text.replace('\n', " ");
                    let mut cut = text.len().min(160);
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text[..cut].to_string()
                })
                .unwrap_or_default();
            results.push(SearchHit {
                episode_id,
                title: episode.title,
                snippet,
                rank: score,
                source: episode.source,
                kind: episode.kind,
                event_time: episode.event_time,
                scope: episode.scope,
                trust: episode.trust,
            });
        }
        Ok(results)
    }

    /// Search episodes; results ranked by BM25.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let sanitized = sanitize_fts_query(query);
        let mut results = self.run_match(&sanitized, limit)?;
        if results.len() < limit {
            if let Some(or_query) = or_fallback_query(query) {
                for hit in self.run_match(&or_query, limit)? {
                    if results.len() >= limit {
                        break;
                    }
                    if !results.iter().any(|r| r.episode_id == hit.episode_id) {
                        results.push(hit);
                    }
                }
            }
        }
        Ok(results)
    }

    fn run_match(&self, match_expr: &str, limit: usize) -> Result<Vec<SearchHit>> {
        if match_expr.is_empty() {
            return Ok(vec![]);
        }
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT f.episode_id, e.title, snippet(episode_fts, 1, '»', '«', '...', 32),
                    bm25(episode_fts), e.source, e.kind, e.event_time, e.scope, e.trust
             FROM episode_fts f
             JOIN episodes e ON e.id = f.episode_id
             WHERE episode_fts MATCH ?1 AND e.superseded_by IS NULL{guard}
             ORDER BY bm25(episode_fts)
             LIMIT ?2"
        ))?;
        let rows = stmt.query_map(rusqlite::params![match_expr, limit as i64], |row| {
            Ok(SearchHit {
                episode_id: row.get(0)?,
                title: row.get(1)?,
                snippet: row.get(2)?,
                rank: row.get(3)?,
                source: row.get(4)?,
                kind: row.get(5)?,
                event_time: row.get(6)?,
                scope: row.get(7)?,
                trust: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// Wrap terms containing FTS5-special characters in double quotes.
pub(crate) fn sanitize_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(sanitize_term)
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_term(term: &str) -> String {
    if term.contains(|c: char| !c.is_alphanumeric() && c != '_') {
        format!("\"{}\"", term.replace('"', ""))
    } else {
        term.to_string()
    }
}

/// OR the informative terms of a natural-language query; None if nothing
/// survives stopword filtering.
fn or_fallback_query(query: &str) -> Option<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "but", "by", "did", "do", "does", "for", "from",
        "had", "has", "have", "how", "i", "in", "is", "it", "its", "me", "my", "of", "on", "or",
        "that", "the", "their", "them", "then", "there", "these", "they", "this", "to", "was",
        "we", "were", "what", "when", "where", "which", "who", "whom", "why", "will", "with",
    ];
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(&t.to_lowercase().as_str()))
        .map(sanitize_term)
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::episodes::{insert_episode, EpisodeRow};
    use crate::time::now_utc;

    fn seed(store: &Store, id: &str, title: &str, body: &str) {
        let conn = store.lock();
        insert_episode(
            &conn,
            &EpisodeRow {
                id: id.into(),
                source: "test".into(),
                source_ref: id.into(),
                kind: "note".into(),
                title: title.into(),
                actor: "human:test".into(),
                event_time: Some("2026-03-15T12:00:00Z".into()),
                tz_offset_minutes: Some(0),
                time_uncertainty: "exact".into(),
                ingest_time: now_utc(),
                trust: "self".into(),
                scope: "work".into(),
                derivation: "observed".into(),
                content_ref: None,
                content_hash: "h".into(),
            },
            None,
            body,
        )
        .unwrap();
    }

    #[test]
    fn test_search_and_or_fallback() {
        let store = Store::open_in_memory().unwrap();
        seed(
            &store,
            "ep_1",
            "Fix auth bug",
            "Fixed the token refresh race in auth.rs",
        );
        seed(
            &store,
            "ep_2",
            "Standup notes",
            "Discussed the roadmap for entity resolution",
        );

        let hits = store.search("auth", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].episode_id, "ep_1");

        // Natural-language query: AND finds nothing, OR fallback does.
        let hits = store
            .search("what did we decide about the entity resolution roadmap", 10)
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].episode_id, "ep_2");
    }
}
