//! Intents: standing queries — Orrery's continuous-monitoring serendipity,
//! rebuilt on the episode log. The consolidation cycle checks each intent
//! against episodes ingested since its watermark; matches are recorded
//! (and surfaced on the change feed) but cascades stay lazy: expansion
//! happens when the matches are *viewed*, not when they're found.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::episodes::{episode_from_row, EpisodeRow, EPISODE_COLUMNS};
use super::{record_mutation, Actor, Store};
use crate::embeddings::{bytes_to_floats, cosine_similarity, floats_to_bytes};
use crate::ids;
use crate::time::now_utc;

/// Semantic-leg match floor: a standing query's embedding must be at
/// least this close to some chunk of a new episode. Conservative — the
/// FTS legs carry precision; this leg is recall for phrasing variance.
const SEMANTIC_MATCH_THRESHOLD: f32 = 0.60;
/// Chunk rows examined per run on the semantic leg (memory/latency cap;
/// a giant one-time ingest simply gets partial semantic coverage).
const SEMANTIC_CHUNK_CAP: usize = 4000;
/// Max episode ids per IN-list. SQLite caps bound variables at 32766 —
/// a bulk sync delivery (48k episodes landing on a fresh peer) must
/// chunk its watermark backlog, not abort the consolidation cycle.
const IN_LIST_CHUNK: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentRow {
    pub id: String,
    pub name: String,
    pub query: String,
    pub created_at: String,
    pub last_checked: Option<String>,
    pub unseen_matches: i64,
    pub matches: i64,
    /// Standing queries: LLM-decomposed at setup, human-editable after.
    pub sub_queries: Vec<String>,
    /// Goal fields (v15): intents grown up.
    pub description: Option<String>,
    pub status: String,
    pub sphere: Option<String>,
    pub target_date: Option<String>,
    /// v29: Projects this goal advances (names) — many-to-many.
    pub projects: Vec<String>,
    /// v29: the note that is this goal's 1:1 description (id, title).
    pub primary_note: Option<(String, String)>,
}

