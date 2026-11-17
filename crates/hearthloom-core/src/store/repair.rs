//! OWNER REPAIRS (v69): a memory an earlier version stored wrongly — mail
//! whose subject or body was never decoded, a calendar event that took in
//! its alarm's text — is replaced, when the owner asks, by a corrected copy
//! that takes its place in the record.
//!
//! Episodes are append-only and a ref keeps the text it first landed with,
//! so the correction lands as a new episode under `<base>#repair:<hash>`
//! (the hash of its title and text) and the base is marked superseded by it
//! — THE SUPERSESSION PATTERN, decided by who wrote the replacement, never
//! by the shape of its ref: Message-IDs and calendar UIDs are chosen by
//! senders, so a ref ending in `#repair:…` is only a name. A replacement is
//! an episode whose recorded writer (`episode_origin.writer`) is
//! [`REPAIR_WRITER`]. Only [`Store::repair_apply`] writes as it (`apply_batch`
//! refuses that actor from every other caller), and a peer's episode counts
//! as one only when its sender's origin says so — origins only a peer key
//! may send. An unknown writer is no repair.
//!
//! A repaired record is its base and its replacements. Each node settles it
//! from what it holds ([`settle`]): the newest replacement whose run wasn't
//! undone stands and the other copies stand behind it; with none, the base
//! stands and the replacements behind it. Arrival order, a second repair and
//! an undo all end in the same state on every node. The copies are one
//! record, as a growing source's are: a copy that arrives takes the
//! record's label (private above all, stamped), a private label on one
//! reaches the others (`growing::spread_private`), retiring one retires them
//! all, and a copy of a retired record arrives retired — or, held from
//! before this rule, is retired when the run reaches the node.
//!
//! A run (`repairs`: synced on /v1/sync/repairs, last writer wins by
//! `updated_at`) carries the copies it made (`repair_copies`, v71): every
//! copy the owner repair lands is recorded under the run its origin names,
//! and every node unions each run's set. Two nodes can make the same copy
//! (its id is its content's) under runs of their own before they sync, and
//! a copy's origin names only the run that reached the node first; a copy
//! counts as undone when any run that made it is. Undoing a run stamps the
//! row; each node then settles those records again
//! ([`Store::repair_settle_run`], caught up every cycle like retirements).
//! What a run added that was missing (a calendar occurrence) is an ordinary
//! import and stays.
//!
//! Sources with a revision rule of their own get no `#repair:` copy: a run
//! re-mints the record under that rule instead ([`RepairWork::revisions`]),
//! written as the owner repair all the same, so the run finds its copies
//! and its undo reaches them. A growing source's day or session stored
//! under an unversioned base lands again as `<base>#r<n>` from its stored
//! text ([`Store::repair_copies_scan`]) — and since two nodes can hold
//! different texts under one such base, each node re-mints its own when a
//! `copies` run reaches it, and the growing rule keeps the fullest copy in
//! view everywhere. A photo day lands as its next `photos#<day>#r<n>`, its
//! minted ledger set with it ([`PhotoLedger`]; the ledger before is kept
//! for the undo, and put back at the run's revision: the run's copy keeps
//! its ref). Undoing such a run puts every copy it made behind the copy
//! that stood before ([`settle_revisions`]), and a copy of an undone run
//! that arrives later lands behind too (`ingest_apply`) — behind the base's
//! id when no other copy of the record is here yet. A growing source's copy
//! of an undone run keeps its `<base>#r<n>` ref, so a local read holding as
//! much puts it back in view instead (`growing::revive_undone_copy`); that
//! read is recorded (`repair_revived`, v72) and travels with the run, so
//! every node puts the copy back the same way. A photo day's correction
//! arriving from a peer stands only over the copies of the day it holds
//! there (`photo::unheld_live_copy`, checked again when the run settles).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use super::growing::{growing_copy, growing_items, growing_revision_sql, GROWING_SOURCES};
use super::retire::RETIRED_PREFIX;
use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::ingest::{EpisodeDraft, IngestBatch};
use crate::paths::Home;
use crate::time::{now_utc, EventTime, TimeUncertainty};

/// The writer of every replacement — the owner's repair, through
/// `Store::repair_apply` only.
pub const REPAIR_WRITER: &str = "repair:owner";

/// What a replacement's ref adds to its base's.
pub const REPAIR_MARK: &str = "#repair:";

/// Sources whose memories a repair may replace: their refs come from
/// outside (Message-IDs, calendar UIDs, ChatGPT conversation ids) or name
/// a record once (an Apple Health day or workout), and no revision rule of
/// their own applies to them.
pub const REPAIRABLE_SOURCES: &[&str] =
    &["mbox", "ics", "openai", "chatlog-chatgpt", "apple-health"];

/// The run kind that re-mints growing sources' unversioned copies: every
/// node it reaches re-mints its own (see the module notes).
pub const COPIES_KIND: &str = "copies";

/// The run kind that re-mints photo days.
pub const PHOTO_DAYS_KIND: &str = "photodays";

/// The base and revision number of a copy a run may re-mint under its
/// source's own rule: a growing source's `<base>#r<n>` (n > 0), or a photo
/// day's `photos#<day>#r<n>` (n ≥ 2, the base being revision 1).
pub(crate) fn revision_of<'a>(source: &str, source_ref: &'a str) -> Option<(&'a str, u64)> {
    if GROWING_SOURCES.contains(&source) {
        return growing_copy(source, source_ref).filter(|(_, n)| *n > 0);
    }
    if source != "photos" {
        return None;
    }
    let (base, n) = source_ref.rsplit_once("#r")?;
    let day = base.strip_prefix("photos#")?;
    chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = n.parse().ok()?;
    (n >= 2).then_some((base, n))
}

/// A photo day's minted ledger as a run sets it (`photo_day`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhotoLedger {
    pub day: String,
    pub count: i64,
    pub keys_hash: String,
    pub revision: i64,
}

/// What one run lands.
#[derive(Debug, Clone, Copy, Default)]
pub struct RepairWork<'a> {
    /// Corrected copies, each replacing its base (`#repair:` refs).
    pub items: &'a [RepairItem],
    /// Copies re-minted under their source's own revision rule (see
    /// [`revision_of`]); they land as the owner repair.
    pub revisions: &'a [EpisodeDraft],
    /// Missing episodes, imported as `addition_actor`.
    pub additions: &'a [EpisodeDraft],
    pub addition_actor: &'a str,
    /// Photo-day ledgers to set; the ledger before is kept for the undo.
    pub photo_ledgers: &'a [PhotoLedger],
}

/// Hex digits of the content hash a replacement's ref carries.
const HASH_LEN: usize = 16;

/// Records settled per lock hold when a run is settled again.
const SETTLE_BATCH: usize = 200;

/// Rows per lock hold when listing a source's records.
const LIST_PAGE: i64 = 5000;

/// The ref a corrected copy of `base` lands under: the base's ref and the
/// hash of the corrected title and text, so the same correction made on two
/// nodes is the same episode.
pub fn replacement_ref(base: &str, title: &str, body: &str) -> String {
    let hash = ids::content_hash(format!("{title}\n{body}").as_bytes());
    format!("{base}{REPAIR_MARK}{}", &hash[..HASH_LEN])
}

/// The base a ref of a replacement's shape names. The shape alone proves
/// nothing (anyone can choose such a ref): see [`repair_of`].
pub fn replacement_base(source_ref: &str) -> Option<&str> {
    let (base, hash) = source_ref.rsplit_once(REPAIR_MARK)?;
    (!base.is_empty()
        && hash.len() == HASH_LEN
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(base)
}

/// The base an episode replaces: only when its source is repairable, its
/// writer is the owner repair, and its ref names a base. Any other writer —
/// an importer, another key, a sender, an unknown one — replaces nothing.
pub(crate) fn repair_of<'a>(
    source: &str,
    source_ref: &'a str,
    writer: Option<&str>,
) -> Option<&'a str> {
    if !REPAIRABLE_SOURCES.contains(&source) || writer != Some(REPAIR_WRITER) {
        return None;
    }
    replacement_base(source_ref)
}

/// Does a batch actor claim the repair's writer (or its family)?
pub(crate) fn is_repair_actor(actor: &str) -> bool {
    actor.starts_with("repair:")
}

/// One stored copy of a repaired record.
#[derive(Debug, Clone)]
struct Copy {
    id: String,
    source_ref: String,
    title: String,
    content_ref: Option<String>,
    superseded_by: Option<String>,
    scope: String,
    scope_changed_at: Option<String>,
    trust: String,
    event_time: Option<String>,
    /// Who wrote it: its recorded origin's writer, else (stored before
    /// origins were recorded) the batch actor, unless that was a peer.
    writer: Option<String>,
    /// A replacement: when it was written (its origin's `at`) and the run
    /// that made it. None for the base.
    replacement: Option<(String, Option<String>)>,
}

const COPY_COLUMNS: &str = "e.id, e.source_ref, e.title, e.content_ref, e.superseded_by, e.scope,
     e.scope_changed_at, e.trust, o.writer, o.session, o.at, e.event_time, e.actor";

type CopyRow = (Copy, Option<String>, Option<String>, Option<String>);

fn copy_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<CopyRow> {
    let writer: Option<String> = r.get(8)?;
    let actor: Option<String> = r.get(12)?;
    Ok((
        Copy {
            id: r.get(0)?,
            source_ref: r.get(1)?,
            title: r.get(2)?,
            content_ref: r.get(3)?,
            superseded_by: r.get(4)?,
            scope: r.get(5)?,
            scope_changed_at: r.get(6)?,
            trust: r.get(7)?,
            event_time: r.get(11)?,
            writer: writer
                .clone()
                .or_else(|| actor.filter(|a| !a.starts_with("peer:"))),
            replacement: None,
        },
        writer,
        r.get(9)?,
        r.get(10)?,
    ))
}

/// The stored copies of the record whose base ref is `base`: the base
/// itself (if stored) and every replacement of it the owner repair wrote.
/// A row whose ref only looks like a replacement is left out.
fn record(conn: &Connection, source: &str, base: &str) -> Result<Vec<Copy>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COPY_COLUMNS}
         FROM episodes e /* all-episodes: every copy of a repaired record, replaced and retired ones too */
         LEFT JOIN episode_origin o ON o.episode_id = e.id
         WHERE e.source = ?1 AND (e.source_ref = ?2
            OR (e.source_ref >= ?2 || '{REPAIR_MARK}' AND e.source_ref < ?2 || '#repair;'))"
    ))?;
    let rows = stmt.query_map(params![source, base], copy_row)?;
    let mut out = Vec::new();
    for row in rows {
        let (mut copy, writer, session, at) = row?;
        if copy.source_ref == base {
            out.push(copy);
        } else if repair_of(source, &copy.source_ref, writer.as_deref()) == Some(base) {
            copy.replacement = Some((at.unwrap_or_default(), session));
            out.push(copy);
        }
    }
    Ok(out)
}

/// Record that the run `run` made the copy `episode_id` (`repair_copies`,
/// v71). Every copy the owner repair writes lands through `apply_batch_as`,
/// which records it under the run its origin names; the runs' rows carry
/// the set to every node, which unions it.
pub(crate) fn record_copy(conn: &Connection, run: &str, episode_id: &str) -> Result<bool> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO repair_copies (run, episode_id) VALUES (?1, ?2)",
        params![run, episode_id],
    )? > 0)
}

/// Did a run that was undone make the copy `episode_id` — on this node or
/// on another that made the same copy under a run of its own?
fn made_by_undone_run(conn: &Connection, episode_id: &str) -> Result<bool> {
    let mut stmt = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM repair_copies c JOIN repairs r ON r.id = c.run
         WHERE c.episode_id = ?1 AND r.undone_at IS NOT NULL)",
    )?;
    Ok(stmt.query_row([episode_id], |r| r.get(0))?)
}

/// Is a copy the owner repair wrote, arriving with the run `session` in its
/// origin, one an undone run made? `ingest_apply` lands it behind. A copy a
/// later read of the owner's files put back in view ([`revived`]) is that
/// read's, not the undone run's, on every node.
pub(crate) fn undone_arrival(
    conn: &Connection,
    episode_id: &str,
    session: Option<&str>,
) -> Result<bool> {
    Ok(
        (undone_run(conn, session)? || made_by_undone_run(conn, episode_id)?)
            && !revived(conn, episode_id)?,
    )
}

/// Did a read of the owner's files, on any node, put the copy
/// `episode_id` an undone run re-minted back in view (`repair_revived`,
/// v72; `growing::revive_undone_copy`)?
pub(crate) fn revived(conn: &Connection, episode_id: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM repair_revived WHERE episode_id = ?1)")?;
    Ok(stmt.query_row([episode_id], |r| r.get(0))?)
}

/// Record that a read put the copy `episode_id` back in view, and pass it
/// on: every run that made it is stamped again, so the repairs leg carries
/// it to the other nodes. Returns whether it was new.
pub(crate) fn record_revived(conn: &Connection, episode_id: &str) -> Result<bool> {
    let new = conn.execute(
        "INSERT OR IGNORE INTO repair_revived (episode_id, revived_at) VALUES (?1, ?2)",
        params![episode_id, now_utc()],
    )? > 0;
    if new {
        conn.execute(
            "UPDATE repairs SET updated_at = updated_at
             WHERE id IN (SELECT run FROM repair_copies WHERE episode_id = ?1)",
            [episode_id],
        )?;
    }
    Ok(new)
}

/// Replacements a run that was undone made.
fn undone_copies(conn: &Connection, copies: &[Copy]) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for copy in copies {
        if let Some((_, session)) = &copy.replacement {
            if undone_arrival(conn, &copy.id, session.as_deref())? {
                out.insert(copy.id.clone());
            }
        }
    }
    Ok(out)
}

/// The copy that stands for the record: the newest replacement (by when it
/// was written, then by ref, so every node picks the same) whose run isn't
/// undone, else the base.
fn standing<'a>(copies: &'a [Copy], undone: &HashSet<String>) -> Option<&'a Copy> {
    copies
        .iter()
        .filter(|c| c.replacement.is_some() && !undone.contains(&c.id))
        .max_by(|a, b| {
            (a.replacement.as_ref().map(|r| &r.0), &a.source_ref)
                .cmp(&(b.replacement.as_ref().map(|r| &r.0), &b.source_ref))
        })
        .or_else(|| copies.iter().find(|c| c.replacement.is_none()))
}

/// Bring a repaired record to the state its copies and runs call for: the
/// standing copy in view, the others behind it (an undone replacement
/// behind the base — by the base's id even when the base hasn't reached
/// this node: an undone correction never stands in its place). A retired
/// copy keeps its retirement; what its undo restores is set to the same
/// state. A copy superseded by anything outside the record is left alone.
/// A copy that goes behind loses its map points (the supersession rule); a
/// copy that comes back is indexed for them again. A private copy's label
/// reaches the others (`growing::spread_private`). The links the extraction
/// pass found on the copies behind a standing replacement are carried to
/// it. Idempotent; returns the copies whose state changed.
pub(crate) fn settle(conn: &Connection, source: &str, base: &str) -> Result<usize> {
    let copies = record(conn, source, base)?;
    if !copies.iter().any(|c| c.replacement.is_some()) {
        return Ok(0);
    }
    let undone = undone_copies(conn, &copies)?;
    // None: every copy here is a correction that was undone, the base absent
    let winner = standing(&copies, &undone);
    let base_id = ids::episode_id(source, base);
    let behind_undone = copies
        .iter()
        .find(|c| c.replacement.is_none())
        .or(winner)
        .map_or(base_id.as_str(), |c| c.id.as_str());
    let mut ids: HashSet<&str> = copies.iter().map(|c| c.id.as_str()).collect();
    ids.insert(&base_id);
    let mut changed = 0;
    for copy in &copies {
        let want: Option<&str> = match winner {
            Some(w) if copy.id == w.id => None,
            _ if undone.contains(&copy.id) => Some(behind_undone),
            Some(w) => Some(w.id.as_str()),
            None => continue,
        };
        match copy.superseded_by.as_deref() {
            Some(mark) if mark.starts_with(RETIRED_PREFIX) => {
                super::retire::set_prior_superseded(conn, &copy.id, mark, want)?;
            }
            current if current == want => {}
            current if current.is_none_or(|s| ids.contains(s)) => {
                conn.execute(
                    "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
                    params![want, copy.id],
                )?;
                changed += 1;
                if want.is_some() {
                    conn.execute(
                        "DELETE FROM location_points WHERE episode_id = ?1",
                        [&copy.id],
                    )?;
                } else {
                    conn.execute(
                        "UPDATE episodes SET location_scanned = 0 WHERE id = ?1",
                        [&copy.id],
                    )?;
                }
            }
            _ => {}
        }
    }
    // A private label holds for the record — also for copies a node stored
    // before it knew this rule (it settles them when the run reaches it).
    let private: Vec<String> = copies
        .iter()
        .filter(|c| c.scope == "private")
        .map(|c| c.id.clone())
        .collect();
    if !private.is_empty() {
        super::growing::spread_private(conn, &Actor::new("system:repair"), &private)?;
    }
    let Some(winner) = winner else {
        return Ok(changed);
    };
    let winner_live: bool = conn.query_row(
        "SELECT superseded_by IS NULL FROM episodes WHERE id = ?1",
        [&winner.id],
        |r| r.get(0),
    )?;
    if winner.replacement.is_some() && winner_live {
        for copy in copies.iter().filter(|c| c.id != winner.id) {
            let carried = conn.execute(
                "INSERT OR IGNORE INTO episode_entities (episode_id, entity_id, role)
                 SELECT ?1, entity_id, role FROM episode_entities /* all-episodes: the links a replaced copy's extraction found */
                 WHERE episode_id = ?2 AND role = 'mentioned'",
                params![winner.id, copy.id],
            )?;
            if carried > 0 {
                conn.execute(
                    "UPDATE entities SET hidden_by = NULL
                     WHERE hidden_by IS NOT NULL AND id IN (
                         SELECT entity_id FROM episode_entities /* all-episodes: links just carried to a live copy */
                         WHERE episode_id = ?1 AND role = 'mentioned')",
                    [&winner.id],
                )?;
            }
        }
    }
    Ok(changed)
}

