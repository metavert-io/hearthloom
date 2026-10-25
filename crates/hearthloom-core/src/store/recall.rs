//! Temporal recall: the day view. "What was I doing on X?" is episodes on
//! that local day (event time shifted by the recorded tz offset) plus the
//! edges valid on that date — the payoff of storing offsets and the
//! bi-temporal model.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use super::episodes::{episode_from_row, EpisodeRow, EPISODE_COLUMNS};
use super::graph::{entity_row, merge_group, resolve_merged};
use super::{EntityRow, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayView {
    pub date: String,
    pub episodes: Vec<DayEpisode>,
    /// Edges valid on this date among entities the day's episodes touch.
    pub valid_edges: Vec<DayEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayEpisode {
    #[serde(flatten)]
    pub episode: EpisodeRow,
    /// (entity name, type, role)
    pub entities: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayEdge {
    pub src: EntityRow,
    pub relation: String,
    pub dst: EntityRow,
    pub t_valid: Option<String>,
}

impl Store {
    /// Episodes whose *local* event date is `date` (YYYY-MM-DD), plus the
    /// valid edges among the entities involved.
    pub fn recall_day(&self, date: &str) -> Result<DayView> {
        if date.len() != 10 || date.as_bytes()[4] != b'-' || date.as_bytes()[7] != b'-' {
            return Err(anyhow!("date must be YYYY-MM-DD, got {date:?}"));
        }
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EPISODE_COLUMNS} FROM episodes
             WHERE event_time IS NOT NULL AND superseded_by IS NULL{guard}
               AND date(datetime(event_time,
                     printf('%+d minutes', COALESCE(tz_offset_minutes, 0)))) = ?1
             ORDER BY event_time"
        ))?;
        let episodes: Vec<EpisodeRow> = stmt
            .query_map([date], episode_from_row)?
            .collect::<std::result::Result<_, _>>()?;

        let mut day_episodes = Vec::new();
        let mut entity_ids = std::collections::BTreeSet::new();
        for episode in episodes {
            let mut stmt = conn.prepare(
                "SELECT e.id, ee.role
                 FROM episode_entities ee JOIN entities e ON e.id = ee.entity_id /* all-episodes: the links of a live episode */
                 WHERE ee.episode_id = ?1 ORDER BY ee.role, e.name",
            )?;
            let rows: Vec<(String, String)> = stmt
                .query_map([&episode.id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            let hidden = self.scope_hider();
            let mut entities = Vec::new();
            for (id, role) in rows {
                // Merged identities display as their surviving entity.
                let winner = resolve_merged(&conn, &id)?;
                let Some(row) = entity_row(&conn, &winner)? else {
                    continue;
                };
                if hidden(&row.scope) {
                    continue;
                }
                entity_ids.insert(winner);
                entities.push((row.name, row.entity_type, role));
            }
            day_episodes.push(DayEpisode { episode, entities });
        }

        // Edges valid at end-of-day among the involved entities.
        let as_of = format!("{date}T23:59:59Z");
        let mut valid_edges = Vec::new();
        if !entity_ids.is_empty() {
            let guard = self.scope_guard("scope");
            let mut stmt = conn.prepare(&format!(
                "SELECT src, relation, dst, t_valid FROM edges
                 WHERE t_expired IS NULL{guard}
                   AND (t_valid IS NULL OR t_valid <= ?1)
                   AND (t_invalid IS NULL OR t_invalid > ?1)"
            ))?;
            let rows: Vec<(String, String, String, Option<String>)> = stmt
                .query_map([&as_of], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            for (src, relation, dst, t_valid) in rows {
                let src = resolve_merged(&conn, &src)?;
                let dst = resolve_merged(&conn, &dst)?;
                if !entity_ids.contains(&src) && !entity_ids.contains(&dst) {
                    continue;
                }
                let (Some(src_row), Some(dst_row)) =
                    (entity_row(&conn, &src)?, entity_row(&conn, &dst)?)
                else {
                    continue;
                };
                valid_edges.push(DayEdge {
                    src: src_row,
                    relation,
                    dst: dst_row,
                    t_valid,
                });
            }
        }

        Ok(DayView {
            date: date.to_string(),
            episodes: day_episodes,
            valid_edges,
        })
    }

    /// Episodes whose local event date falls in [from, to] (YYYY-MM-DD,
    /// inclusive), oldest first.
    pub fn recall_span(&self, from: &str, to: &str, limit: usize) -> Result<Vec<EpisodeRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EPISODE_COLUMNS} FROM episodes
             WHERE event_time IS NOT NULL AND superseded_by IS NULL{guard}
               AND date(datetime(event_time,
                     printf('%+d minutes', COALESCE(tz_offset_minutes, 0))))
                   BETWEEN ?1 AND ?2
             ORDER BY event_time LIMIT ?3"
        ))?;
        let rows = stmt.query_map(rusqlite::params![from, to, limit as i64], episode_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Episodes citing an entity, newest first.
    pub fn recall_entity(&self, name: &str, limit: usize) -> Result<Vec<EpisodeRow>> {
        let Some(entity) = self.entity_by_name(name)? else {
            return Err(anyhow!("unknown entity: {name}"));
        };
        // A private entity is unknown while the vault is locked.
        if self.scope_hidden(&entity.scope) {
            return Err(anyhow!("unknown entity: {name}"));
        }
        let guard = self.scope_guard("ep.scope");
        let conn = self.lock();
        // Episodes citing any member of the merge group count.
        let group = merge_group(&conn, &entity.id)?;
        let marks = vec!["?"; group.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM episodes ep
             JOIN episode_entities ee ON ee.episode_id = ep.id
             WHERE ee.entity_id IN ({marks}) AND ep.superseded_by IS NULL{guard}
             GROUP BY ep.id
             ORDER BY COALESCE(ep.event_time, ep.ingest_time) DESC LIMIT ?",
            EPISODE_COLUMNS
                .split(", ")
                .map(|c| format!("ep.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let params: Vec<String> = group
            .into_iter()
            .chain(std::iter::once((limit as i64).to_string()))
            .collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params), episode_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::episodes::insert_episode;
    use crate::time::now_utc;

    #[test]
    fn test_day_view_respects_local_offset() {
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            // 2026-03-16 01:30 UTC is 2026-03-15 21:30 in EDT (-240).
            insert_episode(
                &conn,
                &EpisodeRow {
                    id: "ep_evening".into(),
                    source: "test".into(),
                    source_ref: "1".into(),
                    kind: "commit".into(),
                    title: "late night commit".into(),
                    actor: "adapter:git".into(),
                    event_time: Some("2026-03-16T01:30:00Z".into()),
                    tz_offset_minutes: Some(-240),
                    time_uncertainty: "exact".into(),
                    ingest_time: now_utc(),
                    trust: "self".into(),
                    scope: "work".into(),
                    derivation: "observed".into(),
                    content_ref: None,
                    content_hash: "h1".into(),
                },
                None,
                "body",
            )
            .unwrap();
        }
        // Local day is the 15th, not the 16th.
        let day = store.recall_day("2026-03-15").unwrap();
        assert_eq!(day.episodes.len(), 1);
        assert_eq!(day.episodes[0].episode.id, "ep_evening");
        let day = store.recall_day("2026-03-16").unwrap();
        assert!(day.episodes.is_empty());
    }

    #[test]
    fn test_bad_date_rejected() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.recall_day("March 15").is_err());
    }
}
