//! The location index behind the Atlas: (episode, time, lat, lon)
//! points derived from episode content — photo GPS lines today, workout
//! GPX starts via the daemon (which knows the archive mount). Derived
//! and rebuildable from the episode log, like every index (law #1).
//!
//! PRIVACY INVARIANT: every read joins `episodes` and takes the vault
//! scope guard — a private day's points never leave the store while
//! the vault is locked (zero leakage, matching every other surface).

use anyhow::Result;
use serde::Serialize;

use super::{Actor, Store};

/// One rendered atlas point: a day's presence at a (rounded) location.
#[derive(Debug, Clone, Serialize)]
pub struct AtlasPoint {
    pub day: String,
    pub lat: f64,
    pub lon: f64,
    /// Episodes contributing to this cluster that day.
    pub n: i64,
    /// Place names cited by those episodes (may be empty).
    pub places: Vec<String>,
    /// Episode sources behind the cluster — the trust ranking: a
    /// sensor (photos) outranks a declaration (facebook check-in)
    /// when the same day disagrees about where you were.
    pub sources: Vec<String>,
}

/// `episodes.location_scanned` once `location_backfill` has read an
/// episode with `parse_location_lines`. 1 is the GPX pass's mark — and,
/// for photos/facebook/visits, the older parser's, which took any
/// `(lat,lon)` on any `[`-line of any facebook episode: those episodes
/// are read once more (their points replaced), so a point forged before
/// this parser does not outlive it.
const PARSED: i64 = 2;