/// A retired record keeps every copy retired: a copy left in view beside a
/// retired one — a node that stored it before it knew this rule settles
/// the record when the run reaches it — is retired under the same
/// retirement, and what it seeded with it. Arrivals don't need this
/// (`apply_batch` retires a copy of a retired record as it lands). Returns
/// the copies retired.
fn retire_with_record(
    conn: &Connection,
    rules: &super::retire::ArrivalRules,
    source: &str,
    base: &str,
) -> Result<usize> {
    let copies = record(conn, source, base)?;
    let Some(rid) = copies
        .iter()
        .filter_map(|c| c.superseded_by.as_deref())
        .find_map(|mark| rules.live_retirement(mark))
    else {
        return Ok(0);
    };
    let mut seeds = HashSet::new();
    for copy in &copies {
        if copy
            .superseded_by
            .as_deref()
            .is_some_and(|s| s.starts_with(RETIRED_PREFIX))
        {
            continue;
        }
        rules.retire(conn, &rid, &copy.id)?;
        seeds.insert(copy.id.clone());
    }
    if !seeds.is_empty() {
        rules.retire_derived(conn, &rid, &seeds)?;
    }
    Ok(seeds.len())
}

/// The record an episode belongs to: its base when it is a replacement,
/// else its own ref. None for an episode of another source, or none at all.
fn record_of(conn: &Connection, episode_id: &str) -> Result<Option<(String, String)>> {
    let row: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT e.source, e.source_ref, o.writer
             FROM episodes e /* all-episodes: one copy of a record */
             LEFT JOIN episode_origin o ON o.episode_id = e.id
             WHERE e.id = ?1",
            [episode_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((source, source_ref, writer)) = row else {
        return Ok(None);
    };
    if !REPAIRABLE_SOURCES.contains(&source.as_str()) {
        return Ok(None);
    }
    let base = repair_of(&source, &source_ref, writer.as_deref())
        .unwrap_or(&source_ref)
        .to_string();
    Ok(Some((source, base)))
}

/// The other stored copies of the repaired record `episode_id` belongs to
/// (empty when it has none); None for an episode of a source no repair
/// reaches. What retiring it retires with it, and what a private label on
/// it reaches (`growing::other_copies`).
pub(crate) fn other_copies(conn: &Connection, episode_id: &str) -> Result<Option<Vec<String>>> {
    let Some((source, base)) = record_of(conn, episode_id)? else {
        return Ok(None);
    };
    Ok(Some(
        record(conn, &source, &base)?
            .into_iter()
            .map(|c| c.id)
            .filter(|id| id != episode_id)
            .collect(),
    ))
}

/// A label the owner took a copy of a repaired record out of the vault
/// with — given with the vault open (`set_episode_scopes`) — holds for the
/// record: its other copies still private under an older (or no) stamp take
/// it, with the same stamp, so the scope leg carries it to every node. The
/// copies are one item (one message, event, conversation or day, read
/// again), and the lookups by Message-ID or UID read every copy: a hidden
/// copy left private would keep the item private there. (A growing
/// source's copies are other reads, which may hold more: they keep their
/// own labels.) Returns the copies relabelled.
pub(crate) fn spread_unvault(
    conn: &Connection,
    actor: &Actor,
    ids: &[String],
    scope: &str,
) -> Result<usize> {
    if scope == "private" {
        return Ok(0);
    }
    let mut label = conn.prepare_cached(
        "SELECT scope, scope_changed_at FROM episodes /* all-episodes: a repaired record's label */
         WHERE id = ?1",
    )?;
    // `scope` stays in the SET list: the relay stamp follows it
    let mut relabel = conn.prepare_cached(
        "UPDATE episodes SET scope = ?2, scope_changed_at = ?3 /* all-episodes: a repaired record's label */
         WHERE id = ?1 AND scope = 'private' AND COALESCE(scope_changed_at, '') <= ?3",
    )?;
    let mut total = 0;
    for id in ids {
        let Some((now, Some(stamp))) = label
            .query_row([id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .optional()?
        else {
            continue;
        };
        if now != scope {
            continue;
        }
        let Some(copies) = other_copies(conn, id)? else {
            continue;
        };
        let mut relabeled = Vec::new();
        for copy in copies {
            if relabel.execute(params![copy, scope, stamp])? > 0 {
                relabeled.push(copy);
            }
        }
        if !relabeled.is_empty() {
            total += relabeled.len();
            record_mutation(
                conn,
                actor,
                "scope_repaired_copies",
                &format!("scope_repaired_copies:{id}:{stamp}"),
                &json!({"from": id, "copies": relabeled, "scope": scope}),
            )?;
        }
    }
    Ok(total)
}

/// What an arriving episode of a repairable source is measured against:
/// the other stored copies of its record, read before it is inserted
/// (`apply_batch`). None when it joins no repaired record.
pub(crate) struct RecordArrival {
    copies: Vec<Copy>,
}

impl RecordArrival {
    /// `base`: the record's base ref (the draft's own ref unless it is a
    /// replacement the owner repair wrote).
    pub(crate) fn read(
        conn: &Connection,
        source: &str,
        base: &str,
        source_ref: &str,
    ) -> Result<Option<RecordArrival>> {
        if !REPAIRABLE_SOURCES.contains(&source) {
            return Ok(None);
        }
        let copies: Vec<Copy> = record(conn, source, base)?
            .into_iter()
            .filter(|c| c.source_ref != source_ref)
            .collect();
        // a base arriving with no replacement stored joins no record
        if copies.is_empty()
            || (base == source_ref && copies.iter().all(|c| c.replacement.is_none()))
        {
            return Ok(None);
        }
        Ok(Some(RecordArrival { copies }))
    }

    /// The latest label given to any copy, if one was.
    fn decided(&self) -> Option<&Copy> {
        self.copies
            .iter()
            .filter(|c| c.scope_changed_at.is_some())
            .max_by(|a, b| a.scope_changed_at.cmp(&b.scope_changed_at))
    }

    /// The record's label: the latest one given to any copy, else the
    /// base's, else any copy's.
    pub(crate) fn inherited_scope(&self) -> Option<&str> {
        self.decided()
            .or_else(|| self.copies.iter().find(|c| c.replacement.is_none()))
            .or_else(|| self.copies.first())
            .map(|c| c.scope.as_str())
    }

    /// The record is private (see `growing::Arriving::private`): carries
    /// when that label was given, if it says.
    pub(crate) fn private(&self) -> Option<Option<&str>> {
        match self.decided() {
            Some(c) => (c.scope == "private").then_some(c.scope_changed_at.as_deref()),
            None => self
                .copies
                .iter()
                .find(|c| c.scope == "private")
                .map(|_| None),
        }
    }

    /// The `retired:<id>` marks of retired copies.
    pub(crate) fn retired_marks(&self) -> impl Iterator<Item = &str> {
        self.copies
            .iter()
            .filter_map(|c| c.superseded_by.as_deref())
            .filter(|s| s.starts_with(RETIRED_PREFIX))
    }
}

/// One repaired record as a repair sees it: the copy in view (or, retired,
/// the one an undo would bring back) and the replacements stored.
#[derive(Debug, Clone)]
pub struct RepairTarget {
    pub source: String,
    pub base_ref: String,
    /// The base's episode id (it may not be stored here, when only a
    /// replacement arrived).
    pub base_id: String,
    pub current_id: String,
    pub title: String,
    content_ref: Option<String>,
    pub scope: String,
    /// The event time (UTC) of the copy in view.
    pub event_time: Option<String>,
    /// Who wrote the original (the base): its recorded origin's writer,
    /// else the batch actor it was stored by, unless that was a peer. None:
    /// unknown, or the base isn't stored here.
    pub base_writer: Option<String>,
    /// The record is retired.
    pub retired: bool,
    /// Replacements stored here, by ref, each with whether its run was
    /// undone.
    pub replacements: Vec<(String, bool)>,
}

impl RepairTarget {
    /// The text of the copy in view, from its raw file.
    pub fn body(&self, home: &Home) -> Option<String> {
        let rel = self.content_ref.as_deref()?;
        std::fs::read_to_string(home.root().join(rel)).ok()
    }

    fn of(
        source: &str,
        base: &str,
        copies: &[Copy],
        undone: &HashSet<String>,
    ) -> Option<RepairTarget> {
        let current = standing(copies, undone)?;
        Some(RepairTarget {
            source: source.to_string(),
            base_ref: base.to_string(),
            base_id: ids::episode_id(source, base),
            current_id: current.id.clone(),
            title: current.title.clone(),
            content_ref: current.content_ref.clone(),
            scope: current.scope.clone(),
            event_time: current.event_time.clone(),
            base_writer: copies
                .iter()
                .find(|c| c.replacement.is_none())
                .and_then(|c| c.writer.clone()),
            retired: current
                .superseded_by
                .as_deref()
                .is_some_and(|s| s.starts_with(RETIRED_PREFIX)),
            replacements: copies
                .iter()
                .filter(|c| c.replacement.is_some())
                .map(|c| (c.source_ref.clone(), undone.contains(&c.id)))
                .collect(),
        })
    }
}

/// One corrected copy to land: the base it replaces and the draft the
/// current reader made of it.
#[derive(Debug, Clone)]
pub struct RepairItem {
    pub base_ref: String,
    pub draft: EpisodeDraft,
}

/// What a repair run did.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RepairApplied {
    /// The run, when anything landed.
    pub run: Option<String>,
    pub replaced: usize,
    pub added: usize,
    /// Copies re-minted under their source's own revision rule.
    pub revised: usize,
    /// Photo-day ledgers set.
    pub ledgers: usize,
    /// Records the caller put into the vault alongside the run (a ChatGPT
    /// conversation flagged "do not remember").
    pub made_private: usize,
    /// Corrected copies, re-mints and additions stored already.
    pub existing: usize,
}

/// One run (the synced row).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RepairRun {
    pub id: String,
    pub kind: String,
    pub replaced: i64,
    pub added: i64,
    pub actor: String,
    pub node: Option<String>,
    pub created_at: String,
    pub undone_at: Option<String>,
    pub updated_at: String,
}

const RUN_COLUMNS: &str =
    "id, kind, replaced, added, actor, node, created_at, undone_at, updated_at";

fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RepairRun> {
    Ok(RepairRun {
        id: r.get(0)?,
        kind: r.get(1)?,
        replaced: r.get(2)?,
        added: r.get(3)?,
        actor: r.get(4)?,
        node: r.get(5)?,
        created_at: r.get(6)?,
        undone_at: r.get(7)?,
        updated_at: r.get(8)?,
    })
}

fn state_key(run: &str) -> String {
    format!("repair.state.{run}")
}

/// `consolidation_state` key: the photo-day ledgers a run set on this
/// node, each with the row before (what its undo puts back).
fn ledgers_key(run: &str) -> String {
    format!("repair.photo_days.{run}")
}

/// One ledger a run set: the row before (None: there was none) and after.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LedgerChange {
    day: String,
    before: Option<LedgerRow>,
    after: PhotoLedger,
}

/// A `photo_day` row as stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct LedgerRow {
    count: i64,
    keys_hash: String,
    episode_id: Option<String>,
    revision: i64,
    minted_at: String,
}

fn ledger_row(conn: &Connection, day: &str) -> Result<Option<LedgerRow>> {
    Ok(conn
        .query_row(
            "SELECT count, keys_hash, episode_id, revision, minted_at FROM photo_day WHERE day = ?1",
            [day],
            |r| {
                Ok(LedgerRow {
                    count: r.get(0)?,
                    keys_hash: r.get(1)?,
                    episode_id: r.get(2)?,
                    revision: r.get(3)?,
                    minted_at: r.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Is the run named `session` one that was undone? A copy it wrote (its
/// recorded writer the owner repair) lands behind the copy in view.
pub(crate) fn undone_run(conn: &Connection, session: Option<&str>) -> Result<bool> {
    let Some(run) = session else {
        return Ok(false);
    };
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM repairs WHERE id = ?1 AND undone_at IS NOT NULL)",
        [run],
        |r| r.get(0),
    )?)
}

/// Was the episode written by the owner repair, and made by a run that was
/// undone (the one its origin names, or another that made it too)?
pub(crate) fn undone_copy(conn: &Connection, episode_id: &str) -> Result<bool> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT o.session FROM episode_origin o
         WHERE o.episode_id = ?1 AND o.writer = '{REPAIR_WRITER}'"
    ))?;
    let Some(session) = stmt
        .query_row([episode_id], |r| r.get::<_, Option<String>>(0))
        .optional()?
    else {
        return Ok(false);
    };
    undone_arrival(conn, episode_id, session.as_deref())
}

/// What a retired episode's undo gives back (`superseded_by` before the
/// retirement `mark`), or None when its undo record doesn't say.
fn prior_of_retired(conn: &Connection, episode_id: &str, mark: &str) -> Result<Option<String>> {
    let Some(rid) = mark.strip_prefix(RETIRED_PREFIX) else {
        return Ok(None);
    };
    let prior: Option<String> = conn
        .query_row(
            "SELECT prior FROM retirement_rows
             WHERE tbl = 'episodes' AND row_key = ?1 AND op = 'updated' AND retirement_id = ?2
             ORDER BY seq LIMIT 1",
            params![episode_id, rid],
            |r| r.get(0),
        )
        .optional()?;
    Ok(prior
        .and_then(|p| serde_json::from_str::<Value>(&p).ok())
        .and_then(|p| p["superseded_by"].as_str().map(String::from))
        .filter(|s| !s.starts_with(RETIRED_PREFIX)))
}

/// One stored copy of a re-minted record.
struct Revised {
    id: String,
    source_ref: String,
    content_ref: Option<String>,
    superseded_by: Option<String>,
    undone: bool,
}

/// Settle a growing source's day or session, or a photo day, that holds
/// copies a run re-minted (`base`: the record's unversioned ref). Nothing
/// changes unless one of them belongs to an undone run: then the copy that
/// stood before them stands again — the fullest of the copies an undone
/// copy took the place of and those in view (a growing source's rank, a
/// photo day's highest revision) — and the undone copies, and the others
/// they took the place of, stand behind it; when no other copy is here,
/// they stand behind the base's id (the base arriving later stands, they
/// never do). A copy standing behind something outside the record (a
/// Chrome day behind the profile day that holds it) stays there. A retired
/// copy keeps its retirement; what its undo restores is set the same way.
/// With no undone copy, a photo day's correction this node couldn't check
/// when it arrived is checked again (`photo::settle_repair_copy`). Returns
/// the copies whose state changed.
pub(crate) fn settle_revisions(
    conn: &Connection,
    home: &Home,
    source: &str,
    base: &str,
) -> Result<usize> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT id, source_ref, content_ref, superseded_by FROM episodes /* all-episodes: every copy of a re-minted record, in any state */
         WHERE source = ?1 AND (source_ref = ?2 OR {})",
        growing_revision_sql(2)
    ))?;
    let rows: Vec<(String, String, Option<String>, Option<String>)> = stmt
        .query_map(params![source, base], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut copies = Vec::with_capacity(rows.len());
    for (id, source_ref, content_ref, superseded_by) in rows {
        let undone = undone_copy(conn, &id)?;
        copies.push(Revised {
            id,
            source_ref,
            content_ref,
            superseded_by,
            undone,
        });
    }
    if !copies.iter().any(|c| c.undone) {
        // a photo day's correction this node couldn't check when it arrived
        // stands once it can (store/photo.rs)
        return if source == "photos" {
            super::photo::settle_repair_copy(conn, home, base)
        } else {
            Ok(0)
        };
    }
    // where each copy stands, a retired one as its undo would put it
    let mut effective: Vec<(Option<String>, Option<String>)> = Vec::with_capacity(copies.len());
    for c in &copies {
        effective.push(match c.superseded_by.as_deref() {
            Some(mark) if mark.starts_with(RETIRED_PREFIX) => {
                (prior_of_retired(conn, &c.id, mark)?, Some(mark.to_string()))
            }
            other => (other.map(String::from), None),
        });
    }
    let ids: HashSet<&str> = copies.iter().map(|c| c.id.as_str()).collect();
    let undone: HashSet<&str> = copies
        .iter()
        .filter(|c| c.undone)
        .map(|c| c.id.as_str())
        .collect();
    let rank = |c: &Revised| -> (u64, bool, String) {
        let n = if source == "photos" {
            revision_of(source, &c.source_ref).map_or(1, |(_, n)| n)
        } else {
            match growing_copy(source, &c.source_ref) {
                Some((_, n)) if n > 0 => return (n, true, c.source_ref.clone()),
                _ => c
                    .content_ref
                    .as_deref()
                    .and_then(|rel| std::fs::read_to_string(home.root().join(rel)).ok())
                    .and_then(|body| growing_items(source, &body))
                    .unwrap_or(0),
            }
        };
        (n, false, c.source_ref.clone())
    };
    // the copies that stood, or stood until an undone copy took their place
    let candidates: Vec<usize> = (0..copies.len())
        .filter(|&i| {
            !copies[i].undone && effective[i].0.as_deref().is_none_or(|s| undone.contains(s))
        })
        .collect();
    // none: only undone copies reached this node — they stand behind the
    // base's id, so the base arriving later stands and they never do
    let best = candidates.iter().copied().max_by_key(|&i| rank(&copies[i]));
    let best_id = match best {
        Some(i) => copies[i].id.clone(),
        None => ids::episode_id(source, base),
    };
    let mut changed = 0;
    for (i, copy) in copies.iter().enumerate() {
        let (now, mark) = &effective[i];
        let want: Option<&str> = if Some(i) == best {
            None
        } else if copy.undone {
            if now.as_deref().is_some_and(|s| !ids.contains(s)) {
                continue;
            }
            Some(&best_id)
        } else if candidates.contains(&i) {
            Some(&best_id)
        } else {
            continue;
        };
        if now.as_deref() == want {
            continue;
        }
        changed += 1;
        if let Some(mark) = mark {
            super::retire::set_prior_superseded(conn, &copy.id, mark, want)?;
            continue;
        }
        conn.execute(
            "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
            params![want, copy.id],
        )?;
        if want.is_some() {
            conn.execute(
                "DELETE FROM location_points WHERE episode_id = ?1",
                [&copy.id],
            )?;
        } else {
            conn.execute(
                "UPDATE episodes SET location_scanned = 0 WHERE id = ?1",
                [&copy.id],
            )?;
        }
    }
    Ok(changed)
}

