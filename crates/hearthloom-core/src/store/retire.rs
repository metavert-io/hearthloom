//! REVERSIBLE RETIREMENT (D.12, schema v59) — take a memory, or a whole
//! source, out of the record without deleting anything, and put all of it
//! back with one undo.
//!
//! The rule that makes it hold everywhere: a retired episode is marked
//! `superseded_by = 'retired:<retirement id>'`. Every reader of the record
//! honours supersession (`test_every_reader_honours_supersession` fails
//! the build on a new one that doesn't), so retirement needs no reader of
//! its own. What the memory FED is switched off, with the prior state of
//! every row touched saved in `retirement_rows`:
//!   entity links; edge support (an edge left with none is retracted);
//!   intent matches; opportunities; map points; semantic pairs;
//!   the cascades it seeded and the opportunity it became (retired the
//!   same way); entities that existed only because of it (hidden, never
//!   deleted); about-me beliefs that cited it or a summary being rewritten
//!   (back to Tend); for a whole source, its registry row.
//! Summaries (daily digests, weekly reviews, chapter headings) are
//! rewritten, never patched: the ones the person leaves ticked in the
//! preview retire with the memory, and the thinking node writes each one
//! again from its inputs minus what was retired, under
//! `<replaced ref>@<retirement id>` (daemon consolidate.rs).
//!
//! Sync: the EVENT syncs (`retirements`, /v1/sync/retirements, LWW by
//! updated_at) and each node applies it itself — entity and edge ids are
//! minted per node — keeping its own undo record. An episode that arrives
//! later from a retired source, a retired memory that arrives late, and a
//! cascade seeded by a retired memory all land retired
//! (`ArrivalRules`, called from ingest_apply).

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::episodes::{episode_from_row, EPISODE_COLUMNS};
use super::{journal_row, record_mutation, Actor, EpisodeRow, Store};
use crate::ids;
use crate::ingest::one_line;
use crate::time::now_utc;

/// `episodes.superseded_by` prefix that marks a retired episode.
pub const RETIRED_PREFIX: &str = "retired:";

/// Summary kinds a retirement rewrites (all source `consolidation`):
/// daily digests, weekly reviews, chapter headings.
pub const SUMMARY_KINDS: &[&str] = &["summary", "review", "chapter"];

/// The only tables an undo replays into.
const TOUCHABLE: &[&str] = &[
    "episodes", "episode_entities", "edge_provenance", "edges", "intent_matches",
    "opportunities", "location_points", "semantic_pairs", "entities", "self_model", "sources",
];

/// Episodes per transaction when applying (the store-lock rule).
const BATCH: usize = 200;

/// How many entities a preview checks one by one.
const PREVIEW_ENTITY_CAP: usize = 5000;

/// One retirement event (the synced row).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetirementRow {
    pub id: String,
    /// `memory` | `source`
    pub kind: String,
    /// An episode id, or a source name.
    pub target: String,
    /// What the person saw: the memory's title, or the source's name.
    pub label: String,
    /// Retired private material — the row hides while the vault is locked.
    pub private: bool,
    /// Summaries ticked to be rewritten.
    pub regenerate: Vec<String>,
    pub reason: String,
    pub actor: String,
    pub created_at: String,
    pub undone_at: Option<String>,
    pub updated_at: String,
}

const RETIREMENT_COLUMNS: &str =
    "id, kind, target, label, private, regenerate, reason, actor, created_at, undone_at, updated_at";

fn retirement_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RetirementRow> {
    let regenerate: String = row.get(5)?;
    Ok(RetirementRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        target: row.get(2)?,
        label: row.get(3)?,
        private: row.get::<_, i64>(4)? != 0,
        regenerate: serde_json::from_str(&regenerate).unwrap_or_default(),
        reason: row.get(6)?,
        actor: row.get(7)?,
        created_at: row.get(8)?,
        undone_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

/// What applying a retirement changed on this node.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct RetireCounts {
    pub episodes: usize,
    pub links: usize,
    pub edges: usize,
    pub edges_retracted: usize,
    pub matches: usize,
    pub opportunities: usize,
    pub map_points: usize,
    pub pairs: usize,
    pub entities_hidden: usize,
    pub beliefs: usize,
}

impl RetireCounts {
    fn add(&mut self, o: RetireCounts) {
        self.episodes += o.episodes;
        self.links += o.links;
        self.edges += o.edges;
        self.edges_retracted += o.edges_retracted;
        self.matches += o.matches;
        self.opportunities += o.opportunities;
        self.map_points += o.map_points;
        self.pairs += o.pairs;
        self.entities_hidden += o.entities_hidden;
        self.beliefs += o.beliefs;
    }
}

/// A summary waiting to be written again without what was retired.
#[derive(Debug, Clone)]
pub struct RewriteJob {
    pub retirement_id: String,
    /// The summary being replaced (retired by this retirement).
    pub summary_id: String,
    /// summary | review | chapter
    pub kind: String,
    /// Its ref, e.g. `chapter:2026-09` — the window it covered.
    pub source_ref: String,
    /// The ref the rewrite lands under: `<source_ref>@<retirement id>`.
    pub new_ref: String,
    pub event_time: Option<String>,
    /// When the replaced summary was written (its window's end).
    pub ingest_time: String,
}

/// A summary's window key without a rewrite (`@…`) or revision (`#…`)
/// suffix: `chapter:2026-09@rt_…` → `chapter:2026-09`.
pub fn base_ref(source_ref: &str) -> &str {
    let end = source_ref.find(['@', '#']).unwrap_or(source_ref.len());
    &source_ref[..end]
}

/// The seed episode named by a cascade's ref, `cascade:<intent>:<seed>`
/// (plus an optional `#r<n>` / `@<retirement>` suffix).
fn cascade_seed(source_ref: &str) -> Option<&str> {
    let rest = source_ref.strip_prefix("cascade:")?;
    let (_, seed) = rest.split_once(':')?;
    seed.split(['#', '@']).next().filter(|s| !s.is_empty())
}

fn applied_key(rid: &str) -> String {
    format!("retirement.applied.{rid}")
}

fn sql_value(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        Value::Number(n) => match n.as_i64() {
            Some(i) => rusqlite::types::Value::Integer(i),
            None => rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::Bool(b) => rusqlite::types::Value::Integer(*b as i64),
        _ => rusqlite::types::Value::Null,
    }
}

/// Every row a one-parameter statement returns, as JSON objects.
fn rows_json(conn: &Connection, sql: &str, param: &str) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let rows = stmt.query_map([param], |row| {
        let mut obj = serde_json::Map::new();
        for (i, c) in cols.iter().enumerate() {
            let v: rusqlite::types::Value = row.get(i)?;
            obj.insert(
                c.clone(),
                match v {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(n) => json!(n),
                    rusqlite::types::Value::Real(f) => json!(f),
                    rusqlite::types::Value::Text(t) => json!(t),
                    rusqlite::types::Value::Blob(_) => Value::Null,
                },
            );
        }
        Ok(Value::Object(obj))
    })?;
    rows.collect::<std::result::Result<_, _>>().map_err(Into::into)
}

/// One line of the undo record.
fn record(conn: &Connection, rid: &str, tbl: &str, op: &str, key: &str, prior: &Value) -> Result<()> {
    conn.execute(
        "INSERT INTO retirement_rows (retirement_id, tbl, op, row_key, prior)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![rid, tbl, op, key, prior.to_string()],
    )?;
    Ok(())
}

/// Delete the rows `filter` selects (one `?1`), saving each one first.
fn delete_saved(conn: &Connection, rid: &str, table: &str, filter: &str, param: &str) -> Result<usize> {
    let rows = rows_json(conn, &format!("SELECT * FROM {table} WHERE {filter}"), param)?;
    for r in &rows {
        record(conn, rid, table, "deleted", "", r)?;
    }
    if !rows.is_empty() {
        conn.execute(&format!("DELETE FROM {table} WHERE {filter}"), [param])?;
    }
    Ok(rows.len())
}

/// The episodes a retirement covers on THIS node that aren't retired yet:
/// the target(s), plus what was derived from them.
fn covered_episodes(conn: &Connection, kind: &str, target: &str) -> Result<Vec<String>> {
    let mut stmt = match kind {
        "memory" => conn.prepare(
            "SELECT id FROM episodes /* all-episodes: the memory being retired */
             WHERE id = ?1 AND (superseded_by IS NULL OR superseded_by NOT LIKE 'retired:%')",
        )?,
        "source" => conn.prepare(
            "SELECT id FROM episodes /* all-episodes: a retired source takes every episode it has */
             WHERE source = ?1 AND (superseded_by IS NULL OR superseded_by NOT LIKE 'retired:%')",
        )?,
        other => bail!("unknown retirement kind {other:?}"),
    };
    let mut ids: Vec<String> = stmt
        .query_map([target], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    let seeds: HashSet<String> = ids.iter().cloned().collect();
    ids.extend(derived_from(conn, &seeds)?);
    Ok(ids)
}

/// Episodes derived from `seeds`, to a fixpoint: cascades they seeded and
/// the opportunity episodes they became. Not yet retired ones only.
fn derived_from(conn: &Connection, seeds: &HashSet<String>) -> Result<Vec<String>> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id, source_ref FROM episodes /* all-episodes: walking what was derived */
         WHERE source = 'consolidation' AND kind = 'cascade'
           AND (superseded_by IS NULL OR superseded_by NOT LIKE 'retired:%')",
    )?;
    let cascades: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    let mut stmt = conn.prepare(
        "SELECT id, episode_id FROM opportunities /* all-episodes: walking what was derived */
         WHERE status = 'kept'",
    )?;
    let kept: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    let mut all: HashSet<String> = seeds.clone();
    let mut frontier: HashSet<String> = seeds.clone();
    let mut out = Vec::new();
    for _ in 0..4 {
        let mut next: HashSet<String> = HashSet::new();
        for (id, source_ref) in &cascades {
            if !all.contains(id) && cascade_seed(source_ref).is_some_and(|s| frontier.contains(s)) {
                next.insert(id.clone());
            }
        }
        for (opportunity, episode) in &kept {
            if !frontier.contains(episode) {
                continue;
            }
            let minted = ids::episode_id("consolidation", &format!("opportunity:{opportunity}"));
            if all.contains(&minted) {
                continue;
            }
            let live: Option<Option<String>> = conn
                .query_row(
                    "SELECT superseded_by FROM episodes /* all-episodes: walking what was derived */
                     WHERE id = ?1",
                    [&minted],
                    |r| r.get(0),
                )
                .optional()?;
            if live.is_some_and(|s| !s.is_some_and(|s| s.starts_with(RETIRED_PREFIX))) {
                next.insert(minted);
            }
        }
        if next.is_empty() {
            break;
        }
        all.extend(next.iter().cloned());
        out.extend(next.iter().cloned());
        frontier = next;
    }
    Ok(out)
}

