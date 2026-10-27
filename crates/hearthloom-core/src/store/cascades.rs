//! Cascades: when an intent matches a fresh episode, expand once — at
//! idle — along four bounded dimensions (Orrery's discovery pattern, on
//! the real substrate):
//!   entity    — who/what the episode cites, and their strongest edges
//!   temporal  — notable neighbors in a ±14-day window
//!   place     — episodes near the same coordinates, ±30 days
//!   semantic  — salient-title-token FTS neighbors (NOT the brute-force
//!               vector scan: that loads every chunk embedding — hundreds
//!               of MB per call at substrate scale — for marginal gain)
//! The composed result lands as ONE kind=cascade episode (source
//! consolidation → derivation=inferred), citing everything it drew on.
//!
//! Every query here excludes scope='private' EXPLICITLY — this feeds a
//! cloud LLM pass, and the cloud-pass rule outranks vault state.

use anyhow::Result;

use super::Store;

/// A fresh intent match awaiting expansion.
#[derive(Debug, Clone)]
pub struct CascadeSeed {
    pub intent_id: String,
    pub intent_name: String,
    pub intent_query: String,
    pub episode_id: String,
    pub title: String,
    pub kind: String,
    pub day: String,
    pub scope: String,
}

/// Bounded context gathered around a seed. Each entry: (episode id or
/// entity name, one display line).
#[derive(Debug, Default, Clone)]
pub struct CascadeContext {
    pub entities: Vec<String>,
    pub entity_neighbors: Vec<String>,
    pub temporal: Vec<(String, String)>,
    pub place: Vec<(String, String)>,
    pub semantic: Vec<(String, String)>,
}