/// One stored day or session of a growing source as `repair copies` sees
/// it: re-minted from its stored text, or why not.
#[derive(Debug, Clone)]
pub struct CopyFinding {
    /// The unversioned copy's episode id.
    pub episode: String,
    pub source: String,
    /// `remint`, `held` (a copy holding as much is stored), `undone`
    /// (re-minted before by a run that was undone), `uncounted` (its text
    /// gives no count), `unreadable`.
    pub status: &'static str,
    pub reason: String,
    /// The copy to land, for `remint`.
    pub draft: Option<EpisodeDraft>,
    pub private: bool,
    pub retired: bool,
}

/// What a growing source's items are called, for reasons.
fn items_word(source: &str) -> &'static str {
    match source {
        "chrome" => "visits",
        "imessage" => "messages",
        _ => "turns",
    }
}

/// A stored copy of a growing source, as the copies scan lists it.
struct Listed {
    id: String,
    source_ref: String,
    kind: String,
    title: String,
    event_time: Option<String>,
    tz_offset_minutes: Option<i64>,
    time_uncertainty: String,
    trust: String,
    scope: String,
    content_ref: Option<String>,
    superseded_by: Option<String>,
}

/// A stamp for a change to a run whose row says `previous`: now, or a
/// second past `previous` when that isn't later (stamps have whole seconds,
/// and last-writer-wins needs the change to read as newer).
pub(super) fn stamp_after(previous: &str) -> String {
    let now = now_utc();
    if now.as_str() > previous {
        return now;
    }
    chrono::DateTime::parse_from_rfc3339(previous)
        .map(|t| {
            (t.with_timezone(&chrono::Utc) + chrono::Duration::seconds(1))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
        .unwrap_or(now)
}

/// What [`Store::repair_same_event`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SameEvent {
    /// The base ref of the stored record that is this event.
    Stored(String),
    /// A stored copy (its episode id) that may be this event or another
    /// event whose UID begins the same way: it can't tell.
    Unsure(String),
    /// None stored.
    Absent,
}

/// A ref's tail as the calendar importer writes one after `<UID>#`: a start
/// or RECURRENCE-ID as a date (`20260316`) or a date-time
/// (`20260316T090000`, `…Z`).
fn occurrence_shaped(tail: &str) -> bool {
    let b = tail.as_bytes();
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    match b.len() {
        8 => digits(b),
        15 | 16 => {
            digits(&b[..8])
                && b[8].eq_ignore_ascii_case(&b'T')
                && digits(&b[9..15])
                && (b.len() == 15 || b[15].eq_ignore_ascii_case(&b'Z'))
        }
        _ => false,
    }
}

/// Is the stored event `episode_id` — its record's base ref `base`, which
/// is `uid` or begins `<uid>#` — one of the calendar UID `uid`? A ref is
/// `<UID>` or `<UID>#<start>`, and a UID is the sender's choice: a tail
/// that isn't a start (`meeting#other`) is another event's UID, and one
/// that is (`meeting#20260316T090000`) may be either. Then the Event entity
/// the copy links to as its subject says: it holds the UID the event was
/// imported under (`ics-uid`). None when that can't tell — no such link (a
/// retired copy has none), or an entity holding both UIDs (events of one
/// name share an entity).
fn event_is_uid(
    conn: &Connection,
    episode_id: &str,
    base: &str,
    uid: &str,
) -> Result<Option<bool>> {
    // the UID the ref reads as when it isn't one of `uid`
    let other = if base == uid {
        match uid.rsplit_once('#') {
            Some((prefix, tail)) if occurrence_shaped(tail) => prefix,
            _ => return Ok(Some(true)),
        }
    } else {
        match base.strip_prefix(uid).and_then(|r| r.strip_prefix('#')) {
            Some(tail) if occurrence_shaped(tail) => base,
            _ => return Ok(Some(false)),
        }
    };
    let mut stmt = conn.prepare_cached(
        "SELECT entity_id FROM episode_entities /* all-episodes: the UID a stored copy was imported under */
         WHERE episode_id = ?1 AND role = 'subject'",
    )?;
    let subjects: Vec<String> = stmt
        .query_map([episode_id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if subjects.is_empty() {
        return Ok(None);
    }
    let mut roots = HashSet::new();
    for s in &subjects {
        roots.insert(super::graph::resolve_merged(conn, s)?);
    }
    let holds = |value: &str| -> Result<bool> {
        let owner: Option<String> = conn
            .query_row(
                "SELECT entity_id FROM entity_attributes WHERE kind = 'ics-uid' AND value = ?1",
                [value],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match owner {
            Some(o) => roots.contains(&super::graph::resolve_merged(conn, &o)?),
            None => false,
        })
    };
    Ok(match (holds(uid)?, holds(other)?) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    })
}

fn exists(conn: &Connection, episode_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM episodes /* all-episodes: an existence check */ WHERE id = ?1)",
        [episode_id],
        |r| r.get(0),
    )?)
}

