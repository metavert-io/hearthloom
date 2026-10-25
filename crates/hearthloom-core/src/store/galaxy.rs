//! The Galaxy: the record seen whole. Sources are suns; the entities
//! each source cites most are its planets; an entity drawing evidence
//! from several sources hangs BETWEEN them (identity resolution already
//! merged the "facebook Jane" and "email Jane" — the layout shows
//! the bridge instead of drawing sameness wires). Co-citation links
//! planets that keep appearing together; semantic comet pairs (computed
//! at idle, never here — law #3) tie regions by meaning.
//!
//! Perf shape: two grouped queries over a MATERIALIZED window CTE (the
//! relationship-directory lesson: drive from dated episodes INTO the
//! link table), one json_each-bounded co-citation pass, one small read
//! of the precomputed pairs. No traversal, hard caps everywhere.

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::Store;
use crate::embeddings::{bytes_to_floats, cosine_similarity};

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyNode {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub importance: i64,
    pub total: i64,
    /// (source, evidence count) — the gravity spokes.
    pub spokes: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyLink {
    pub a: String,
    pub b: String,
    pub n: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CometEnd {
    pub episode_id: String,
    pub title: String,
    pub day: String,
    pub source: String,
    /// Selected-node ids this episode cites (anchors for layout).
    pub entities: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyComet {
    pub a: CometEnd,
    pub b: CometEnd,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyNote {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    /// Entity ids this note @-mentions (connective lines to planets).
    pub mentions: Vec<String>,
    pub views: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GalaxyData {
    pub sources: Vec<(String, i64)>,
    pub nodes: Vec<GalaxyNode>,
    pub links: Vec<GalaxyLink>,
    pub comets: Vec<GalaxyComet>,
    /// Live notes — the desk's thinking, orbiting its own sun.
    pub notes: Vec<GalaxyNote>,
}

impl Store {
    /// The whole-record map for a time window. Vault-guarded; the
    /// record's owner (kind='me') is excluded — they would connect
    /// everything and collapse the galaxy into a single star.
    pub fn galaxy(&self, from: &str, to: &str, per_source: usize) -> Result<GalaxyData> {
        let guard = self.scope_guard("e.scope");
        let entity_guard = self.scope_guard("en.scope");
        let conn = self.lock();

        let me: String = conn
            .query_row(
                "SELECT entity_id FROM relationships
                 WHERE kind = 'me' AND status != 'rejected' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default();

        // 1. Suns: episode volume per source in the window.
        let mut stmt = conn.prepare(&format!(
            "SELECT e.source, COUNT(*) FROM episodes e
             WHERE substr(COALESCE(e.event_time, e.ingest_time), 1, 10) BETWEEN ?1 AND ?2
               AND e.superseded_by IS NULL{guard}
             GROUP BY e.source ORDER BY 2 DESC LIMIT 14"
        ))?;
        let sources: Vec<(String, i64)> = stmt
            .query_map([from, to], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let source_set: std::collections::HashSet<&str> =
            sources.iter().map(|(s, _)| s.as_str()).collect();

        // 2. Planets: per-(entity, source) evidence, windowed-first.
        let mut stmt = conn.prepare(&format!(
            "WITH w AS MATERIALIZED (
                 SELECT e.id, e.source FROM episodes e
                 WHERE substr(COALESCE(e.event_time, e.ingest_time), 1, 10)
                       BETWEEN ?1 AND ?2 AND e.superseded_by IS NULL{guard}
             )
             SELECT en.id, en.name, en.type, en.declared_importance, w.source, COUNT(*) AS n
             FROM w
             CROSS JOIN episode_entities ee ON ee.episode_id = w.id
             CROSS JOIN entities en ON en.id = ee.entity_id
             -- Projects orbit too (Jon: the git sun should rank REPOS
             -- by commit volume — each commit cites its repo Project,
             -- so within-source citations ARE commit counts)
             WHERE en.type IN ('Person', 'Org', 'Group', 'Project')
               AND en.merged_into IS NULL AND en.id != ?3{entity_guard}
             GROUP BY en.id, w.source"
        ))?;
        type SpokeRow = (String, String, String, i64, String, i64);
        let spoke_rows: Vec<SpokeRow> = stmt
            .query_map(rusqlite::params![from, to, me], |row| {
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

        // Rank per source, take the top K of each, then union.
        let mut per_source_rank: std::collections::HashMap<&str, Vec<&SpokeRow>> =
            std::collections::HashMap::new();
        for row in &spoke_rows {
            if source_set.contains(row.4.as_str()) {
                per_source_rank.entry(row.4.as_str()).or_default().push(row);
            }
        }
        let mut selected: std::collections::HashSet<String> = std::collections::HashSet::new();
        for rows in per_source_rank.values_mut() {
            rows.sort_by_key(|r| std::cmp::Reverse(r.5));
            for row in rows.iter().take(per_source) {
                selected.insert(row.0.clone());
            }
        }
        let mut nodes: std::collections::HashMap<String, GalaxyNode> =
            std::collections::HashMap::new();
        for (id, name, entity_type, importance, source, n) in &spoke_rows {
            if !selected.contains(id) || !source_set.contains(source.as_str()) {
                continue;
            }
            let node = nodes.entry(id.clone()).or_insert_with(|| GalaxyNode {
                id: id.clone(),
                name: name.clone(),
                entity_type: entity_type.clone(),
                importance: *importance,
                total: 0,
                spokes: Vec::new(),
            });
            node.total += n;
            node.spokes.push((source.clone(), *n));
        }
        let mut nodes: Vec<GalaxyNode> = nodes.into_values().collect();
        nodes.sort_by_key(|n| std::cmp::Reverse(n.total));
        nodes.truncate(120); // hard cap regardless of source count

        // 3. Co-citation links among the selected planets.
        let id_json = serde_json::to_string(
            &nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
        )?;
        let mut stmt = conn.prepare(&format!(
            "SELECT ee1.entity_id, ee2.entity_id, COUNT(DISTINCT ee1.episode_id) AS n
             FROM episode_entities ee1
             JOIN episode_entities ee2 ON ee2.episode_id = ee1.episode_id
                  AND ee2.entity_id > ee1.entity_id
             JOIN episodes e ON e.id = ee1.episode_id
             WHERE ee1.entity_id IN (SELECT value FROM json_each(?1))
               AND ee2.entity_id IN (SELECT value FROM json_each(?1))
               AND substr(COALESCE(e.event_time, e.ingest_time), 1, 10)
                   BETWEEN ?2 AND ?3 AND e.superseded_by IS NULL{guard}
             GROUP BY 1, 2 HAVING n >= 2
             ORDER BY n DESC LIMIT 150"
        ))?;
        let links: Vec<GalaxyLink> = stmt
            .query_map(rusqlite::params![id_json, from, to], |row| {
                Ok(GalaxyLink {
                    a: row.get(0)?,
                    b: row.get(1)?,
                    n: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;

        // 4. Comets: the idle-computed semantic pairs, windowed + guarded.
        let mut stmt = conn.prepare(&format!(
            "SELECT p.a, ea.title, substr(COALESCE(ea.event_time, ea.ingest_time), 1, 10), ea.source,
                    p.b, eb.title, substr(COALESCE(eb.event_time, eb.ingest_time), 1, 10), eb.source,
                    p.score
             FROM semantic_pairs p
             JOIN episodes ea ON ea.id = p.a{}
             JOIN episodes eb ON eb.id = p.b{}
             WHERE substr(COALESCE(ea.event_time, ea.ingest_time), 1, 10) BETWEEN ?1 AND ?2
               AND substr(COALESCE(eb.event_time, eb.ingest_time), 1, 10) BETWEEN ?1 AND ?2
               AND ea.superseded_by IS NULL AND eb.superseded_by IS NULL
             ORDER BY p.score DESC LIMIT 16",
            self.scope_guard("ea.scope"),
            self.scope_guard("eb.scope"),
        ))?;
        type PairRow = (String, String, String, String, String, String, String, String, f64);
        let pair_rows: Vec<PairRow> = stmt
            .query_map([from, to], |row| {
                Ok((
                    row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                    row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?,
                    row.get(8)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let cited = |episode: &str| -> Result<Vec<String>> {
            let mut stmt = conn.prepare(
                "SELECT entity_id FROM episode_entities /* all-episodes: links of a pair's live episode */
                 WHERE episode_id = ?1
                   AND entity_id IN (SELECT value FROM json_each(?2)) LIMIT 3",
            )?;
            let rows = stmt.query_map([episode, id_json.as_str()], |row| row.get(0))?;
            Ok(rows.collect::<std::result::Result<_, _>>()?)
        };
        let mut comets = Vec::new();
        for (a_id, a_title, a_day, a_src, b_id, b_title, b_day, b_src, score) in pair_rows {
            comets.push(GalaxyComet {
                a: CometEnd {
                    entities: cited(&a_id)?,
                    episode_id: a_id,
                    title: a_title,
                    day: a_day,
                    source: a_src,
                },
                b: CometEnd {
                    entities: cited(&b_id)?,
                    episode_id: b_id,
                    title: b_title,
                    day: b_day,
                    source: b_src,
                },
                score,
            });
        }

        // 5. Live notes: most-read first, with their @-mention links.
        // Notes are FILES, not episodes — they get their own sun.
        let note_guard = self.scope_guard("n.scope");
        let mut stmt = conn.prepare(&format!(
            "SELECT n.id, n.title, n.tags, n.views FROM notes_index n
             WHERE n.status != 'deleted' AND n.kind = 'note'{note_guard}
             ORDER BY n.views DESC, n.updated_at DESC LIMIT 36"
        ))?;
        let note_rows: Vec<(String, String, Option<String>, i64)> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut notes = Vec::new();
        for (id, title, tags, views) in note_rows {
            let mut stmt = conn.prepare(
                "SELECT entity_id FROM note_mentions WHERE note_id = ?1 LIMIT 6",
            )?;
            let mentions: Vec<String> = stmt
                .query_map([&id], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            notes.push(GalaxyNote {
                id,
                title,
                tags: tags
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or_default(),
                mentions,
                views,
            });
        }

        Ok(GalaxyData {
            sources,
            nodes,
            links,
            comets,
            notes,
        })
    }

    /// Idle pass: recompute the semantic comet pairs from EXISTING
    /// chunk embeddings (content vectors — never name embeddings, the
    /// merge-scan lesson). The feed is the top entity-rich episodes
    /// PER SOURCE (a global notable list is all monthly chapters —
    /// kind weight 100 — one source, zero cross-source pairs; found
    /// live 2026-07-10). Meta-episodes (chapters, cascades, digests,
    /// health aggregates) are excluded: comets tie real moments.
    /// Same-source pairs and near-duplicates (>0.95) are dropped.
    /// Full replace.
    ///
    /// The candidate feed aggregates episode_entities ONCE (one pass over
    /// its episode_id-led autoindex) and probes episodes by primary key —
    /// never a correlated COUNT per episode: that form re-scanned the
    /// index 252k times and cost 31s/cycle on the live substrate
    /// (measured 2026-08-16; this form 2.7s). Zero-entity episodes drop
    /// out of the feed by construction, which is the stated intent
    /// ("entity-rich") — they only ever appeared as filler in sparse
    /// sources.
    pub fn refresh_semantic_pairs(&self, keep: usize) -> Result<usize> {
        let embeddings: Vec<(String, String, Vec<f32>)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "WITH counts AS (
                     SELECT episode_id, COUNT(*) AS n
                     FROM episode_entities GROUP BY episode_id
                 ),
                 ranked AS (
                     SELECT e.id, e.source,
                            ROW_NUMBER() OVER (
                                PARTITION BY e.source
                                ORDER BY cnt.n DESC
                            ) AS rn
                     FROM counts cnt
                     JOIN episodes e ON e.id = cnt.episode_id
                     WHERE e.scope != 'private' AND e.superseded_by IS NULL
                       AND e.kind NOT IN ('chapter', 'cascade', 'daily_digest',
                                          'health_day', 'note', 'task_done', 'workout')
                 )
                 SELECT r.id, r.source, c.embedding FROM ranked r
                 JOIN chunks c ON c.episode_id = r.id AND c.embedding IS NOT NULL
                 WHERE r.rn <= 15
                   AND length(c.text) >= 240 -- substance: short blurbs rhyme trivially
                 GROUP BY r.id",
            )?;
            let rows: Vec<(String, String, Vec<u8>)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows.into_iter()
                .map(|(id, source, blob)| (id, source, bytes_to_floats(&blob)))
                .collect()
        }; // lock released before the O(n²) math

        let mut scored: Vec<(usize, usize, f32)> = Vec::new();
        for i in 0..embeddings.len() {
            for j in (i + 1)..embeddings.len() {
                if embeddings[i].1 == embeddings[j].1 {
                    continue; // same source — not a bridge
                }
                let s = cosine_similarity(&embeddings[i].2, &embeddings[j].2);
                if (0.45..=0.95).contains(&s) {
                    scored.push((i, j, s));
                }
            }
        }
        scored.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(keep);

        let conn = self.lock();
        conn.execute("DELETE FROM semantic_pairs", [])?;
        let now = crate::time::now_utc();
        for (i, j, s) in &scored {
            conn.execute(
                "INSERT OR IGNORE INTO semantic_pairs (a, b, score, computed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![embeddings[*i].0, embeddings[*j].0, *s as f64, now],
            )?;
        }
        Ok(scored.len())
    }
}