impl Store {
    /// Fresh matches to expand, non-private only, marked handled in the
    /// same breath (a failing cascade must not retry forever).
    pub fn uncascaded_matches(&self, limit: usize) -> Result<Vec<CascadeSeed>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT i.id, i.name, i.query, e.id, e.title, e.kind,
                    substr(COALESCE(e.event_time, e.ingest_time), 1, 10), e.scope
             FROM intent_matches m
             JOIN intents i ON i.id = m.intent_id
             JOIN episodes e ON e.id = m.episode_id
             WHERE m.cascaded = 0 AND e.scope != 'private' AND e.superseded_by IS NULL
             ORDER BY m.found_at DESC LIMIT ?1",
        )?;
        let seeds: Vec<CascadeSeed> = stmt
            .query_map([limit as i64], |row| {
                Ok(CascadeSeed {
                    intent_id: row.get(0)?,
                    intent_name: row.get(1)?,
                    intent_query: row.get(2)?,
                    episode_id: row.get(3)?,
                    title: row.get(4)?,
                    kind: row.get(5)?,
                    day: row.get(6)?,
                    scope: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        // Mark everything pending as handled — including private matches
        // this SELECT skipped, which must never queue for a cloud pass.
        conn.execute(
            "UPDATE intent_matches SET cascaded = 1 WHERE cascaded = 0",
            [],
        )?;
        Ok(seeds)
    }

    /// Bounded, non-private context around one seed episode.
    pub fn cascade_context(&self, episode_id: &str, day: &str) -> Result<CascadeContext> {
        let conn = self.lock();
        let mut ctx = CascadeContext::default();

        // Entities the episode cites (skip the mechanical Device rows).
        {
            let mut stmt = conn.prepare(
                "SELECT en.name, en.type FROM episode_entities ee
                 JOIN entities en ON en.id = ee.entity_id
                 WHERE ee.episode_id = ?1 AND en.scope != 'private' /* all-episodes: one live seed's links */
                   AND en.type != 'Device'
                 LIMIT 8",
            )?;
            let rows = stmt.query_map([episode_id], |row| {
                Ok(format!(
                    "{} ({})",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?
                ))
            })?;
            ctx.entities = rows.collect::<std::result::Result<_, _>>()?;
        }

        // Their strongest current edges (one hop, strongest-first cap —
        // the traverse_top discipline, explicit non-private).
        {
            let mut stmt = conn.prepare(
                "SELECT src.name, ed.relation, dst.name FROM episode_entities ee
                 JOIN edges ed ON ed.src = ee.entity_id
                 JOIN entities src ON src.id = ed.src
                 JOIN entities dst ON dst.id = ed.dst
                 WHERE ee.episode_id = ?1 AND ed.t_expired IS NULL /* all-episodes: one live seed's links */
                   AND ed.scope != 'private'
                   AND src.scope != 'private' AND dst.scope != 'private'
                 ORDER BY ed.strength DESC LIMIT 10",
            )?;
            let rows = stmt.query_map([episode_id], |row| {
                Ok(format!(
                    "{} —{}→ {}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?
                ))
            })?;
            ctx.entity_neighbors = rows.collect::<std::result::Result<_, _>>()?;
        }

        // Temporal neighbors: interesting kinds in a ±14d window.
        {
            let mut stmt = conn.prepare(
                "SELECT id, substr(COALESCE(event_time, ingest_time), 1, 10) || ' ' ||
                        kind || ': ' || title
                 FROM episodes
                 WHERE substr(COALESCE(event_time, ingest_time), 1, 10)
                       BETWEEN date(?1, '-14 days') AND date(?1, '+14 days')
                   AND id != ?2 AND scope != 'private' AND superseded_by IS NULL
                   AND kind IN ('calendar_event', 'workout', 'article', 'photos',
                                'conversation', 'commit', 'clinical_record')
                 ORDER BY ABS(julianday(substr(COALESCE(event_time, ingest_time), 1, 10))
                              - julianday(?1))
                 LIMIT 10",
            )?;
            let rows = stmt.query_map(rusqlite::params![day, episode_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            ctx.temporal = rows.collect::<std::result::Result<_, _>>()?;
        }

        // Place neighbors: other episodes near the same coordinates.
        if let Ok((lat, lon)) = conn.query_row(
            "SELECT lat, lon FROM location_points WHERE episode_id = ?1 LIMIT 1",
            [episode_id],
            |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
        ) {
            let mut stmt = conn.prepare(
                "SELECT e.id, p.day || ' ' || e.kind || ': ' || e.title
                 FROM location_points p
                 JOIN episodes e ON e.id = p.episode_id
                 WHERE p.lat BETWEEN ?1 - 0.25 AND ?1 + 0.25
                   AND p.lon BETWEEN ?2 - 0.25 AND ?2 + 0.25
                   AND p.day BETWEEN date(?3, '-30 days') AND date(?3, '+30 days')
                   AND e.id != ?4 AND e.scope != 'private' AND e.superseded_by IS NULL
                 LIMIT 6",
            )?;
            let rows = stmt.query_map(rusqlite::params![lat, lon, day, episode_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            ctx.place = rows.collect::<std::result::Result<_, _>>()?;
        }

        // Semantic-lite: FTS on the three longest title tokens.
        if let Ok(title) = conn.query_row(
            "SELECT title FROM episodes WHERE id = ?1 /* all-episodes: the live seed's own title */",
            [episode_id],
            |row| row.get::<_, String>(0),
        ) {
            let mut tokens: Vec<&str> = title
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| t.len() >= 4)
                .collect();
            tokens.sort_by_key(|t| std::cmp::Reverse(t.len()));
            tokens.truncate(3);
            if !tokens.is_empty() {
                let fts = tokens
                    .iter()
                    .map(|t| format!("\"{t}\""))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let mut stmt = conn.prepare(
                    "SELECT e.id, substr(COALESCE(e.event_time, e.ingest_time), 1, 10)
                            || ' ' || e.kind || ': ' || e.title
                     FROM episode_fts f
                     JOIN episodes e ON e.fts_rowid = f.rowid
                     WHERE episode_fts MATCH ?1 AND e.id != ?2
                       AND e.scope != 'private' AND e.superseded_by IS NULL
                     LIMIT 6",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![fts, episode_id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default();
                ctx.semantic = rows;
            }
        }
        Ok(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EntityRef, EpisodeDraft, IngestBatch};
    use crate::paths::Home;
    use crate::store::Actor;
    use crate::time::EventTime;

    #[test]
    fn test_cascade_seeds_and_context_exclude_private() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        store.add_intent(&actor, "watch", "orrery").unwrap();
        let mut secret = EpisodeDraft {
            source: "test".into(),
            source_ref: "s".into(),
            kind: "note".into(),
            title: "secret orrery plans".into(),
            body: "orrery".into(),
            event_time: EventTime::parse_rfc3339(
                "2026-03-15T10:00:00Z",
                crate::time::TimeUncertainty::Exact,
            )
            .unwrap(),
            trust: "self".into(),
            scope: Some("private".into()),
            entities: vec![],
            edges: vec![],
        };
        let mut public = secret.clone();
        public.source_ref = "p".into();
        public.title = "public orrery notes".into();
        public.scope = Some("work".into());
        public.entities = vec![EntityRef {
            entity_type: "Person".into(),
            name: "Ada".into(),
            attributes: vec![],
            role: "author".into(),
        }];
        secret.entities = vec![];
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![secret, public],
                },
            )
            .unwrap();
        store.run_intents(&actor).unwrap();

        // Both matched, but only the public one becomes a seed; the
        // private match is still marked handled (never re-queued).
        let seeds = store.uncascaded_matches(10).unwrap();
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].title, "public orrery notes");
        assert!(store.uncascaded_matches(10).unwrap().is_empty());

        let ctx = store
            .cascade_context(&seeds[0].episode_id, &seeds[0].day)
            .unwrap();
        assert_eq!(ctx.entities, vec!["Ada (Person)".to_string()]);
        // The private episode shares title tokens but must not surface.
        assert!(ctx
            .semantic
            .iter()
            .all(|(_, line)| !line.contains("secret")));
        assert!(ctx
            .temporal
            .iter()
            .all(|(_, line)| !line.contains("secret")));
    }
}