/// Retire ONE episode inside an open transaction: mark it, switch off what
/// it fed, save the prior state of every row touched. Entities it linked
/// go into `linked` (candidates for hiding). An episode already retired —
/// by this retirement or another — is left alone.
fn retire_episode(
    conn: &Connection,
    rid: &str,
    episode_id: &str,
    now: &str,
    linked: &mut HashSet<String>,
) -> Result<RetireCounts> {
    let mut c = RetireCounts::default();
    let prior: Option<Option<String>> = conn
        .query_row(
            "SELECT superseded_by FROM episodes /* all-episodes: the row being retired */
             WHERE id = ?1",
            [episode_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(prior) = prior else { return Ok(c) };
    if prior.as_deref().is_some_and(|p| p.starts_with(RETIRED_PREFIX)) {
        return Ok(c);
    }
    record(conn, rid, "episodes", "updated", episode_id, &json!({"superseded_by": prior}))?;
    conn.execute(
        "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
        params![format!("{RETIRED_PREFIX}{rid}"), episode_id],
    )?;
    c.episodes = 1;

    let links = rows_json(
        conn,
        "SELECT * FROM episode_entities /* all-episodes: the links being switched off */
         WHERE episode_id = ?1",
        episode_id,
    )?;
    for link in &links {
        record(conn, rid, "episode_entities", "deleted", "", link)?;
        if let Some(entity) = link["entity_id"].as_str() {
            linked.insert(entity.to_string());
        }
    }
    if !links.is_empty() {
        conn.execute("DELETE FROM episode_entities WHERE episode_id = ?1", [episode_id])?;
    }
    c.links = links.len();

    // Edge support: the provenance goes, the edge loses the unit of
    // strength it gave, and an edge left with no support is retracted.
    let mut stmt = conn.prepare(
        "SELECT edge_id FROM edge_provenance /* all-episodes: the support being removed */
         WHERE episode_id = ?1",
    )?;
    let edges: Vec<String> = stmt
        .query_map([episode_id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    for edge_id in &edges {
        record(
            conn, rid, "edge_provenance", "deleted", "",
            &json!({"edge_id": edge_id, "episode_id": episode_id}),
        )?;
        conn.execute(
            "DELETE FROM edge_provenance WHERE edge_id = ?1 AND episode_id = ?2",
            params![edge_id, episode_id],
        )?;
        let Some((strength, expired)) = conn
            .query_row("SELECT strength, t_expired FROM edges WHERE id = ?1", [edge_id], |r| {
                Ok((r.get::<_, f64>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .optional()?
        else {
            continue;
        };
        let remaining: i64 = conn.query_row(
            "SELECT COUNT(*) FROM edge_provenance /* all-episodes: support left after removal */
             WHERE edge_id = ?1",
            [edge_id],
            |r| r.get(0),
        )?;
        let lowered = (strength - 1.0).max(1.0);
        let retract = remaining == 0 && expired.is_none();
        record(
            conn, rid, "edges", "updated", edge_id,
            &json!({
                "strength_delta": strength - lowered,
                "t_expired": expired,
                "set_expired": if retract { json!(now) } else { Value::Null },
            }),
        )?;
        conn.execute(
            "UPDATE edges SET strength = ?1,
                 t_expired = CASE WHEN ?2 THEN ?3 ELSE t_expired END
             WHERE id = ?4",
            params![lowered, retract, now, edge_id],
        )?;
        c.edges += 1;
        if retract {
            c.edges_retracted += 1;
        }
    }
    c.matches = delete_saved(conn, rid, "intent_matches", "episode_id = ?1", episode_id)?;
    c.opportunities = delete_saved(conn, rid, "opportunities", "episode_id = ?1", episode_id)?;
    c.map_points = delete_saved(conn, rid, "location_points", "episode_id = ?1", episode_id)?;
    c.pairs = delete_saved(conn, rid, "semantic_pairs", "a = ?1 OR b = ?1", episode_id)?;
    Ok(c)
}

/// What decides "existed only because of it", loaded once (merged_into
/// has no index; goal_projects is tiny).
struct OrphanContext {
    merge_targets: HashSet<String>,
    goal_projects: HashSet<String>,
}

impl OrphanContext {
    fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT DISTINCT merged_into FROM entities WHERE merged_into IS NOT NULL")?;
        let merge_targets = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut stmt = conn.prepare("SELECT project_id FROM goal_projects")?;
        let goal_projects = stmt
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(OrphanContext { merge_targets, goal_projects })
    }

    /// Would this entity have nothing left — no live evidence beyond
    /// `leaving` links, nothing the person declared about it, no merge
    /// pointing at it? Declared importance, a relationship, a mention in a
    /// note or task and a goal's project link all count as declared.
    fn orphan(&self, conn: &Connection, id: &str, leaving: i64) -> Result<bool> {
        if self.merge_targets.contains(id) || self.goal_projects.contains(id) {
            return Ok(false);
        }
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT en.declared_importance,
                        (SELECT COUNT(*) FROM episode_entities ee
                         JOIN episodes e ON e.id = ee.episode_id
                         WHERE ee.entity_id = en.id AND e.superseded_by IS NULL)
                 FROM entities en
                 WHERE en.id = ?1 AND en.hidden_by IS NULL AND en.merged_into IS NULL
                   AND NOT EXISTS (SELECT 1 FROM relationships r WHERE r.entity_id = en.id)
                   AND NOT EXISTS (SELECT 1 FROM note_mentions m WHERE m.entity_id = en.id)
                   AND NOT EXISTS (SELECT 1 FROM task_mentions m WHERE m.entity_id = en.id)",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.is_some_and(|(declared, live)| declared == 0 && live <= leaving))
    }
}

/// Hide the entities in `candidates` that have nothing left. Hidden,
/// never deleted; a new live link un-hides (ingest_apply).
fn hide_orphans(conn: &Connection, rid: &str, candidates: &HashSet<String>, ctx: &OrphanContext) -> Result<usize> {
    let mut hidden = 0;
    for id in candidates {
        if ctx.orphan(conn, id, 0)? {
            record(conn, rid, "entities", "updated", id, &json!({"hidden_by": null}))?;
            conn.execute("UPDATE entities SET hidden_by = ?1 WHERE id = ?2", params![rid, id])?;
            hidden += 1;
        }
    }
    Ok(hidden)
}

/// About-me beliefs citing anything in `cited` go back to Tend (confirmed
/// → proposed), prior state saved and journaled — self_model is
/// operational state and syncs.
fn beliefs_back(conn: &Connection, rid: &str, actor: &Actor, cited: &HashSet<String>, now: &str) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, status, confirmed_at, evidence FROM self_model
         WHERE status = 'confirmed' AND evidence IS NOT NULL",
    )?;
    let rows: Vec<(String, String, Option<String>, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    let mut back = 0;
    for (id, status, confirmed_at, evidence) in rows {
        let evidence: Vec<String> = serde_json::from_str(&evidence).unwrap_or_default();
        if !evidence.iter().any(|e| cited.contains(e)) {
            continue;
        }
        record(conn, rid, "self_model", "updated", &id, &json!({"status": status, "confirmed_at": confirmed_at}))?;
        conn.execute(
            "UPDATE self_model SET status = 'proposed', confirmed_at = NULL, updated_at = ?1
             WHERE id = ?2",
            params![now, id],
        )?;
        journal_row(conn, actor, "self_model", "self_model", &id, "update")?;
        back += 1;
    }
    Ok(back)
}

/// Put one recorded change back. Conditional where something newer may
/// have happened since: a memory superseded by a newer revision stays
/// superseded, a belief the person decided on again stays decided.
#[allow(clippy::too_many_arguments)]
fn restore_row(
    conn: &Connection,
    rid: &str,
    actor: &Actor,
    tbl: &str,
    op: &str,
    key: &str,
    prior: &Value,
    now: &str,
) -> Result<()> {
    if !TOUCHABLE.contains(&tbl) {
        return Ok(());
    }
    match (op, tbl) {
        ("deleted", _) => {
            let Some(obj) = prior.as_object() else { return Ok(()) };
            if obj.is_empty()
                || obj.keys().any(|c| !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_'))
            {
                return Ok(());
            }
            let cols: Vec<&str> = obj.keys().map(String::as_str).collect();
            let marks: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
            let inserted = conn.execute(
                &format!("INSERT OR IGNORE INTO {tbl} ({}) VALUES ({})", cols.join(", "), marks.join(", ")),
                rusqlite::params_from_iter(obj.values().map(sql_value)),
            )?;
            // A link coming back is live evidence again: an entity another
            // retirement hid for lack of evidence is visible again.
            if inserted > 0 && tbl == "episode_entities" {
                if let Some(entity) = obj.get("entity_id").and_then(|v| v.as_str()) {
                    conn.execute(
                        "UPDATE entities SET hidden_by = NULL WHERE id = ?1 AND hidden_by IS NOT NULL",
                        [entity],
                    )?;
                }
            }
        }
        ("updated", "episodes") => {
            conn.execute(
                "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2 AND superseded_by = ?3",
                params![prior["superseded_by"].as_str(), key, format!("{RETIRED_PREFIX}{rid}")],
            )?;
        }
        ("updated", "edges") => {
            conn.execute(
                "UPDATE edges SET strength = strength + ?1,
                     t_expired = CASE WHEN ?2 IS NOT NULL AND t_expired = ?2 THEN ?3 ELSE t_expired END
                 WHERE id = ?4",
                params![
                    prior["strength_delta"].as_f64().unwrap_or(0.0),
                    prior["set_expired"].as_str(),
                    prior["t_expired"].as_str(),
                    key
                ],
            )?;
        }
        ("updated", "entities") => {
            conn.execute(
                "UPDATE entities SET hidden_by = NULL WHERE id = ?1 AND hidden_by = ?2",
                params![key, rid],
            )?;
        }
        ("updated", "self_model") => {
            let changed = conn.execute(
                "UPDATE self_model SET status = ?1, confirmed_at = ?2, updated_at = ?3
                 WHERE id = ?4 AND status = 'proposed'",
                params![
                    prior["status"].as_str().unwrap_or("confirmed"),
                    prior["confirmed_at"].as_str(),
                    now,
                    key
                ],
            )?;
            if changed > 0 {
                journal_row(conn, actor, "self_model", "self_model", key, "update")?;
            }
        }
        ("updated", "sources") => {
            conn.execute(
                "UPDATE sources SET retired_by = NULL WHERE name = ?1 AND retired_by = ?2",
                params![key, rid],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Summaries rewritten for an undone retirement step back behind the
/// summary each one replaced (which is live again).
fn hide_rewrites(conn: &Connection, rid: &str) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, source_ref FROM episodes
         WHERE source = 'consolidation' AND source_ref LIKE ?1 AND superseded_by IS NULL",
    )?;
    let rows: Vec<(String, String)> = stmt
        .query_map([format!("%@{rid}")], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    for (id, source_ref) in &rows {
        let replaced = source_ref.rsplit_once('@').map(|(r, _)| r).unwrap_or(source_ref);
        conn.execute(
            "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
            params![ids::episode_id("consolidation", replaced), id],
        )?;
    }
    Ok(rows.len())
}

/// What happens to a NEW episode as it lands.
pub(crate) enum Arrival {
    /// Nothing covers it.
    Live,
    /// A live retirement covers it: it lands retired under this id.
    Retire(String),
    /// A rewrite whose retirement was undone: it lands behind this summary.
    Behind(String),
}

/// What `apply_batch` checks each new episode against — loaded once per
/// batch (the table is small; empty for nearly everyone).
#[derive(Default)]
pub(crate) struct ArrivalRules {
    sources: HashMap<String, String>,
    memories: HashMap<String, String>,
    live: HashSet<String>,
    undone: HashSet<String>,
    orphans: OnceCell<OrphanContext>,
}

impl ArrivalRules {
    pub(crate) fn load(conn: &Connection) -> Result<Self> {
        let mut rules = ArrivalRules::default();
        let mut stmt = conn.prepare("SELECT id, kind, target, regenerate, undone_at FROM retirements")?;
        let rows: Vec<(String, String, String, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for (id, kind, target, regenerate, undone) in rows {
            if undone.is_some() {
                rules.undone.insert(id);
                continue;
            }
            if kind == "source" {
                rules.sources.insert(target, id.clone());
            } else {
                rules.memories.insert(target, id.clone());
            }
            for summary in serde_json::from_str::<Vec<String>>(&regenerate).unwrap_or_default() {
                rules.memories.insert(summary, id.clone());
            }
            rules.live.insert(id);
        }
        Ok(rules)
    }

    /// Does a retirement cover this newly inserted episode? Covered: its
    /// source is retired; it is a retired memory or a summary being
    /// rewritten that arrived late; it is a cascade seeded by a retired
    /// memory. A rewrite whose retirement was undone lands behind the
    /// summary it replaced.
    pub(crate) fn covering(
        &self,
        conn: &Connection,
        episode_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Arrival> {
        if self.live.is_empty() && self.undone.is_empty() {
            return Ok(Arrival::Live);
        }
        if let Some(rid) = self.sources.get(source).or_else(|| self.memories.get(episode_id)) {
            return Ok(Arrival::Retire(rid.clone()));
        }
        // A fuller revision of a retired memory (`<base>#r<n>` — photos
        // days, visits, contact cards) stays retired: the person retired
        // that day / visit / card, not one copy of it.
        if let Some((base, _)) = source_ref
            .rsplit_once("#r")
            .filter(|(_, n)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            .filter(|_| super::ingest_apply::REVISIONED_SOURCES.contains(&source))
        {
            let mut stmt = conn.prepare(
                "SELECT superseded_by FROM episodes /* all-episodes: is an earlier revision retired? */
                 WHERE source = ?1 AND (source_ref = ?2 OR source_ref LIKE ?2 || '#r%')
                   AND superseded_by LIKE 'retired:%' AND id != ?3",
            )?;
            let states: Vec<String> = stmt
                .query_map(params![source, base, episode_id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            if let Some(rid) = states
                .iter()
                .filter_map(|s| s.strip_prefix(RETIRED_PREFIX))
                .find(|rid| self.live.contains(*rid))
            {
                return Ok(Arrival::Retire(rid.to_string()));
            }
        }
        if source != "consolidation" {
            return Ok(Arrival::Live);
        }
        if let Some(seed) = cascade_seed(source_ref) {
            let seed_state: Option<Option<String>> = conn
                .query_row(
                    "SELECT superseded_by FROM episodes /* all-episodes: is the seed retired? */
                     WHERE id = ?1",
                    [seed],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(rid) = seed_state
                .flatten()
                .and_then(|s| s.strip_prefix(RETIRED_PREFIX).map(String::from))
                .filter(|rid| self.live.contains(rid))
            {
                return Ok(Arrival::Retire(rid));
            }
        }
        if let Some((replaced, rid)) = source_ref.rsplit_once('@') {
            if self.undone.contains(rid) {
                return Ok(Arrival::Behind(ids::episode_id("consolidation", replaced)));
            }
        }
        Ok(Arrival::Live)
    }

    /// Retire a newly arrived episode (inside the ingest transaction).
    pub(crate) fn retire(&self, conn: &Connection, rid: &str, episode_id: &str) -> Result<()> {
        let mut linked = HashSet::new();
        retire_episode(conn, rid, episode_id, &now_utc(), &mut linked)?;
        if !linked.is_empty() {
            if self.orphans.get().is_none() {
                let _ = self.orphans.set(OrphanContext::load(conn)?);
            }
            if let Some(ctx) = self.orphans.get() {
                hide_orphans(conn, rid, &linked, ctx)?;
            }
        }
        Ok(())
    }
}

/// A preview's view of one covered episode.
struct Covered {
    id: String,
    title: String,
    source: String,
    kind: String,
    scope: String,
    ingest_time: String,
    event_key: String,
}

impl Store {
    /// One retirement event.
    pub fn retirement(&self, id: &str) -> Result<Option<RetirementRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {RETIREMENT_COLUMNS} FROM retirements WHERE id = ?1"),
            [id],
            retirement_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Retirements, newest first, each with this node's state (`applying`,
    /// `done`, `undone`) and what it changed here. Locked, retirements of
    /// private material are left out entirely.
    pub fn retirements(&self, limit: usize) -> Result<Vec<Value>> {
        let hide_private = !self.private_visible();
        let rows: Vec<RetirementRow> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(&format!(
                "SELECT {RETIREMENT_COLUMNS} FROM retirements ORDER BY created_at DESC LIMIT ?1"
            ))?;
            let rows = stmt
                .query_map([limit as i64], retirement_from_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut out = Vec::new();
        for r in rows.into_iter().filter(|r| !(hide_private && r.private)) {
            let applied = self.consolidation_state(&applied_key(&r.id))?;
            let changed: Value = {
                let conn = self.lock();
                let mut stmt = conn.prepare(
                    "SELECT tbl, COUNT(*) FROM retirement_rows WHERE retirement_id = ?1 GROUP BY tbl",
                )?;
                let pairs: Vec<(String, i64)> = stmt
                    .query_map([&r.id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                Value::Object(pairs.into_iter().map(|(t, n)| (t, json!(n))).collect())
            };
            let state = if r.undone_at.is_some() {
                "undone"
            } else if applied.is_some() {
                "done"
            } else {
                "applying"
            };
            let mut v = serde_json::to_value(&r)?;
            v["state"] = json!(state);
            v["changed"] = changed;
            out.push(v);
        }
        Ok(out)
    }

    /// Record a retirement (the synced event). The caller applies it with
    /// `retirement_apply` — off the request path for a whole source.
    /// `regenerate` = the summaries left ticked in the preview.
    pub fn retire(
        &self,
        actor: &Actor,
        kind: &str,
        target: &str,
        regenerate: &[String],
        reason: &str,
    ) -> Result<RetirementRow> {
        let (label, private) = {
            let conn = self.lock();
            match kind {
                "memory" => {
                    let (title, scope, superseded): (String, String, Option<String>) = conn
                        .query_row(
                            "SELECT title, scope, superseded_by FROM episodes /* all-episodes: the memory being retired */
                             WHERE id = ?1",
                            [target],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?
                        .ok_or_else(|| anyhow!("no memory {target}"))?;
                    if superseded.is_some_and(|s| s.starts_with(RETIRED_PREFIX)) {
                        bail!("that memory is already retired");
                    }
                    (if title.trim().is_empty() { target.to_string() } else { title }, scope == "private")
                }
                "source" => {
                    let (n, private): (i64, i64) = conn.query_row(
                        "SELECT COUNT(*), COALESCE(SUM(scope = 'private'), 0) FROM episodes /* all-episodes: sizing a source */
                         WHERE source = ?1",
                        [target],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    if n == 0 {
                        bail!("no source called {target}");
                    }
                    let live: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM retirements
                         WHERE kind = 'source' AND target = ?1 AND undone_at IS NULL",
                        [target],
                        |r| r.get(0),
                    )?;
                    if live > 0 {
                        bail!("{target} is already retired");
                    }
                    (target.to_string(), private > 0)
                }
                other => bail!("retire a memory or a source, not {other:?}"),
            }
        };
        let id = format!("rt_{}", uuid::Uuid::new_v4().simple());
        let now = now_utc();
        let mut regenerate: Vec<String> = regenerate.to_vec();
        regenerate.sort();
        regenerate.dedup();
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO retirements
                     (id, kind, target, label, private, regenerate, reason, actor, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![
                    id, kind, target, label, private as i64,
                    serde_json::to_string(&regenerate)?, reason.trim(), actor.actor, now
                ],
            )?;
            record_mutation(
                &conn,
                actor,
                "retired",
                &format!("retired:{id}"),
                &json!({"id": id, "kind": kind, "target": target, "regenerate": regenerate.len()}),
            )?;
        }
        self.retirement(&id)?.ok_or_else(|| anyhow!("retirement {id} vanished"))
    }

    /// Beliefs a live retirement sent back to Tend: proposition id → what
    /// was retired (generic for private material while the vault is
    /// locked), so Tend can say why a confirmed belief is asking again.
    pub fn beliefs_sent_back(&self) -> Result<HashMap<String, String>> {
        let hide_private = !self.private_visible();
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT rr.row_key, r.label, r.private FROM retirement_rows rr
             JOIN retirements r ON r.id = rr.retirement_id AND r.undone_at IS NULL
             WHERE rr.tbl = 'self_model'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, label, private) = row?;
            out.insert(id, if hide_private && private != 0 { "a memory you retired".to_string() } else { label });
        }
        Ok(out)
    }

    /// Has this node applied (or undone) a retirement yet?
    pub fn retirement_state_here(&self, rid: &str) -> Result<Option<String>> {
        self.consolidation_state(&applied_key(rid))
    }

    /// Apply a retirement on THIS node. Idempotent — what is already
    /// retired is skipped, so a crash mid-way resumes cleanly. Batched:
    /// ≤200 episodes per lock hold, 40ms between holds.
    pub fn retirement_apply(&self, rid: &str) -> Result<RetireCounts> {
        let r = self.retirement(rid)?.ok_or_else(|| anyhow!("no retirement {rid}"))?;
        if r.undone_at.is_some() {
            return Ok(RetireCounts::default());
        }
        let actor = Actor::new(r.actor.clone());
        let now = now_utc();
        let ids: Vec<String> = {
            let conn = self.lock();
            let mut ids = covered_episodes(&conn, &r.kind, &r.target)?;
            let mut seen: HashSet<String> = ids.iter().cloned().collect();
            for summary in &r.regenerate {
                if seen.insert(summary.clone()) {
                    ids.push(summary.clone());
                }
            }
            ids
        };
        let mut counts = RetireCounts::default();
        let mut linked = HashSet::new();
        for chunk in ids.chunks(BATCH) {
            {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                for id in chunk {
                    counts.add(retire_episode(&tx, rid, id, &now, &mut linked)?);
                }
                tx.commit()?;
            }
            if ids.len() > BATCH {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
        {
            let mut conn = self.lock();
            let tx = conn.transaction()?;
            // Candidates from the whole undo record, so a run that resumes
            // after a crash still sees the entities of the first part.
            let mut stmt = tx.prepare(
                "SELECT prior FROM retirement_rows WHERE retirement_id = ?1 AND tbl = 'episode_entities'",
            )?;
            let priors: Vec<String> = stmt
                .query_map([rid], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            for prior in priors {
                if let Some(entity) = serde_json::from_str::<Value>(&prior)
                    .ok()
                    .and_then(|v| v["entity_id"].as_str().map(String::from))
                {
                    linked.insert(entity);
                }
            }
            let ctx = OrphanContext::load(&tx)?;
            counts.entities_hidden += hide_orphans(&tx, rid, &linked, &ctx)?;
            let mut stmt = tx.prepare("SELECT id FROM episodes WHERE superseded_by = ?1")?;
            let retired: HashSet<String> = stmt
                .query_map([format!("{RETIRED_PREFIX}{rid}")], |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            counts.beliefs += beliefs_back(&tx, rid, &actor, &retired, &now)?;
            if r.kind == "source" {
                let prior: Option<Option<String>> = tx
                    .query_row("SELECT retired_by FROM sources WHERE name = ?1", [&r.target], |row| row.get(0))
                    .optional()?;
                if let Some(None) = prior {
                    record(&tx, rid, "sources", "updated", &r.target, &json!({"retired_by": null}))?;
                    tx.execute("UPDATE sources SET retired_by = ?1 WHERE name = ?2", params![rid, r.target])?;
                }
            }
            tx.commit()?;
        }
        self.set_consolidation_state(&applied_key(rid), &now)?;
        Ok(counts)
    }

    /// Undo a retirement: the event is stamped (it syncs, so every node
    /// undoes it) and this node restores what it changed.
    pub fn retirement_undo(&self, actor: &Actor, rid: &str) -> Result<usize> {
        let now = now_utc();
        {
            let conn = self.lock();
            let changed = conn.execute(
                "UPDATE retirements SET undone_at = ?1, updated_at = ?1 WHERE id = ?2 AND undone_at IS NULL",
                params![now, rid],
            )?;
            if changed == 0 {
                bail!("no retirement {rid} to undo");
            }
            record_mutation(&conn, actor, "retirement_undone", &format!("retirement_undone:{rid}"), &json!({"id": rid}))?;
        }
        self.retirement_undo_local(rid)
    }

    /// Restore what this node changed for a retirement, newest change
    /// first, ≤500 changes per lock hold. Returns how many were restored.
    pub fn retirement_undo_local(&self, rid: &str) -> Result<usize> {
        let now = now_utc();
        let actor = Actor::new("system:retirement");
        let mut restored = 0;
        loop {
            let count = {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                let batch: Vec<(i64, String, String, String, String)> = {
                    let mut stmt = tx.prepare(
                        "SELECT seq, tbl, op, row_key, prior FROM retirement_rows
                         WHERE retirement_id = ?1 ORDER BY seq DESC LIMIT 500",
                    )?;
                    let rows = stmt
                        .query_map([rid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    rows
                };
                for (seq, tbl, op, key, prior) in &batch {
                    let prior: Value = serde_json::from_str(prior).unwrap_or(Value::Null);
                    restore_row(&tx, rid, &actor, tbl, op, key, &prior, &now)?;
                    tx.execute("DELETE FROM retirement_rows WHERE seq = ?1", [seq])?;
                }
                tx.commit()?;
                batch.len()
            };
            if count == 0 {
                break;
            }
            restored += count;
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        {
            let conn = self.lock();
            hide_rewrites(&conn, rid)?;
        }
        self.set_consolidation_state(&applied_key(rid), "undone")?;
        Ok(restored)
    }

    /// Retirements this node still has to apply or undo — for the catch-up
    /// after a restart or a sync.
    pub fn retirements_pending_here(&self) -> Result<Vec<(String, bool)>> {
        let rows: Vec<(String, Option<String>)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare("SELECT id, undone_at FROM retirements ORDER BY created_at")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut pending = Vec::new();
        for (id, undone) in rows {
            let here = self.consolidation_state(&applied_key(&id))?;
            match (undone.is_some(), here.as_deref()) {
                (false, None) => pending.push((id, false)),
                (true, Some(state)) if state != "undone" => pending.push((id, true)),
                _ => {}
            }
        }
        Ok(pending)
    }

    /// The preview shown before one confirm: grouped counts of what the
    /// retirement would change, the summaries it would rewrite (ticked by
    /// default; `exact` false where only the time window matched), the
    /// beliefs that would go back to Tend, the entities that would be
    /// hidden, and what can't be traced. Writes nothing.
    pub fn retirement_preview(&self, kind: &str, target: &str) -> Result<Value> {
        let hide = self.scope_hider();
        let covered: Vec<Covered> = {
            let conn = self.lock();
            let ids = covered_episodes(&conn, kind, target)?;
            let mut out = Vec::with_capacity(ids.len());
            let mut stmt = conn.prepare(
                "SELECT id, title, source, kind, scope, ingest_time, COALESCE(event_time, ingest_time)
                 FROM episodes /* all-episodes: describing what would be retired */ WHERE id = ?1",
            )?;
            for id in &ids {
                if let Some(c) = stmt
                    .query_row([id], |r| {
                        Ok(Covered {
                            id: r.get(0)?,
                            title: r.get(1)?,
                            source: r.get(2)?,
                            kind: r.get(3)?,
                            scope: r.get(4)?,
                            ingest_time: r.get(5)?,
                            event_key: r.get(6)?,
                        })
                    })
                    .optional()?
                {
                    out.push(c);
                }
            }
            out
        };
        if covered.is_empty() {
            bail!("nothing to retire there");
        }
        let direct = match kind {
            "memory" => 1usize.min(covered.len()),
            _ => covered.iter().filter(|c| c.source == target).count(),
        };
        let set: HashSet<String> = covered.iter().map(|c| c.id.clone()).collect();
        let ids: Vec<&str> = covered.iter().map(|c| c.id.as_str()).collect();

        // Grouped counts, ≤500 ids per query and per lock hold.
        let mut entity_links: HashMap<String, i64> = HashMap::new();
        let mut live_links: HashMap<String, i64> = HashMap::new();
        let mut edge_support: HashMap<String, i64> = HashMap::new();
        let (mut links, mut matches, mut opportunities, mut map_points, mut pairs) = (0i64, 0i64, 0i64, 0i64, 0i64);
        for chunk in ids.chunks(500) {
            let conn = self.lock();
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT ee.entity_id, COUNT(*), SUM(e.superseded_by IS NULL)
                 FROM episode_entities ee JOIN episodes e ON e.id = ee.episode_id
                 WHERE ee.episode_id IN ({marks}) /* all-episodes: the links that would go */
                 GROUP BY ee.entity_id"
            ))?;
            for row in stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
            })? {
                let (entity, n, live) = row?;
                links += n;
                *entity_links.entry(entity.clone()).or_default() += n;
                *live_links.entry(entity).or_default() += live;
            }
            drop(stmt);
            let mut stmt = conn.prepare(&format!(
                "SELECT edge_id, COUNT(*) FROM edge_provenance
                 WHERE episode_id IN ({marks}) /* all-episodes: the support that would go */
                 GROUP BY edge_id"
            ))?;
            for row in stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (edge, n) = row?;
                *edge_support.entry(edge).or_default() += n;
            }
            drop(stmt);
            let count = |sql: String| -> Result<i64> {
                Ok(conn.query_row(&sql, rusqlite::params_from_iter(chunk.iter()), |r| r.get(0))?)
            };
            matches += count(format!(
                "SELECT COUNT(*) FROM intent_matches WHERE episode_id IN ({marks}) /* all-episodes: would go */"
            ))?;
            opportunities += count(format!(
                "SELECT COUNT(*) FROM opportunities WHERE episode_id IN ({marks}) /* all-episodes: would go */"
            ))?;
            map_points += count(format!(
                "SELECT COUNT(*) FROM location_points WHERE episode_id IN ({marks})"
            ))?;
            let twice: Vec<&str> = chunk.iter().chain(chunk.iter()).copied().collect();
            pairs += conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM semantic_pairs /* all-episodes: would go */
                     WHERE a IN ({marks}) OR b IN ({marks})"
                ),
                rusqlite::params_from_iter(twice.iter()),
                |r| r.get::<_, i64>(0),
            )?;
        }
        // Edges left with no support would be retracted.
        let mut retracted = 0i64;
        let edge_ids: Vec<&String> = edge_support.keys().collect();
        for chunk in edge_ids.chunks(500) {
            let conn = self.lock();
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT p.edge_id, COUNT(*) FROM edge_provenance p /* all-episodes: total support */
                 JOIN edges g ON g.id = p.edge_id AND g.t_expired IS NULL
                 WHERE p.edge_id IN ({marks}) GROUP BY p.edge_id"
            ))?;
            for row in stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })? {
                let (edge, total) = row?;
                if total <= edge_support.get(&edge).copied().unwrap_or(0) {
                    retracted += 1;
                }
            }
        }
        // Entities that would have nothing left.
        let mut hidden: Vec<Value> = Vec::new();
        let mut hidden_total = 0usize;
        let mut unchecked = 0usize;
        {
            let conn = self.lock();
            let ctx = OrphanContext::load(&conn)?;
            let mut stmt = conn.prepare("SELECT name, type, scope FROM entities WHERE id = ?1")?;
            for (i, (entity, _)) in entity_links.iter().enumerate() {
                if i >= PREVIEW_ENTITY_CAP {
                    unchecked = entity_links.len() - PREVIEW_ENTITY_CAP;
                    break;
                }
                if !ctx.orphan(&conn, entity, live_links.get(entity).copied().unwrap_or(0))? {
                    continue;
                }
                hidden_total += 1;
                if hidden.len() < 40 {
                    if let Some((name, kind, scope)) = stmt
                        .query_row([entity], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
                        .optional()?
                    {
                        if !hide(&scope) {
                            hidden.push(json!({"id": entity, "name": name, "type": kind}));
                        }
                    }
                }
            }
        }
        let summaries = self.summaries_reading(&covered)?;
        let summary_ids: HashSet<String> = summaries
            .iter()
            .filter_map(|s| s["id"].as_str().map(String::from))
            .collect();
        // Beliefs: always back to Tend when they cite what is retired; back
        // when a summary they cite is rewritten (the UI follows the ticks).
        let mut beliefs = Vec::new();
        {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT id, sphere, text, evidence FROM self_model
                 WHERE status = 'confirmed' AND evidence IS NOT NULL",
            )?;
            let rows: Vec<(String, String, String, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (id, sphere, text, evidence) in rows {
                if hide(&sphere) {
                    continue;
                }
                let evidence: Vec<String> = serde_json::from_str(&evidence).unwrap_or_default();
                let direct = evidence.iter().any(|e| set.contains(e));
                let via: Vec<&String> = evidence.iter().filter(|e| summary_ids.contains(*e)).collect();
                if direct || !via.is_empty() {
                    beliefs.push(json!({"id": id, "text": text, "always": direct, "via": via}));
                }
            }
        }
        let asked: i64 = if kind == "memory" {
            let conn = self.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM ask_log WHERE citations LIKE ?1",
                [format!("%{target}%")],
                |r| r.get(0),
            )?
        } else {
            0
        };
        let mut untraceable: Vec<String> = Vec::new();
        if links > 0 {
            untraceable.push(
                "Attributes it added to people and organisations (an email address, a handle) stay: \
                 attributes don't record where they came from."
                    .into(),
            );
            untraceable.push(
                "Relationship briefs and topic pages don't record what they read; they refresh on \
                 their own schedule."
                    .into(),
            );
        }
        if summaries.iter().any(|s| s["exact"] == json!(false)) {
            untraceable.push(
                "Older digests and weekly reviews didn't record what they read; those listed matched \
                 by when the memory arrived, so a few may not have used it."
                    .into(),
            );
        }
        if covered.iter().any(|c| matches!(c.source.as_str(), "photos" | "visits")) {
            untraceable.push(
                "Location observations from your devices stay — they are device data, not part of \
                 the memory — and still feed visits."
                    .into(),
            );
        }
        if asked > 0 {
            untraceable.push(format!(
                "{asked} earlier Ask answer{} cited it; they stay in your Ask history.",
                if asked == 1 { "" } else { "s" }
            ));
        }
        let first = &covered[0];
        Ok(json!({
            "kind": kind,
            "target": target,
            "label": if kind == "memory" { first.title.clone() } else { target.to_string() },
            "private": covered.iter().any(|c| c.scope == "private"),
            "episodes": direct,
            "derived": covered.len() - direct,
            "counts": {
                "entity_links": links,
                "edges": edge_support.len(),
                "edges_retracted": retracted,
                "intent_matches": matches,
                "opportunities": opportunities,
                "map_points": map_points,
                "semantic_pairs": pairs,
                "entities_hidden": hidden_total,
                "entities_unchecked": unchecked,
            },
            "entities_hidden": hidden,
            "summaries": summaries,
            "beliefs": beliefs,
            "untraceable": untraceable,
        }))
    }

    /// Digests, weekly reviews and chapters that read any of `covered`:
    /// exact where the summary recorded its inputs (v59 on) or, for a
    /// chapter, its heading lists the memory's title; by time window for
    /// older digests and reviews (`exact: false`).
    fn summaries_reading(&self, covered: &[Covered]) -> Result<Vec<Value>> {
        let mut hits: HashMap<String, Value> = HashMap::new();
        let covered_ids: Vec<&str> = covered.iter().map(|c| c.id.as_str()).collect();
        let is_summary = |c: &Covered| c.source == "consolidation";
        // 1. recorded inputs — exact
        for chunk in covered_ids.chunks(500) {
            let conn = self.lock();
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT e.id, e.kind, e.title, COALESCE(e.event_time, e.ingest_time)
                 FROM summary_inputs s JOIN episodes e ON e.id = s.summary_id
                 WHERE s.episode_id IN ({marks}) AND e.superseded_by IS NULL"
            ))?;
            for row in stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
            })? {
                let (id, kind, title, day) = row?;
                if SUMMARY_KINDS.contains(&kind.as_str()) {
                    hits.insert(id.clone(), json!({"id": id, "kind": kind, "title": title,
                        "day": &day[..day.len().min(10)], "exact": true, "why": "it read this"}));
                }
            }
        }
        let recorded: HashSet<String> = {
            let conn = self.lock();
            let mut stmt = conn.prepare("SELECT DISTINCT summary_id FROM summary_inputs")?;
            let rows = stmt
                .query_map([], |r| r.get(0))?
                .collect::<std::result::Result<HashSet<String>, _>>()?;
            rows
        };
        let conn = self.lock();
        // 2. chapters that recorded nothing (written before v59): the
        //    heading lists the titles it was written from. Only those —
        //    a chapter that recorded its inputs is matched by step 1
        //    alone, never by its body, whose list quotes titles other
        //    people wrote (a calendar title with a line break in it once
        //    listed one more). Titles compare `one_line`d, as
        //    consolidate.rs chapter_draft now writes them.
        let mut by_month: HashMap<String, HashSet<String>> = HashMap::new();
        for c in covered.iter().filter(|c| !is_summary(c) && c.scope != "private") {
            if c.event_key.len() >= 7 {
                by_month.entry(c.event_key[..7].to_string()).or_default().insert(one_line(&c.title));
            }
        }
        let mut stmt = conn.prepare(
            "SELECT e.id, e.title, f.body FROM episodes e
             JOIN episode_fts f ON f.rowid = e.fts_rowid
             WHERE e.source = 'consolidation' AND e.kind = 'chapter' AND e.superseded_by IS NULL
               AND substr(COALESCE(e.event_time, e.ingest_time), 1, 7) = ?1",
        )?;
        for (month, titles) in &by_month {
            let rows: Vec<(String, String, String)> = stmt
                .query_map([month], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (id, heading, body) in rows {
                if hits.contains_key(&id) || recorded.contains(&id) {
                    continue;
                }
                // the list starts after the separator chapter_draft writes,
                // not at the first "written from:" (a heading could say it)
                let read: Vec<String> = body
                    .split_once("\n\nwritten from:\n")
                    .map(|(_, list)| list.lines().filter_map(|l| l.trim().strip_prefix("- ")).map(one_line).collect())
                    .unwrap_or_default();
                if let Some(title) = read.iter().find(|t| titles.contains(*t)) {
                    hits.insert(id.clone(), json!({"id": id, "kind": "chapter", "title": heading,
                        "day": month, "exact": true, "why": format!("its heading was written from “{title}”")}));
                }
            }
        }
        drop(stmt);
        // 3. weekly reviews: what arrived in the 7 days before it was written
        let mut arrivals: Vec<&str> = covered
            .iter()
            .filter(|c| !is_summary(c) && c.scope != "private" && c.kind != "health_day" && !c.title.is_empty())
            .map(|c| c.ingest_time.as_str())
            .collect();
        arrivals.sort_unstable();
        if let (Some(first), Some(_)) = (arrivals.first().copied(), arrivals.last()) {
            let mut stmt = conn.prepare(
                "SELECT id, title, ingest_time, COALESCE(event_time, ingest_time) FROM episodes
                 WHERE source = 'consolidation' AND kind = 'review' AND superseded_by IS NULL
                   AND ingest_time >= ?1",
            )?;
            let rows: Vec<(String, String, String, String)> = stmt
                .query_map([first], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (id, title, written, day) in rows {
                if hits.contains_key(&id) || recorded.contains(&id) {
                    continue;
                }
                let from = shift_days(&written, -7);
                let lo = arrivals.partition_point(|t| *t < from.as_str());
                if lo < arrivals.len() && arrivals[lo] <= written.as_str() {
                    hits.insert(id.clone(), json!({"id": id, "kind": "review", "title": title,
                        "day": &day[..day.len().min(10)], "exact": false, "why": "written the week it arrived"}));
                }
            }
        }
        // 4. daily digests: the newest 120 non-private episodes (by event
        //    time) that had arrived when it was written
        let digestible: Vec<&Covered> = covered
            .iter()
            .filter(|c| !is_summary(c) && c.scope != "private")
            .collect();
        if let Some(earliest) = digestible.iter().map(|c| c.ingest_time.as_str()).min() {
            let mut stmt = conn.prepare(
                "SELECT id, title, ingest_time FROM episodes
                 WHERE source = 'consolidation' AND kind = 'summary' AND superseded_by IS NULL
                   AND ingest_time >= ?1
                 ORDER BY ingest_time LIMIT 60",
            )?;
            let digests: Vec<(String, String, String)> = stmt
                .query_map([earliest], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            let mut newer = conn.prepare(
                "SELECT COUNT(*) FROM episodes /* all-episodes: what a past digest saw */
                 WHERE event_time > ?1 AND ingest_time <= ?2 AND scope != 'private'",
            )?;
            for (id, title, written) in digests {
                if hits.contains_key(&id) || recorded.contains(&id) {
                    continue;
                }
                // the newest covered episode that had arrived by then
                let Some(candidate) = digestible
                    .iter()
                    .filter(|c| c.ingest_time.as_str() <= written.as_str())
                    .max_by(|a, b| a.event_key.cmp(&b.event_key))
                else {
                    continue;
                };
                let ahead: i64 = newer.query_row(params![candidate.event_key, written], |r| r.get(0))?;
                if ahead < 120 {
                    hits.insert(id.clone(), json!({"id": id, "kind": "summary", "title": title,
                        "day": &written[..written.len().min(10)], "exact": false,
                        "why": "among the newest things when it was written"}));
                }
            }
        }
        let mut out: Vec<Value> = hits.into_values().collect();
        out.sort_by(|a, b| a["day"].as_str().cmp(&b["day"].as_str()));
        Ok(out)
    }

    /* ————— summaries: inputs and rewrites ————— */

    /// Record what a summary read — so a later retirement knows exactly
    /// which summaries it touches (synced with the retirements leg).
    pub fn record_summary_inputs(&self, summary_id: &str, inputs: &[String]) -> Result<()> {
        let now = now_utc();
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for input in inputs {
            tx.execute(
                "INSERT OR IGNORE INTO summary_inputs (summary_id, episode_id, ts) VALUES (?1, ?2, ?3)",
                params![summary_id, input, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Summaries waiting to be written again: ticked in a live
    /// retirement, retired here by it, and not rewritten yet (a rewrite
    /// that EXISTS counts, even if something later retired it — so a
    /// rewrite can never loop).
    pub fn rewrite_jobs(&self, limit: usize) -> Result<Vec<RewriteJob>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, regenerate FROM retirements
             WHERE undone_at IS NULL AND regenerate != '[]' ORDER BY created_at",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut jobs = Vec::new();
        for (rid, regenerate) in rows {
            for summary in serde_json::from_str::<Vec<String>>(&regenerate).unwrap_or_default() {
                if jobs.len() >= limit {
                    return Ok(jobs);
                }
                type Summary = (String, String, Option<String>, Option<String>, String);
                let row: Option<Summary> = conn
                    .query_row(
                        "SELECT kind, source_ref, superseded_by, event_time, ingest_time
                         FROM episodes /* all-episodes: the summary being replaced */
                         WHERE id = ?1 AND source = 'consolidation'",
                        [&summary],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                    )
                    .optional()?;
                let Some((kind, source_ref, superseded, event_time, ingest_time)) = row else { continue };
                if !SUMMARY_KINDS.contains(&kind.as_str())
                    || superseded.as_deref() != Some(format!("{RETIRED_PREFIX}{rid}").as_str())
                {
                    continue;
                }
                let new_ref = format!("{source_ref}@{rid}");
                let done: bool = conn
                    .query_row(
                        "SELECT 1 FROM episodes /* all-episodes: was it rewritten already? */
                         WHERE source = 'consolidation' AND source_ref = ?1
                         UNION ALL
                         SELECT 1 FROM consolidation_state WHERE key = ?2",
                        params![new_ref, format!("rewrite.skip.{new_ref}")],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !done {
                    jobs.push(RewriteJob {
                        retirement_id: rid.clone(),
                        summary_id: summary,
                        kind,
                        source_ref,
                        new_ref,
                        event_time,
                        ingest_time,
                    });
                }
            }
        }
        Ok(jobs)
    }

    /// A rewrite that came out empty (nothing left to summarise) is done
    /// and never retried — the queue must not spend on it every cycle.
    pub fn rewrite_skipped(&self, new_ref: &str) -> Result<()> {
        self.set_consolidation_state(&format!("rewrite.skip.{new_ref}"), &now_utc())
    }

    /// The live, non-private episodes a summary recorded reading (None
    /// when it recorded nothing — written before v59).
    pub fn recorded_inputs(&self, summary_id: &str) -> Result<Option<Vec<EpisodeRow>>> {
        let conn = self.lock();
        let any: i64 = conn.query_row(
            "SELECT COUNT(*) FROM summary_inputs WHERE summary_id = ?1",
            [summary_id],
            |r| r.get(0),
        )?;
        if any == 0 {
            return Ok(None);
        }
        let columns = EPISODE_COLUMNS
            .split(", ")
            .map(|c| format!("e.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {columns} FROM summary_inputs s JOIN episodes e ON e.id = s.episode_id
             WHERE s.summary_id = ?1 AND e.superseded_by IS NULL AND e.scope != 'private'
             ORDER BY COALESCE(e.event_time, e.ingest_time) DESC"
        ))?;
        let rows = stmt
            .query_map([summary_id], episode_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some(rows))
    }

    /// What a daily digest written at `at` would read now: the newest
    /// `limit` live, non-private episodes (by event time) that had
    /// arrived by then — `recent_episodes` as of a moment in the past.
    pub fn digest_inputs_as_of(&self, at: &str, limit: usize) -> Result<Vec<EpisodeRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {EPISODE_COLUMNS} FROM episodes
             WHERE scope != 'private' AND superseded_by IS NULL AND ingest_time <= ?1
             ORDER BY COALESCE(event_time, ingest_time) DESC LIMIT ?2"
        ))?;
        let rows = stmt
            .query_map(params![at, limit as i64], episode_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Retirement events and summary inputs newer than the watermark, for
    /// the peer leg (paged by the local `synced_at` relay stamp).
    pub fn retirements_sync_export(&self, since: &str) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RETIREMENT_COLUMNS} FROM retirements WHERE synced_at > ?1 ORDER BY synced_at LIMIT 500"
        ))?;
        let retirements: Vec<RetirementRow> = stmt
            .query_map([since], retirement_from_row)?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut stmt = conn.prepare(
            "SELECT summary_id, episode_id, ts FROM summary_inputs
             WHERE synced_at > ?1 ORDER BY synced_at LIMIT 2000",
        )?;
        let inputs: Vec<Value> = stmt
            .query_map([since], |r| {
                Ok(json!([r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?]))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let a = super::page_cursor(&conn, "synced_at", "FROM retirements WHERE synced_at > ?1", since, 500)?;
        let b = super::page_cursor(&conn, "synced_at", "FROM summary_inputs WHERE synced_at > ?1", since, 2000)?;
        let mut full = Vec::new();
        if retirements.len() >= 500 {
            full.push(a.clone());
        }
        if inputs.len() >= 2000 {
            full.push(b.clone());
        }
        let next = full.into_iter().min().unwrap_or_else(|| a.max(b));
        Ok(json!({"retirements": retirements, "summary_inputs": inputs, "next": next}))
    }

    /// Apply a peer's page: retirements last-writer-wins by updated_at,
    /// summary inputs by union. Returns the retirements whose state
    /// changed here — the caller applies or undoes them on this node.
    pub fn retirements_sync_apply(&self, batch: &Value) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut changed = Vec::new();
        for r in batch.get("retirements").and_then(|v| v.as_array()).into_iter().flatten() {
            let (Some(id), Some(kind), Some(target), Some(updated)) = (
                r["id"].as_str(),
                r["kind"].as_str(),
                r["target"].as_str(),
                r["updated_at"].as_str(),
            ) else {
                continue;
            };
            if !id.starts_with("rt_") || !matches!(kind, "memory" | "source") {
                continue;
            }
            let local: Option<String> = tx
                .query_row("SELECT updated_at FROM retirements WHERE id = ?1", [id], |row| row.get(0))
                .optional()?;
            if local.as_deref().is_some_and(|l| l >= updated) {
                continue;
            }
            let regenerate: Vec<String> = r["regenerate"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            tx.execute(
                "INSERT INTO retirements
                     (id, kind, target, label, private, regenerate, reason, actor, created_at, undone_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                     label = excluded.label, private = excluded.private,
                     regenerate = excluded.regenerate, reason = excluded.reason,
                     undone_at = excluded.undone_at, updated_at = excluded.updated_at",
                params![
                    id,
                    kind,
                    target,
                    r["label"].as_str().unwrap_or(""),
                    r["private"].as_bool().unwrap_or(false) as i64,
                    serde_json::to_string(&regenerate)?,
                    r["reason"].as_str().unwrap_or(""),
                    r["actor"].as_str().unwrap_or("peer"),
                    r["created_at"].as_str().unwrap_or(updated),
                    r["undone_at"].as_str(),
                    updated
                ],
            )?;
            changed.push(id.to_string());
        }
        for input in batch.get("summary_inputs").and_then(|v| v.as_array()).into_iter().flatten() {
            let (Some(summary), Some(episode)) = (input[0].as_str(), input[1].as_str()) else { continue };
            tx.execute(
                "INSERT OR IGNORE INTO summary_inputs (summary_id, episode_id, ts) VALUES (?1, ?2, ?3)",
                params![summary, episode, input[2].as_str().unwrap_or("")],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }
}

/// An RFC 3339 stamp moved by whole days (text compare stays valid).
fn shift_days(stamp: &str, days: i64) -> String {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .map(|t| (t.with_timezone(&chrono::Utc) + chrono::Duration::days(days)).format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|_| stamp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EdgeAssertion, EntityRef, EpisodeDraft, IngestBatch};
    use crate::time::{EventTime, TimeUncertainty};
    use crate::Home;

    fn draft(source: &str, source_ref: &str, title: &str, entities: Vec<EntityRef>, edges: Vec<EdgeAssertion>) -> EpisodeDraft {
        EpisodeDraft {
            source: source.into(),
            source_ref: source_ref.into(),
            kind: "email".into(),
            title: title.into(),
            body: format!("{title} body"),
            event_time: EventTime {
                utc: Some("2026-09-10T12:00:00Z".into()),
                tz_offset_minutes: Some(0),
                uncertainty: TimeUncertainty::Minute,
            },
            trust: "imported".into(),
            scope: Some("personal".into()),
            entities,
            edges,
        }
    }

    fn person(name: &str, role: &str) -> EntityRef {
        EntityRef {
            entity_type: "Person".into(),
            name: name.into(),
            role: role.into(),
            attributes: vec![],
        }
    }

    fn ingest(store: &Store, home: &Home, episodes: Vec<EpisodeDraft>) {
        store
            .apply_batch(home, &IngestBatch { actor: "adapter:test".into(), session_id: None, episodes })
            .unwrap();
    }

    /// Every row of the tables a retirement touches, for before/after
    /// comparisons.
    fn snapshot(store: &Store) -> Vec<String> {
        let conn = store.lock();
        let mut out = Vec::new();
        for (table, order) in [
            ("episodes", "id"), ("episode_entities", "episode_id, entity_id, role"),
            ("edge_provenance", "edge_id, episode_id"), ("edges", "id"),
            ("intent_matches", "intent_id, episode_id"), ("location_points", "episode_id, lat"),
            ("entities", "id"), ("self_model", "id"), ("sources", "name"),
        ] {
            let cols = match table {
                "episodes" => "id, superseded_by",
                "edges" => "id, strength, t_expired",
                "entities" => "id, hidden_by",
                "self_model" => "id, status, confirmed_at",
                "sources" => "name, retired_by",
                _ => "*",
            };
            let sql = format!("SELECT {cols} FROM {table} ORDER BY {order}");
            let mut stmt = conn.prepare(&sql).unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    let mut line = format!("{table}:");
                    for i in 0..n {
                        let v: rusqlite::types::Value = r.get(i)?;
                        line.push_str(&format!("{v:?}|"));
                    }
                    Ok(line)
                })
                .unwrap();
            out.extend(rows.map(|r| r.unwrap()));
        }
        out
    }

    fn setup() -> (tempfile::TempDir, Home, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        (tmp, home, store)
    }

    fn id_of(source: &str, source_ref: &str) -> String {
        ids::episode_id(source, source_ref)
    }

    #[test]
    fn test_retire_and_undo_put_everything_back() {
        let (_tmp, home, store) = setup();
        // A (poisoned) introduces Mallory and links her to Ada; B also cites Ada.
        ingest(&store, &home, vec![
            draft("mbox", "<a@x>", "Wire the money", vec![person("Mallory", "author"), person("Ada", "recipient")],
                  vec![EdgeAssertion { src: 0, relation: "knows".into(), dst: 1 }]),
            draft("mbox", "<b@x>", "Lunch", vec![person("Ada", "author")], vec![]),
        ]);
        let a = id_of("mbox", "<a@x>");
        let b = id_of("mbox", "<b@x>");
        {
            let conn = store.lock();
            conn.execute("INSERT INTO location_points (episode_id, ts, day, lat, lon) VALUES (?1, NULL, '2026-09-10', 1.0, 2.0)", [&a]).unwrap();
            conn.execute("INSERT INTO intents (id, name, query, created_at) VALUES ('in_1', 'money', 'money', '2026-09-01T00:00:00Z')", []).unwrap();
            conn.execute("INSERT INTO intent_matches (intent_id, episode_id, found_at) VALUES ('in_1', ?1, '2026-09-11T00:00:00Z')", [&a]).unwrap();
            conn.execute(
                "INSERT INTO self_model (id, sphere, kind, text, origin, status, evidence, created_at, updated_at, confirmed_at)
                 VALUES ('sm_1', 'work', 'identity', 'I move money for Mallory', 'inferred', 'confirmed', ?1,
                         '2026-09-11T00:00:00Z', '2026-09-11T00:00:00Z', '2026-09-11T00:00:00Z')",
                [serde_json::to_string(&vec![a.clone()]).unwrap()],
            ).unwrap();
        }
        store.sync_sources().unwrap();
        let before = snapshot(&store);

        let preview = store.retirement_preview("memory", &a).unwrap();
        assert_eq!(preview["episodes"], 1);
        assert_eq!(preview["counts"]["entity_links"], 2);
        assert_eq!(preview["counts"]["edges_retracted"], 1);
        assert_eq!(preview["counts"]["map_points"], 1);
        assert_eq!(preview["counts"]["entities_hidden"], 1);
        assert_eq!(preview["entities_hidden"][0]["name"], "Mallory");
        assert_eq!(preview["beliefs"][0]["always"], true);

        let actor = Actor::new("human:test");
        let r = store.retire(&actor, "memory", &a, &[], "phishing").unwrap();
        let counts = store.retirement_apply(&r.id).unwrap();
        assert_eq!(counts.episodes, 1);
        assert_eq!(counts.links, 2);
        assert_eq!(counts.edges_retracted, 1);
        assert_eq!(counts.map_points, 1);
        assert_eq!(counts.matches, 1);
        assert_eq!(counts.entities_hidden, 1);
        assert_eq!(counts.beliefs, 1);
        {
            let conn = store.lock();
            let sup: String = conn.query_row("SELECT superseded_by FROM episodes WHERE id = ?1", [&a], |r| r.get(0)).unwrap();
            assert_eq!(sup, format!("retired:{}", r.id));
            let live: Option<String> = conn.query_row("SELECT superseded_by FROM episodes WHERE id = ?1", [&b], |r| r.get(0)).unwrap();
            assert!(live.is_none());
            let mallory: Option<String> = conn.query_row("SELECT hidden_by FROM entities WHERE name = 'Mallory'", [], |r| r.get(0)).unwrap();
            assert_eq!(mallory.as_deref(), Some(r.id.as_str()));
            let ada: Option<String> = conn.query_row("SELECT hidden_by FROM entities WHERE name = 'Ada'", [], |r| r.get(0)).unwrap();
            assert!(ada.is_none(), "Ada still has live evidence");
            let status: String = conn.query_row("SELECT status FROM self_model WHERE id = 'sm_1'", [], |r| r.get(0)).unwrap();
            assert_eq!(status, "proposed");
        }
        // Tend can say why the belief is asking again.
        assert_eq!(
            store.beliefs_sent_back().unwrap().get("sm_1").map(String::as_str),
            Some("Wire the money")
        );
        // Applying again changes nothing.
        assert_eq!(store.retirement_apply(&r.id).unwrap(), RetireCounts::default());

        store.retirement_undo(&actor, &r.id).unwrap();
        assert_eq!(snapshot(&store), before);
        assert!(store.beliefs_sent_back().unwrap().is_empty());
        assert_eq!(store.retirements(10).unwrap()[0]["state"], "undone");
    }

    #[test]
    fn test_a_retired_source_keeps_its_late_arrivals_retired() {
        let (_tmp, home, store) = setup();
        ingest(&store, &home, vec![draft("facebook", "p1", "post one", vec![person("Eve", "author")], vec![])]);
        let actor = Actor::new("human:test");
        let r = store.retire(&actor, "source", "facebook", &[], "").unwrap();
        store.retirement_apply(&r.id).unwrap();
        // a later import of the same source lands retired, its new entity hidden
        ingest(&store, &home, vec![draft("facebook", "p2", "post two", vec![person("Trent", "author")], vec![])]);
        let conn = store.lock();
        let sup: String = conn
            .query_row("SELECT superseded_by FROM episodes WHERE id = ?1", [id_of("facebook", "p2")], |r| r.get(0))
            .unwrap();
        assert_eq!(sup, format!("retired:{}", r.id));
        let links: i64 = conn
            .query_row("SELECT COUNT(*) FROM episode_entities WHERE episode_id = ?1", [id_of("facebook", "p2")], |r| r.get(0))
            .unwrap();
        assert_eq!(links, 0);
        let trent: Option<String> = conn.query_row("SELECT hidden_by FROM entities WHERE name = 'Trent'", [], |r| r.get(0)).unwrap();
        assert_eq!(trent.as_deref(), Some(r.id.as_str()));
    }

    #[test]
    fn test_a_fuller_revision_of_a_retired_memory_stays_retired() {
        let (_tmp, home, store) = setup();
        let mut day = draft("photos", "photos:2026-09-03", "12 photos", vec![], vec![]);
        day.kind = "photos".into();
        ingest(&store, &home, vec![day.clone()]);
        let first = id_of("photos", "photos:2026-09-03");
        let actor = Actor::new("human:test");
        let r = store.retire(&actor, "memory", &first, &[], "").unwrap();
        store.retirement_apply(&r.id).unwrap();
        day.source_ref = "photos:2026-09-03#r2".into();
        day.title = "14 photos".into();
        ingest(&store, &home, vec![day]);
        let conn = store.lock();
        let state = |id: &str| -> Option<String> {
            conn.query_row("SELECT superseded_by FROM episodes WHERE id = ?1", [id], |r| r.get(0)).unwrap()
        };
        assert_eq!(state(&first), Some(format!("retired:{}", r.id)), "the retired copy keeps its mark");
        assert_eq!(state(&id_of("photos", "photos:2026-09-03#r2")), Some(format!("retired:{}", r.id)));
    }

    #[test]
    fn test_cascades_and_kept_opportunities_retire_with_their_seed() {
        let (_tmp, home, store) = setup();
        ingest(&store, &home, vec![draft("mbox", "<s@x>", "seed", vec![], vec![])]);
        let seed = id_of("mbox", "<s@x>");
        let mut cascade = draft("consolidation", &format!("cascade:in_1:{seed}"), "brief", vec![], vec![]);
        cascade.kind = "cascade".into();
        ingest(&store, &home, vec![cascade]);
        let actor = Actor::new("human:test");
        let r = store.retire(&actor, "memory", &seed, &[], "").unwrap();
        let counts = store.retirement_apply(&r.id).unwrap();
        assert_eq!(counts.episodes, 2);
        // a second cascade of the same seed arriving later lands retired too
        let mut late = draft("consolidation", &format!("cascade:in_2:{seed}"), "late brief", vec![], vec![]);
        late.kind = "cascade".into();
        ingest(&store, &home, vec![late]);
        let conn = store.lock();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM episodes WHERE superseded_by = ?1", [format!("retired:{}", r.id)], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn test_a_retirement_travels_to_the_peer_and_so_does_its_undo() {
        let (_tmp, home, a) = setup();
        let b = Store::open_in_memory().unwrap();
        let episodes = vec![draft("mbox", "<p@x>", "Wire the money", vec![person("Mallory", "author")], vec![])];
        ingest(&a, &home, episodes.clone());
        ingest(&b, &home, episodes);
        let target = id_of("mbox", "<p@x>");
        let actor = Actor::new("human:test");
        let r = a.retire(&actor, "memory", &target, &[], "").unwrap();
        a.retirement_apply(&r.id).unwrap();

        let changed = b.retirements_sync_apply(&a.retirements_sync_export("").unwrap()).unwrap();
        assert_eq!(changed, vec![r.id.clone()]);
        assert_eq!(b.retirements_pending_here().unwrap(), vec![(r.id.clone(), false)]);
        b.retirement_apply(&r.id).unwrap();
        let retired = |s: &Store| -> Option<String> {
            s.lock().query_row("SELECT superseded_by FROM episodes WHERE id = ?1", [&target], |r| r.get(0)).unwrap()
        };
        assert_eq!(retired(&b), Some(format!("retired:{}", r.id)));
        // nothing new to apply: the same page again changes nothing
        assert!(b.retirements_sync_apply(&a.retirements_sync_export("").unwrap()).unwrap().is_empty());

        // stamps are compared as text; make sure the undo is strictly newer
        std::thread::sleep(std::time::Duration::from_millis(1100));
        a.retirement_undo(&actor, &r.id).unwrap();
        let changed = b.retirements_sync_apply(&a.retirements_sync_export("").unwrap()).unwrap();
        assert_eq!(changed, vec![r.id.clone()]);
        assert_eq!(b.retirements_pending_here().unwrap(), vec![(r.id.clone(), true)]);
        b.retirement_undo_local(&r.id).unwrap();
        assert_eq!(retired(&b), None);
        assert!(b.retirements_pending_here().unwrap().is_empty());
    }

    #[test]
    fn test_summaries_are_found_rewritten_and_put_back() {
        let (_tmp, home, store) = setup();
        ingest(&store, &home, vec![
            draft("mbox", "<m@x>", "Wire the money", vec![], vec![]),
            draft("mbox", "<n@x>", "Lunch", vec![], vec![]),
        ]);
        let memory = id_of("mbox", "<m@x>");
        let other = id_of("mbox", "<n@x>");
        let mut chapter = draft("consolidation", "chapter:2026-09", "A month of money",
            vec![], vec![]);
        chapter.kind = "chapter".into();
        chapter.body = "chapter heading for 2026-09: A month of money\n\nwritten from:\n- Wire the money\n- Lunch".into();
        let mut digest = draft("consolidation", "summary:2026-09-11", "Consolidation digest 2026-09-11", vec![], vec![]);
        digest.kind = "summary".into();
        ingest(&store, &home, vec![chapter, digest]);
        let chapter_id = id_of("consolidation", "chapter:2026-09");
        let digest_id = id_of("consolidation", "summary:2026-09-11");
        store.record_summary_inputs(&digest_id, &[memory.clone(), other.clone()]).unwrap();

        let preview = store.retirement_preview("memory", &memory).unwrap();
        let found: Vec<(&str, bool)> = preview["summaries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["kind"].as_str().unwrap(), s["exact"].as_bool().unwrap()))
            .collect();
        assert!(found.contains(&("chapter", true)), "{found:?}");
        assert!(found.contains(&("summary", true)), "{found:?}");

        let actor = Actor::new("human:test");
        let r = store.retire(&actor, "memory", &memory, std::slice::from_ref(&digest_id), "").unwrap();
        store.retirement_apply(&r.id).unwrap();
        let jobs = store.rewrite_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1, "only the ticked summary is rewritten");
        assert_eq!(jobs[0].summary_id, digest_id);
        assert_eq!(jobs[0].new_ref, format!("summary:2026-09-11@{}", r.id));
        // its recorded inputs, minus what was retired
        let inputs = store.recorded_inputs(&digest_id).unwrap().unwrap();
        assert_eq!(inputs.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec![other.as_str()]);

        // the rewrite lands; nothing is left to do
        let mut rewrite = draft("consolidation", &jobs[0].new_ref, "Consolidation digest 2026-09-11", vec![], vec![]);
        rewrite.kind = "summary".into();
        ingest(&store, &home, vec![rewrite]);
        assert!(store.rewrite_jobs(10).unwrap().is_empty());

        // undo: the original is back, the rewrite steps behind it
        store.retirement_undo(&actor, &r.id).unwrap();
        let conn = store.lock();
        let state = |id: &str| -> Option<String> {
            conn.query_row("SELECT superseded_by FROM episodes WHERE id = ?1", [id], |r| r.get(0)).unwrap()
        };
        assert_eq!(state(&digest_id), None);
        assert_eq!(state(&memory), None);
        assert_eq!(state(&id_of("consolidation", &jobs[0].new_ref)), Some(digest_id.clone()));
        assert_eq!(state(&chapter_id), None);
    }

    /// A chapter that recorded what it read is matched by that record
    /// alone — a title its body lists without having read it (a calendar
    /// title carrying "\n- <another title>") ticks nothing. A chapter from
    /// before the record still matches by its list, read from the real
    /// separator on (a heading may say "written from:" too).
    #[test]
    fn test_chapter_provenance_prefers_recorded_inputs() {
        let (_tmp, home, store) = setup();
        ingest(&store, &home, vec![
            draft("mbox", "<m@x>", "Wire the money", vec![], vec![]),
            draft("ics", "lunch-1", "Lunch", vec![], vec![]),
            draft("mbox", "<p@x>", "Payroll", vec![], vec![]),
        ]);
        let (memory, lunch, payroll) = (id_of("mbox", "<m@x>"), id_of("ics", "lunch-1"), id_of("mbox", "<p@x>"));
        let mut recorded = draft("consolidation", "chapter:2026-09", "Lunches", vec![], vec![]);
        recorded.kind = "chapter".into();
        recorded.body = "chapter heading for 2026-09: Lunches\n\nwritten from:\n- Lunch\n- Wire the money".into();
        let mut legacy = draft("consolidation", "chapter:2026-09@rt_old", "Pay day", vec![], vec![]);
        legacy.kind = "chapter".into();
        legacy.body = "chapter heading for 2026-09: Pay day written from: - Wire the money\n\nwritten from:\n- Payroll".into();
        ingest(&store, &home, vec![recorded, legacy]);
        let (recorded, legacy) = (id_of("consolidation", "chapter:2026-09"), id_of("consolidation", "chapter:2026-09@rt_old"));
        store.record_summary_inputs(&recorded, std::slice::from_ref(&lunch)).unwrap();

        let reading = |target: &str| -> Vec<(String, String)> {
            let preview = store.retirement_preview("memory", target).unwrap();
            let mut found: Vec<(String, String)> = preview["summaries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| (s["id"].as_str().unwrap().to_string(), s["why"].as_str().unwrap().to_string()))
                .collect();
            found.sort();
            found
        };
        assert!(reading(&memory).is_empty(), "the forged entries tick no chapter");
        assert_eq!(reading(&lunch), vec![(recorded.clone(), "it read this".to_string())]);
        assert_eq!(reading(&payroll), vec![(legacy.clone(), "its heading was written from “Payroll”".to_string())]);
    }

    /// THE RETIREMENT CONTRACT (D.12): every SQL statement that READS the
    /// record — episodes and what hangs off them (entity links, edge
    /// provenance, chunks, full-text rows, intent matches, semantic pairs,
    /// opportunities) — must honour supersession (`superseded_by` appears
    /// in it) or say why it reads every episode: `/* all-episodes: why */`
    /// inside the statement, or a `// all-episodes: why` line just above
    /// it (prompt text). Writes are exempt. A reader that does neither
    /// fails the build — a retired memory would come back through it.
    #[test]
    fn test_every_reader_honours_supersession() {
        const TABLES: &[&str] = &[
            "episodes", "episode_entities", "edge_provenance", "chunks", "episode_fts",
            "intent_matches", "semantic_pairs", "opportunities",
        ];
        fn reads_record(literal: &str) -> bool {
            let words: Vec<&str> = literal.split_whitespace().collect();
            words.windows(2).any(|w| {
                (w[0].eq_ignore_ascii_case("from") || w[0].eq_ignore_ascii_case("join")) && {
                    let t = w[1].trim_start_matches("main.");
                    let t = t.trim_end_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
                    TABLES.contains(&t)
                }
            })
        }
        fn is_write(literal: &str) -> bool {
            let head = literal.trim_start().to_ascii_uppercase();
            ["UPDATE", "DELETE", "CREATE", "ALTER", "DROP"].iter().any(|w| head.starts_with(w))
                || (head.starts_with("INSERT") && !head.contains("SELECT"))
        }
        /// (line, text) of every "…" and r#"…"# literal outside comments.
        fn literals(src: &str) -> Vec<(usize, String)> {
            let bytes = src.as_bytes();
            let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
            let (mut i, mut out) = (0, Vec::new());
            while i < bytes.len() {
                if bytes[i] == b'r' && (i == 0 || !ident(bytes[i - 1])) {
                    let mut j = i + 1;
                    while bytes.get(j) == Some(&b'#') {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'"') {
                        let mut closing = vec![b'"'];
                        closing.extend(std::iter::repeat_n(b'#', j - i - 1));
                        let start = j + 1;
                        let mut k = start;
                        while k + closing.len() <= bytes.len() && bytes[k..k + closing.len()] != closing[..] {
                            k += 1;
                        }
                        let line = src[..i].matches('\n').count() + 1;
                        out.push((line, src[start..k.min(bytes.len())].to_string()));
                        i = k + closing.len();
                        continue;
                    }
                }
                match bytes[i] {
                    b'/' if bytes.get(i + 1) == Some(&b'/') => {
                        while i < bytes.len() && bytes[i] != b'\n' {
                            i += 1;
                        }
                    }
                    b'\'' => {
                        // a char literal ('"', '\'') — or a lifetime, which has no closing quote
                        if bytes.get(i + 1) == Some(&b'\\') {
                            i += 4;
                        } else if bytes.get(i + 2) == Some(&b'\'') {
                            i += 3;
                        } else {
                            i += 1;
                        }
                    }
                    b'"' => {
                        let start = i + 1;
                        let mut j = start;
                        while j < bytes.len() && bytes[j] != b'"' {
                            j += if bytes[j] == b'\\' { 2 } else { 1 };
                        }
                        let line = src[..i].matches('\n').count() + 1;
                        out.push((line, src[start..j.min(bytes.len())].to_string()));
                        i = j + 1;
                    }
                    _ => i += 1,
                }
            }
            out
        }
        let mut offenders = Vec::new();
        let roots = [
            concat!(env!("CARGO_MANIFEST_DIR"), "/src"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/../hearthloom-daemon/src"),
        ];
        let mut stack: Vec<std::path::PathBuf> = roots.iter().map(std::path::PathBuf::from).collect();
        let mut files = 0;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                files += 1;
                let src = std::fs::read_to_string(&path).unwrap();
                let body = src.split("#[cfg(test)]").next().unwrap_or(&src);
                let lines: Vec<&str> = body.lines().collect();
                for (line, literal) in literals(body) {
                    if !reads_record(&literal) || is_write(&literal) {
                        continue;
                    }
                    if literal.contains("superseded_by") || literal.contains("all-episodes") {
                        continue;
                    }
                    let above = lines[line.saturating_sub(3)..line.saturating_sub(1)].join("\n");
                    if above.contains("// all-episodes:") {
                        continue;
                    }
                    let name = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(&path).display().to_string();
                    let snippet: String = literal.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(110).collect();
                    offenders.push(format!("{name}:{line}: {snippet}"));
                }
            }
        }
        assert!(files > 50, "the scan found only {files} source files");
        assert!(
            offenders.is_empty(),
            "{} reader(s) of the record ignore supersession — add `AND <alias>.superseded_by IS NULL`, \
             or `/* all-episodes: <why> */` if it truly must see retired and replaced memories:\n{}",
            offenders.len(),
            offenders.join("\n")
        );
    }

    #[test]
    fn test_refs() {
        assert_eq!(base_ref("chapter:2026-09@rt_1"), "chapter:2026-09");
        assert_eq!(base_ref("summary:2026-09-11"), "summary:2026-09-11");
        assert_eq!(cascade_seed("cascade:in_1:ep_abc"), Some("ep_abc"));
        assert_eq!(cascade_seed("cascade:in_1:ep_abc#r2"), Some("ep_abc"));
        assert_eq!(cascade_seed("cascade:in_1:ep_abc@rt_9"), Some("ep_abc"));
        assert_eq!(cascade_seed("chapter:2026-09"), None);
    }
}