impl Store {
    /// Every record of `source` as a repair sees it, read a page at a time.
    pub fn repair_targets(&self, source: &str) -> Result<Vec<RepairTarget>> {
        let mut bases: std::collections::BTreeMap<String, Vec<Copy>> = Default::default();
        let mut after = 0i64;
        loop {
            let page: Vec<(i64, CopyRow)> = {
                let conn = self.lock();
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COPY_COLUMNS}, e.rowid
                     FROM episodes e /* all-episodes: a repair reads every copy of every record */
                     LEFT JOIN episode_origin o ON o.episode_id = e.id
                     WHERE e.rowid > ?1 AND e.source = ?2
                     ORDER BY e.rowid LIMIT ?3"
                ))?;
                let rows = stmt.query_map(params![after, source, LIST_PAGE], |r| {
                    Ok((r.get::<_, i64>(13)?, copy_row(r)?))
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            let Some((last, _)) = page.last() else { break };
            after = *last;
            let full = page.len() as i64 >= LIST_PAGE;
            for (_, (mut copy, writer, session, at)) in page {
                let base = match repair_of(source, &copy.source_ref, writer.as_deref()) {
                    Some(base) => {
                        copy.replacement = Some((at.unwrap_or_default(), session));
                        base.to_string()
                    }
                    None => copy.source_ref.clone(),
                };
                bases.entry(base).or_default().push(copy);
            }
            if !full {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        let undone: HashSet<String> = {
            let conn = self.lock();
            let mut set = HashSet::new();
            for copies in bases
                .values()
                .filter(|c| c.iter().any(|c| c.replacement.is_some()))
            {
                set.extend(undone_copies(&conn, copies)?);
            }
            set
        };
        Ok(bases
            .iter()
            .filter_map(|(base, copies)| RepairTarget::of(source, base, copies, &undone))
            .collect())
    }

    /// One record by its base ref, as a repair sees it.
    pub fn repair_target(&self, source: &str, base_ref: &str) -> Result<Option<RepairTarget>> {
        let conn = self.lock();
        let copies = record(&conn, source, base_ref)?;
        let undone = undone_copies(&conn, &copies)?;
        Ok(RepairTarget::of(source, base_ref, &copies, &undone))
    }

    /// A stored event of the calendar UID `uid` that starts at `event_time`
    /// (UTC) — one an earlier importer stored under another ref (`<uid>` or
    /// `<uid>#<start>`) than the current one gives it. A UID is the
    /// sender's choice, so a ref that begins `<uid>#` may be another
    /// event's own (UID `meeting#other`, or `meeting#20260316T090000`):
    /// each candidate's UID is checked ([`event_is_uid`]), and one that
    /// can't be told apart is reported as such, never taken.
    pub fn repair_same_event(
        &self,
        source: &str,
        uid: &str,
        event_time: &str,
    ) -> Result<SameEvent> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT e.id, e.source_ref, o.writer
             FROM episodes e /* all-episodes: is this event stored under another ref? */
             LEFT JOIN episode_origin o ON o.episode_id = e.id
             WHERE e.source = ?1 AND e.event_time = ?3
               AND (e.source_ref = ?2 OR (e.source_ref >= ?2 || '#' AND e.source_ref < ?2 || '$'))
             ORDER BY e.source_ref",
        )?;
        let rows: Vec<(String, String, Option<String>)> = stmt
            .query_map(params![source, uid, event_time], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        // each record (its base ref): what its copies here say, and one of them
        let mut records: BTreeMap<String, (Vec<Option<bool>>, String)> = BTreeMap::new();
        for (id, source_ref, writer) in rows {
            let base = repair_of(source, &source_ref, writer.as_deref())
                .unwrap_or(&source_ref)
                .to_string();
            let verdict = event_is_uid(&conn, &id, &base, uid)?;
            records
                .entry(base)
                .or_insert_with(|| (Vec::new(), id))
                .0
                .push(verdict);
        }
        let mut unsure = None;
        for (base, (verdicts, id)) in records {
            if verdicts.iter().all(|v| *v == Some(true)) {
                return Ok(SameEvent::Stored(base));
            }
            if !verdicts.iter().all(|v| *v == Some(false)) {
                unsure.get_or_insert(id);
            }
        }
        Ok(unsure.map_or(SameEvent::Absent, SameEvent::Unsure))
    }

    /// Land a repair run: each corrected copy under its replacement ref,
    /// written as the owner repair (its base's label and trust; the base
    /// goes behind it), and the missing episodes `additions` as an ordinary
    /// import by `addition_actor`. Nothing new to land: no run, nothing
    /// changes. The caller checks the owner and the vault.
    pub fn repair_apply(
        &self,
        home: &Home,
        actor: &Actor,
        kind: &str,
        items: &[RepairItem],
        additions: &[EpisodeDraft],
        addition_actor: &str,
    ) -> Result<RepairApplied> {
        self.repair_apply_work(
            home,
            actor,
            kind,
            RepairWork {
                items,
                additions,
                addition_actor,
                ..Default::default()
            },
        )
    }

    /// Land a repair run ([`RepairWork`]): corrected copies in their
    /// bases' place, copies re-minted under their source's own revision
    /// rule, missing episodes, and photo-day ledgers. Nothing new to land
    /// and no ledger to change: no run, nothing changes. The caller checks
    /// the owner and the vault.
    pub fn repair_apply_work(
        &self,
        home: &Home,
        actor: &Actor,
        kind: &str,
        work: RepairWork<'_>,
    ) -> Result<RepairApplied> {
        let addition_actor = work.addition_actor;
        if is_repair_actor(addition_actor)
            || addition_actor.starts_with("peer:")
            || (addition_actor.is_empty() && !work.additions.is_empty())
        {
            bail!("additions land as an ordinary import, not as {addition_actor:?}");
        }
        if !kind.bytes().all(|b| b.is_ascii_lowercase()) || kind.is_empty() || kind.len() > 20 {
            bail!("unknown repair kind {kind:?}");
        }
        let (items, additions) = (work.items, work.additions);
        let pause = || std::thread::sleep(std::time::Duration::from_millis(40));
        let mut drafts = Vec::with_capacity(items.len());
        let mut fresh = 0usize;
        let mut existing = 0usize;
        for (i, chunk) in items.chunks(SETTLE_BATCH).enumerate() {
            if i > 0 {
                pause();
            }
            let conn = self.lock();
            for item in chunk {
                let source = item.draft.source.as_str();
                if !REPAIRABLE_SOURCES.contains(&source) {
                    bail!("{source} memories can't be repaired");
                }
                let copies = record(&conn, source, &item.base_ref)?;
                // the original; or, when only corrected copies of it reached
                // this node, one of them (they carry its trust)
                let Some(base) = copies
                    .iter()
                    .find(|c| c.replacement.is_none())
                    .or_else(|| copies.first())
                else {
                    bail!(
                        "no memory {} to repair",
                        ids::episode_id(source, &item.base_ref)
                    );
                };
                if let Some((_, original)) = record_of(&conn, &base.id)? {
                    if original != item.base_ref {
                        bail!(
                            "{} is a repaired copy — repair the original memory {}",
                            base.id,
                            ids::episode_id(source, &original)
                        );
                    }
                }
                let label = RecordArrival {
                    copies: copies.clone(),
                }
                .inherited_scope()
                .unwrap_or(&base.scope)
                .to_string();
                let mut draft = item.draft.clone();
                draft.source_ref = replacement_ref(&item.base_ref, &draft.title, &draft.body);
                draft.scope = Some(label);
                draft.trust = base.trust.clone();
                if exists(&conn, &ids::episode_id(source, &draft.source_ref))? {
                    existing += 1;
                } else {
                    fresh += 1;
                }
                drafts.push(draft);
            }
        }
        for chunk in additions.chunks(SETTLE_BATCH) {
            let conn = self.lock();
            for draft in chunk {
                if exists(&conn, &ids::episode_id(&draft.source, &draft.source_ref))? {
                    existing += 1;
                } else {
                    fresh += 1;
                }
            }
        }
        // re-minted copies: each a revision its source's own rule places
        let mut revisions = Vec::with_capacity(work.revisions.len());
        for chunk in work.revisions.chunks(SETTLE_BATCH) {
            let conn = self.lock();
            for draft in chunk {
                let id = ids::episode_id(&draft.source, &draft.source_ref);
                if revision_of(&draft.source, &draft.source_ref).is_none() {
                    bail!("{id} isn't a copy a repair re-mints");
                }
                if exists(&conn, &id)? {
                    existing += 1;
                } else {
                    fresh += 1;
                    revisions.push(draft.clone());
                }
            }
        }
        // photo-day ledgers that change
        let mut ledgers: Vec<LedgerChange> = Vec::new();
        {
            let conn = self.lock();
            for l in work.photo_ledgers {
                let before = ledger_row(&conn, &l.day)?;
                let same = before.as_ref().is_some_and(|b| {
                    (b.count, b.keys_hash.as_str(), b.revision)
                        == (l.count, l.keys_hash.as_str(), l.revision)
                });
                if !same {
                    ledgers.push(LedgerChange {
                        day: l.day.clone(),
                        before,
                        after: l.clone(),
                    });
                }
            }
        }
        if fresh == 0 && ledgers.is_empty() {
            return Ok(RepairApplied {
                existing,
                ..Default::default()
            });
        }
        let run = format!("rp_{}", uuid::Uuid::new_v4().simple());
        let now = now_utc();
        {
            let conn = self.lock();
            let node = super::provenance::node_id_on(&conn)?;
            conn.execute(
                &format!(
                    "INSERT INTO repairs ({RUN_COLUMNS})
                     VALUES (?1, ?2, 0, 0, ?3, ?4, ?5, NULL, ?5)"
                ),
                params![run, kind, actor.actor, node, now],
            )?;
        }
        // a batch per transaction, a pause between (the store-lock rule)
        let mut replaced = 0;
        for (i, chunk) in drafts.chunks(SETTLE_BATCH).enumerate() {
            if i > 0 {
                pause();
            }
            replaced += self
                .apply_batch_as(
                    home,
                    &IngestBatch {
                        actor: REPAIR_WRITER.into(),
                        session_id: Some(run.clone()),
                        episodes: chunk.to_vec(),
                    },
                    &[],
                    true,
                )?
                .episodes_new;
        }
        if !drafts.is_empty() && !revisions.is_empty() {
            pause();
        }
        let revised = self.land_revisions(home, &run, &revisions)?;
        if !ledgers.is_empty() {
            // what the undo puts back, kept before anything changes
            self.set_consolidation_state(&ledgers_key(&run), &serde_json::to_string(&ledgers)?)?;
            let conn = self.lock();
            for change in &ledgers {
                let l = &change.after;
                conn.execute(
                    "INSERT INTO photo_day (day, count, keys_hash, episode_id, revision, minted_at)
                     VALUES (?1, ?2, ?3, NULL, ?4, ?5)
                     ON CONFLICT(day) DO UPDATE SET count = excluded.count,
                         keys_hash = excluded.keys_hash, episode_id = excluded.episode_id,
                         revision = excluded.revision, minted_at = excluded.minted_at",
                    params![l.day, l.count, l.keys_hash, l.revision, now_utc()],
                )?;
            }
        }
        let mut added = 0;
        for (i, chunk) in additions.chunks(SETTLE_BATCH).enumerate() {
            if i > 0 || !drafts.is_empty() {
                pause();
            }
            added += self
                .apply_batch(
                    home,
                    &IngestBatch {
                        actor: addition_actor.into(),
                        session_id: Some(run.clone()),
                        episodes: chunk.to_vec(),
                    },
                )?
                .episodes_new;
        }
        {
            let conn = self.lock();
            // a re-minted copy counts as one the run replaced
            conn.execute(
                "UPDATE repairs SET replaced = ?2, added = ?3, updated_at = ?4 WHERE id = ?1",
                params![
                    run,
                    (replaced + revised) as i64,
                    added as i64,
                    stamp_after(&now)
                ],
            )?;
            record_mutation(
                &conn,
                actor,
                "repair_applied",
                &format!("repair_applied:{run}"),
                &json!({"run": run, "kind": kind, "replaced": replaced, "revised": revised,
                        "added": added, "ledgers": ledgers.len()}),
            )?;
        }
        self.set_consolidation_state(&state_key(&run), "applied")?;
        Ok(RepairApplied {
            run: Some(run),
            replaced,
            added,
            revised,
            ledgers: ledgers.len(),
            made_private: 0,
            existing,
        })
    }

    /// Land copies re-minted under their source's own revision rule as the
    /// owner repair, under the run `run` (the rule places each: a growing
    /// source keeps its fullest copy in view, a photo day its newest
    /// revision). A growing source's copy re-minted from its stored text
    /// keeps the links that text had (its draft names none). Returns the
    /// copies that landed new.
    fn land_revisions(&self, home: &Home, run: &str, drafts: &[EpisodeDraft]) -> Result<usize> {
        let mut landed = 0;
        for (i, chunk) in drafts.chunks(SETTLE_BATCH).enumerate() {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            let fresh: Vec<bool> = {
                let conn = self.lock();
                chunk
                    .iter()
                    .map(|d| exists(&conn, &ids::episode_id(&d.source, &d.source_ref)).map(|e| !e))
                    .collect::<Result<_>>()?
            };
            landed += self
                .apply_batch_as(
                    home,
                    &IngestBatch {
                        actor: REPAIR_WRITER.into(),
                        session_id: Some(run.to_string()),
                        episodes: chunk.to_vec(),
                    },
                    &[],
                    true,
                )?
                .episodes_new;
            let conn = self.lock();
            for (draft, fresh) in chunk.iter().zip(fresh) {
                if !fresh || !draft.entities.is_empty() {
                    continue;
                }
                let Some((base, _)) = growing_copy(&draft.source, &draft.source_ref) else {
                    continue;
                };
                conn.execute(
                    "INSERT OR IGNORE INTO episode_entities (episode_id, entity_id, role)
                     SELECT ?1, entity_id, role FROM episode_entities /* all-episodes: a re-minted copy keeps the links of the text it was made from */
                     WHERE episode_id = ?2",
                    params![
                        ids::episode_id(&draft.source, &draft.source_ref),
                        ids::episode_id(&draft.source, base)
                    ],
                )?;
            }
        }
        Ok(landed)
    }

    /// Every day or session of a growing source stored under an unversioned
    /// base — what earlier versions stored, each node keeping the text that
    /// reached it first — and whether `repair copies` re-mints it here as
    /// `<base>#r<n>` from its stored text (`n` the items it holds, the
    /// growing rule's name for a copy). A base is left when a copy holding
    /// as much is stored already (re-minted by this node or another, or
    /// read again since), when it stands behind a copy that holds it, or
    /// when a run that re-minted it was undone. Reads every stored copy a
    /// page at a time; texts are read with the store released.
    pub fn repair_copies_scan(&self, home: &Home) -> Result<Vec<CopyFinding>> {
        let mut out = Vec::new();
        for source in GROWING_SOURCES {
            let mut listed: Vec<Listed> = Vec::new();
            let mut after = String::new();
            loop {
                let page: Vec<Listed> = {
                    let conn = self.lock();
                    let mut stmt = conn.prepare_cached(
                        "SELECT id, source_ref, kind, title, event_time, tz_offset_minutes,
                                time_uncertainty, trust, scope, content_ref, superseded_by
                         FROM episodes /* all-episodes: every stored copy of a day or session, replaced and retired ones too */
                         WHERE source = ?1 AND source_ref > ?2 ORDER BY source_ref LIMIT ?3",
                    )?;
                    let rows = stmt.query_map(params![source, after, LIST_PAGE], |r| {
                        Ok(Listed {
                            id: r.get(0)?,
                            source_ref: r.get(1)?,
                            kind: r.get(2)?,
                            title: r.get(3)?,
                            event_time: r.get(4)?,
                            tz_offset_minutes: r.get(5)?,
                            time_uncertainty: r.get(6)?,
                            trust: r.get(7)?,
                            scope: r.get(8)?,
                            content_ref: r.get(9)?,
                            superseded_by: r.get(10)?,
                        })
                    })?;
                    rows.collect::<std::result::Result<_, _>>()?
                };
                let Some(last) = page.last() else { break };
                after = last.source_ref.clone();
                let full = page.len() as i64 >= LIST_PAGE;
                listed.extend(page);
                if !full {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            // the copies of each base
            let mut groups: BTreeMap<&str, Vec<&Listed>> = BTreeMap::new();
            for row in &listed {
                if let Some((base, _)) = growing_copy(source, &row.source_ref) {
                    groups.entry(base).or_default().push(row);
                }
            }
            for (i, (base, copies)) in groups.iter().enumerate() {
                let Some(stored) = copies.iter().find(|c| c.source_ref == *base) else {
                    continue;
                };
                if i > 0 && i % SETTLE_BATCH == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
                let retired = stored
                    .superseded_by
                    .as_deref()
                    .is_some_and(|s| s.starts_with(RETIRED_PREFIX));
                let mut finding = CopyFinding {
                    episode: stored.id.clone(),
                    source: source.to_string(),
                    status: "remint",
                    reason: String::new(),
                    draft: None,
                    private: stored.scope == "private",
                    retired,
                };
                let body = stored
                    .content_ref
                    .as_deref()
                    .and_then(|rel| std::fs::read_to_string(home.root().join(rel)).ok());
                let Some(body) = body else {
                    finding.status = "unreadable";
                    finding.reason = "its stored text can't be read".into();
                    out.push(finding);
                    continue;
                };
                let Some(n) = growing_items(source, &body).filter(|n| *n > 0) else {
                    finding.status = "uncounted";
                    finding.reason = "its stored text gives no count to name a copy by".into();
                    out.push(finding);
                    continue;
                };
                let target = format!("{base}#r{n}");
                let (fuller, same, undone_same) = {
                    let conn = self.lock();
                    let mut fuller = false;
                    let mut same = false;
                    let mut undone_same = false;
                    for c in copies.iter().filter(|c| c.source_ref != *base) {
                        let Some((_, m)) = growing_copy(source, &c.source_ref) else {
                            continue;
                        };
                        if c.source_ref == target {
                            same = true;
                            undone_same = undone_copy(&conn, &c.id)?;
                        } else if m >= n && !undone_copy(&conn, &c.id)? {
                            fuller = true;
                        }
                    }
                    (fuller, same, undone_same)
                };
                let behind_other = stored.superseded_by.as_deref().is_some_and(|s| {
                    !s.starts_with(RETIRED_PREFIX) && !copies.iter().any(|c| c.id == s)
                });
                let word = items_word(source);
                if same && undone_same {
                    finding.status = "undone";
                    finding.reason = "re-minted by a run that was undone; left as it is".into();
                } else if same {
                    finding.status = "held";
                    finding.reason = format!("a copy holding its {n} {word} is stored already");
                } else if fuller {
                    finding.status = "held";
                    finding.reason = format!("a copy holding at least its {n} {word} is stored");
                } else if behind_other {
                    finding.status = "held";
                    finding.reason = "it stands behind a copy that holds it".into();
                } else {
                    finding.reason = format!("{n} {word}, stored under an unversioned ref");
                    if retired {
                        finding.reason.push_str("; retired, the copy lands retired");
                    }
                    finding.draft = Some(EpisodeDraft {
                        source: source.to_string(),
                        source_ref: target,
                        kind: stored.kind.clone(),
                        title: stored.title.clone(),
                        body,
                        event_time: EventTime {
                            utc: stored.event_time.clone(),
                            tz_offset_minutes: stored.tz_offset_minutes.map(|m| m as i32),
                            uncertainty: TimeUncertainty::parse(&stored.time_uncertainty)
                                .unwrap_or(TimeUncertainty::Unknown),
                        },
                        trust: stored.trust.clone(),
                        // the record's label: the growing rule gives it
                        scope: None,
                        entities: vec![],
                        edges: vec![],
                    });
                }
                out.push(finding);
            }
        }
        Ok(out)
    }

    /// Re-mint, under the run `run`, the copies this node stores under an
    /// unversioned base that no copy holding as much covers yet — what a
    /// `copies` run does on each node it reaches. Returns the copies landed.
    fn repair_remint_here(&self, home: &Home, run: &str) -> Result<usize> {
        let drafts: Vec<EpisodeDraft> = self
            .repair_copies_scan(home)?
            .into_iter()
            .filter_map(|f| f.draft)
            .collect();
        self.land_revisions(home, run, &drafts)
    }

    /// Put back the photo-day ledgers the run `run` set on this node — each
    /// one that still says what the run set (a day minted again since keeps
    /// its newer ledger). The run's copy stays stored under its revision, so
    /// the ledger put back names that revision (or the one before, if
    /// higher): the finalize pass mints a day at the ledger's revision + 1,
    /// and a ref taken already would never land. A day that had no ledger
    /// gets the one the finalize pass adopts for a day it didn't mint (the
    /// count the copy in view shows), at that revision. Returns how many.
    fn restore_photo_ledgers(&self, home: &Home, run: &str) -> Result<usize> {
        let Some(saved) = self.consolidation_state(&ledgers_key(run))? else {
            return Ok(0);
        };
        let changes: Vec<LedgerChange> = serde_json::from_str(&saved).unwrap_or_default();
        // what the copy in view of each day without a ledger before shows
        let mut shown: std::collections::HashMap<String, i64> = Default::default();
        for change in changes.iter().filter(|c| c.before.is_none()) {
            if let Some((_, _, count)) = self.current_photo_episode(home, &change.day)? {
                shown.insert(change.day.clone(), count);
            }
        }
        let conn = self.lock();
        let mut restored = 0;
        for change in changes {
            let now = ledger_row(&conn, &change.day)?;
            let set_by_run = now.as_ref().is_some_and(|n| {
                (n.count, n.keys_hash.as_str(), n.revision)
                    == (
                        change.after.count,
                        change.after.keys_hash.as_str(),
                        change.after.revision,
                    )
            });
            if !set_by_run {
                continue;
            }
            let revision = change.after.revision;
            match (change.before, shown.get(&change.day)) {
                (Some(b), _) => {
                    conn.execute(
                        "UPDATE photo_day SET count = ?2, keys_hash = ?3, episode_id = ?4,
                             revision = ?5, minted_at = ?6
                         WHERE day = ?1",
                        params![
                            change.day,
                            b.count,
                            b.keys_hash,
                            b.episode_id,
                            b.revision.max(revision),
                            b.minted_at
                        ],
                    )?;
                }
                (None, Some(count)) => {
                    conn.execute(
                        "UPDATE photo_day SET count = ?2, keys_hash = 'legacy', episode_id = NULL,
                             revision = ?3, minted_at = ?4
                         WHERE day = ?1",
                        params![change.day, count, revision, now_utc()],
                    )?;
                }
                (None, None) => {
                    conn.execute("DELETE FROM photo_day WHERE day = ?1", [&change.day])?;
                }
            }
            restored += 1;
        }
        Ok(restored)
    }

    /// Undo a run: the row is stamped (it syncs, so every node undoes it)
    /// and this node puts each record it replaced back as it was — the base
    /// in view, the run's copies behind it (a re-minted record: the copy
    /// that stood before, and the photo-day ledgers the run set). What the
    /// run added stays.
    pub fn repair_undo(&self, home: &Home, actor: &Actor, run: &str) -> Result<usize> {
        {
            let conn = self.lock();
            let previous: Option<String> = conn
                .query_row(
                    "SELECT updated_at FROM repairs WHERE id = ?1 AND undone_at IS NULL",
                    [run],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(previous) = previous else {
                bail!("no repair {run} to undo");
            };
            let now = stamp_after(&previous);
            conn.execute(
                "UPDATE repairs SET undone_at = ?1, updated_at = ?1 WHERE id = ?2 AND undone_at IS NULL",
                params![now, run],
            )?;
            record_mutation(
                &conn,
                actor,
                "repair_undone",
                &format!("repair_undone:{run}"),
                &json!({"run": run}),
            )?;
        }
        self.repair_settle_run(home, run)
    }

    /// One run.
    pub fn repair_run(&self, id: &str) -> Result<Option<RepairRun>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {RUN_COLUMNS} FROM repairs WHERE id = ?1"),
            [id],
            run_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Runs, newest first.
    pub fn repair_runs(&self, limit: usize) -> Result<Vec<RepairRun>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM repairs ORDER BY created_at DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map([limit as i64], run_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Settle again every record a run replaced or re-minted (after its
    /// undo, or when the run reached this node after its copies). A
    /// `copies` run that reaches a node first re-mints that node's own
    /// unversioned copies under the run ([`Store::repair_copies_scan`]):
    /// each node holds its own text under such a base, and only it can
    /// name that text by its count. An undone run's photo-day ledgers on
    /// this node are put back. Returns the copies whose state changed.
    pub fn repair_settle_run(&self, home: &Home, run: &str) -> Result<usize> {
        let row = self.repair_run(run)?;
        let undone = row.as_ref().is_some_and(|r| r.undone_at.is_some());
        let mut changed = 0;
        if !undone && row.as_ref().is_some_and(|r| r.kind == COPIES_KIND) {
            changed += self.repair_remint_here(home, run)?;
        }
        // a `copies` run's re-mints here: the run's row carries them on to
        // the other nodes (a node that made the same copy under a run of
        // its own learns this run made it too)
        if changed > 0 {
            self.lock().execute(
                "UPDATE repairs SET updated_at = updated_at WHERE id = ?1",
                [run],
            )?;
        }
        // (replaced, source, base): a corrected copy's record, or a
        // re-minted one's — every copy the run made, on any node
        let records: Vec<(bool, String, String)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(&format!(
                "SELECT e.source, e.source_ref
                 FROM repair_copies c
                 JOIN episodes e ON e.id = c.episode_id /* all-episodes: every copy a run made, in any state */
                 JOIN episode_origin o ON o.episode_id = e.id
                 WHERE c.run = ?1 AND o.writer = '{REPAIR_WRITER}'"
            ))?;
            let rows = stmt.query_map([run], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
            let set: BTreeSet<(bool, String, String)> = rows
                .filter_map(|r| r.ok())
                .filter_map(|(source, source_ref): (String, String)| {
                    if REPAIRABLE_SOURCES.contains(&source.as_str()) {
                        replacement_base(&source_ref).map(|b| (true, source.clone(), b.to_string()))
                    } else {
                        revision_of(&source, &source_ref)
                            .map(|(b, _)| (false, source.clone(), b.to_string()))
                    }
                })
                .collect();
            set.into_iter().collect()
        };
        for (i, chunk) in records.chunks(SETTLE_BATCH).enumerate() {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            let mut conn = self.lock();
            let tx = conn.transaction()?;
            let rules = super::retire::ArrivalRules::load(&tx)?;
            for (replaced, source, base) in chunk {
                changed += if *replaced {
                    settle(&tx, source, base)? + retire_with_record(&tx, &rules, source, base)?
                } else {
                    settle_revisions(&tx, home, source, base)?
                        + super::growing::restore_revived(&tx, home, source, base)?
                };
            }
            tx.commit()?;
        }
        if undone {
            changed += self.restore_photo_ledgers(home, run)?;
        }
        self.set_consolidation_state(&state_key(run), if undone { "undone" } else { "applied" })?;
        Ok(changed)
    }

    /// Runs this node hasn't settled in their current state — one made or
    /// undone on another node, or one interrupted here.
    pub fn repairs_pending_here(&self) -> Result<Vec<String>> {
        let rows: Vec<(String, Option<String>)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare("SELECT id, undone_at FROM repairs ORDER BY created_at")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows
        };
        let mut pending = Vec::new();
        for (id, undone) in rows {
            let want = if undone.is_some() {
                "undone"
            } else {
                "applied"
            };
            if self.consolidation_state(&state_key(&id))?.as_deref() != Some(want) {
                pending.push(id);
            }
        }
        Ok(pending)
    }

    /// Runs newer than the watermark, for the peer leg (paged by the local
    /// `synced_at` relay stamp), each with the copies it made (`copies`)
    /// and those of them a later read put back in view (`revived`).
    pub fn repairs_sync_export(&self, since: &str) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLUMNS} FROM repairs WHERE synced_at > ?1 ORDER BY synced_at LIMIT 500"
        ))?;
        let rows: Vec<RepairRun> = stmt
            .query_map([since], run_row)?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut made = conn.prepare_cached(
            "SELECT episode_id FROM repair_copies WHERE run = ?1 ORDER BY episode_id",
        )?;
        let mut put_back = conn.prepare_cached(
            "SELECT c.episode_id FROM repair_copies c
             JOIN repair_revived v ON v.episode_id = c.episode_id
             WHERE c.run = ?1 ORDER BY c.episode_id",
        )?;
        let mut runs = Vec::with_capacity(rows.len());
        for run in rows {
            let copies: Vec<String> = made
                .query_map([&run.id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            let revived: Vec<String> = put_back
                .query_map([&run.id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            let mut row = serde_json::to_value(&run)?;
            row["copies"] = json!(copies);
            // additive: the copies a later read put back in view
            if !revived.is_empty() {
                row["revived"] = json!(revived);
            }
            runs.push(row);
        }
        drop(made);
        drop(put_back);
        let next = super::page_cursor(
            &conn,
            "synced_at",
            "FROM repairs WHERE synced_at > ?1",
            since,
            500,
        )?;
        Ok(json!({"repairs": runs, "next": next}))
    }

    /// Apply a peer's page: last writer wins by `updated_at`, and an undo
    /// stays undone; the copies each run made, and those a read put back in
    /// view, are unioned, whatever the row. Returns the runs to settle again
    /// here — a row that changed, an undone run that made copies this node
    /// didn't know of, or one whose copy a read put back.
    pub fn repairs_sync_apply(&self, batch: &Value) -> Result<Vec<String>> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut changed = Vec::new();
        for r in batch
            .get("repairs")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(kind), Some(updated)) = (
                r["id"].as_str(),
                r["kind"].as_str(),
                r["updated_at"].as_str(),
            ) else {
                continue;
            };
            let plain = |s: &str| {
                !s.is_empty()
                    && s.len() <= 64
                    && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            };
            if !id.starts_with("rp_") || !plain(id) || !plain(kind) {
                continue;
            }
            let local: Option<(String, Option<String>)> = tx
                .query_row(
                    "SELECT updated_at, undone_at FROM repairs WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            // newer wins; an undo is never lost to a row that predates it
            let brings_undo =
                |undone: &Option<String>| undone.is_none() && r["undone_at"].as_str().is_some();
            let newer = !local
                .as_ref()
                .is_some_and(|(l, undone)| l.as_str() >= updated && !brings_undo(undone));
            if newer {
                upsert_run(&tx, r, id, kind, updated)?;
            }
            // the copies it made: a set every node unions, whatever the row
            let mut grew = 0;
            for copy in r["copies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str())
                .filter(|c| is_episode_id(c))
            {
                grew += usize::from(record_copy(&tx, id, copy)?);
            }
            // its copies a read put back in view, on any node: a set every
            // node unions too
            let mut put_back = 0;
            for copy in r["revived"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c.as_str())
                .filter(|c| is_episode_id(c))
            {
                let made_it: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM repair_copies WHERE run = ?1 AND episode_id = ?2)",
                    params![id, copy],
                    |row| row.get(0),
                )?;
                if !made_it
                    || tx.execute(
                        "INSERT OR IGNORE INTO repair_revived (episode_id, revived_at)
                         VALUES (?1, ?2)",
                        params![copy, now_utc()],
                    )? == 0
                {
                    continue;
                }
                put_back += 1;
                // the undone runs that made it settle its record here again
                let runs: Vec<String> = {
                    let mut stmt = tx.prepare_cached(
                        "SELECT c.run FROM repair_copies c JOIN repairs r ON r.id = c.run
                         WHERE c.episode_id = ?1 AND r.undone_at IS NOT NULL",
                    )?;
                    let rows = stmt.query_map([copy], |row| row.get(0))?;
                    rows.collect::<std::result::Result<_, _>>()?
                };
                for run in runs {
                    tx.execute(
                        "DELETE FROM consolidation_state WHERE key = ?1",
                        [state_key(&run)],
                    )?;
                    if !changed.contains(&run) {
                        changed.push(run);
                    }
                }
            }
            let undone: bool = tx.query_row(
                "SELECT undone_at IS NOT NULL FROM repairs WHERE id = ?1",
                [id],
                |row| row.get(0),
            )?;
            if (grew > 0 || put_back > 0) && !newer {
                // passed on to the other nodes
                tx.execute(
                    "UPDATE repairs SET updated_at = updated_at WHERE id = ?1",
                    [id],
                )?;
            }
            // an undone run that made more copies than this node knew of is
            // settled here again
            if grew > 0 && undone {
                tx.execute(
                    "DELETE FROM consolidation_state WHERE key = ?1",
                    [state_key(id)],
                )?;
            }
            if (newer || (grew > 0 && undone)) && !changed.iter().any(|c| c == id) {
                changed.push(id.to_string());
            }
        }
        tx.commit()?;
        Ok(changed)
    }
}

/// An episode id as `ids::episode_id` makes one.
fn is_episode_id(s: &str) -> bool {
    s.len() == 19
        && s.starts_with("ep_")
        && s[3..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Insert or update a peer's run row (the caller decided it is newer).
fn upsert_run(tx: &Connection, r: &Value, id: &str, kind: &str, updated: &str) -> Result<()> {
    tx.execute(
        &format!(
            "INSERT INTO repairs ({RUN_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                 replaced = CASE WHEN excluded.updated_at > repairs.updated_at
                                 THEN excluded.replaced ELSE repairs.replaced END,
                 added = CASE WHEN excluded.updated_at > repairs.updated_at
                              THEN excluded.added ELSE repairs.added END,
                 undone_at = COALESCE(repairs.undone_at, excluded.undone_at),
                 updated_at = MAX(repairs.updated_at, excluded.updated_at)"
        ),
        params![
            id,
            kind,
            r["replaced"].as_i64().unwrap_or(0),
            r["added"].as_i64().unwrap_or(0),
            r["actor"].as_str().unwrap_or("peer"),
            r["node"].as_str(),
            r["created_at"].as_str().unwrap_or(updated),
            r["undone_at"].as_str(),
            updated
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::EntityRef;
    use crate::time::{EventTime, TimeUncertainty};

    const BASE: &str = "invoice-77@bank.example";

    fn mail(source_ref: &str, title: &str, body: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "mbox".into(),
            source_ref: source_ref.into(),
            kind: "email".into(),
            title: title.into(),
            body: body.into(),
            event_time: EventTime::parse_rfc3339(
                "2026-03-16T09:30:00-04:00",
                TimeUncertainty::Exact,
            )
            .unwrap(),
            trust: "imported".into(),
            scope: Some("personal".into()),
            entities: vec![EntityRef {
                entity_type: "Person".into(),
                name: "Ann".into(),
                attributes: vec![("email".into(), "ann@bank.example".into())],
                role: "author".into(),
            }],
            edges: vec![],
        }
    }

    fn encoded() -> EpisodeDraft {
        mail(
            BASE,
            "=?UTF-8?Q?Caf=C3=A9?=",
            "subject: =?UTF-8?Q?Caf=C3=A9?=\nfrom: ann <ann@bank.example>\n\nTWVldCBhdCB0aGUgY2Fmw6k=",
        )
    }

    fn decoded() -> EpisodeDraft {
        mail(
            BASE,
            "Café",
            "subject: Café\nfrom: Ann <ann@bank.example>\n\nMeet at the café",
        )
    }

    fn node(tmp: &tempfile::TempDir, name: &str) -> (Home, Store) {
        let home = Home::at(tmp.path().join(name));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        (home, store)
    }

    fn apply(store: &Store, home: &Home, actor: &str, drafts: Vec<EpisodeDraft>) {
        store
            .apply_batch(
                home,
                &IngestBatch {
                    actor: actor.into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap();
    }

    fn repair_of_base(store: &Store, home: &Home, base: &str) -> RepairApplied {
        let mut draft = decoded();
        draft.source_ref = base.into();
        store
            .repair_apply(
                home,
                &Actor::new("human:owner"),
                "mail",
                &[RepairItem {
                    base_ref: base.into(),
                    draft,
                }],
                &[],
                "adapter:ics",
            )
            .unwrap()
    }

    fn repair(store: &Store, home: &Home) -> RepairApplied {
        repair_of_base(store, home, BASE)
    }

    fn fixed_ref_of(base: &str) -> String {
        let d = decoded();
        replacement_ref(base, &d.title, &d.body)
    }

    fn fixed_ref() -> String {
        fixed_ref_of(BASE)
    }

    fn state(store: &Store, source_ref: &str) -> (Option<String>, String) {
        store
            .lock()
            .query_row(
                "SELECT superseded_by, scope FROM episodes WHERE source = 'mbox' AND source_ref = ?1",
                [source_ref],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    fn stamp(store: &Store, id: &str) -> Option<String> {
        store
            .lock()
            .query_row(
                "SELECT scope_changed_at FROM episodes WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn live(store: &Store) -> Vec<String> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT source_ref FROM episodes WHERE superseded_by IS NULL ORDER BY source_ref",
            )
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<std::result::Result<_, _>>().unwrap()
    }

    /// The episodes one node holds after `after`, applied on another as its
    /// peer's pull (origins beside them).
    fn sync(from: &Store, from_home: &Home, to: &Store, to_home: &Home, after: i64) -> i64 {
        let page = from
            .sync_export_for(from_home, after, 100, Some("to"))
            .unwrap();
        to.apply_batch_from(
            to_home,
            &IngestBatch {
                actor: "peer:from".into(),
                session_id: None,
                episodes: page.episodes,
            },
            &page.origins,
        )
        .unwrap();
        page.next_cursor
    }

    /// Runs one node holds, applied on another and settled there.
    fn sync_runs(from: &Store, to: &Store, to_home: &Home) {
        let page = from.repairs_sync_export("").unwrap();
        to.repairs_sync_apply(&page).unwrap();
        for run in to.repairs_pending_here().unwrap() {
            to.repair_settle_run(to_home, &run).unwrap();
        }
    }

    #[test]
    fn test_ref_shapes() {
        let r = replacement_ref(BASE, "t", "b");
        assert_eq!(replacement_base(&r), Some(BASE));
        assert_eq!(r.len(), BASE.len() + REPAIR_MARK.len() + HASH_LEN);
        // a base that itself holds the mark: the last one counts
        let nested = replacement_ref(&r, "t", "b");
        assert_eq!(replacement_base(&nested), Some(r.as_str()));
        for not_one in [
            BASE,
            "x#repair:",
            "#repair:0123456789abcdef",
            "x#repair:0123456789ABCDEF",
            "x#repair:0123456789abcde",
            "x#repair:0123456789abcdefa",
        ] {
            assert_eq!(replacement_base(not_one), None, "{not_one}");
        }
        assert_eq!(repair_of("mbox", &r, Some(REPAIR_WRITER)), Some(BASE));
        assert_eq!(repair_of("mbox", &r, Some("adapter:mbox")), None);
        assert_eq!(repair_of("mbox", &r, None), None);
        assert_eq!(repair_of("desk", &r, Some(REPAIR_WRITER)), None);
    }

    /// A corrected copy takes the memory's place: the memory stands behind
    /// it, it carries the memory's label (a private one with its stamp), and
    /// its origin names the owner repair and the run.
    #[test]
    fn test_a_corrected_copy_takes_the_place_of_the_memory_it_replaces() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "mini");
        apply(&store, &home, "adapter:mbox", vec![encoded()]);
        let base_id = ids::episode_id("mbox", BASE);
        store
            .set_episode_scopes(
                &Actor::new("human:test"),
                std::slice::from_ref(&base_id),
                "private",
            )
            .unwrap();

        let done = repair(&store, &home);
        let run = done.run.clone().unwrap();
        assert_eq!((done.replaced, done.added, done.existing), (1, 0, 0));
        let fixed = fixed_ref();
        let fixed_id = ids::episode_id("mbox", &fixed);
        assert_eq!(
            state(&store, BASE),
            (Some(fixed_id.clone()), "private".into())
        );
        assert_eq!(state(&store, &fixed), (None, "private".into()));
        assert!(stamp(&store, &base_id).is_some());
        assert_eq!(
            stamp(&store, &fixed_id),
            stamp(&store, &base_id),
            "the record's label, with its stamp"
        );
        let origin = store.episode_origin(&fixed_id).unwrap().unwrap();
        assert_eq!(origin.writer.as_deref(), Some(REPAIR_WRITER));
        assert_eq!(origin.session.as_deref(), Some(run.as_str()));
        let row = store.repair_run(&run).unwrap().unwrap();
        assert_eq!((row.kind.as_str(), row.replaced), ("mail", 1));
        // the corrected copy names the original, never another copy
        let refused = store.repair_apply(
            &home,
            &Actor::new("human:owner"),
            "mail",
            &[RepairItem {
                base_ref: fixed.clone(),
                draft: decoded(),
            }],
            &[],
            "adapter:ics",
        );
        assert!(refused.is_err());

        // a private label given to a corrected copy reaches the memory
        // behind it, where lookups by Message-ID find it
        let mut plain = encoded();
        plain.source_ref = "plain@x".into();
        apply(&store, &home, "adapter:mbox", vec![plain]);
        assert_eq!(repair_of_base(&store, &home, "plain@x").replaced, 1);
        let plain_fixed = ids::episode_id("mbox", &fixed_ref_of("plain@x"));
        assert!(store
            .private_source_refs("mbox", &["plain@x".to_string()])
            .unwrap()
            .is_empty());
        store
            .set_episode_scopes(&Actor::new("human:test"), &[plain_fixed], "private")
            .unwrap();
        assert!(store
            .private_source_refs("mbox", &["plain@x".to_string()])
            .unwrap()
            .contains("plain@x"));
    }

    /// Taking a repaired memory's corrected copy out of the vault (vault
    /// open) takes the whole record out: the memory behind it no longer
    /// reads as private by its Message-ID, and settling the run again
    /// doesn't put the label back. With the vault locked nothing moves.
    #[test]
    fn test_taking_a_correction_out_of_the_vault_takes_its_record_out() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "mini");
        let actor = Actor::new("human:test");
        apply(&store, &home, "adapter:mbox", vec![encoded()]);
        // private with no stamp, as an earlier version could leave it
        store
            .lock()
            .execute(
                "UPDATE episodes SET scope = 'private', scope_changed_at = NULL",
                [],
            )
            .unwrap();
        let run = repair(&store, &home).run.unwrap();
        let fixed = fixed_ref();
        let fixed_id = ids::episode_id("mbox", &fixed);
        assert_eq!(state(&store, &fixed).1, "private");
        let private_now = |store: &Store| {
            store
                .private_source_refs("mbox", &[BASE.to_string()])
                .unwrap()
                .contains(BASE)
        };
        assert!(private_now(&store));

        // locked: the label stays
        store
            .set_episode_scopes(&actor, std::slice::from_ref(&fixed_id), "personal")
            .unwrap();
        assert_eq!(state(&store, &fixed).1, "private");
        assert!(private_now(&store));

        store.vault_unlock(5);
        store
            .set_episode_scopes(&actor, std::slice::from_ref(&fixed_id), "personal")
            .unwrap();
        assert_eq!(state(&store, &fixed).1, "personal");
        assert_eq!(state(&store, BASE).1, "personal");
        assert_eq!(
            stamp(&store, &ids::episode_id("mbox", BASE)),
            stamp(&store, &fixed_id)
        );
        assert!(!private_now(&store));
        store.repair_settle_run(&home, &run).unwrap();
        assert_eq!(state(&store, &fixed).1, "personal");
        assert!(!private_now(&store));
    }

    /// A node that holds only a corrected copy (the original never reached
    /// it) can correct the record again: the new copy stands, the earlier
    /// one behind it, with the record's label and trust — the run isn't
    /// refused for want of the original.
    #[test]
    fn test_a_record_whose_original_is_absent_here_can_be_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (late_home, late) = node(&tmp, "late");
        apply(&mini, &mini_home, "adapter:mbox", vec![encoded()]);
        repair(&mini, &mini_home);
        let fixed = fixed_ref();
        let page = mini
            .sync_export_for(&mini_home, 0, 100, Some("late"))
            .unwrap();
        let only_fixed: Vec<EpisodeDraft> = page
            .episodes
            .into_iter()
            .filter(|d| d.source_ref == fixed)
            .collect();
        late.apply_batch_from(
            &late_home,
            &IngestBatch {
                actor: "peer:mini".into(),
                session_id: None,
                episodes: only_fixed,
            },
            &page.origins,
        )
        .unwrap();
        assert_eq!(live(&late), std::slice::from_ref(&fixed));

        let mut again = decoded();
        again.body.push_str(" (read again)");
        let second = replacement_ref(BASE, &again.title, &again.body);
        let done = late
            .repair_apply(
                &late_home,
                &Actor::new("human:owner"),
                "mail",
                &[RepairItem {
                    base_ref: BASE.into(),
                    draft: again,
                }],
                &[],
                "adapter:ics",
            )
            .unwrap();
        assert_eq!(done.replaced, 1);
        assert_eq!(live(&late), std::slice::from_ref(&second));
        assert_eq!(
            state(&late, &fixed).0,
            Some(ids::episode_id("mbox", &second))
        );
        let trust: String = late
            .lock()
            .query_row(
                "SELECT trust FROM episodes WHERE source_ref = ?1",
                [&second],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(trust, "imported");
    }

    /// Asking again for the same correction changes nothing: no second
    /// copy, no run, no state moved.
    #[test]
    fn test_repairing_twice_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "mini");
        apply(&store, &home, "adapter:mbox", vec![encoded()]);
        assert!(repair(&store, &home).run.is_some());
        let snapshot = |store: &Store| -> (i64, i64, i64, Vec<String>) {
            let counts = {
                let conn = store.lock();
                let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
                (
                    count("SELECT COUNT(*) FROM episodes"),
                    count("SELECT COUNT(*) FROM repairs"),
                    count("SELECT COUNT(*) FROM mutations"),
                )
            };
            (counts.0, counts.1, counts.2, live(store))
        };
        let before = snapshot(&store);
        let again = repair(&store, &home);
        assert!(again.run.is_none());
        assert_eq!((again.replaced, again.existing), (0, 1));
        assert_eq!(snapshot(&store), before);
    }

    /// A peer that receives the corrected copy puts its own copy of the
    /// memory behind it — whichever arrives first — and the record's label
    /// there (private) holds for the copy that arrives.
    #[test]
    fn test_a_peer_puts_its_copy_behind_the_correction_in_any_order() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (cloud_home, cloud) = node(&tmp, "cloud");
        apply(&mini, &mini_home, "adapter:mbox", vec![encoded()]);
        let cursor = sync(&mini, &mini_home, &cloud, &cloud_home, 0);
        // the owner vaulted it on the peer before the correction arrived
        cloud
            .set_episode_scopes(
                &Actor::new("human:test"),
                &[ids::episode_id("mbox", BASE)],
                "private",
            )
            .unwrap();
        repair(&mini, &mini_home);
        sync(&mini, &mini_home, &cloud, &cloud_home, cursor);
        let fixed = fixed_ref();
        let fixed_id = ids::episode_id("mbox", &fixed);
        assert_eq!(
            state(&cloud, BASE),
            (Some(fixed_id.clone()), "private".into())
        );
        assert_eq!(state(&cloud, &fixed), (None, "private".into()));

        // a third node that gets the correction before the memory
        let (late_home, late) = node(&tmp, "late");
        let page = mini
            .sync_export_for(&mini_home, 0, 100, Some("late"))
            .unwrap();
        let (first, second): (Vec<_>, Vec<_>) = page
            .episodes
            .into_iter()
            .partition(|d| d.source_ref == fixed);
        assert_eq!((first.len(), second.len()), (1, 1));
        for drafts in [first, second] {
            late.apply_batch_from(
                &late_home,
                &IngestBatch {
                    actor: "peer:mini".into(),
                    session_id: None,
                    episodes: drafts,
                },
                &page.origins,
            )
            .unwrap();
        }
        assert_eq!(state(&late, BASE).0, Some(fixed_id));
        assert_eq!(live(&late), [fixed]);
    }

    /// Only a copy the owner repair wrote replaces anything: a look-alike
    /// ref from an importer, another key, an ember, a peer's other writer
    /// or a peer that doesn't say supersedes nothing, and no batch but the
    /// repair's may claim its writer.
    #[test]
    fn test_a_look_alike_ref_replaces_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "mini");
        apply(&store, &home, "adapter:mbox", vec![encoded()]);
        let look_alike = |n: u8| {
            let mut d = decoded();
            d.body.push_str(&format!(" {n}"));
            d.source_ref = replacement_ref(BASE, &d.title, &d.body);
            d
        };
        apply(&store, &home, "adapter:mbox", vec![look_alike(1)]);
        apply(&store, &home, "agent:key:helper", vec![look_alike(2)]);
        apply(&store, &home, "plugin:minidash", vec![look_alike(3)]);
        let peer = |d: EpisodeDraft, writer: Option<&str>| {
            let origins: Vec<crate::store::Origin> = writer
                .map(|w| crate::store::Origin {
                    source: d.source.clone(),
                    source_ref: d.source_ref.clone(),
                    writer: Some(w.into()),
                    ..Default::default()
                })
                .into_iter()
                .collect();
            store
                .apply_batch_from(
                    &home,
                    &IngestBatch {
                        actor: "peer:laptop".into(),
                        session_id: None,
                        episodes: vec![d],
                    },
                    &origins,
                )
                .unwrap();
        };
        peer(look_alike(4), Some("adapter:mbox"));
        peer(look_alike(5), None);
        // the repair's writer, for a source no repair reaches
        let mut desk = look_alike(6);
        desk.source = "desk".into();
        peer(desk, Some(REPAIR_WRITER));
        assert_eq!(state(&store, BASE).0, None, "the memory stays in view");
        assert_eq!(live(&store).len(), 7);

        for actor in [REPAIR_WRITER, "repair:someone"] {
            let refused = store.apply_batch(
                &home,
                &IngestBatch {
                    actor: actor.into(),
                    session_id: None,
                    episodes: vec![look_alike(7)],
                },
            );
            assert!(refused.is_err(), "{actor}");
        }
        assert_eq!(state(&store, BASE).0, None);
        assert_eq!(live(&store).len(), 7);
    }