/// FTS5-safe form of a standing query: every token quoted (hyphens,
/// apostrophes and stray operators would otherwise be syntax), implicit
/// AND between tokens. None when nothing tokenizable survives.
fn fts_safe(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentMatch {
    pub episode: EpisodeRow,
    pub found_at: String,
    pub seen: bool,
}

impl Store {
    pub fn add_intent(&self, actor: &Actor, name: &str, query: &str) -> Result<String> {
        let conn = self.lock();
        let id = ids::intent_id();
        // New intents start at the current cursor: they watch the future,
        // not the backlog (run a plain search for the past).
        let last_seq: i64 =
            conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM mutations", [], |row| {
                row.get(0)
            })?;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO intents (id, name, query, created_at, last_seq)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, name, query, now_utc(), last_seq],
        )?;
        if changed == 0 {
            return Err(anyhow!("intent {name:?} already exists"));
        }
        record_mutation(
            &conn,
            actor,
            "intent_added",
            &format!("intent_added:{id}"),
            &json!({"id": id, "name": name, "query": query}),
        )?;
        super::journal_row(&conn, actor, "intent", "intents", &id, "create")?;
        Ok(id)
    }

    pub fn remove_intent(&self, actor: &Actor, name: &str) -> Result<()> {
        let conn = self.lock();
        let id: Option<String> = conn
            .query_row("SELECT id FROM intents WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .ok();
        let id = id.ok_or_else(|| anyhow!("no intent named {name:?}"))?;
        // Journal a delete-tombstone with the row's content BEFORE the row is
        // gone, so even a removed intent/goal stays restorable.
        super::journal_row(&conn, actor, "intent", "intents", &id, "delete")?;
        conn.execute("DELETE FROM intents WHERE id = ?1", [&id])?;
        conn.execute("DELETE FROM intent_matches WHERE intent_id = ?1", [&id])?;
        record_mutation(
            &conn,
            actor,
            "intent_removed",
            &format!("intent_removed:{id}"),
            &json!({"id": id, "name": name}),
        )?;
        Ok(())
    }

    /// Restore an intent/goal from its operational-journal history —
    /// re-inserts the last known post-image (works for a removed goal).
    /// Mirrors `restore_task`.
    pub fn restore_intent(&self, actor: &Actor, id: &str) -> Result<()> {
        let conn = self.lock();
        let post: Option<String> = conn
            .query_row(
                "SELECT post_image FROM op_journal
                 WHERE entity_kind='intent' AND entity_id=?1 AND post_image IS NOT NULL
                 ORDER BY ts DESC, seq DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(post) = post else {
            return Err(anyhow!("no journal history for intent {id}"));
        };
        let obj: serde_json::Value = serde_json::from_str(&post)?;
        let Some(map) = obj.as_object() else {
            return Err(anyhow!("bad journal post-image for {id}"));
        };
        let cols: Vec<&String> = map.keys().collect();
        let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT OR REPLACE INTO intents ({}) VALUES ({})",
            cols.iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            placeholders.join(", ")
        );
        let params: Vec<rusqlite::types::Value> = cols
            .iter()
            .map(|c| super::json_to_sql(&map[c.as_str()]))
            .collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))?;
        super::journal_row(&conn, actor, "intent", "intents", id, "update")?;
        Ok(())
    }

    pub fn list_intents(&self) -> Result<Vec<IntentRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.name, i.query, i.created_at, i.last_checked,
                    (SELECT COUNT(*) FROM intent_matches m
                     WHERE m.intent_id = i.id AND m.seen = 0 AND m.episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)),
                    (SELECT COUNT(*) FROM intent_matches m2
                     WHERE m2.intent_id = i.id AND m2.episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)),
                    (SELECT GROUP_CONCAT(sq.query, char(31))
                     FROM standing_queries sq WHERE sq.intent_id = i.id),
                    i.description, i.status, i.sphere, i.target_date,
                    (SELECT GROUP_CONCAT(p.name, char(31)) FROM goal_projects gp
                     JOIN entities p ON p.id = gp.project_id AND p.merged_into IS NULL
                     WHERE gp.goal_id = i.id),
                    (SELECT n.id || char(31) || n.title FROM notes_index n
                     WHERE n.primary_for = i.id AND n.status != 'deleted' LIMIT 1)
             FROM intents i
             ORDER BY CASE i.status WHEN 'active' THEN 0 ELSE 1 END, i.name",
        )?;
        let rows = stmt.query_map([], |row| {
            let subs: Option<String> = row.get(7)?;
            let projects: Option<String> = row.get(12)?;
            let primary: Option<String> = row.get(13)?;
            Ok(IntentRow {
                id: row.get(0)?,
                name: row.get(1)?,
                query: row.get(2)?,
                created_at: row.get(3)?,
                last_checked: row.get(4)?,
                unseen_matches: row.get(5)?,
                matches: row.get(6)?,
                sub_queries: subs
                    .map(|s| s.split('\u{1f}').map(String::from).collect())
                    .unwrap_or_default(),
                description: row.get(8)?,
                status: row.get(9)?,
                sphere: row.get(10)?,
                target_date: row.get(11)?,
                projects: projects
                    .map(|s| s.split('\u{1f}').map(String::from).collect())
                    .unwrap_or_default(),
                primary_note: primary.and_then(|s| {
                    s.split_once('\u{1f}')
                        .map(|(a, b)| (a.to_string(), b.to_string()))
                }),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Link or unlink a Project↔Goal (many-to-many, symmetric — the
    /// same junction serves both the goal card and the project modal).
    pub fn link_goal_project(
        &self,
        actor: &Actor,
        goal_name: &str,
        project_id: &str,
        link: bool,
    ) -> Result<()> {
        let conn = self.lock();
        let goal_id: String = conn
            .query_row("SELECT id FROM intents WHERE name = ?1", [goal_name], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| anyhow!("no goal named {goal_name:?}"))?;
        let project = super::graph::resolve_merged(&conn, project_id)?;
        if link {
            conn.execute(
                "INSERT OR IGNORE INTO goal_projects (goal_id, project_id) VALUES (?1, ?2)",
                [&goal_id, &project],
            )?;
        } else {
            conn.execute(
                "DELETE FROM goal_projects WHERE goal_id = ?1 AND project_id = ?2",
                [&goal_id, &project],
            )?;
        }
        record_mutation(
            &conn,
            actor,
            "goal_project_link",
            &format!("goal_project:{goal_id}:{project}:{link}:{}", now_utc()),
            &json!({"goal": goal_name, "project": project, "linked": link}),
        )?;
        Ok(())
    }

    /// Goals a project advances (names) — the project modal's list.
    pub fn goals_for_project(&self, project_id: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let project = super::graph::resolve_merged(&conn, project_id)?;
        let mut stmt = conn.prepare(
            "SELECT i.name FROM goal_projects gp
             JOIN intents i ON i.id = gp.goal_id
             WHERE gp.project_id = ?1 ORDER BY i.name",
        )?;
        let rows = stmt.query_map([&project], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Update a goal — prose, status, sphere, target, and now the
    /// statement (`query`) and name themselves. Changing the statement
    /// redraws the watch: standing queries are wiped and the goal is
    /// marked for re-decomposition (returns true so the caller can
    /// trigger the setup pass immediately). Matches are kept — they
    /// were true when they were caught.
    #[allow(clippy::too_many_arguments)]
    pub fn update_goal(
        &self,
        actor: &Actor,
        name: &str,
        description: Option<&str>,
        status: Option<&str>,
        sphere: Option<&str>,
        target_date: Option<&str>,
        query: Option<&str>,
        new_name: Option<&str>,
    ) -> Result<bool> {
        if let Some(status) = status {
            if !matches!(status, "active" | "achieved" | "abandoned") {
                return Err(anyhow!("status must be active|achieved|abandoned"));
            }
        }
        let query = query.map(str::trim).filter(|q| !q.is_empty());
        let new_name = new_name.map(str::trim).filter(|n| !n.is_empty());
        let conn = self.lock();
        let (id, old_query): (String, String) = conn
            .query_row(
                "SELECT id, query FROM intents WHERE name = ?1",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("no intent named {name:?}"))?;
        let restatement = query.map(|q| q != old_query).unwrap_or(false);
        conn.execute(
            "UPDATE intents SET
                 description = COALESCE(?1, description),
                 status = COALESCE(?2, status),
                 sphere = COALESCE(?3, sphere),
                 target_date = COALESCE(?4, target_date),
                 query = COALESCE(?5, query),
                 name = COALESCE(?6, name)
             WHERE id = ?7",
            rusqlite::params![
                description,
                status,
                sphere,
                target_date,
                query,
                new_name,
                id
            ],
        )?;
        if restatement {
            conn.execute("DELETE FROM standing_queries WHERE intent_id = ?1", [&id])?;
            conn.execute("UPDATE intents SET decomposed = 0 WHERE id = ?1", [&id])?;
        }
        record_mutation(
            &conn,
            actor,
            "goal_updated",
            &format!("goal_updated:{id}:{}", now_utc()),
            &json!({"name": new_name.unwrap_or(name), "status": status,
                    "sphere": sphere, "restated": restatement}),
        )?;
        super::journal_row(&conn, actor, "intent", "intents", &id, "update")?;
        Ok(restatement)
    }

    /// Drop one standing query from a goal (human-directed watches are
    /// editable; the LLM's suggestions are not sacred).
    pub fn remove_standing_query(
        &self,
        actor: &Actor,
        intent_id: &str,
        query: &str,
    ) -> Result<bool> {
        let conn = self.lock();
        let changed = conn.execute(
            "DELETE FROM standing_queries WHERE intent_id = ?1 AND query = ?2",
            [intent_id, query],
        )?;
        if changed > 0 {
            record_mutation(
                &conn,
                actor,
                "standing_query_removed",
                &format!("standing_query_removed:{intent_id}:{}:{}", query, now_utc()),
                &json!({"intent": intent_id, "query": query}),
            )?;
        }
        Ok(changed > 0)
    }

    pub fn intent_id_by_name(&self, name: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row("SELECT id FROM intents WHERE name = ?1", [name], |row| {
            row.get(0)
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
    }

    /// Match ONE intent against the whole existing record — the setup
    /// scan for a new goal, so it knows what's ALREADY true instead of
    /// waiting for the future (intents normally watch forward only).
    /// FTS legs only: the semantic leg would brute-force every chunk
    /// embedding at substrate scale. Backlog matches arrive marked
    /// cascaded=1 — cascade LLM spend is reserved for fresh news.
    /// Returns new match count.
    pub fn run_intent_backlog(
        &self,
        actor: &Actor,
        intent_id: &str,
        per_leg_cap: usize,
    ) -> Result<usize> {
        let conn = self.lock();
        let (name, query): (String, String) = conn.query_row(
            "SELECT name, query FROM intents WHERE id = ?1",
            [intent_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let standing: Vec<String> = {
            let mut stmt =
                conn.prepare("SELECT query FROM standing_queries WHERE intent_id = ?1")?;
            let rows = stmt.query_map([intent_id], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut fts_queries: Vec<String> = vec![query];
        fts_queries.extend(standing.iter().filter_map(|q| fts_safe(q)));

        let mut matched = 0usize;
        for fts_query in &fts_queries {
            let mut stmt = conn.prepare(&format!(
                "SELECT episode_id FROM episode_fts WHERE episode_fts MATCH ?1
                   AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL) LIMIT {per_leg_cap}"
            ))?;
            let hits: Vec<String> = stmt
                .query_map([fts_query], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default();
            for episode_id in hits {
                matched += conn.execute(
                    "INSERT OR IGNORE INTO intent_matches
                         (intent_id, episode_id, found_at, cascaded)
                     VALUES (?1, ?2, ?3, 1)",
                    rusqlite::params![intent_id, episode_id, now_utc()],
                )?;
            }
        }
        if matched > 0 {
            record_mutation(
                &conn,
                actor,
                "intent_backlog_scanned",
                &format!("intent_backlog:{intent_id}:{}", now_utc()),
                &json!({"intent": intent_id, "name": name, "matches": matched}),
            )?;
        }
        Ok(matched)
    }

    /// Intents the idle decomposition pass hasn't split yet.
    pub fn intents_needing_decomposition(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, name, query FROM intents WHERE decomposed = 0 LIMIT ?1")?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Store an intent's decomposed standing queries (with embeddings when
    /// an embedder is present) and mark it decomposed. Idempotent per
    /// (intent, query) via the UNIQUE constraint.
    pub fn add_standing_queries(
        &self,
        actor: &Actor,
        intent_id: &str,
        queries: &[(String, Option<Vec<f32>>)],
    ) -> Result<usize> {
        let conn = self.lock();
        let mut added = 0usize;
        for (query, embedding) in queries {
            let query = query.trim();
            if query.is_empty() {
                continue;
            }
            added += conn.execute(
                "INSERT OR IGNORE INTO standing_queries
                     (id, intent_id, query, embedding, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    ids::standing_query_id(),
                    intent_id,
                    query,
                    embedding.as_deref().map(floats_to_bytes),
                    now_utc(),
                ],
            )?;
        }
        conn.execute(
            "UPDATE intents SET decomposed = 1 WHERE id = ?1",
            [intent_id],
        )?;
        record_mutation(
            &conn,
            actor,
            "intent_decomposed",
            &format!("intent_decomposed:{intent_id}"),
            &json!({"intent": intent_id, "queries": added}),
        )?;
        Ok(added)
    }

    /// Check every intent against episodes ingested since its watermark.
    /// Returns (intent name, new match count) for intents that fired.
    pub fn run_intents(&self, actor: &Actor) -> Result<Vec<(String, usize)>> {
        let (latest, intents) = {
            let conn = self.lock();
            let latest: i64 =
                conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM mutations", [], |row| {
                    row.get(0)
                })?;
            let intents: Vec<(String, String, String, i64)> = {
                let mut stmt = conn.prepare("SELECT id, name, query, last_seq FROM intents")?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            (latest, intents)
        };

        let mut fired = Vec::new();
        for (id, name, query, last_seq) in intents {
            // Re-take the (unfair) store lock per intent — a bulk-sync
            // backlog must not hold it for the whole pass while HTTP
            // handlers queue behind it.
            let conn = self.lock();
            // Episodes that arrived since this intent's watermark.
            let new_ids: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT json_extract(payload, '$.id') FROM mutations
                     WHERE seq > ?1 AND kind = 'episode_ingested'",
                )?;
                let rows = stmt.query_map([last_seq], |row| row.get(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            let mut matched = 0;
            if !new_ids.is_empty() {
                // Leg 1+2: FTS — the intent's own query (verbatim, it's
                // user-written FTS syntax) plus each decomposed standing
                // query (tokenized safe).
                let mut fts_queries: Vec<String> = vec![query.clone()];
                let standing: Vec<(String, Option<Vec<u8>>)> = {
                    let mut stmt = conn.prepare(
                        "SELECT query, embedding FROM standing_queries
                         WHERE intent_id = ?1",
                    )?;
                    let rows = stmt.query_map([&id], |row| Ok((row.get(0)?, row.get(1)?)))?;
                    rows.collect::<std::result::Result<_, _>>()?
                };
                fts_queries.extend(standing.iter().filter_map(|(q, _)| fts_safe(q)));
                let mut hits: Vec<String> = Vec::new();
                for fts_query in &fts_queries {
                    for id_chunk in new_ids.chunks(IN_LIST_CHUNK) {
                        let marks = vec!["?"; id_chunk.len()].join(",");
                        let mut stmt = conn.prepare(&format!(
                            "SELECT episode_id FROM episode_fts
                             WHERE episode_fts MATCH ? AND episode_id IN ({marks})
                               AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)"
                        ))?;
                        let params: Vec<String> = std::iter::once(fts_query.clone())
                            .chain(id_chunk.iter().cloned())
                            .collect();
                        // FTS syntax errors (user-typed operators, stray quotes)
                        // surface at step time — a bad leg is skipped, never fatal.
                        let leg: Vec<String> = stmt
                            .query_map(rusqlite::params_from_iter(params), |row| row.get(0))
                            .map(|rows| rows.filter_map(|r| r.ok()).collect())
                            .unwrap_or_default();
                        hits.extend(leg);
                    }
                }
                // Leg 3: semantic — a standing query's embedding close to
                // any chunk of a new episode (recall for phrasing variance;
                // the embed backfill ran earlier in this same cycle).
                let sq_vectors: Vec<Vec<f32>> = standing
                    .iter()
                    .filter_map(|(_, blob)| blob.as_deref().map(bytes_to_floats))
                    .collect();
                if !sq_vectors.is_empty() {
                    // The cap is global across id-chunks — a giant backlog
                    // gets partial semantic coverage, same as before.
                    let mut budget = SEMANTIC_CHUNK_CAP;
                    for id_chunk in new_ids.chunks(IN_LIST_CHUNK) {
                        if budget == 0 {
                            break;
                        }
                        let marks = vec!["?"; id_chunk.len()].join(",");
                        let mut stmt = conn.prepare(&format!(
                            "SELECT episode_id, embedding FROM chunks
                             WHERE episode_id IN ({marks}) AND embedding IS NOT NULL
                               AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)
                             LIMIT {budget}"
                        ))?;
                        let chunk_rows: Vec<(String, Vec<u8>)> = stmt
                            .query_map(rusqlite::params_from_iter(id_chunk.iter()), |row| {
                                Ok((row.get(0)?, row.get(1)?))
                            })?
                            .collect::<std::result::Result<_, _>>()?;
                        budget = budget.saturating_sub(chunk_rows.len());
                        for (episode_id, blob) in chunk_rows {
                            let chunk_vec = bytes_to_floats(&blob);
                            if sq_vectors.iter().any(|sq| {
                                cosine_similarity(sq, &chunk_vec) >= SEMANTIC_MATCH_THRESHOLD
                            }) {
                                hits.push(episode_id);
                            }
                        }
                    }
                }
                for episode_id in hits {
                    let changed = conn.execute(
                        "INSERT OR IGNORE INTO intent_matches (intent_id, episode_id, found_at)
                         VALUES (?1, ?2, ?3)",
                        rusqlite::params![id, episode_id, now_utc()],
                    )?;
                    matched += changed;
                }
            }
            conn.execute(
                "UPDATE intents SET last_seq = ?1, last_checked = ?2 WHERE id = ?3",
                rusqlite::params![latest, now_utc(), id],
            )?;
            if matched > 0 {
                record_mutation(
                    &conn,
                    actor,
                    "intent_matched",
                    &format!("intent_matched:{id}:{latest}"),
                    &json!({"intent": id, "name": name, "new_matches": matched}),
                )?;
                fired.push((name, matched));
            }
        }
        Ok(fired)
    }

    /// The matches for an intent (the lazy cascade: viewing marks seen).
    pub fn intent_matches(&self, name: &str, mark_seen: bool) -> Result<Vec<IntentMatch>> {
        let guard = self.scope_guard("ep.scope");
        let conn = self.lock();
        let id: Option<String> = conn
            .query_row("SELECT id FROM intents WHERE name = ?1", [name], |row| {
                row.get(0)
            })
            .ok();
        let id = id.ok_or_else(|| anyhow!("no intent named {name:?}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {}, m.found_at, m.seen FROM intent_matches m
             JOIN episodes ep ON ep.id = m.episode_id
             WHERE m.intent_id = ?1 AND ep.superseded_by IS NULL{guard}
             ORDER BY m.found_at DESC LIMIT 100",
            EPISODE_COLUMNS
                .split(", ")
                .map(|c| format!("ep.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let matches: Vec<IntentMatch> = stmt
            .query_map([&id], |row| {
                Ok(IntentMatch {
                    episode: episode_from_row(row)?,
                    found_at: row.get("found_at")?,
                    seen: row.get::<_, i64>("seen")? != 0,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        if mark_seen {
            conn.execute(
                "UPDATE intent_matches SET seen = 1 WHERE intent_id = ?1",
                [&id],
            )?;
        }
        Ok(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::paths::Home;
    use crate::time::EventTime;

    fn note(source_ref: &str, title: &str, body: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "test".into(),
            source_ref: source_ref.into(),
            kind: "note".into(),
            title: title.into(),
            body: body.into(),
            event_time: EventTime::unknown(),
            trust: "self".into(),
            scope: Some("work".into()),
            entities: vec![],
            edges: vec![],
        }
    }

    #[test]
    fn test_intent_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        // Pre-existing episode: intents watch the future, not the backlog.
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![note("old", "old fusion note", "fusion reactors are neat")],
                },
            )
            .unwrap();

        store.add_intent(&actor, "fusion-news", "fusion").unwrap();
        assert!(store.add_intent(&actor, "fusion-news", "x").is_err());

        // Nothing new yet.
        assert!(store.run_intents(&actor).unwrap().is_empty());

        // A matching and a non-matching episode arrive.
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![
                        note("new1", "breakthrough", "cold fusion claims again"),
                        note("new2", "lunch", "sandwich was fine"),
                    ],
                },
            )
            .unwrap();
        let fired = store.run_intents(&actor).unwrap();
        assert_eq!(fired, vec![("fusion-news".to_string(), 1)]);

        // Re-run: watermark advanced, nothing re-fires.
        assert!(store.run_intents(&actor).unwrap().is_empty());

        let intents = store.list_intents().unwrap();
        assert_eq!(intents[0].unseen_matches, 1);

        // Viewing is the lazy cascade trigger; it marks matches seen.
        let matches = store.intent_matches("fusion-news", true).unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].episode.title, "breakthrough");
        assert!(!matches[0].seen);
        assert_eq!(store.list_intents().unwrap()[0].unseen_matches, 0);

        store.remove_intent(&actor, "fusion-news").unwrap();
        assert!(store.intent_matches("fusion-news", false).is_err());
    }

    #[test]
    fn test_run_intents_survives_bulk_backlog() {
        // A sync delivery can land tens of thousands of episodes between
        // watermark checks — the IN-list must chunk (the tenant hit
        // SQLite's 32766-variable cap at 48k and aborted every cycle).
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        store.add_intent(&actor, "fusion-news", "fusion").unwrap();

        // Backlog spanning several IN_LIST_CHUNKs, hits in first,
        // middle, and last chunk.
        let n = IN_LIST_CHUNK * 2 + 50;
        let episodes: Vec<EpisodeDraft> = (0..n)
            .map(|i| {
                let body = if i == 0 || i == IN_LIST_CHUNK + 1 || i == n - 1 {
                    "fusion breakthrough".to_string()
                } else {
                    format!("filler note {i}")
                };
                note(&format!("bulk{i}"), "bulk", &body)
            })
            .collect();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes,
                },
            )
            .unwrap();

        let fired = store.run_intents(&actor).unwrap();
        assert_eq!(fired, vec![("fusion-news".to_string(), 3)]);
    }

    #[test]
    fn test_standing_queries_fts_and_semantic_legs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        store
            .add_intent(&actor, "job-watch", "\"ai infrastructure\"")
            .unwrap();
        let intent_id = store.list_intents().unwrap()[0].id.clone();
        assert_eq!(store.intents_needing_decomposition(10).unwrap().len(), 1);
        // Decompose: one FTS-shaped query, one embedded-only query.
        let sq_vec = vec![1.0_f32, 0.0, 0.0];
        store
            .add_standing_queries(
                &actor,
                &intent_id,
                &[
                    ("inference-hosting startups".into(), None),
                    ("gpu clusters".into(), Some(sq_vec.clone())),
                ],
            )
            .unwrap();
        assert!(store.intents_needing_decomposition(10).unwrap().is_empty());
        assert_eq!(store.list_intents().unwrap()[0].sub_queries.len(), 2);

        // Episode A matches a standing query by FTS (hyphenated token —
        // fts_safe must survive it); episode B matches nothing textually
        // but its chunk embedding sits next to "gpu clusters".
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![
                        note(
                            "a",
                            "funding news",
                            "a wave of inference-hosting startups raised",
                        ),
                        note(
                            "b",
                            "hardware note",
                            "big pile of accelerators in the basement",
                        ),
                    ],
                },
            )
            .unwrap();
        let episode_b = crate::ids::episode_id("test", "b");
        store
            .set_chunk_embedding(&format!("{episode_b}_0"), &[0.95, 0.05, 0.0])
            .unwrap();

        let fired = store.run_intents(&actor).unwrap();
        assert_eq!(fired, vec![("job-watch".to_string(), 2)]);
        let matches = store.intent_matches("job-watch", false).unwrap();
        let titles: Vec<&str> = matches.iter().map(|m| m.episode.title.as_str()).collect();
        assert!(titles.contains(&"funding news"));
        assert!(titles.contains(&"hardware note"));
    }
}