/// The points a body's location lines give, as (hhmm, lat, lon). A
/// location line is `[HH:MM] <label> (<lat>,<lon>)` with the coordinate
/// pair CLOSING the line, so a pair inside a label — a file named
/// `x (1,2).heic`, a `(3,4)` in a message — never becomes a point. Which
/// bodies count, and which of their lines:
/// - photos: every location line; a photo line may go on with
///   ` — <place>`, but only with a place the day's `places:` header lists;
/// - facebook: `fb_post` only (check-ins and place tags). Messages,
///   events, albums and groups carry text other people wrote;
/// - visits: the LAST line only — the one `visit_draft` writes; the
///   `place:` and `observations:` lines above it are never read.
pub(crate) fn parse_location_lines(
    source: &str,
    kind: &str,
    body: &str,
) -> Vec<(String, f64, f64)> {
    match (source, kind) {
        ("photos", _) => {
            let places = body
                .lines()
                .take_while(|line| !line.is_empty())
                .find_map(|line| line.strip_prefix("places: "));
            body.lines()
                .filter_map(|line| photo_line(line, places))
                .collect()
        }
        ("facebook", "fb_post") => body.lines().filter_map(location_line).collect(),
        ("visits", _) => body
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .and_then(location_line)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

/// `[HH:MM] <label> (<lat>,<lon>)`, the pair closing the line.
fn location_line(line: &str) -> Option<(String, f64, f64)> {
    let (hhmm, rest) = line_time(line)?;
    let (lat, lon) = closing_pair(rest)?;
    Some((hhmm, lat, lon))
}

/// A photo day line: `[HH:MM] <file>[ (<lat>,<lon>)][ — <place>]`. A file
/// name can hold an em dash too, so the day's `places:` header decides
/// where the place begins; anything else after a pair means no point.
fn photo_line(line: &str, places: Option<&str>) -> Option<(String, f64, f64)> {
    let (hhmm, rest) = line_time(line)?;
    let rest = match places.and_then(|listed| place_suffix(rest, listed)) {
        Some(at) => &rest[..at],
        None => rest,
    };
    let (lat, lon) = closing_pair(rest)?;
    Some((hhmm, lat, lon))
}

/// Where a ` — <place>` ending starts, when `listed` (the `places:`
/// header, `, `-joined) names that place.
fn place_suffix(rest: &str, listed: &str) -> Option<usize> {
    let listed = format!(", {listed}, ");
    rest.match_indices(" \u{2014} ")
        .find(|(at, sep)| {
            let place = &rest[at + sep.len()..];
            !place.is_empty() && listed.contains(&format!(", {place}, "))
        })
        .map(|(at, _)| at)
}

/// The strict `[HH:MM] ` opening: ("HH:MM", the rest of the line).
fn line_time(line: &str) -> Option<(String, &str)> {
    let b = line.as_bytes();
    let digits = |i: usize, j: usize| b[i].is_ascii_digit() && b[j].is_ascii_digit();
    (b.len() > 8
        && b[0] == b'['
        && digits(1, 2)
        && b[3] == b':'
        && digits(4, 5)
        && b[6] == b']'
        && b[7] == b' ')
        .then(|| (line[1..6].to_string(), &line[8..]))
}

/// The `(<lat>,<lon>)` pair that ends `s`, in range.
fn closing_pair(s: &str) -> Option<(f64, f64)> {
    let inner = s.trim_end().strip_suffix(')')?;
    let (lat, lon) = inner[inner.rfind('(')? + 1..].split_once(',')?;
    let (lat, lon) = (
        lat.trim().parse::<f64>().ok()?,
        lon.trim().parse::<f64>().ok()?,
    );
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

impl Store {
    /// Index location points for episodes not yet scanned. Photos parse
    /// here; other sources insert via `insert_location_points` (the
    /// daemon drives GPX, which needs the archive mount). Returns the
    /// number of points added.
    /// Returns (episodes scanned, points added) — scanned is the drain
    /// signal for callers looping in small lock-bounded batches (a
    /// pointless batch still scans; points alone can't tell "done").
    pub fn location_backfill(&self, actor: &Actor, batch: usize) -> Result<(usize, usize)> {
        let pending: Vec<(String, String, String, String, String)> = {
            let conn = self.lock();
            // Drive FROM the FTS side: equality on a non-MATCH FTS5
            // column can't use an index, so joining the other way scans
            // the whole FTS table once PER EPISODE (quadratic — this
            // wedged the daemon for 11 minutes on first deploy). One
            // linear FTS scan probing episodes by primary key instead.
            // facebook posts carry the same `[HH:MM] place (lat,lon)`
            // line format as photo day-episodes (check-ins/place tags);
            // no other facebook kind is read (parse_location_lines), so
            // their bodies are not even loaded — they are only marked.
            let mut stmt = conn.prepare(
                "SELECT e.id, e.source, e.kind, COALESCE(substr(e.event_time, 1, 10), ''),
                        CASE WHEN e.source = 'facebook' AND e.kind != 'fb_post' THEN ''
                             ELSE f.body END
                 FROM episodes e
                 JOIN episode_fts f ON f.rowid = e.fts_rowid
                 WHERE e.location_scanned < ?2 AND e.superseded_by IS NULL
                   AND e.source IN ('photos', 'facebook', 'visits')
                 LIMIT ?1",
            )?;
            let rows = stmt.query_map(rusqlite::params![batch as i64, PARSED], |row| {
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
        if pending.is_empty() {
            return Ok((0, 0));
        }
        let mut added = 0usize;
        {
            let mut conn = self.lock();
            let tx = conn.transaction()?;
            for (id, source, kind, day, body) in &pending {
                // a second read replaces what the older parser indexed
                tx.execute("DELETE FROM location_points WHERE episode_id = ?1", [id])?;
                if !day.is_empty() {
                    for (hhmm, lat, lon) in parse_location_lines(source, kind, body) {
                        tx.execute(
                            "INSERT INTO location_points (episode_id, ts, day, lat, lon)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            rusqlite::params![id, format!("{day}T{hhmm}"), day, lat, lon],
                        )?;
                        added += 1;
                    }
                }
                tx.execute(
                    "UPDATE episodes SET location_scanned = ?2 WHERE id = ?1",
                    rusqlite::params![id, PARSED],
                )?;
            }
            tx.commit()?;
        }
        if added > 0 {
            self.record_event(
                actor,
                "locations_indexed",
                &serde_json::json!({"episodes": pending.len(), "points": added}),
            )?;
        }
        Ok((pending.len(), added))
    }

    /// Insert externally-derived points (e.g. GPX starts) for an episode
    /// and mark it scanned. Idempotent per episode via the scanned flag —
    /// callers must only pass episodes still unscanned.
    pub fn insert_location_points(
        &self,
        episode_id: &str,
        points: &[(Option<String>, String, f64, f64)], // (ts, day, lat, lon)
    ) -> Result<()> {
        let conn = self.lock();
        for (ts, day, lat, lon) in points {
            conn.execute(
                "INSERT INTO location_points (episode_id, ts, day, lat, lon)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![episode_id, ts, day, lat, lon],
            )?;
        }
        conn.execute(
            "UPDATE episodes SET location_scanned = 1 WHERE id = ?1",
            [episode_id],
        )?;
        Ok(())
    }

    /// Episodes (id, day, body-ref path hint) of one source AND kind that
    /// still need external scanning (the daemon's GPX pass reads
    /// apple-health `workout`s only: a clinical record's body quotes text
    /// its provider wrote, which must never name a file to open).
    pub fn location_pending(
        &self,
        source: &str,
        kind: &str,
        batch: usize,
    ) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        // Same FTS-drives-the-join rule as location_backfill (see above).
        let mut stmt = conn.prepare(
            "SELECT e.id, COALESCE(substr(e.event_time, 1, 10), ''), f.body
             FROM episodes e
             JOIN episode_fts f ON f.rowid = e.fts_rowid
             WHERE e.location_scanned = 0 AND e.source = ?1 AND e.kind = ?3
               AND e.superseded_by IS NULL
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![source, batch as i64, kind], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Day-bucketed, location-clustered points for the Atlas, newest
    /// window capped. VAULT-GUARDED via the episodes join.
    pub fn atlas_points(&self, from: &str, to: &str, cap: usize) -> Result<Vec<AtlasPoint>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT p.day, ROUND(p.lat, 2) rlat, ROUND(p.lon, 2) rlon,
                    COUNT(DISTINCT p.episode_id) n,
                    COALESCE(GROUP_CONCAT(DISTINCT en.name), ''),
                    COALESCE(GROUP_CONCAT(DISTINCT e.source), '')
             FROM location_points p
             JOIN episodes e ON e.id = p.episode_id AND e.superseded_by IS NULL{guard}
             LEFT JOIN episode_entities ee ON ee.episode_id = p.episode_id
             LEFT JOIN entities en ON en.id = ee.entity_id AND en.type = 'Place'
             WHERE p.day >= ?1 AND p.day <= ?2
             GROUP BY p.day, rlat, rlon
             ORDER BY p.day ASC
             LIMIT ?3"
        ))?;
        let rows = stmt.query_map(rusqlite::params![from, to, cap as i64], |row| {
            let places: String = row.get(4)?;
            let sources: String = row.get(5)?;
            Ok(AtlasPoint {
                day: row.get(0)?,
                lat: row.get(1)?,
                lon: row.get(2)?,
                n: row.get(3)?,
                places: places
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect(),
                sources: sources
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect(),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/* ————— the Dial: time histogram ————— */

impl Store {
    /// Episode counts per time bucket for the Dial's drum density.
    /// `bucket` ∈ year|month|day → substr widths 4|7|10 over the same
    /// COALESCE(event_time, ingest_time) Browse sorts by. Optional
    /// full-text `q` (FTS MATCH — indexed), source, and place filters
    /// mirror the Browse episode filters. VAULT-GUARDED.
    #[allow(clippy::too_many_arguments)]
    pub fn timeline_histogram(
        &self,
        from: &str,
        to: &str,
        bucket: &str,
        q: Option<&str>,
        source: Option<&str>,
        place: Option<&str>,
    ) -> Result<Vec<(String, i64)>> {
        let width = match bucket {
            "year" => 4,
            "month" => 7,
            _ => 10,
        };
        let guard = self.scope_guard("e.scope");
        let mut clauses = String::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(from.to_string()), Box::new(to.to_string())];
        if let Some(q) = q.filter(|s| !s.trim().is_empty()) {
            clauses.push_str(
                " AND e.fts_rowid IN (SELECT rowid FROM episode_fts WHERE episode_fts MATCH ?) /* all-episodes: a filter inside a query guarded where it is used */",
            );
            // Quote for FTS5 so user text is tokens, not syntax.
            let fts: String = q
                .split_whitespace()
                .map(|t| format!("\"{}\"", t.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" ");
            params.push(Box::new(fts));
        }
        if let Some(source) = source.filter(|s| !s.is_empty()) {
            clauses.push_str(" AND e.source = ?");
            params.push(Box::new(source.to_string()));
        }
        if let Some(place) = place.filter(|s| !s.trim().is_empty()) {
            clauses.push_str(
                " AND e.id IN (SELECT ee.episode_id FROM episode_entities ee \
                 JOIN entities en ON en.id = ee.entity_id \
                 WHERE en.type = 'Place' AND en.name LIKE ? ESCAPE '\\') /* all-episodes: a filter inside a query guarded where it is used */",
            );
            params.push(Box::new(format!(
                "%{}%",
                place
                    .trim()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )));
        }
        let conn = self.lock();
        let sql = format!(
            "SELECT substr(COALESCE(e.event_time, e.ingest_time), 1, {width}) b, COUNT(*)
             FROM episodes e
             WHERE e.superseded_by IS NULL
               AND substr(COALESCE(e.event_time, e.ingest_time), 1, 10) >= ?1
               AND substr(COALESCE(e.event_time, e.ingest_time), 1, 10) <= ?2
               {clauses}{guard}
             GROUP BY b ORDER BY b"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/* ————— the Dial: notable events (label layer) ————— */

/// One label-worthy episode for the Dial's leader lines.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NotableEpisode {
    pub id: String,
    pub day: String,
    pub title: String,
    pub kind: String,
    pub source: String,
    pub score: f64,
}

impl Store {
    /// Top-K most interesting episodes in a window — the cartographic
    /// label-placement query: importance-ranked, viewport-bounded, at
    /// most `per_bucket` winners per bucket so one busy year can't eat
    /// the label budget. With `q`, interestingness IS relevance (BM25).
    /// VAULT-GUARDED; never returns more than K rows.
    #[allow(clippy::too_many_arguments)]
    pub fn timeline_notable(
        &self,
        from: &str,
        to: &str,
        bucket: &str,
        k: usize,
        q: Option<&str>,
        source: Option<&str>,
        place: Option<&str>,
    ) -> Result<Vec<NotableEpisode>> {
        let width = match bucket {
            "year" => 4,
            "month" => 7,
            _ => 10,
        };
        let guard = self.scope_guard("e.scope");
        let mut clauses = String::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(from.to_string()), Box::new(to.to_string())];
        let q = q.filter(|s| !s.trim().is_empty());
        if let Some(q) = q {
            clauses.push_str(" AND e.fts_rowid IN (SELECT rowid FROM episode_fts WHERE episode_fts MATCH ?) /* all-episodes: a filter inside a query guarded where it is used */");
            let fts: String = q
                .split_whitespace()
                .map(|t| format!("\"{}\"", t.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" ");
            params.push(Box::new(fts));
        }
        if let Some(source) = source.filter(|s| !s.is_empty()) {
            clauses.push_str(" AND e.source = ?");
            params.push(Box::new(source.to_string()));
        }
        if let Some(place) = place.filter(|s| !s.trim().is_empty()) {
            clauses.push_str(
                " AND e.id IN (SELECT ee.episode_id FROM episode_entities ee \
                 JOIN entities en ON en.id = ee.entity_id \
                 WHERE en.type = 'Place' AND en.name LIKE ? ESCAPE '\\') /* all-episodes: a filter inside a query guarded where it is used */",
            );
            params.push(Box::new(format!(
                "%{}%",
                place
                    .trim()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )));
        }
        // Interestingness: kind weight (events > creations > ambient) +
        // entity richness + quiet-day bonus. With q present, BM25
        // relevance replaces the heuristic core.
        //
        // PERFORMANCE SHAPE (learned the hard way): CTE-to-CTE joins in
        // SQLite are nested loops over UNINDEXED intermediates — at a
        // full-life window (66k rows) the old dz/ent joins took 60+s
        // while holding the store lock, wedging every view behind the
        // dial. Per-day counts now come from a window function and
        // entity counts from a correlated subquery against the INDEXED
        // base table. (The first-at-a-place bonus was dropped for the
        // same reason — marginal signal, quadratic cost.)
        // Orrery's composite-rescoring idea: beyond kind/richness, an
        // episode gains rank for citing a declared-important entity and
        // for having matched an active intent (both via correlated
        // subqueries on INDEXED tables — episode_entities(episode_id),
        // intent_matches(episode_id) — per the CTE post-mortem rules).
        let score_expr = if q.is_some() {
            "50.0 + kw.w * 0.3 \
             + MIN((SELECT COUNT(*) FROM episode_entities ee WHERE ee.episode_id = win.id), 10) /* all-episodes: a filter inside a query guarded where it is used */"
        } else {
            "CAST(kw.w AS REAL) /* all-episodes: a filter inside a query guarded where it is used */ \
             + MIN((SELECT COUNT(*) FROM episode_entities ee WHERE ee.episode_id = win.id), 10) * 3 \
             + (SELECT COALESCE(MAX(en.declared_importance), 0) * 8 FROM episode_entities ee2 \
                JOIN entities en ON en.id = ee2.entity_id WHERE ee2.episode_id = win.id) \
             + CASE WHEN EXISTS (SELECT 1 FROM intent_matches im WHERE im.episode_id = win.id) \
                    THEN 12 ELSE 0 END \
             + CASE WHEN win.day_n <= 5 THEN 8 ELSE 0 END"
        };
        let sql = format!(
            "WITH win AS (
                 SELECT e.id, e.title, e.kind, e.source,
                        substr(COALESCE(e.event_time, e.ingest_time), 1, 10) AS day,
                        substr(COALESCE(e.event_time, e.ingest_time), 1, {width}) AS bucket,
                        COUNT(*) OVER (
                            PARTITION BY substr(COALESCE(e.event_time, e.ingest_time), 1, 10)
                        ) AS day_n
                 FROM episodes e
                 WHERE e.superseded_by IS NULL
                   AND substr(COALESCE(e.event_time, e.ingest_time), 1, 10) >= ?1
                   AND substr(COALESCE(e.event_time, e.ingest_time), 1, 10) <= ?2
                   {clauses}{guard}
             ),
             scored AS (
                 SELECT win.id, win.title, win.kind, win.source, win.day,
                        ({score_expr}) AS score,
                        ROW_NUMBER() OVER (
                            PARTITION BY win.bucket
                            ORDER BY ({score_expr}) DESC, win.id
                        ) AS rn
                 FROM win
                 JOIN (SELECT 'chapter' k, 100 w UNION ALL SELECT 'calendar_event', 30
                       UNION ALL SELECT 'clinical_record', 40
                       UNION ALL SELECT 'opportunity', 50 UNION ALL SELECT 'cascade', 45
                       UNION ALL SELECT 'ecg', 35 UNION ALL SELECT 'workout', 22
                       UNION ALL SELECT 'article', 18 UNION ALL SELECT 'photos', 16
                       UNION ALL SELECT 'note', 15
                       UNION ALL SELECT 'conversation', 14 UNION ALL SELECT 'commit', 10
                       UNION ALL SELECT 'task_done', 12
                       UNION ALL SELECT 'messages', 8 UNION ALL SELECT 'agent_session', 8
                       UNION ALL SELECT 'email', 6 UNION ALL SELECT 'browsing', 2
                       UNION ALL SELECT 'health_day', 0 UNION ALL SELECT 'summary', 0) kw
                   ON kw.k = win.kind
             )
             SELECT id, day, title, kind, source, score FROM scored
             WHERE rn <= 2
             ORDER BY score DESC, day
             LIMIT {k}",
            k = k.min(24)
        );
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |row| {
                Ok(NotableEpisode {
                    id: row.get(0)?,
                    day: row.get(1)?,
                    title: row.get(2)?,
                    kind: row.get(3)?,
                    source: row.get(4)?,
                    score: row.get(5)?,
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/* ————— Memories: on this day ————— */

impl Store {
    /// The most notable episode(s) from this calendar date in EARLIER
    /// years — the memories-resurfacing strip. Same kind-weighted
    /// notability as the Dial labels, ≤2 per year, newest year first.
    /// VAULT-GUARDED.
    pub fn on_this_day(
        &self,
        mmdd: &str,
        before_year: &str,
        k: usize,
    ) -> Result<Vec<NotableEpisode>> {
        let guard = self.scope_guard("e.scope");
        let conn = self.lock();
        let sql = format!(
            "WITH win AS (
                 SELECT e.id, e.title, e.kind, e.source,
                        substr(COALESCE(e.event_time, e.ingest_time), 1, 10) AS day,
                        substr(COALESCE(e.event_time, e.ingest_time), 1, 4) AS year
                 FROM episodes e
                 WHERE substr(COALESCE(e.event_time, e.ingest_time), 6, 5) = ?1
                   AND substr(COALESCE(e.event_time, e.ingest_time), 1, 4) < ?2
                   AND e.superseded_by IS NULL
                   {guard}
             ),
             ent AS (
                 SELECT ee.episode_id, COUNT(*) c FROM episode_entities ee
                 WHERE ee.episode_id IN (SELECT id FROM win) GROUP BY ee.episode_id
             ),
             scored AS (
                 SELECT win.*,
                        (CAST(kw.w AS REAL) + MIN(COALESCE(ent.c, 0), 10) * 3) AS score,
                        ROW_NUMBER() OVER (
                            PARTITION BY win.year
                            ORDER BY (CAST(kw.w AS REAL) + MIN(COALESCE(ent.c, 0), 10) * 3) DESC, win.id
                        ) AS rn
                 FROM win
                 JOIN (SELECT 'calendar_event' k, 30 w UNION ALL SELECT 'clinical_record', 40
                       UNION ALL SELECT 'ecg', 35 UNION ALL SELECT 'workout', 22
                       UNION ALL SELECT 'article', 18 UNION ALL SELECT 'photos', 24
                       UNION ALL SELECT 'conversation', 14 UNION ALL SELECT 'commit', 10
                       UNION ALL SELECT 'messages', 12 UNION ALL SELECT 'agent_session', 6
                       UNION ALL SELECT 'email', 6 UNION ALL SELECT 'browsing', 2
                       UNION ALL SELECT 'health_day', 0 UNION ALL SELECT 'summary', 0
                       UNION ALL SELECT 'chapter', 0) kw
                   ON kw.k = win.kind
                 LEFT JOIN ent ON ent.episode_id = win.id
             )
             SELECT id, day, title, kind, source, score FROM scored
             WHERE rn <= 2 AND score >= 10
             ORDER BY year DESC
             LIMIT {k}",
            k = k.min(24)
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![mmdd, before_year], |row| {
            Ok(NotableEpisode {
                id: row.get(0)?,
                day: row.get(1)?,
                title: row.get(2)?,
                kind: row.get(3)?,
                source: row.get(4)?,
                score: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/* ————— Memories: chapter headings (idle-time LLM) ————— */

impl Store {
    /// Months that have real activity but no chapter episode yet,
    /// oldest first. Chapter months are keyed by source_ref
    /// `chapter:YYYY-MM`.
    pub fn months_needing_chapters(&self, limit: usize) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT substr(COALESCE(event_time, ingest_time), 1, 7) m
             FROM episodes
             WHERE kind != 'chapter' AND superseded_by IS NULL
             GROUP BY m
             HAVING COUNT(*) >= 10
                AND m NOT IN (SELECT substr(source_ref, 9) FROM episodes WHERE kind = 'chapter')
                -- a retired chapter still counts as written: rewrites go
                -- through the retirement's own queue (store/retire.rs)
             ORDER BY m
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Representative titles for a month, for the chapter-heading LLM
    /// prompt. PRIVATE IS HARD-EXCLUDED — this feeds a cloud LLM, so
    /// the exclusion must not depend on vault state (the standing
    /// cloud-pass rule).
    /// (id, title) pairs — the ids are recorded as the chapter's inputs.
    pub fn chapter_inputs(&self, month: &str, k: usize) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, title FROM episodes
             WHERE substr(COALESCE(event_time, ingest_time), 1, 7) = ?1
               AND scope != 'private' AND superseded_by IS NULL
               AND kind IN ('calendar_event', 'workout', 'article', 'photos',
                            'commit', 'conversation', 'clinical_record', 'messages')
             ORDER BY CASE kind
                 WHEN 'calendar_event' THEN 0 WHEN 'clinical_record' THEN 1
                 WHEN 'article' THEN 2 WHEN 'workout' THEN 3 WHEN 'photos' THEN 4
                 WHEN 'conversation' THEN 5 WHEN 'commit' THEN 6 ELSE 7 END, rowid
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![month, k as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/* ————— constellation: windowed evidence ————— */

impl Store {
    /// (entity_id, episodes-in-window, last-day-in-window) for a set of
    /// entities — the "sky tonight" recency signal. VAULT-GUARDED via
    /// the episodes join. One query for the whole node set.
    pub fn entity_evidence_window(
        &self,
        ids: &[String],
        from: &str,
        to: &str,
    ) -> Result<Vec<(String, i64, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let guard = self.scope_guard("e.scope");
        let marks = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT ee.entity_id, COUNT(*),
                    MAX(substr(COALESCE(e.event_time, e.ingest_time), 1, 10))
             FROM episode_entities ee
             JOIN episodes e ON e.id = ee.episode_id AND e.superseded_by IS NULL{guard}
             WHERE ee.entity_id IN ({marks})
               AND substr(COALESCE(e.event_time, e.ingest_time), 1, 10) >= ?
               AND substr(COALESCE(e.event_time, e.ingest_time), 1, 10) <= ?
             GROUP BY ee.entity_id"
        ))?;
        let params: Vec<&dyn rusqlite::ToSql> = ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .chain([&from as &dyn rusqlite::ToSql, &to as &dyn rusqlite::ToSql])
            .collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::time::{EventTime, TimeUncertainty};
    use crate::Home;

    fn photo_episode(day: &str, body: &str) -> EpisodeDraft {
        episode("photos", "photos", &format!("photos:{day}"), day, body)
    }

    fn episode(source: &str, kind: &str, source_ref: &str, day: &str, body: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: source.into(),
            source_ref: source_ref.into(),
            kind: kind.into(),
            title: format!("{kind}: {day}"),
            body: body.into(),
            event_time: EventTime::parse_rfc3339(&format!("{day}T12:00:00Z"), TimeUncertainty::Day)
                .unwrap(),
            trust: "imported".into(),
            scope: Some("personal".into()),
            entities: vec![],
            edges: vec![],
        }
    }

    #[test]
    fn test_parse_photo_points() {
        let body = "date: 2026-07-08\nphotos: 3\n\n\
                    [06:40] a.png\n\
                    [07:30] b.heic (41.8902,12.4922)\n\
                    [07:48] c.heic (41.8901,12.4922)\n\
                    junk (999,999)\n";
        let pts = parse_location_lines("photos", "photos", body);
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0], ("07:30".into(), 41.8902, 12.4922));
    }

    /// The pair must CLOSE the line: a pair inside a label — a file name,
    /// a message, a venue — is never a point. The formats the writers
    /// really emit (photos.rs day_draft, facebook fb_post place lines,
    /// visits.rs visit_draft) still parse.
    #[test]
    fn test_location_lines_take_only_the_closing_pair() {
        let pts = |source: &str, kind: &str, body: &str| -> Vec<(f64, f64)> {
            parse_location_lines(source, kind, body)
                .into_iter()
                .map(|(_, lat, lon)| (lat, lon))
                .collect()
        };
        // photos: day_draft's header + lines, with and without a place
        let photos = "date: 2026-07-08\nphotos: 6\nplaces: Rome, Boston, MA\n\n\
                      [07:30] b.heic (41.8902,12.4922) \u{2014} Rome\n\
                      [07:31] c.heic (42.3601,-71.0589) \u{2014} Boston, MA\n\
                      [07:32] d.heic \u{2014} Rome\n\
                      [07:33] x (1,2).heic\n\
                      [07:34] y (3,4) \u{2014} z.heic\n\
                      [07:35] e (5,6).heic (41.0000,12.0000)\n";
        assert_eq!(
            pts("photos", "photos", photos),
            vec![(41.8902, 12.4922), (42.3601, -71.0589), (41.0, 12.0)]
        );
        // an em dash in the file name: only a LISTED place ends the line
        let dashed = "date: 2026-07-08\nphotos: 2\nplaces: Rome\n\n\
                      [08:00] a \u{2014} b.heic (41.1000,12.1000) \u{2014} Rome\n\
                      [08:01] q (7,8) \u{2014} Paris\n";
        assert_eq!(pts("photos", "photos", dashed), vec![(41.1, 12.1)]);
        // a listed place that itself ends in a pair is still the place
        let odd =
            "date: 2026-07-08\nphotos: 1\nplaces: Foo (3,4)\n\n[09:00] a.heic \u{2014} Foo (3,4)\n";
        assert!(pts("photos", "photos", odd).is_empty());
        // the strict [HH:MM] opening
        assert!(pts(
            "photos",
            "photos",
            "[7:30] a (1,2)\n[07:30](1,2)\n(07:30) a (1,2)\n"
        )
        .is_empty());

        // facebook: fb_post place lines (f64 Display coordinates, maybe no name)
        let post = "Test post\n\nattached: a (9,9)\n\n[12:00] Lakeside Park (12.34,-56.78)\n[12:00]  (40,-74)\n";
        assert_eq!(
            pts("facebook", "fb_post", post),
            vec![(12.34, -56.78), (40.0, -74.0)]
        );
        // every other facebook kind carries other people's text
        let forged = "[12:00] ada: meet me (40.71,-74.00)\n[12:00] x (40.71,-74.00)\n";
        for kind in ["fb_message", "fb_group", "fb_event", "fb_album"] {
            assert!(pts("facebook", kind, forged).is_empty(), "{kind}");
        }

        // visits: the last line only (the one visit_draft writes)
        let visit = "place: Cafe\n[12:00] Forged (1,2)\narrival: 2026-08-10T15:00:00Z\n\
                     observations: calendar\u{d7}1, ios-visits\u{d7}1\n\
                     [11:00] Cafe (42.36010,-71.05890)\n";
        assert_eq!(pts("visits", "visit", visit), vec![(42.3601, -71.0589)]);
        let unnamed = "arrival: 2026-08-10T15:00:00Z\nobservations: ios-visits\u{d7}1\n\
                       [11:00] (42.360, -71.059) (42.36010,-71.05890)\n";
        assert_eq!(pts("visits", "visit", unnamed), vec![(42.3601, -71.0589)]);

        // nothing else is read at all
        assert!(pts("mbox", "email", "[12:00] x (1,2)\n").is_empty());
    }

    /// Only fb_post bodies are read, and an episode the older parser
    /// indexed is read once more: its forged points go, its real ones stay.
    #[test]
    fn test_backfill_rereads_with_the_strict_parser() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let actor = Actor::new("human:test");
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:facebook".into(),
                    session_id: None,
                    episodes: vec![
                        episode(
                            "facebook",
                            "fb_message",
                            "thread:ada:2026-07-01",
                            "2026-07-01",
                            "ada: hi\n[12:00] ada (40.71,-74.00)\n",
                        ),
                        episode(
                            "facebook",
                            "fb_post",
                            "post:1",
                            "2026-07-01",
                            "Lunch\n\n[12:00] Lakeside Park (12.34,-56.78)\n",
                        ),
                        episode(
                            "photos",
                            "photos",
                            "photos:2026-07-01",
                            "2026-07-01",
                            "date: 2026-07-01\nphotos: 1\n\n[08:00] x (3,4).heic\n",
                        ),
                    ],
                },
            )
            .unwrap();
        let id = |source: &str, source_ref: &str| crate::ids::episode_id(source, source_ref);
        let message = id("facebook", "thread:ada:2026-07-01");
        let photo = id("photos", "photos:2026-07-01");
        // what the older parser left behind: points from the message and
        // from the file name, every episode marked scanned
        {
            let conn = store.lock();
            for (episode, lat, lon) in [(&message, 40.71, -74.0), (&photo, 3.0, 4.0)] {
                conn.execute(
                    "INSERT INTO location_points (episode_id, ts, day, lat, lon)
                     VALUES (?1, '2026-07-01T12:00', '2026-07-01', ?2, ?3)",
                    rusqlite::params![episode, lat, lon],
                )
                .unwrap();
            }
            conn.execute("UPDATE episodes SET location_scanned = 1", [])
                .unwrap();
        }
        let (scanned, added) = store.location_backfill(&actor, 100).unwrap();
        assert_eq!((scanned, added), (3, 1));
        assert_eq!(store.location_backfill(&actor, 100).unwrap(), (0, 0));
        let points: Vec<(String, f64, f64)> = {
            let conn = store.lock();
            let mut stmt = conn
                .prepare("SELECT episode_id, lat, lon FROM location_points")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert_eq!(points, vec![(id("facebook", "post:1"), 12.34, -56.78)]);
    }

    /// The GPX pass only ever sees workouts — never a clinical record,
    /// whose body quotes what a provider wrote.
    #[test]
    fn test_location_pending_is_one_kind() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:apple-health".into(),
                    session_id: None,
                    episodes: vec![
                        episode(
                            "apple-health",
                            "workout",
                            "workout:2026-07-01",
                            "2026-07-01",
                            "workout: Running\nroute: /workout-routes/route_1.gpx\n",
                        ),
                        episode(
                            "apple-health",
                            "clinical_record",
                            "clinical:1",
                            "2026-07-01",
                            "Observation: x\nroute: ../../../etc/passwd\ndate: 2026-07-01",
                        ),
                    ],
                },
            )
            .unwrap();
        let pending = store
            .location_pending("apple-health", "workout", 10)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].0,
            crate::ids::episode_id("apple-health", "workout:2026-07-01")
        );
    }

    #[test]
    fn test_backfill_and_atlas_vault_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let actor = Actor::new("human:test");

        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:photos".into(),
                    session_id: None,
                    episodes: vec![
                        photo_episode(
                            "2026-07-01",
                            "[08:00] a.heic (41.89,12.49)\n[09:00] b.heic (41.89,12.49)",
                        ),
                        photo_episode("2026-07-02", "[10:00] c.heic (48.85,2.35)"),
                    ],
                },
            )
            .unwrap();
        let (scanned, added) = store.location_backfill(&actor, 100).unwrap();
        assert_eq!((scanned, added), (2, 3));
        // Idempotent: nothing left to scan.
        assert_eq!(store.location_backfill(&actor, 100).unwrap(), (0, 0));

        let points = store.atlas_points("2026-01-01", "2026-12-31", 100).unwrap();
        assert_eq!(points.len(), 2, "clustered per day+rounded location");
        assert_eq!(points[0].n, 1); // one episode, two photos, same cluster

        // Vault guard: mark the Paris day private → it vanishes while locked.
        let paris_id = {
            let conn = store.lock();
            conn.query_row(
                "SELECT episode_id FROM location_points WHERE day='2026-07-02'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
        };
        store
            .set_episode_scopes(&actor, &[paris_id], "private")
            .unwrap();
        let locked = store.atlas_points("2026-01-01", "2026-12-31", 100).unwrap();
        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].day, "2026-07-01");
        store.vault_unlock(15);
        assert_eq!(
            store
                .atlas_points("2026-01-01", "2026-12-31", 100)
                .unwrap()
                .len(),
            2
        );
    }
}