    /// Undoing a run brings the memory back in view on this node and, once
    /// the run's row reaches them, on its peers — a copy that arrives after
    /// the undo lands behind the memory, and asking for the same correction
    /// again leaves it undone.
    #[test]
    fn test_undo_brings_the_memory_back_on_every_node() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (cloud_home, cloud) = node(&tmp, "cloud");
        apply(&mini, &mini_home, "adapter:mbox", vec![encoded()]);
        let run = repair(&mini, &mini_home).run.unwrap();
        sync(&mini, &mini_home, &cloud, &cloud_home, 0);
        sync_runs(&mini, &cloud, &cloud_home);
        let fixed = fixed_ref();
        let base_id = ids::episode_id("mbox", BASE);
        assert_eq!(live(&cloud), std::slice::from_ref(&fixed));
        assert!(cloud.repairs_pending_here().unwrap().is_empty());

        assert_eq!(
            mini.repair_undo(&mini_home, &Actor::new("human:owner"), &run)
                .unwrap(),
            2
        );
        assert_eq!(live(&mini), [BASE]);
        assert_eq!(state(&mini, &fixed).0, Some(base_id.clone()));
        assert!(mini
            .repair_undo(&mini_home, &Actor::new("human:owner"), &run)
            .is_err());

        sync_runs(&mini, &cloud, &cloud_home);
        assert_eq!(live(&cloud), [BASE]);
        assert_eq!(state(&cloud, &fixed).0, Some(base_id.clone()));
        assert!(cloud.repairs_pending_here().unwrap().is_empty());

        // a node that gets the run's row, then the episodes
        let (late_home, late) = node(&tmp, "late");
        sync_runs(&mini, &late, &late_home);
        sync(&mini, &mini_home, &late, &late_home, 0);
        assert_eq!(live(&late), [BASE]);

        // the same correction again: made already, so nothing changes
        let again = repair(&mini, &mini_home);
        assert!(again.run.is_none());
        assert_eq!(live(&mini), [BASE]);
    }

    /// The corrected copy only (its original never reached this node): an
    /// undo hides it, whether the undone run arrives after it or before,
    /// and the original stands once it arrives.
    #[test]
    fn test_an_undone_correction_stays_hidden_where_its_original_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        apply(&mini, &mini_home, "adapter:mbox", vec![encoded()]);
        let run = repair(&mini, &mini_home).run.unwrap();
        let fixed = fixed_ref();
        let base_id = ids::episode_id("mbox", BASE);
        let only_fixed = |to: &Store, to_home: &Home| {
            let page = mini
                .sync_export_for(&mini_home, 0, 100, Some("to"))
                .unwrap();
            let drafts: Vec<EpisodeDraft> = page
                .episodes
                .into_iter()
                .filter(|d| d.source_ref == fixed)
                .collect();
            to.apply_batch_from(
                to_home,
                &IngestBatch {
                    actor: "peer:mini".into(),
                    session_id: None,
                    episodes: drafts,
                },
                &page.origins,
            )
            .unwrap();
        };

        // the copy, then the undo
        let (late_home, late) = node(&tmp, "late");
        only_fixed(&late, &late_home);
        sync_runs(&mini, &late, &late_home);
        assert_eq!(live(&late), std::slice::from_ref(&fixed));
        mini.repair_undo(&mini_home, &Actor::new("human:owner"), &run)
            .unwrap();
        sync_runs(&mini, &late, &late_home);
        assert!(live(&late).is_empty(), "{:?}", live(&late));
        assert_eq!(state(&late, &fixed).0, Some(base_id.clone()));

        // the undo, then the copy
        let (later_home, later) = node(&tmp, "later");
        sync_runs(&mini, &later, &later_home);
        only_fixed(&later, &later_home);
        assert!(live(&later).is_empty(), "{:?}", live(&later));

        // the original arrives: it stands, the correction behind it
        for (home, store) in [(&late_home, &late), (&later_home, &later)] {
            sync(&mini, &mini_home, store, home, 0);
            assert_eq!(live(store), [BASE]);
            assert_eq!(state(store, &fixed).0, Some(base_id.clone()));
        }
    }

    /// Two nodes that make the same correction before they sync hold one
    /// copy (its id is its content's) whose origin names a different run on
    /// each: undoing one of the runs brings the memory back on both, and on
    /// a third node that got the copy from the node whose run still stands.
    #[test]
    fn test_an_undo_reaches_the_same_correction_made_on_another_node() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (cloud_home, cloud) = node(&tmp, "cloud");
        apply(&mini, &mini_home, "adapter:mbox", vec![encoded()]);
        sync(&mini, &mini_home, &cloud, &cloud_home, 0);
        let mini_run = repair(&mini, &mini_home).run.unwrap();
        let cloud_run = repair(&cloud, &cloud_home).run.unwrap();
        assert_ne!(mini_run, cloud_run);
        let fixed = fixed_ref();
        let fixed_id = ids::episode_id("mbox", &fixed);
        let session = |store: &Store| store.episode_origin(&fixed_id).unwrap().unwrap().session;
        assert_eq!(session(&mini).as_deref(), Some(mini_run.as_str()));
        assert_eq!(session(&cloud).as_deref(), Some(cloud_run.as_str()));

        sync(&mini, &mini_home, &cloud, &cloud_home, 0);
        sync(&cloud, &cloud_home, &mini, &mini_home, 0);
        sync_runs(&mini, &cloud, &cloud_home);
        sync_runs(&cloud, &mini, &mini_home);
        assert_eq!(live(&mini), std::slice::from_ref(&fixed));
        assert_eq!(live(&cloud), std::slice::from_ref(&fixed));

        mini.repair_undo(&mini_home, &Actor::new("human:owner"), &mini_run)
            .unwrap();
        assert_eq!(live(&mini), [BASE]);
        sync_runs(&mini, &cloud, &cloud_home);
        assert_eq!(live(&cloud), [BASE]);
        assert_eq!(state(&cloud, &fixed).0, Some(ids::episode_id("mbox", BASE)));

        // the copy from the cloud (its origin names the cloud's run), the
        // runs from the mini
        let (late_home, late) = node(&tmp, "late");
        sync(&cloud, &cloud_home, &late, &late_home, 0);
        assert_eq!(session(&late).as_deref(), Some(cloud_run.as_str()));
        sync_runs(&mini, &late, &late_home);
        assert_eq!(live(&late), [BASE]);
        assert!(late.repairs_pending_here().unwrap().is_empty());

        // nothing more to exchange: applying the pages again changes nothing
        assert!(cloud
            .repairs_sync_apply(&mini.repairs_sync_export("").unwrap())
            .unwrap()
            .is_empty());
    }

    /// A stored event found by its UID and start under another ref is one of
    /// that UID only when the ref reads so and its Event entity holds that
    /// UID: `meeting#other`, and an event whose own UID is
    /// `meeting#<start>`, are other events; a copy whose entity holds both
    /// UIDs can't tell.
    #[test]
    fn test_an_event_under_another_ref_is_matched_by_its_own_uid() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "mini");
        const AT: &str = "2026-03-16T14:00:00Z";
        let event = |source_ref: &str, uid: &str, title: &str, at: &str| EpisodeDraft {
            source: "ics".into(),
            source_ref: source_ref.into(),
            kind: "calendar_event".into(),
            title: title.into(),
            body: format!("event: {title}\nstart: {at}\n"),
            event_time: EventTime::parse_rfc3339(at, TimeUncertainty::Exact).unwrap(),
            trust: "imported".into(),
            scope: None,
            entities: vec![EntityRef {
                entity_type: "Event".into(),
                name: title.into(),
                attributes: vec![("ics-uid".into(), uid.into())],
                role: "subject".into(),
            }],
            edges: vec![],
        };
        apply(
            &store,
            &home,
            "adapter:ics",
            vec![
                event("meeting#other", "meeting#other", "Budget", AT),
                event(
                    "meeting#20260316T140000Z",
                    "meeting#20260316T140000Z",
                    "Offsite",
                    AT,
                ),
                // an override an earlier importer stored under its
                // RECURRENCE-ID as written
                event("r-1#20260316T140000Z", "r-1", "Standup", AT),
                event("solo", "solo", "Walk", AT),
                // two events of one name share an entity, which holds both
                event("team#20260316T140000Z", "team#20260316T140000Z", "Sync", AT),
                event("team", "team", "Sync", "2026-03-09T14:00:00Z"),
            ],
        );
        let found = |uid: &str| store.repair_same_event("ics", uid, AT).unwrap();
        assert_eq!(found("meeting"), SameEvent::Absent);
        assert_eq!(
            found("r-1"),
            SameEvent::Stored("r-1#20260316T140000Z".into())
        );
        assert_eq!(found("solo"), SameEvent::Stored("solo".into()));
        assert_eq!(
            found("team"),
            SameEvent::Unsure(ids::episode_id("ics", "team#20260316T140000Z"))
        );
        assert_eq!(found("nothing"), SameEvent::Absent);
    }

    /// A node that stored the corrected copy before it knew the rule (both
    /// copies in view, the old one private there and the new one not)
    /// settles the record when the run reaches it: the old copy behind the
    /// new, and the private label on both.
    #[test]
    fn test_a_node_that_held_a_correction_unsettled_settles_it_when_the_run_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (old_home, old) = node(&tmp, "old");
        apply(&mini, &mini_home, "adapter:mbox", vec![encoded()]);
        repair(&mini, &mini_home);
        sync(&mini, &mini_home, &old, &old_home, 0);
        let fixed = fixed_ref();
        // what a node too old for the rule holds: both in view, labels apart
        old.lock()
            .execute(
                "UPDATE episodes SET superseded_by = NULL, scope = 'personal', scope_changed_at = NULL",
                [],
            )
            .unwrap();
        old.lock()
            .execute(
                "UPDATE episodes SET scope = 'private', scope_changed_at = '2026-11-01T00:00:00Z'
                 WHERE source_ref = ?1",
                [BASE],
            )
            .unwrap();
        assert_eq!(live(&old).len(), 2);
        assert_eq!(state(&old, &fixed).1, "personal");

        sync_runs(&mini, &old, &old_home);
        assert_eq!(live(&old), std::slice::from_ref(&fixed));
        assert_eq!(state(&old, &fixed).1, "private");
        assert!(old.repairs_pending_here().unwrap().is_empty());
    }

    /// A node that stored a retired memory's correction before it knew the
    /// rule (the correction in view, the memory retired) retires the
    /// correction when the run reaches it; undoing the retirement then shows
    /// the correction, the memory behind it.
    #[test]
    fn test_a_correction_held_unsettled_beside_a_retired_memory_is_retired_when_the_run_arrives() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "old");
        let actor = Actor::new("human:test");
        apply(&store, &home, "adapter:mbox", vec![encoded()]);
        let base_id = ids::episode_id("mbox", BASE);
        let r = store.retire(&actor, "memory", &base_id, &[], "").unwrap();
        store.retirement_apply(&r.id).unwrap();
        let run = repair(&store, &home).run.unwrap();
        let fixed = fixed_ref();
        let fixed_id = ids::episode_id("mbox", &fixed);
        // what a node too old for the rule holds: the correction landed as
        // an ordinary episode in view, the memory retired from in view, and
        // the run not settled here yet
        {
            let conn = store.lock();
            conn.execute(
                "UPDATE episodes SET superseded_by = NULL WHERE id = ?1",
                [&fixed_id],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM retirement_rows WHERE tbl = 'episodes' AND row_key = ?1",
                [&fixed_id],
            )
            .unwrap();
            conn.execute(
                "UPDATE retirement_rows SET prior = '{\"superseded_by\":null}'
                 WHERE tbl = 'episodes' AND row_key = ?1",
                [&base_id],
            )
            .unwrap();
            conn.execute(
                "DELETE FROM consolidation_state WHERE key = ?1",
                [state_key(&run)],
            )
            .unwrap();
        }
        assert_eq!(live(&store), std::slice::from_ref(&fixed));

        for pending in store.repairs_pending_here().unwrap() {
            store.repair_settle_run(&home, &pending).unwrap();
        }
        let mark = Some(format!("{RETIRED_PREFIX}{}", r.id));
        assert!(
            live(&store).is_empty(),
            "the retired record stays out of view"
        );
        assert_eq!(state(&store, &fixed).0, mark);
        assert_eq!(state(&store, BASE).0, mark);

        store.retirement_undo(&actor, &r.id).unwrap();
        assert_eq!(live(&store), [fixed]);
        assert_eq!(state(&store, BASE).0, Some(fixed_id));
    }

    /// A retired memory's correction arrives retired; undoing the
    /// retirement shows the correction, not the memory it replaced.
    /// Retiring the correction retires the record, and its undo restores it.
    #[test]
    fn test_a_retired_memory_stays_retired_through_its_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "mini");
        let actor = Actor::new("human:test");
        apply(&store, &home, "adapter:mbox", vec![encoded()]);
        let base_id = ids::episode_id("mbox", BASE);
        let r = store.retire(&actor, "memory", &base_id, &[], "").unwrap();
        store.retirement_apply(&r.id).unwrap();
        repair(&store, &home);
        let fixed = fixed_ref();
        let fixed_id = ids::episode_id("mbox", &fixed);
        let mark = Some(format!("{RETIRED_PREFIX}{}", r.id));
        assert_eq!(state(&store, BASE).0, mark);
        assert_eq!(state(&store, &fixed).0, mark);
        assert!(live(&store).is_empty());
        store.retirement_undo(&actor, &r.id).unwrap();
        assert_eq!(live(&store), std::slice::from_ref(&fixed));
        assert_eq!(state(&store, BASE).0, Some(fixed_id.clone()));

        let r2 = store.retire(&actor, "memory", &fixed_id, &[], "").unwrap();
        store.retirement_apply(&r2.id).unwrap();
        assert!(live(&store).is_empty());
        store.retirement_undo(&actor, &r2.id).unwrap();
        assert_eq!(live(&store), [fixed]);
        assert_eq!(state(&store, BASE).0, Some(fixed_id));
    }

    const DAY: &str = "+15551234567#2026-03-16";

    /// A chat-day as an earlier version stored it: under its unversioned
    /// base, holding the messages its node had read.
    fn chat_day(messages: &[&str]) -> EpisodeDraft {
        let mut body = format!(
            "messages: {}\nchat: +15551234567\ndate: 2026-03-16\n\n",
            messages.len()
        );
        for (i, m) in messages.iter().enumerate() {
            body.push_str(&format!("[10:0{i}] +15551234567: {m}\n"));
        }
        EpisodeDraft {
            source: "imessage".into(),
            source_ref: DAY.into(),
            kind: "message_day".into(),
            title: "Messages with +15551234567".into(),
            body,
            event_time: EventTime::parse_rfc3339("2026-03-16T14:00:00Z", TimeUncertainty::Day)
                .unwrap(),
            trust: "self".into(),
            scope: None,
            entities: vec![EntityRef {
                entity_type: "Person".into(),
                name: "Sam".into(),
                attributes: vec![("phone".into(), "+15551234567".into())],
                role: "participant".into(),
            }],
            edges: vec![],
        }
    }

    /// (ref, text) of the chat-day copies in view.
    fn chat_days_live(store: &Store, home: &Home) -> Vec<(String, String)> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT source_ref, content_ref FROM episodes
                 WHERE source = 'imessage' AND superseded_by IS NULL ORDER BY source_ref",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap();
        rows.map(|r| {
            let (source_ref, content) = r.unwrap();
            (
                source_ref,
                std::fs::read_to_string(home.root().join(content)).unwrap(),
            )
        })
        .collect()
    }

    fn copies_run(store: &Store, home: &Home) -> RepairApplied {
        let drafts: Vec<EpisodeDraft> = store
            .repair_copies_scan(home)
            .unwrap()
            .into_iter()
            .filter_map(|f| f.draft)
            .collect();
        store
            .repair_apply_work(
                home,
                &Actor::new("human:owner"),
                COPIES_KIND,
                RepairWork {
                    revisions: &drafts,
                    ..Default::default()
                },
            )
            .unwrap()
    }

    /// Two nodes that each stored a chat-day under its unversioned base —
    /// one with two messages, the other with three — keep their own texts
    /// whatever they sync. After `repair copies` on one of them (the run
    /// reaching the other, which re-mints its own copy) and a sync both
    /// ways, both show the fuller copy, with its links. Running it again
    /// changes nothing; the undo, on either node, brings each node's own
    /// copy back, and a node that hears of the undo first keeps the copies
    /// arriving later behind the memory.
    #[test]
    fn test_two_nodes_with_different_unversioned_copies_converge_after_repair_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (laptop_home, laptop) = node(&tmp, "laptop");
        apply(
            &mini,
            &mini_home,
            "adapter:imessage",
            vec![chat_day(&["lunch?", "noon"])],
        );
        apply(
            &laptop,
            &laptop_home,
            "adapter:imessage",
            vec![chat_day(&["lunch?", "noon", "see you there"])],
        );
        let both_ways = || {
            sync(&mini, &mini_home, &laptop, &laptop_home, 0);
            sync(&laptop, &laptop_home, &mini, &mini_home, 0);
        };
        both_ways();
        let texts = |store: &Store, home: &Home| chat_days_live(store, home);
        assert_ne!(texts(&mini, &mini_home), texts(&laptop, &laptop_home));

        // the dry run's finding: one copy to re-mint, named by its count
        let found = mini.repair_copies_scan(&mini_home).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].status, found[0].reason.as_str()),
            ("remint", "2 messages, stored under an unversioned ref")
        );
        assert_eq!(found[0].episode, ids::episode_id("imessage", DAY));

        let run = copies_run(&mini, &mini_home).run.expect("a run");
        sync_runs(&mini, &laptop, &laptop_home);
        both_ways();
        for (store, home) in [(&mini, &mini_home), (&laptop, &laptop_home)] {
            let live = texts(store, home);
            assert_eq!(live.len(), 1, "{live:?}");
            assert_eq!(live[0].0, format!("{DAY}#r3"));
            assert!(live[0].1.contains("see you there"));
            // the fuller copy keeps the day's links
            let linked: i64 = store
                .lock()
                .query_row(
                    "SELECT COUNT(*) FROM episode_entities WHERE episode_id = ?1",
                    [ids::episode_id("imessage", &format!("{DAY}#r3"))],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(linked > 0);
        }
        // nothing left to re-mint on either node
        for (store, home) in [(&mini, &mini_home), (&laptop, &laptop_home)] {
            assert!(copies_run(store, home).run.is_none());
        }

        // the undo: each node's own copy back in view
        let mini_text = chat_day(&["lunch?", "noon"]).body;
        let laptop_text = chat_day(&["lunch?", "noon", "see you there"]).body;
        mini.repair_undo(&mini_home, &Actor::new("human:owner"), &run)
            .unwrap();
        sync_runs(&mini, &laptop, &laptop_home);
        assert_eq!(
            texts(&mini, &mini_home),
            [(DAY.to_string(), mini_text.clone())]
        );
        assert_eq!(
            texts(&laptop, &laptop_home),
            [(DAY.to_string(), laptop_text.clone())]
        );
        // and nothing re-minted again
        for (store, home) in [(&mini, &mini_home), (&laptop, &laptop_home)] {
            let found = store.repair_copies_scan(home).unwrap();
            assert_eq!(found[0].status, "undone");
            assert!(copies_run(store, home).run.is_none());
        }
        // a node that hears of the undo before the copies arrive
        let (late_home, late) = node(&tmp, "late");
        sync_runs(&mini, &late, &late_home);
        sync(&mini, &mini_home, &late, &late_home, 0);
        assert_eq!(
            texts(&late, &late_home),
            [(DAY.to_string(), mini_text.clone())]
        );
    }

    /// After the undo of a `copies` run, a read of the owner's files that
    /// names a copy the run re-minted (the read holds as many messages)
    /// puts it back in view on the node that read them — and, carried with
    /// the run, on every other node, which can't read those files: a node
    /// that shows the two-message copy shows the three-message one too. A
    /// peer's batch naming the copy alone puts nothing back.
    #[test]
    fn test_a_read_that_puts_an_undone_copy_back_reaches_every_node() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (laptop_home, laptop) = node(&tmp, "laptop");
        let (hub_home, hub) = node(&tmp, "hub");
        let short = chat_day(&["lunch?", "noon"]);
        let long = chat_day(&["lunch?", "noon", "see you there"]);
        apply(&mini, &mini_home, "adapter:imessage", vec![short.clone()]);
        apply(
            &laptop,
            &laptop_home,
            "adapter:imessage",
            vec![long.clone()],
        );
        let run = copies_run(&mini, &mini_home).run.expect("a run");
        sync_runs(&mini, &laptop, &laptop_home);
        sync(&mini, &mini_home, &hub, &hub_home, 0);
        sync(&laptop, &laptop_home, &hub, &hub_home, 0);
        sync(&laptop, &laptop_home, &mini, &mini_home, 0);
        sync_runs(&mini, &hub, &hub_home);
        mini.repair_undo(&mini_home, &Actor::new("human:owner"), &run)
            .unwrap();
        sync_runs(&mini, &hub, &hub_home);
        // the hub holds mini's two-message copy, and the undone copies
        let texts = |store: &Store, home: &Home| chat_days_live(store, home);
        assert_eq!(
            texts(&hub, &hub_home),
            [(DAY.to_string(), short.body.clone())]
        );
        assert_eq!(texts(&mini, &mini_home), [(DAY.to_string(), short.body)]);

        // the undone copy doesn't count as the fullest for a read
        let fullest = mini
            .growing_latest(&mini_home, "imessage", DAY)
            .unwrap()
            .unwrap();
        assert_eq!((fullest.source_ref.as_str(), fullest.items), (DAY, 2));

        // a peer naming the copy changes nothing
        let mut read = long.clone();
        read.source_ref = format!("{DAY}#r3");
        apply(&hub, &hub_home, "peer:mini", vec![read.clone()]);
        assert_eq!(texts(&hub, &hub_home)[0].0, DAY);

        // mini reads three messages: the read's ref is the undone copy's
        apply(&mini, &mini_home, "adapter:imessage", vec![read]);
        assert_eq!(texts(&mini, &mini_home)[0].0, format!("{DAY}#r3"));
        // the run carries it to the hub, which puts the copy back too
        sync_runs(&mini, &hub, &hub_home);
        assert_eq!(
            texts(&hub, &hub_home),
            [(format!("{DAY}#r3"), long.body.clone())]
        );
        // and a node that gets the copy afterwards lands it in view
        let (late_home, late) = node(&tmp, "late");
        sync_runs(&hub, &late, &late_home);
        sync(&hub, &hub_home, &late, &late_home, 0);
        assert_eq!(texts(&late, &late_home), [(format!("{DAY}#r3"), long.body)]);
    }

    /// A node holding an undone copy without the memory it was made from
    /// (the copy came first, and waits behind the memory's id) puts it back
    /// in view when a read on another node does, and the memory arriving
    /// afterwards stands behind it.
    #[test]
    fn test_a_copy_put_back_before_its_memory_arrives_stays_in_view() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        let (laptop_home, laptop) = node(&tmp, "laptop");
        let short = chat_day(&["lunch?", "noon"]);
        let long = chat_day(&["lunch?", "noon", "see you there"]);
        apply(&mini, &mini_home, "adapter:imessage", vec![short.clone()]);
        apply(
            &laptop,
            &laptop_home,
            "adapter:imessage",
            vec![long.clone()],
        );
        let run = copies_run(&mini, &mini_home).run.expect("a run");
        sync_runs(&mini, &laptop, &laptop_home);
        sync(&laptop, &laptop_home, &mini, &mini_home, 0);
        mini.repair_undo(&mini_home, &Actor::new("human:owner"), &run)
            .unwrap();
        let three = format!("{DAY}#r3");

        // the undone three-message copy reaches a node before any memory of
        // the day
        let (late_home, late) = node(&tmp, "late");
        sync_runs(&mini, &late, &late_home);
        send(&mini, &mini_home, &late, &late_home, &[&three]);
        assert!(chat_days_live(&late, &late_home).is_empty());

        // mini reads three messages, and the read reaches that node
        let mut read = long.clone();
        read.source_ref = three.clone();
        apply(&mini, &mini_home, "adapter:imessage", vec![read]);
        sync_runs(&mini, &late, &late_home);
        assert_eq!(
            chat_days_live(&late, &late_home),
            [(three.clone(), long.body.clone())]
        );

        // the two-message memory arrives last
        send(&mini, &mini_home, &late, &late_home, &[DAY]);
        assert_eq!(chat_days_live(&late, &late_home), [(three, long.body)]);
    }

    const PHOTO_DAY: &str = "photos#2026-07-20";
    const PHOTO_FIX: &str = "photos#2026-07-20#r2";
    /// 2026-07-20T00:00:00Z.
    const JULY_20: i64 = 1_784_505_600;
    /// A day minted while each photo was stored twice: the phone's key and
    /// the one an earlier Mac scan gave it (the library's file name).
    const DOUBLED: &[(&str, &str)] = &[
        ("10:00", "IMG_1"),
        ("10:00", "UUID-1"),
        ("11:00", "IMG_2"),
        ("11:00", "UUID-2"),
    ];
    const ONCE: &[(&str, &str)] = &[("10:00", "IMG_1"), ("11:00", "IMG_2")];

    /// A photo day as the finalize pass writes it, showing each (`HH:MM`,
    /// file).
    fn photo_day(source_ref: &str, photos: &[(&str, &str)]) -> EpisodeDraft {
        let mut body = format!("date: 2026-07-20\nphotos: {}\n\n", photos.len());
        for (at, p) in photos {
            body.push_str(&format!("[{at}] {p} (42.0000,-71.0000)\n"));
        }
        EpisodeDraft {
            source: "photos".into(),
            source_ref: source_ref.into(),
            kind: "photos".into(),
            title: format!("{} photos", photos.len()),
            body,
            event_time: EventTime::parse_rfc3339("2026-07-20T10:00:00Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: "self".into(),
            scope: None,
            entities: vec![],
            edges: vec![],
        }
    }

    /// A photo of 2026-07-20 at `hour`:00 UTC as a device reports it, with
    /// the key an earlier Mac scan gave it.
    fn photo_point(hour: i64, name: &str, earlier: Option<&str>) -> crate::store::PhotoPointIn {
        let at = JULY_20 + hour * 3600;
        crate::store::PhotoPointIn {
            key: format!("{at}|{name}"),
            filename: name.into(),
            taken_at: chrono::DateTime::from_timestamp(at, 0)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
            tz_offset_minutes: 0,
            lat: Some(42.0),
            lon: Some(-71.0),
            place: None,
            earlier_key: earlier.map(|e| format!("{at}|{e}")),
        }
    }

    /// The day's photos stored once each, the earlier scan's keys taken
    /// over (what `DOUBLED`'s node holds now).
    fn photos_once(store: &Store) {
        store
            .photos_add_points(
                "iPhone",
                &[
                    photo_point(10, "IMG_1", None),
                    photo_point(11, "IMG_2", None),
                ],
            )
            .unwrap();
        store
            .photos_add_points(
                "mac",
                &[
                    photo_point(10, "IMG_1", Some("UUID-1")),
                    photo_point(11, "IMG_2", Some("UUID-2")),
                ],
            )
            .unwrap();
    }

    /// One node's photo points, applied on another (the photos leg).
    fn sync_points(from: &Store, to: &Store, to_home: &Home) {
        let page = from.photo_points_export(0, 500).unwrap();
        to.photo_points_apply_and_settle(to_home, &page).unwrap();
    }

    /// The episodes `refs` of one node, applied on another one at a time in
    /// that order, as its peer's pull.
    fn send(from: &Store, from_home: &Home, to: &Store, to_home: &Home, refs: &[&str]) {
        let page = from.sync_export_for(from_home, 0, 100, Some("to")).unwrap();
        for r in refs {
            let draft = page
                .episodes
                .iter()
                .find(|d| d.source_ref == *r)
                .cloned()
                .unwrap_or_else(|| panic!("{r} not stored"));
            to.apply_batch_from(
                to_home,
                &IngestBatch {
                    actor: "peer:from".into(),
                    session_id: None,
                    episodes: vec![draft],
                },
                &page.origins,
            )
            .unwrap();
        }
    }

    /// A photo-day run re-minting the day as `fixed`.
    fn photo_run(store: &Store, home: &Home, fixed: EpisodeDraft) -> String {
        store
            .repair_apply_work(
                home,
                &Actor::new("human:owner"),
                PHOTO_DAYS_KIND,
                RepairWork {
                    revisions: std::slice::from_ref(&fixed),
                    ..Default::default()
                },
            )
            .unwrap()
            .run
            .expect("a run")
    }

    fn photos_live(store: &Store) -> Vec<String> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT source_ref FROM episodes
                 WHERE source = 'photos' AND superseded_by IS NULL ORDER BY source_ref",
            )
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<std::result::Result<_, _>>().unwrap()
    }

    fn photo_scope(store: &Store, source_ref: &str) -> String {
        store
            .lock()
            .query_row(
                "SELECT scope FROM episodes WHERE source = 'photos' AND source_ref = ?1",
                [source_ref],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A photo day re-minted by a run that was then undone stands behind the
    /// day on every node, whichever reaches a node first: the undo (the copy
    /// lands behind the day in view) or the copy (it takes the day's place,
    /// and the undo puts it back behind).
    #[test]
    fn test_an_undone_photo_day_copy_stays_behind_on_every_node() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        apply(
            &mini,
            &mini_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, DOUBLED)],
        );
        photos_once(&mini);
        let run = photo_run(&mini, &mini_home, photo_day(PHOTO_FIX, ONCE));
        assert_eq!(photos_live(&mini), [PHOTO_FIX]);

        // the copy reaches the laptop before the undo
        let (laptop_home, laptop) = node(&tmp, "laptop");
        sync_points(&mini, &laptop, &laptop_home);
        sync(&mini, &mini_home, &laptop, &laptop_home, 0);
        sync_runs(&mini, &laptop, &laptop_home);
        assert_eq!(photos_live(&laptop), [PHOTO_FIX]);

        mini.repair_undo(&mini_home, &Actor::new("human:owner"), &run)
            .unwrap();
        assert_eq!(photos_live(&mini), [PHOTO_DAY]);
        sync_runs(&mini, &laptop, &laptop_home);
        assert_eq!(photos_live(&laptop), [PHOTO_DAY]);

        // the undo reaches a third node before the copy
        let (late_home, late) = node(&tmp, "late");
        sync_runs(&mini, &late, &late_home);
        sync(&mini, &mini_home, &late, &late_home, 0);
        assert_eq!(photos_live(&late), [PHOTO_DAY]);
    }

    /// A copy of an undone run that reaches a node before the memory it was
    /// made from — the undo first, or the undo after it — never stands: it
    /// waits behind the memory's id, and the memory arriving last stands. A
    /// photo day, and a chat-day re-minted by `repair copies`.
    #[test]
    fn test_an_undone_copy_arriving_before_its_memory_never_stands() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        apply(
            &mini,
            &mini_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, DOUBLED)],
        );
        photos_once(&mini);
        let photo = photo_run(&mini, &mini_home, photo_day(PHOTO_FIX, ONCE));
        apply(
            &mini,
            &mini_home,
            "adapter:imessage",
            vec![chat_day(&["lunch?", "noon"])],
        );
        let copies = copies_run(&mini, &mini_home).run.expect("a run");
        for run in [&photo, &copies] {
            mini.repair_undo(&mini_home, &Actor::new("human:owner"), run)
                .unwrap();
        }
        let chat_fix = format!("{DAY}#r2");

        // the undo, the copy, then the memory
        let (first_home, first) = node(&tmp, "undo-first");
        sync_points(&mini, &first, &first_home);
        sync_runs(&mini, &first, &first_home);
        send(
            &mini,
            &mini_home,
            &first,
            &first_home,
            &[PHOTO_FIX, &chat_fix],
        );
        assert!(photos_live(&first).is_empty());
        assert!(chat_days_live(&first, &first_home).is_empty());
        send(&mini, &mini_home, &first, &first_home, &[PHOTO_DAY, DAY]);
        assert_eq!(photos_live(&first), [PHOTO_DAY]);
        assert_eq!(chat_days_live(&first, &first_home)[0].0, DAY);

        // the copy, the undo, then the memory
        let (copy_home, copy_first) = node(&tmp, "copy-first");
        sync_points(&mini, &copy_first, &copy_home);
        send(
            &mini,
            &mini_home,
            &copy_first,
            &copy_home,
            &[PHOTO_FIX, &chat_fix],
        );
        sync_runs(&mini, &copy_first, &copy_home);
        assert!(photos_live(&copy_first).is_empty());
        assert!(chat_days_live(&copy_first, &copy_home).is_empty());
        send(
            &mini,
            &mini_home,
            &copy_first,
            &copy_home,
            &[PHOTO_DAY, DAY],
        );
        assert_eq!(photos_live(&copy_first), [PHOTO_DAY]);
        assert_eq!(chat_days_live(&copy_first, &copy_home)[0].0, DAY);
    }

    /// A photo day's correction made on one node stands on another only
    /// where it holds every photo that node's copy of the day shows: a node
    /// whose copy shows a photo the correction lacks keeps its copy in view;
    /// a node that can't yet tell (the photos, and the keys they took the
    /// place of, come on their own leg) keeps its copy until it can, and the
    /// correction stands once the run settles there.
    #[test]
    fn test_a_photo_day_correction_stands_only_where_it_holds_the_day() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        apply(
            &mini,
            &mini_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, DOUBLED)],
        );
        photos_once(&mini);
        photo_run(&mini, &mini_home, photo_day(PHOTO_FIX, ONCE));

        // a node whose own copy of the day shows a third photo
        let (laptop_home, laptop) = node(&tmp, "laptop");
        let fuller = [("10:00", "IMG_1"), ("11:00", "IMG_2"), ("12:00", "IMG_3")];
        apply(
            &laptop,
            &laptop_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, &fuller)],
        );
        sync_points(&mini, &laptop, &laptop_home);
        sync(&mini, &mini_home, &laptop, &laptop_home, 0);
        sync_runs(&mini, &laptop, &laptop_home);
        assert_eq!(photos_live(&laptop), [PHOTO_DAY]);

        // a node that holds mini's copy, before the photos reach it
        let (hub_home, hub) = node(&tmp, "hub");
        sync(&mini, &mini_home, &hub, &hub_home, 0);
        assert_eq!(photos_live(&hub), [PHOTO_DAY]);
        sync_points(&mini, &hub, &hub_home);
        sync_runs(&mini, &hub, &hub_home);
        assert_eq!(photos_live(&hub), [PHOTO_FIX]);
    }

    /// A photo day's correction whose run settles on a node before the
    /// photos that show it holds the day's copy there (the photos leg is
    /// paged, or failed that cycle) waits behind that copy, and stands once
    /// the photos arrive; a node whose own copy shows a photo the correction
    /// lacks keeps its copy when they do.
    #[test]
    fn test_a_photo_day_correction_stands_when_its_photos_arrive_after_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        apply(
            &mini,
            &mini_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, DOUBLED)],
        );
        photos_once(&mini);
        photo_run(&mini, &mini_home, photo_day(PHOTO_FIX, ONCE));

        let (hub_home, hub) = node(&tmp, "hub");
        sync(&mini, &mini_home, &hub, &hub_home, 0);
        sync_runs(&mini, &hub, &hub_home);
        assert_eq!(photos_live(&hub), [PHOTO_DAY]);
        assert!(hub.repairs_pending_here().unwrap().is_empty());
        sync_points(&mini, &hub, &hub_home);
        assert_eq!(photos_live(&hub), [PHOTO_FIX]);

        let (laptop_home, laptop) = node(&tmp, "laptop");
        let fuller = [("10:00", "IMG_1"), ("11:00", "IMG_2"), ("12:00", "IMG_3")];
        apply(
            &laptop,
            &laptop_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, &fuller)],
        );
        sync(&mini, &mini_home, &laptop, &laptop_home, 0);
        sync_runs(&mini, &laptop, &laptop_home);
        sync_points(&mini, &laptop, &laptop_home);
        assert_eq!(photos_live(&laptop), [PHOTO_DAY]);
    }

    /// The keys a photo took the place of can reach a node from a peer that
    /// stored the photo in another time zone, so under another day. The
    /// point stored here keeps its own day, and that day's waiting
    /// correction is the one checked again.
    #[test]
    fn test_a_photo_day_correction_stands_when_a_peer_dates_its_photo_otherwise() {
        // 2026-07-21T00:30:00Z: 20:30 on July 20 four hours behind UTC
        let at = JULY_20 + 86_400 + 1_800;
        let point = |name: &str, earlier: Option<&str>| crate::store::PhotoPointIn {
            key: format!("{at}|{name}"),
            filename: name.into(),
            taken_at: "2026-07-21T00:30:00Z".into(),
            tz_offset_minutes: -240,
            lat: Some(42.0),
            lon: Some(-71.0),
            place: None,
            earlier_key: earlier.map(|e| format!("{at}|{e}")),
        };
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        mini.photos_add_points("iPhone", &[point("IMG_9", None)])
            .unwrap();
        apply(
            &mini,
            &mini_home,
            "agent:photo-finalize",
            vec![photo_day(
                PHOTO_DAY,
                &[("20:30", "IMG_9"), ("20:30", "UUID-9")],
            )],
        );
        mini.photos_add_points("mac", &[point("IMG_9", Some("UUID-9"))])
            .unwrap();
        photo_run(
            &mini,
            &mini_home,
            photo_day(PHOTO_FIX, &[("20:30", "IMG_9")]),
        );

        // the hub holds the phone's photo, on July 20
        let (hub_home, hub) = node(&tmp, "hub");
        hub.photos_add_points("iPhone", &[point("IMG_9", None)])
            .unwrap();
        send(&mini, &mini_home, &hub, &hub_home, &[PHOTO_DAY, PHOTO_FIX]);
        sync_runs(&mini, &hub, &hub_home);
        assert_eq!(photos_live(&hub), [PHOTO_DAY]);

        // a peer that dated the photo in UTC (July 21) sends the key it
        // took the place of
        let page = serde_json::json!({"points": [{
            "key": format!("{at}|IMG_9"),
            "filename": "IMG_9",
            "at_unix": at,
            "tz_offset_minutes": 0,
            "lat": 42.0,
            "lon": -71.0,
            "device": "mac",
            "replaces": [format!("{at}|UUID-9")],
        }]});
        hub.photo_points_apply_and_settle(&hub_home, &page).unwrap();
        assert_eq!(photos_live(&hub), [PHOTO_FIX]);
    }

    /// A photo day's correction made where the day isn't private lands
    /// private on a node where it is, and the label comes back with it; a
    /// private label that reaches a node after the correction did reaches
    /// the correction too.
    #[test]
    fn test_a_photo_day_correction_keeps_the_day_private_where_it_is() {
        let tmp = tempfile::tempdir().unwrap();
        let (mini_home, mini) = node(&tmp, "mini");
        apply(
            &mini,
            &mini_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, DOUBLED)],
        );
        photos_once(&mini);
        let (laptop_home, laptop) = node(&tmp, "laptop");
        sync_points(&mini, &laptop, &laptop_home);
        send(&mini, &mini_home, &laptop, &laptop_home, &[PHOTO_DAY]);
        laptop
            .set_episode_scopes(
                &Actor::new("human:owner"),
                &[ids::episode_id("photos", PHOTO_DAY)],
                "private",
            )
            .unwrap();

        // mini, where the day isn't private, corrects it under its label
        let mut fixed = photo_day(PHOTO_FIX, ONCE);
        fixed.scope = Some("personal".into());
        photo_run(&mini, &mini_home, fixed);
        assert_eq!(photo_scope(&mini, PHOTO_FIX), "personal");
        send(&mini, &mini_home, &laptop, &laptop_home, &[PHOTO_FIX]);
        assert_eq!(photos_live(&laptop), [PHOTO_FIX]);
        assert_eq!(photo_scope(&laptop, PHOTO_FIX), "private");
        // the label goes back on the scope leg
        let changes = laptop.scope_changes_since("").unwrap();
        mini.apply_scope_changes(&Actor::new("peer:laptop"), &changes)
            .unwrap();
        assert_eq!(photo_scope(&mini, PHOTO_FIX), "private");
        assert_eq!(photo_scope(&mini, PHOTO_DAY), "private");

        // a node that had the correction before the day's label reached it
        let (hub_home, hub) = node(&tmp, "hub");
        sync_points(&mini, &hub, &hub_home);
        let mut personal_fix = photo_day(PHOTO_FIX, ONCE);
        personal_fix.scope = Some("personal".into());
        apply(
            &hub,
            &hub_home,
            "agent:photo-finalize",
            vec![photo_day(PHOTO_DAY, DOUBLED)],
        );
        apply(&hub, &hub_home, "agent:photo-finalize", vec![personal_fix]);
        assert_eq!(photo_scope(&hub, PHOTO_FIX), "personal");
        let base_only: Vec<_> = changes
            .into_iter()
            .filter(|(id, _, _)| *id == ids::episode_id("photos", PHOTO_DAY))
            .collect();
        hub.apply_scope_changes(&Actor::new("peer:laptop"), &base_only)
            .unwrap();
        assert_eq!(photo_scope(&hub, PHOTO_FIX), "private");
    }

    /// `repair copies` re-mints a Chrome day and a Claude Code session
    /// stored under unversioned refs, each named by its count; a private
    /// session's copy lands private, a retired day's lands retired.
    #[test]
    fn test_repair_copies_reaches_every_growing_source_and_keeps_labels() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(&tmp, "mini");
        let draft = |source: &str, source_ref: &str, kind: &str, body: &str| EpisodeDraft {
            source: source.into(),
            source_ref: source_ref.into(),
            kind: kind.into(),
            title: format!("{kind} {source_ref}"),
            body: body.into(),
            event_time: EventTime::parse_rfc3339("2026-11-10T09:00:00Z", TimeUncertainty::Day)
                .unwrap(),
            trust: "self".into(),
            scope: None,
            entities: vec![],
            edges: vec![],
        };
        let chrome = draft(
            "chrome",
            "profile:1#2026-11-10",
            "browsing",
            "browsing (Work) on 2026-11-10: 2 visits\n\n[09:00] Example — example.com\n[10:00] Other — example.org\n",
        );
        let mut session = draft(
            "claude-code",
            "sess-1",
            "claude_code_session",
            "claude code session sess-1\nturns: 2\nworkspace: w\n\nyou: a\n\nclaude: b\n\n",
        );
        session.scope = Some("private".into());
        apply(&store, &home, "adapter:chrome", vec![chrome]);
        apply(&store, &home, "adapter:claude-code", vec![session]);
        let chrome_id = ids::episode_id("chrome", "profile:1#2026-11-10");
        let retirement = store
            .retire(&Actor::new("human:owner"), "memory", &chrome_id, &[], "")
            .unwrap();
        store.retirement_apply(&retirement.id).unwrap();

        let found = store.repair_copies_scan(&home).unwrap();
        let mut reasons: Vec<(&str, &str, &str)> = found
            .iter()
            .map(|f| (f.source.as_str(), f.status, f.reason.as_str()))
            .collect();
        reasons.sort();
        assert_eq!(
            reasons,
            [
                (
                    "chrome",
                    "remint",
                    "2 visits, stored under an unversioned ref; retired, the copy lands retired"
                ),
                (
                    "claude-code",
                    "remint",
                    "2 turns, stored under an unversioned ref"
                ),
            ]
        );
        assert!(found.iter().any(|f| f.source == "claude-code" && f.private));
        assert!(copies_run(&store, &home).run.is_some());
        let (superseded, scope): (Option<String>, String) = store
            .lock()
            .query_row(
                "SELECT superseded_by, scope FROM episodes WHERE source = 'chrome' AND source_ref = ?1",
                ["profile:1#2026-11-10#r2"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(superseded, Some(format!("retired:{}", retirement.id)));
        assert_ne!(scope, "private");
        let (superseded, scope): (Option<String>, String) = store
            .lock()
            .query_row(
                "SELECT superseded_by, scope FROM episodes WHERE source = 'claude-code' AND source_ref = ?1",
                ["sess-1#r2"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((superseded, scope.as_str()), (None, "private"));
    }

    /// The lookups a repair makes stay on indexes.
    #[test]
    fn test_repair_lookups_use_indexes() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.lock();
        for sql in [
            format!(
                "SELECT e.source, e.source_ref FROM repair_copies c
                 JOIN episodes e ON e.id = c.episode_id
                 JOIN episode_origin o ON o.episode_id = e.id
                 WHERE c.run = ?1 AND o.writer = '{REPAIR_WRITER}'"
            ),
            "SELECT 1 FROM repair_copies c JOIN repairs r ON r.id = c.run
             WHERE c.episode_id = ?1 AND r.undone_at IS NOT NULL"
                .to_string(),
            format!(
                "SELECT {COPY_COLUMNS} FROM episodes e LEFT JOIN episode_origin o ON o.episode_id = e.id
                 WHERE e.source = ?1 AND (e.source_ref = ?2
                    OR (e.source_ref >= ?2 || '{REPAIR_MARK}' AND e.source_ref < ?2 || '#repair;'))"
            ),
        ] {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let args = vec!["x"; stmt.parameter_count()];
            let plan: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(args), |r| r.get(3))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert!(
                plan.iter().all(|step| !step.starts_with("SCAN")),
                "{sql}\n{plan:?}"
            );
        }
    }
}
