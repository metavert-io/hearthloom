//! Growing sources (`GROWING_SOURCES`): a Chrome or iMessage day, a Claude
//! Code session — records that can gain items after they were stored. The
//! local importers land each read as `<base>#r<n>`, `n` the number of
//! items it holds (daemon `local_import`; an unversioned `<base>` is an
//! earlier version's); every node keeps the copy that holds the most live,
//! whatever order copies arrive in, and the copies are one record: a new
//! copy takes on what the owner decided about it — its label (private
//! above all) and its retirement — and a private label or a retirement
//! given to one copy reaches the others ([`other_copies`],
//! [`spread_private`]).
//!
//! What a copy holds is counted from its text ([`growing_items`]): the
//! adapters write the true count on its first lines, ahead of anything
//! someone else wrote, so a text cut to fit its size cap still counts every
//! item. A revision's count is its `n`.
//!
//! Chrome days were stored by earlier versions under the profile's shown
//! name (`<name>#<date>`); a day keyed by its profile replaces such a day
//! when it holds all of it — its visit count and every visit line
//! ([`chrome_day_holds`]) — and leaves a same-named day of another profile
//! (on this machine or another) alone.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};

use super::{record_mutation, Actor, Store};
use crate::paths::Home;
use crate::time::now_utc;

/// Sources whose episode covers something that can still grow after it
/// was stored: a Chrome or iMessage day that gains late items, a Claude
/// Code session picked up again. A revision `<base>#r<n>` is a copy of
/// `<base>` holding `n` items; the copy holding the most stays live (the
/// unversioned base counted from its text, a tie going to the revision).
/// Each base ends in a part the adapter writes (`#<date>`, a session id
/// without `#`), so no ref chosen elsewhere reads as a copy of another
/// record.
pub const GROWING_SOURCES: &[&str] = &["chrome", "imessage", "claude-code"];

/// A growing source's ref as (base, n): `<base>#r<n>` → (base, n); any
/// other ref is its own base, n = 0.
pub fn growing_copy<'a>(source: &str, source_ref: &'a str) -> Option<(&'a str, u64)> {
    if !GROWING_SOURCES.contains(&source) {
        return None;
    }
    let revision = source_ref.rsplit_once("#r").and_then(|(base, n)| {
        (!base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .then(|| n.parse::<u64>().ok().map(|n| (base, n)))
            .flatten()
    });
    Some(revision.unwrap_or((source_ref, 0)))
}

/// SQL over `episodes`: the row is a revision `?{base}#r<digits>` of the
/// base in `?{base}` (exact; the range keeps it on the source_ref index).
pub(crate) fn growing_revision_sql(base: usize) -> String {
    format!(
        "(source_ref >= ?{base} || '#r0' AND source_ref < ?{base} || '#r:'
          AND substr(source_ref, length(?{base}) + 3) NOT GLOB '*[^0-9]*')"
    )
}

/// How many items a copy of a growing source holds, from its text: a
/// Chrome day's visits (`browsing (<name>) on <date>: <n> visits`), an
/// iMessage chat-day's messages (`messages: <n>`, its first line), a Claude
/// Code session's turns (`turns: <n>`, its second line). Text stored before
/// the count was written is counted by its item lines. A chat-day counts
/// at least its message lines even with a count line: a message is written
/// as it was sent, so its text can hold lines shaped like messages (a
/// pasted chat log), which a day stored before the count line can only be
/// counted with. A day whose messages hold such lines says how many the
/// whole day holds (`lines: <m>`, its second line, ahead of the chat's
/// name): a long day's text leaves messages out, and with them their lines
/// — so they weigh the same in each read of the day, shown or not, and
/// never hold its later messages back. `None` for any other source, or a
/// Chrome day whose count can't be read.
pub fn growing_items(source: &str, body: &str) -> Option<u64> {
    let mut lines = body.lines();
    let first = lines.next().unwrap_or_default();
    let count = |line: Option<&str>, label: &str| {
        line.and_then(|l| l.strip_prefix(label))
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<u64>().ok())
    };
    match source {
        "chrome" => first
            .rsplit_once(": ")?
            .1
            .strip_suffix(" visits")
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))?
            .parse()
            .ok(),
        "imessage" => {
            let messages = count(Some(first), "messages: ");
            let whole_day = messages.and(count(lines.next(), "lines: "));
            Some(
                [messages, whole_day]
                    .into_iter()
                    .flatten()
                    .fold(chat_lines(body), u64::max),
            )
        }
        "claude-code" => {
            Some(count(lines.next(), "turns: ").unwrap_or_else(|| session_turns(body)))
        }
        _ => None,
    }
}

/// The lines of a chat-day's text shaped like messages (`[HH:MM] `): its
/// messages, and lines their own text holds that look like them.
pub fn chat_lines(text: &str) -> u64 {
    text.lines().filter(|l| message_line(l)).count() as u64
}

/// A chat-day line `[HH:MM] <sender>: <text>`.
fn message_line(line: &str) -> bool {
    let b = line.as_bytes();
    b.len() >= 8
        && b[0] == b'['
        && b[1..3].iter().all(u8::is_ascii_digit)
        && b[3] == b':'
        && b[4..6].iter().all(u8::is_ascii_digit)
        && &b[6..8] == b"] "
}

/// The turns of a session text: each `you: ` / `claude: ` that opens a
/// paragraph.
fn session_turns(body: &str) -> u64 {
    let mut previous_blank = false;
    let mut turns = 0;
    for line in body.lines() {
        if previous_blank && (line.starts_with("you: ") || line.starts_with("claude: ")) {
            turns += 1;
        }
        previous_blank = line.is_empty();
    }
    turns
}

/// A Chrome day's key part names its profile (`profile:<creation time>`,
/// `profile-dir:<…>`) rather than its shown name.
fn is_profile_key(key: &str) -> bool {
    key.starts_with("profile:") || key.starts_with("profile-dir:")
}

/// The profile name a Chrome day's text shows, for the day `date`.
fn chrome_day_name<'a>(body: &'a str, date: &str) -> Option<&'a str> {
    let first = body.lines().next()?.strip_prefix("browsing (")?;
    Some(&first[..first.rfind(&format!(") on {date}: "))?])
}

/// The ref earlier versions stored a Chrome day under — the profile's
/// shown name and the date, `<name>#<date>` — for a day (or a copy of
/// one) keyed by its profile; `None` for anything else.
pub fn chrome_named_ref(source_ref: &str, body: &str) -> Option<String> {
    let (base, _) = growing_copy("chrome", source_ref)?;
    let (key, date) = base.rsplit_once('#')?;
    if !is_profile_key(key) {
        return None;
    }
    let name = chrome_day_name(body, date)?;
    Some(format!("{name}#{date}"))
}

/// Does the Chrome day `outer` hold all of the day `inner`: at least as
/// many visits (the count each text opens with — consecutive visits under
/// one title share a line, and a long day's text leaves lines out, so lines
/// alone undercount), the visit `inner` opens with, and every other visit
/// line `inner` shows, unless it falls where `outer` leaves lines out — and
/// `inner` leaves lines out too? A day is long in every copy of it (one
/// cap, one way of writing it): a text showing its whole day is held only
/// by one showing each of its lines, so another profile's short day that
/// opens with the same visit isn't taken for a long day's. A day whose
/// count can't be read holds nothing and is held by nothing.
pub fn chrome_day_holds(outer: &str, inner: &str) -> bool {
    let (Some(have), Some(need)) = (
        growing_items("chrome", outer),
        growing_items("chrome", inner),
    ) else {
        return false;
    };
    if have < need {
        return false;
    }
    let outer = chrome_day_lines(outer);
    let shown: HashSet<&str> = outer.iter().filter_map(ChromeLine::visit).collect();
    // the minutes where `outer` may leave visits out: from the visit before
    // each gap to the one after it (to the day's end after `[truncated]`)
    let minute = |i: usize| outer.get(i).and_then(ChromeLine::visit).map(visit_minute);
    let gaps: Vec<(Option<&str>, Option<&str>)> = outer
        .iter()
        .enumerate()
        .filter(|(_, l)| matches!(l, ChromeLine::Gap))
        .map(|(i, _)| (i.checked_sub(1).and_then(minute), minute(i + 1)))
        .collect();
    let left_out = |at: &str| {
        gaps.iter()
            .any(|(from, to)| from.is_none_or(|f| f <= at) && to.is_none_or(|t| at <= t))
    };
    let inner = chrome_day_lines(inner);
    let inner_cut = inner.iter().any(|l| matches!(l, ChromeLine::Gap));
    let mut visits = inner.iter().filter_map(ChromeLine::visit);
    // each text shows its day from the start
    let Some(first) = visits.next() else {
        return true;
    };
    shown.contains(first)
        && visits.all(|line| shown.contains(line) || (inner_cut && left_out(visit_minute(line))))
}

/// A line of a Chrome day's text, from its first visit on.
enum ChromeLine {
    /// `[HH:MM] <title> — <site>`, its whitespace runs read as one space.
    Visit(String),
    /// Lines left out here: `[truncated]` (earlier versions, at the end),
    /// `[<n> lines not shown]` (between the first and the latest lines).
    Gap,
}

impl ChromeLine {
    fn visit(&self) -> Option<&str> {
        match self {
            ChromeLine::Visit(line) => Some(line),
            ChromeLine::Gap => None,
        }
    }
}

/// `[HH:MM]` of a visit line.
fn visit_minute(line: &str) -> &str {
    &line[..7]
}

/// The visits and gaps a Chrome day's text shows. Earlier versions wrote a
/// page's title as it was, line breaks and all: a line that opens no visit
/// continues the one before (a title is on one line now), so both read the
/// same.
fn chrome_day_lines(text: &str) -> Vec<ChromeLine> {
    let gap = |line: &str| {
        line == "[truncated]"
            || line
                .strip_prefix('[')
                .and_then(|l| l.strip_suffix(" lines not shown]"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let mut out: Vec<ChromeLine> = Vec::new();
    for line in text.lines() {
        if message_line(line) {
            out.push(ChromeLine::Visit(
                line.split_whitespace().collect::<Vec<_>>().join(" "),
            ));
        } else if gap(line) {
            out.push(ChromeLine::Gap);
        } else if let Some(ChromeLine::Visit(visit)) = out.last_mut() {
            for word in line.split_whitespace() {
                visit.push(' ');
                visit.push_str(word);
            }
        }
    }
    out
}

/// (items held, is a revision, ref): the larger copy outranks; equal
/// counts go to the revision, then to the larger ref, so every node picks
/// the same.
type Rank = (u64, bool, String);

/// A stored episode as these rules see it.
struct Stored {
    id: String,
    source_ref: String,
    content_hash: String,
    content_ref: Option<String>,
    superseded_by: Option<String>,
    scope: String,
    scope_changed_at: Option<String>,
}

/// Stored copies' texts read before the store was taken, by episode id:
/// None for one whose raw file couldn't be read (it counts as holding
/// nothing, as an unreadable file always has). A copy that isn't a key
/// wasn't read.
pub(crate) type ReadTexts = HashMap<String, Option<String>>;

/// Where these rules read a stored copy's text: its raw file, or the texts
/// read before the store was taken ([`ReadTexts`]; a copy not among them
/// has none).
#[derive(Clone, Copy)]
pub(crate) enum Texts<'a> {
    Files(&'a Home),
    Read(&'a ReadTexts),
}

impl Texts<'_> {
    fn of(self, stored: &Stored) -> Option<String> {
        match self {
            Texts::Files(home) => stored.body(home),
            Texts::Read(texts) => texts.get(&stored.id).cloned().flatten(),
        }
    }
}

/// Does a copy's ref say what it holds (`<base>#r<n>`), so its rank needs
/// no text?
fn counted(source: &str, source_ref: &str) -> bool {
    matches!(growing_copy(source, source_ref), Some((_, n)) if n > 0)
}

impl Stored {
    fn body(&self, home: &Home) -> Option<String> {
        let rel = self.content_ref.as_deref()?;
        std::fs::read_to_string(home.root().join(rel)).ok()
    }

    fn rank(&self, texts: Texts<'_>, source: &str) -> Rank {
        match growing_copy(source, &self.source_ref) {
            Some((_, n)) if n > 0 => (n, true, self.source_ref.clone()),
            _ => (
                texts
                    .of(self)
                    .and_then(|body| growing_items(source, &body))
                    .unwrap_or(0),
                false,
                self.source_ref.clone(),
            ),
        }
    }

    /// Live, or retired — not replaced by another copy.
    fn standing(&self) -> bool {
        self.superseded_by
            .as_deref()
            .is_none_or(|s| s.starts_with(super::retire::RETIRED_PREFIX))
    }
}

const STORED_COLUMNS: &str =
    "id, source_ref, content_hash, content_ref, superseded_by, scope, scope_changed_at";

fn stored_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Stored> {
    Ok(Stored {
        id: r.get(0)?,
        source_ref: r.get(1)?,
        content_hash: r.get(2)?,
        content_ref: r.get(3)?,
        superseded_by: r.get(4)?,
        scope: r.get(5)?,
        scope_changed_at: r.get(6)?,
    })
}

/// Every stored copy of `base` (the base and its revisions), in any state.
fn copies_of(conn: &Connection, source: &str, base: &str) -> Result<Vec<Stored>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {STORED_COLUMNS} FROM episodes /* all-episodes: every copy of one day or session decides how a new one lands */
         WHERE source = ?1 AND (source_ref = ?2 OR {})",
        growing_revision_sql(2)
    ))?;
    let rows = stmt.query_map(params![source, base], stored_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Chrome days keyed by a profile with a visit on `date`, in any state.
fn profile_days_on(conn: &Connection, date: &str) -> Result<Vec<Stored>> {
    let mut stmt = conn.prepare_cached(&profile_days_sql())?;
    let rows = stmt.query_map([date], stored_row)?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// [`profile_days_on`]'s statement. It reads the one day's episodes by
/// event time: `+source` keeps the planner off the source index, which
/// would visit every Chrome day stored, once per day arriving.
fn profile_days_sql() -> String {
    format!(
        "SELECT {STORED_COLUMNS} FROM episodes /* all-episodes: which profile day replaces a day stored under a name */
         WHERE +source = 'chrome' AND event_time >= ?1 AND event_time < ?1 || 'U'
           AND (substr(source_ref, 1, 8) = 'profile:' OR substr(source_ref, 1, 12) = 'profile-dir:')"
    )
}

/// The newest stored copy of a growing source's base: the one holding the
/// most (see [`GROWING_SOURCES`]), replaced or retired ones included — a
/// fuller read is new only against the fullest copy there is.
#[derive(Debug, Clone)]
pub struct GrowingCopy {
    pub source_ref: String,
    pub content_hash: String,
    /// What it holds ([`growing_items`]; a revision's `n`).
    pub items: u64,
    content_ref: Option<String>,
}

impl GrowingCopy {
    /// Its text, from the raw file (`None` if that can't be read).
    pub fn body(&self, home: &Home) -> Option<String> {
        let rel = self.content_ref.as_deref()?;
        std::fs::read_to_string(home.root().join(rel)).ok()
    }
}

impl Store {
    /// The fullest stored copy of `base` of a growing source, in any state
    /// (see [`GrowingCopy`]) — except a copy an owner repair run re-minted
    /// and that run was undone (store/repair.rs): it stands behind the copy
    /// in view, so a read holding as much is new here
    /// ([`revive_undone_copy`]). One indexed lookup; the base's text is read
    /// after the store is released.
    pub fn growing_latest(
        &self,
        home: &Home,
        source: &str,
        base: &str,
    ) -> Result<Option<GrowingCopy>> {
        let copies = {
            let conn = self.lock();
            let mut kept = Vec::new();
            for copy in copies_of(&conn, source, base)? {
                if !super::repair::undone_copy(&conn, &copy.id)? {
                    kept.push(copy);
                }
            }
            kept
        };
        Ok(copies
            .into_iter()
            .map(|c| (c.rank(Texts::Files(home), source), c))
            .max_by(|a, b| a.0.cmp(&b.0))
            .map(|((items, _, _), c)| GrowingCopy {
                source_ref: c.source_ref,
                content_hash: c.content_hash,
                items,
                content_ref: c.content_ref,
            }))
    }
}

/// A stored episode related to an arriving copy, with its rank.
struct Related {
    stored: Stored,
    rank: Rank,
}

/// What an arriving copy of a growing source is measured against, read
/// before it is inserted (`apply_batch`).
pub(crate) struct Arriving {
    rank: Rank,
    /// The id its base (`<base>`) has, stored here or not.
    base_id: String,
    /// The other copies of its day or session.
    copies: Vec<Related>,
    /// A Chrome day keyed by its profile: the days stored under the
    /// profile's shown name that it holds — earlier copies of it.
    held: Vec<Related>,
    /// A Chrome day stored under a profile's shown name: the profile-keyed
    /// days holding it — later copies of it.
    holders: Vec<Related>,
}

/// The stored episodes a copy of `base` (`source_ref`, text `body`) is
/// measured against, before their texts are looked at: (the copies of its
/// day or session, the Chrome days stored under a profile's shown name it
/// may hold, the profile-keyed Chrome days that may hold it).
type Candidates = (Vec<Stored>, Vec<Stored>, Vec<Stored>);

fn candidates(
    conn: &Connection,
    source: &str,
    source_ref: &str,
    base: &str,
    body: &str,
) -> Result<Candidates> {
    let copies = copies_of(conn, source, base)?;
    let (mut held, mut holders) = (Vec::new(), Vec::new());
    if source == "chrome" {
        if let Some(named) = chrome_named_ref(source_ref, body) {
            held = copies_of(conn, source, &named)?;
        } else if let Some((name, date)) = base.rsplit_once('#') {
            let named_day = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
                && chrome_day_name(body, date) == Some(name);
            if named_day {
                holders = profile_days_on(conn, date)?;
            }
        }
    }
    Ok((copies, held, holders))
}

/// The stored episodes whose texts [`Arriving::read`] reads to measure the
/// copy `source_ref` (text `body`): the copies of its record whose refs
/// don't say what they hold, and the Chrome days it is compared with.
fn texts_needed(
    conn: &Connection,
    source: &str,
    source_ref: &str,
    body: &str,
) -> Result<Vec<Stored>> {
    let Some((base, _)) = growing_copy(source, source_ref) else {
        return Ok(Vec::new());
    };
    let (copies, held, holders) = candidates(conn, source, source_ref, base, body)?;
    Ok(copies
        .into_iter()
        .filter(|s| !counted(source, &s.source_ref))
        .chain(held)
        .chain(holders)
        .filter(|s| s.source_ref != source_ref)
        .collect())
}

impl Arriving {
    /// `None` for a draft that isn't a growing source's. `texts`: where the
    /// stored copies' texts come from — with the store held, only the texts
    /// read before it was taken ([`Texts::Read`]).
    pub(crate) fn read(
        conn: &Connection,
        texts: Texts<'_>,
        source: &str,
        source_ref: &str,
        body: &str,
    ) -> Result<Option<Arriving>> {
        let Some((base, n)) = growing_copy(source, source_ref) else {
            return Ok(None);
        };
        let rank = if n > 0 {
            (n, true, source_ref.to_string())
        } else {
            (
                growing_items(source, body).unwrap_or(0),
                false,
                source_ref.to_string(),
            )
        };
        let related = |rows: Vec<Stored>, keep: &dyn Fn(&Stored) -> bool| -> Vec<Related> {
            rows.into_iter()
                .filter(|s| s.source_ref != source_ref && keep(s))
                .map(|stored| Related {
                    rank: stored.rank(texts, source),
                    stored,
                })
                .collect()
        };
        let (copies, held, holders) = candidates(conn, source, source_ref, base, body)?;
        let copies = related(copies, &|_| true);
        let held = related(held, &|s| {
            texts.of(s).is_some_and(|old| chrome_day_holds(body, &old))
        });
        let holders = related(holders, &|s| {
            texts.of(s).is_some_and(|day| {
                chrome_named_ref(&s.source_ref, &day).as_deref() == Some(base)
                    && chrome_day_holds(&day, body)
            })
        });
        Ok(Some(Arriving {
            rank,
            base_id: crate::ids::episode_id(source, base),
            copies,
            held,
            holders,
        }))
    }

    fn related(&self) -> impl Iterator<Item = &Related> {
        self.copies.iter().chain(&self.held).chain(&self.holders)
    }

    /// The latest label given to any copy of the record (by the owner, or
    /// carried from another copy), if one was.
    fn decided(&self) -> Option<&Related> {
        self.related()
            .filter(|r| r.stored.scope_changed_at.is_some())
            .max_by(|a, b| a.stored.scope_changed_at.cmp(&b.stored.scope_changed_at))
    }

    /// The label the record has, for a copy whose draft names none: the
    /// latest one given to any copy (a correction the owner made stays),
    /// else that of its fullest stored copy.
    pub(crate) fn inherited_scope(&self) -> Option<&str> {
        self.decided()
            .or_else(|| self.copies.iter().max_by(|a, b| a.rank.cmp(&b.rank)))
            .or_else(|| self.held.iter().max_by(|a, b| a.rank.cmp(&b.rank)))
            .or_else(|| self.holders.iter().max_by(|a, b| a.rank.cmp(&b.rank)))
            .map(|r| r.stored.scope.as_str())
    }

    /// The record is private, so the new copy is too: the latest label
    /// given to any copy of it (by the owner, or carried from another copy)
    /// is private — or, no copy having been relabeled, some copy is
    /// private. Carries when that label was given, if it says.
    pub(crate) fn private(&self) -> Option<Option<&str>> {
        match self.decided() {
            Some(r) => {
                (r.stored.scope == "private").then_some(r.stored.scope_changed_at.as_deref())
            }
            None => self
                .related()
                .find(|r| r.stored.scope == "private")
                .map(|_| None),
        }
    }

    /// The `retired:<id>` marks of retired copies of the record.
    pub(crate) fn retired_marks(&self) -> impl Iterator<Item = &str> {
        self.related()
            .filter_map(|r| r.stored.superseded_by.as_deref())
            .filter(|s| s.starts_with(super::retire::RETIRED_PREFIX))
    }

    /// After the copy `episode_id` is inserted: when a standing copy (its
    /// own, or a profile day holding it) holds more, it lands behind that
    /// one; otherwise every standing copy it outranks goes behind it — a
    /// retired one once its retirement is undone. A copy an undone repair
    /// run re-minted (`undone`, store/repair.rs) lands behind the fullest
    /// standing copy whatever it holds — none being here yet, behind the
    /// base's id, so the base arriving later stands and it never does — and
    /// puts none behind itself.
    pub(crate) fn settle(&self, conn: &Connection, episode_id: &str, undone: bool) -> Result<()> {
        let best = |set: &[Related]| {
            set.iter()
                .filter(|r| r.stored.standing())
                .max_by(|a, b| a.rank.cmp(&b.rank))
                .map(|r| (r.rank.clone(), r.stored.id.clone()))
        };
        if undone {
            let ahead = best(&self.copies)
                .or_else(|| best(&self.holders))
                .map(|(_, id)| id)
                .or_else(|| (self.base_id != episode_id).then(|| self.base_id.clone()));
            if let Some(ahead) = ahead {
                conn.execute(
                    "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
                    params![ahead, episode_id],
                )?;
            }
            return Ok(());
        }
        let ahead = best(&self.copies)
            .filter(|(rank, _)| *rank > self.rank)
            .or_else(|| best(&self.holders))
            .map(|(_, id)| id);
        if let Some(ahead) = ahead {
            conn.execute(
                "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
                params![ahead, episode_id],
            )?;
            return Ok(());
        }
        for r in self.copies.iter().chain(&self.held) {
            match r.stored.superseded_by.as_deref() {
                None => {
                    conn.execute(
                        "UPDATE episodes SET superseded_by = ?1
                         WHERE id = ?2 AND superseded_by IS NULL",
                        params![episode_id, r.stored.id],
                    )?;
                }
                Some(mark) if mark.starts_with(super::retire::RETIRED_PREFIX) => {
                    super::retire::undo_behind(conn, &r.stored.id, mark, episode_id)?;
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

/// A read of this node's own files (`apply_batch`, not a peer's or a
/// repair's batch) named `<base>#r<n>`, a ref an owner repair run took
/// for a copy holding `n` items and that run was undone: the copy stands
/// behind the copy in view, and the read can't land under the taken ref.
/// The read holds at least `n` items, so the copy stands again by the
/// growing rule — as the read itself would have, had the run never been
/// made. A retired copy, or one standing behind something outside its
/// record, is left. The read is recorded (`repair::record_revived`) and
/// travels with the run, so every node puts the copy back the same way —
/// only this node can read the files that hold it. `texts`: the copy's own
/// text and those of the copies it is measured against, read before the
/// store was taken ([`named_copy_texts`]); the read's text stands in for its
/// own when that couldn't be. Returns whether it changed.
pub(crate) fn revive_undone_copy(
    conn: &Connection,
    draft: &crate::ingest::EpisodeDraft,
    texts: &ReadTexts,
) -> Result<bool> {
    let Some((_, n)) = growing_copy(&draft.source, &draft.source_ref) else {
        return Ok(false);
    };
    if n == 0 || growing_items(&draft.source, &draft.body).is_none_or(|have| have < n) {
        return Ok(false);
    }
    let id = crate::ids::episode_id(&draft.source, &draft.source_ref);
    if !super::repair::undone_copy(conn, &id)? {
        return Ok(false);
    }
    let body = texts
        .get(&id)
        .and_then(Option::as_deref)
        .unwrap_or(&draft.body);
    let Some(changed) = stand_by_rule(conn, &id, Some(body), texts, false)? else {
        return Ok(false);
    };
    super::repair::record_revived(conn, &id)?;
    Ok(changed)
}

/// Read these copies' texts (id, raw file) — with the store released: a
/// slow or cold disk must never hold the lock every request waits on.
fn read_texts(home: &Home, copies: Vec<(String, Option<String>)>) -> ReadTexts {
    copies
        .into_iter()
        .map(|(id, rel)| {
            let text = rel.and_then(|rel| std::fs::read_to_string(home.root().join(rel)).ok());
            (id, text)
        })
        .collect()
}

/// One copy to place by the growing rule: (id, source, ref, raw file).
type ToPlace = (String, String, String, Option<String>);

/// Read, with the store released, the texts of `first` and of the copies
/// `to_place`, then those of the stored copies each is measured against
/// ([`texts_needed`] — which ones depends on the copy's text: one short
/// hold for where they are, the files read after it). `stand_in`: a text
/// that names the copies to measure against when a copy's own can't be
/// read.
fn texts_to_place(
    store: &super::Store,
    home: &Home,
    first: Vec<(String, Option<String>)>,
    to_place: Vec<ToPlace>,
    stand_in: &HashMap<String, &str>,
) -> Result<ReadTexts> {
    let mut files = first;
    files.extend(
        to_place
            .iter()
            .map(|(id, _, _, rel)| (id.clone(), rel.clone())),
    );
    let mut texts = read_texts(home, files);
    if to_place.is_empty() {
        return Ok(texts);
    }
    let files: BTreeMap<String, Option<String>> = {
        let conn = store.lock();
        let mut files = BTreeMap::new();
        for (id, source, source_ref, _) in &to_place {
            let Some(body) = texts
                .get(id)
                .and_then(Option::as_deref)
                .or_else(|| stand_in.get(id).copied())
            else {
                continue;
            };
            for s in texts_needed(&conn, source, source_ref, body)? {
                if !texts.contains_key(&s.id) {
                    files.insert(s.id, s.content_ref);
                }
            }
        }
        files
    };
    texts.extend(read_texts(home, files.into_iter().collect()));
    Ok(texts)
}

/// Before a batch of this node's own reads takes the store: the stored
/// texts of the growing copies (`<base>#r<n>`) its drafts name that are
/// stored already and that an undone repair run made, and of the copies
/// each is measured against — what [`revive_undone_copy`] places a copy by.
pub(crate) fn named_copy_texts(
    store: &super::Store,
    home: &Home,
    drafts: &[crate::ingest::EpisodeDraft],
) -> Result<ReadTexts> {
    let named: Vec<(String, &crate::ingest::EpisodeDraft)> = drafts
        .iter()
        .filter(|d| counted(&d.source, &d.source_ref))
        .map(|d| (crate::ids::episode_id(&d.source, &d.source_ref), d))
        .collect();
    if named.is_empty() {
        return Ok(HashMap::new());
    }
    let to_place = {
        let conn = store.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT content_ref FROM episodes /* all-episodes: a copy behind another, by design */
             WHERE id = ?1",
        )?;
        let mut to_place = Vec::new();
        for (id, d) in &named {
            if let Some(rel) = stmt
                .query_row([id], |r| r.get::<_, Option<String>>(0))
                .optional()?
            {
                if super::repair::undone_copy(&conn, id)? {
                    to_place.push((id.clone(), d.source.clone(), d.source_ref.clone(), rel));
                }
            }
        }
        to_place
    };
    if to_place.is_empty() {
        return Ok(HashMap::new());
    }
    let stand_in: HashMap<String, &str> = named
        .iter()
        .map(|(id, d)| (id.clone(), d.body.as_str()))
        .collect();
    texts_to_place(store, home, Vec::new(), to_place, &stand_in)
}

/// Before a settle batch takes the store: the stored texts settling these
/// growing records (source, base) reads — of their copies whose refs don't
/// say what they hold (`repair::settle_revisions` ranks them by it), of the
/// copies a read put back in view that stand behind another copy, and of
/// the copies each of those is measured against (what [`restore_revived`]
/// places).
pub(crate) fn settle_texts(
    store: &super::Store,
    home: &Home,
    records: &[(String, String)],
) -> Result<ReadTexts> {
    let (uncounted, to_place) = {
        let conn = store.lock();
        let (mut uncounted, mut to_place) = (Vec::new(), Vec::new());
        for (source, base) in records {
            if !GROWING_SOURCES.contains(&source.as_str()) {
                continue;
            }
            for copy in copies_of(&conn, source, base)? {
                if copy.superseded_by.is_some() && super::repair::revived(&conn, &copy.id)? {
                    to_place.push((copy.id, source.clone(), copy.source_ref, copy.content_ref));
                } else if !counted(source, &copy.source_ref) {
                    uncounted.push((copy.id, copy.content_ref));
                }
            }
        }
        (uncounted, to_place)
    };
    texts_to_place(store, home, uncounted, to_place, &HashMap::new())
}

/// Put the copy `id`, standing behind another copy of its own day or
/// session (or behind the id of its base, not stored here yet), where the
/// growing rule puts it — in view when it holds the most, the copies it
/// outranks behind it. A retired copy, or one standing behind something
/// outside its record, is left (None). `body`: its text, and `texts` those
/// of the copies it is measured against, read by the caller before it took
/// the store — nothing here reads a file. When its text is missing, it is
/// left. When a copy it is measured against wasn't read (it arrived since):
/// with `fail_unread` (a run's settling, which the next catch-up does
/// again) an error, else it is left. Returns whether where it stands
/// changed.
fn stand_by_rule(
    conn: &Connection,
    id: &str,
    body: Option<&str>,
    texts: &ReadTexts,
    fail_unread: bool,
) -> Result<Option<bool>> {
    let row: Option<(String, String, Option<String>)> = conn
        .query_row(
            "SELECT source, source_ref, superseded_by FROM episodes /* all-episodes: a copy behind another, by design */
             WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((source, source_ref, Some(behind))) = row else {
        return Ok(None);
    };
    if behind.starts_with(super::retire::RETIRED_PREFIX) {
        return Ok(None);
    }
    let Some(body) = body else {
        return Ok(None);
    };
    if let Some(unread) = texts_needed(conn, &source, &source_ref, body)?
        .into_iter()
        .find(|s| !texts.contains_key(&s.id))
    {
        if fail_unread {
            bail!(
                "{id}: the text of {} wasn't read before settling",
                unread.id
            );
        }
        return Ok(None);
    }
    let Some(arriving) = Arriving::read(conn, Texts::Read(texts), &source, &source_ref, body)?
    else {
        return Ok(None);
    };
    // behind another copy of its record — or behind its base's id, the base
    // not having reached this node (an undone copy waits there, and the
    // base arriving later is placed against it by the growing rule)
    if behind != arriving.base_id && !arriving.copies.iter().any(|r| r.stored.id == behind) {
        return Ok(None);
    }
    conn.execute(
        "UPDATE episodes SET superseded_by = NULL WHERE id = ?1",
        [id],
    )?;
    arriving.settle(conn, id, false)?;
    let now: Option<String> = conn.query_row(
        "SELECT superseded_by FROM episodes /* all-episodes: where a copy stands now */ WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    Ok(Some(now.as_deref() != Some(behind.as_str())))
}

/// The copies of a growing source's `base` that a read put back in view on
/// some node (`repair_revived`) and that stand behind another copy here:
/// placed by the growing rule, as that read placed them there. `texts`:
/// their stored texts and those of the copies they are measured against,
/// read before the store was taken ([`settle_texts`]); a text that wasn't
/// read (a copy arrived since) fails the settling, which the run's next
/// catch-up does again. Returns how many were placed.
pub(crate) fn restore_revived(
    conn: &Connection,
    source: &str,
    base: &str,
    texts: &ReadTexts,
) -> Result<usize> {
    if !GROWING_SOURCES.contains(&source) {
        return Ok(0);
    }
    let mut placed = 0;
    for copy in copies_of(conn, source, base)? {
        if copy.superseded_by.is_none() || !super::repair::revived(conn, &copy.id)? {
            continue;
        }
        let Some(text) = texts.get(&copy.id) else {
            bail!("{}: its text wasn't read before settling", copy.id);
        };
        if stand_by_rule(conn, &copy.id, text.as_deref(), texts, true)? == Some(true) {
            placed += 1;
        }
    }
    Ok(placed)
}

/// The most copies one record's lookup gathers (a day or a session has a
/// handful).
const MAX_COPIES: usize = 256;

/// The other stored copies of the record `episode_id` belongs to (none for
/// an episode of any other source) — what retiring it retires with it, and
/// what a private label on it reaches: the copies of its day or session,
/// and the copies one of them was put behind or put behind itself
/// ([`Arriving::settle`] — a Chrome day keyed by its profile and the day
/// stored under the profile's shown name it took the place of), with
/// theirs in turn — also when a retirement took them, its mark replacing
/// the one that said where they stood (read back from its undo record).
pub(crate) fn other_copies(conn: &Connection, episode_id: &str) -> Result<Vec<String>> {
    // a repaired record (store/repair.rs): its base and replacements
    if let Some(copies) = super::repair::other_copies(conn, episode_id)? {
        return Ok(copies);
    }
    let mut row = conn.prepare_cached(
        "SELECT source, source_ref, superseded_by FROM episodes /* all-episodes: one copy of a record */
         WHERE id = ?1",
    )?;
    let mut behind = conn.prepare_cached(
        "SELECT id FROM episodes /* all-episodes: the copies one copy took the place of */
         WHERE superseded_by = ?1 AND source = ?2",
    )?;
    // a retired copy's mark replaced the one it had; its undo record keeps it
    let mut before_retired = conn.prepare_cached(
        "SELECT prior FROM retirement_rows
         WHERE tbl = 'episodes' AND row_key = ?1 AND op = 'updated' AND retirement_id = ?2
         ORDER BY seq LIMIT 1",
    )?;
    // a Chrome day's first line names the profile it was stored under —
    // read whole: a profile's name can be any length
    let mut head = conn.prepare_cached(
        "SELECT substr(f.body, 1, instr(f.body || char(10), char(10)) - 1) FROM episodes e /* all-episodes: one copy of a record */
         JOIN episode_fts f ON f.rowid = e.fts_rowid
         WHERE e.id = ?1",
    )?;
    let mut seen: HashSet<String> = HashSet::from([episode_id.to_string()]);
    let mut frontier = vec![episode_id.to_string()];
    let mut record_source: Option<String> = None;
    let mut out = Vec::new();
    while let Some(id) = frontier.pop() {
        let found: Option<(String, String, Option<String>)> = row
            .query_row([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((source, source_ref, superseded_by)) = found else {
            continue;
        };
        // one record never spans two sources
        if *record_source.get_or_insert_with(|| source.clone()) != source {
            continue;
        }
        let Some((base, _)) = growing_copy(&source, &source_ref) else {
            continue;
        };
        if id != episode_id {
            out.push(id.clone());
            if out.len() >= MAX_COPIES {
                break;
            }
        }
        let mut next: Vec<String> = copies_of(conn, &source, base)?
            .into_iter()
            .map(|c| c.id)
            .collect();
        let ahead = match superseded_by {
            Some(mark) => match mark.strip_prefix(super::retire::RETIRED_PREFIX) {
                Some(rid) => saved_ahead(&mut before_retired, &id, rid)?,
                None => Some(mark),
            },
            None => None,
        };
        next.extend(ahead);
        next.extend(
            behind
                .query_map(params![id, source], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?,
        );
        // a day stored under the profile's shown name that stood behind
        // this one when a retirement took it: its mark no longer names this
        // copy, the undo record does
        let named = match source.as_str() {
            "chrome" => head
                .query_row([&id], |r| r.get::<_, Option<String>>(0))
                .optional()?
                .flatten()
                .and_then(|first| chrome_named_ref(&source_ref, &first)),
            _ => None,
        };
        if let Some(named) = named {
            for copy in copies_of(conn, &source, &named)? {
                let Some(rid) = copy
                    .superseded_by
                    .as_deref()
                    .and_then(|m| m.strip_prefix(super::retire::RETIRED_PREFIX))
                else {
                    continue;
                };
                if saved_ahead(&mut before_retired, &copy.id, rid)?.as_deref() == Some(id.as_str())
                {
                    next.push(copy.id);
                }
            }
        }
        for n in next {
            if seen.insert(n.clone()) {
                frontier.push(n);
            }
        }
    }
    Ok(out)
}

/// The copy a retired episode stood behind before the retirement `rid`
/// took it, from the retirement's undo record (`before_retired` in
/// [`other_copies`]).
fn saved_ahead(
    before_retired: &mut rusqlite::CachedStatement<'_>,
    id: &str,
    rid: &str,
) -> Result<Option<String>> {
    Ok(before_retired
        .query_row(params![id, rid], |r| r.get::<_, String>(0))
        .optional()?
        .and_then(|prior| serde_json::from_str::<serde_json::Value>(&prior).ok())
        .and_then(|prior| prior["superseded_by"].as_str().map(String::from))
        .filter(|s| !s.starts_with(super::retire::RETIRED_PREFIX)))
}

/// A private label holds for the whole record: after `ids` were labelled
/// (or landed) private, every other copy of their records
/// ([`other_copies`]) that isn't private takes the label with the same
/// stamp — unless that copy was labelled since. A copy private already
/// under an older stamp takes the newer one, so a later label sent for it
/// alone is measured against the record's latest decision. A repaired
/// record (a memory and the corrected copies that replaced it, store/
/// repair.rs) is one record the same way, and so is a photo day (its base
/// and its revisions, store/photo.rs). Ids of other sources, and ones
/// not private now, are passed over; a private label with
/// no stamp spreads stamped now, to copies that aren't private only (a
/// stamp of its own would change on every call). A label stamped
/// [`UNKNOWN_TIME`] reaches only copies with no stamp. The copies relabelled
/// carry the stamp, so the scope leg takes the label to every node holding
/// one of them. Only ever moves copies into the vault. Returns how many
/// copies it relabelled or restamped.
pub(crate) fn spread_private(conn: &Connection, actor: &Actor, ids: &[String]) -> Result<usize> {
    let mut label = conn.prepare_cached(
        "SELECT source, scope, scope_changed_at FROM episodes /* all-episodes: a private label holds on every copy */
         WHERE id = ?1",
    )?;
    // `scope` stays in the SET list: the relay stamp follows it
    let mut relabel = conn.prepare_cached(
        "UPDATE episodes SET scope = 'private', scope_changed_at = ?2 /* all-episodes: a private label holds on every copy */
         WHERE id = ?1 AND COALESCE(scope_changed_at, '') <= ?2
           AND (scope != 'private' OR (?3 AND COALESCE(scope_changed_at, '') < ?2))",
    )?;
    let mut total = 0;
    for id in ids {
        let found: Option<(String, String, Option<String>)> = label
            .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let Some((source, scope, stamp)) = found else {
            continue;
        };
        if scope != "private" {
            continue;
        }
        let Some(copies) = label_reaches(conn, &source, id)? else {
            continue;
        };
        let stamped = stamp.is_some();
        let stamp = stamp.unwrap_or_else(now_utc);
        let mut relabeled = Vec::new();
        for copy in copies {
            if relabel.execute(params![copy, stamp, stamped])? > 0 {
                relabeled.push(copy);
            }
        }
        if !relabeled.is_empty() {
            total += relabeled.len();
            // keyed by the copies too: a later spread of the same label
            // reaching copies that joined the record since is a write of
            // its own, recorded as one
            record_mutation(
                conn,
                actor,
                "scope_private_copies",
                &format!(
                    "scope_private_copies:{id}:{stamp}:{}",
                    crate::ids::content_hash(relabeled.join("\n").as_bytes())
                ),
                &serde_json::json!({"from": id, "copies": relabeled, "scope": "private"}),
            )?;
        }
    }
    Ok(total)
}

/// The other copies of `id`'s record a private label reaches — a photo
/// day's base and revisions (store/photo.rs), else [`other_copies`]; `None`
/// for a source whose records have one copy.
fn label_reaches(conn: &Connection, source: &str, id: &str) -> Result<Option<Vec<String>>> {
    if source == "photos" {
        return Ok(Some(
            super::photo::other_day_copies(conn, id)?.unwrap_or_default(),
        ));
    }
    let has_copies =
        GROWING_SOURCES.contains(&source) || super::repair::REPAIRABLE_SOURCES.contains(&source);
    Ok(if has_copies {
        Some(other_copies(conn, id)?)
    } else {
        None
    })
}

/// The stamp of a private label whose time isn't known: a peer's copy
/// private by its sender's label (the episode leg carries a label without
/// its stamp), or by a label here that has none, and the copies it reaches.
/// It sorts below every real stamp, so any decision outranks it — the
/// sender's own, which the scope leg brings, or one about another copy —
/// and above no stamp at all, so among labels with no time of their own
/// private holds. Being a stamp, it travels on the scope leg to the nodes
/// that hold those copies with no stamp.
pub(crate) const UNKNOWN_TIME: &str = "0000-01-01T00:00:00Z";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::time::{EventTime, TimeUncertainty};

    fn chrome_day(source_ref: &str, name: &str, visits: &[&str]) -> EpisodeDraft {
        let date = "2026-11-10";
        let mut body = format!(
            "browsing ({name}) on {date}: {} visits\n\ntop sites: example.com ({})\n\n",
            visits.len(),
            visits.len()
        );
        for v in visits {
            body.push_str(&format!("[{v}] Example — example.com\n"));
        }
        EpisodeDraft {
            source: "chrome".into(),
            source_ref: source_ref.into(),
            kind: "browsing".into(),
            title: format!("Browsing ({name})"),
            body,
            event_time: EventTime {
                utc: Some(format!("{date}T{}:00Z", visits[0])),
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Hour,
            },
            trust: "self".into(),
            scope: None,
            entities: vec![],
            edges: vec![],
        }
    }

    fn apply(store: &Store, home: &Home, actor: &str, days: Vec<EpisodeDraft>) {
        store
            .apply_batch(
                home,
                &IngestBatch {
                    actor: actor.into(),
                    session_id: None,
                    episodes: days,
                },
            )
            .unwrap();
    }

    fn live(store: &Store) -> Vec<String> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT source_ref FROM episodes
                 WHERE source = 'chrome' AND superseded_by IS NULL ORDER BY source_ref",
            )
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<std::result::Result<_, _>>().unwrap()
    }

    /// A day stored under a profile's shown name is replaced by the
    /// profile-keyed day that holds all of it — in either arrival order —
    /// and stays in view beside a same-named profile's day that doesn't,
    /// on every node; a private one keeps the replacement private.
    #[test]
    fn test_days_stored_under_a_name_give_way_only_to_days_holding_them() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        // two Macs stored a "Work" day under the same name; each node
        // keeps the text that reached it first
        let mac_one = chrome_day("Work#2026-11-10", "Work", &["09:00", "10:00"]);
        let mac_two = chrome_day("Work#2026-11-10", "Work", &["13:00"]);
        let one_now = chrome_day("profile:1#2026-11-10", "Work", &["09:00", "10:00", "11:00"]);
        let two_now = chrome_day("profile:2#2026-11-10", "Work", &["13:00", "14:00"]);
        for (first, mine, theirs) in [
            (&mac_one, &one_now, &two_now),
            (&mac_two, &two_now, &one_now),
        ] {
            let store = Store::open_in_memory().unwrap();
            apply(&store, &home, "adapter:chrome", vec![first.clone()]);
            apply(&store, &home, "adapter:chrome", vec![mine.clone()]);
            apply(&store, &home, "peer:other", vec![theirs.clone()]);
            assert_eq!(
                live(&store),
                ["profile:1#2026-11-10", "profile:2#2026-11-10"]
            );
        }
        // a node that holds the named day of the other Mac shows both
        let store = Store::open_in_memory().unwrap();
        apply(&store, &home, "peer:other", vec![mac_two.clone()]);
        apply(&store, &home, "adapter:chrome", vec![one_now.clone()]);
        assert_eq!(live(&store), ["Work#2026-11-10", "profile:1#2026-11-10"]);

        // the named day arriving after the day that holds it
        let store = Store::open_in_memory().unwrap();
        apply(&store, &home, "peer:other", vec![one_now.clone()]);
        apply(&store, &home, "peer:other", vec![mac_one.clone()]);
        assert_eq!(live(&store), ["profile:1#2026-11-10"]);

        // private stays private
        let store = Store::open_in_memory().unwrap();
        let mut private = mac_one.clone();
        private.scope = Some("private".into());
        apply(&store, &home, "adapter:chrome", vec![private]);
        apply(&store, &home, "adapter:chrome", vec![one_now.clone()]);
        let scope: String = store
            .lock()
            .query_row(
                "SELECT scope FROM episodes WHERE source_ref = 'profile:1#2026-11-10'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(scope, "private");
    }

    fn superseded(store: &Store, source_ref: &str) -> Option<String> {
        store
            .lock()
            .query_row(
                "SELECT superseded_by FROM episodes WHERE source = 'chrome' AND source_ref = ?1",
                [source_ref],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Retiring a day stored under a profile's shown name retires the
    /// profile's day that took its place — whether the retirement comes
    /// after that day, before it, or before the named day itself arrives
    /// (from a node that never held the profile's day).
    #[test]
    fn test_retiring_a_named_day_retires_the_profile_day_that_replaced_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let (named, profile) = ("Work#2026-11-10", "profile:1#2026-11-10");
        let named_day = chrome_day(named, "Work", &["09:00", "10:00"]);
        let profile_day = chrome_day(profile, "Work", &["09:00", "10:00", "11:00"]);
        let named_id = crate::ids::episode_id("chrome", named);
        let actor = Actor::new("human:test");

        // the profile's day here already
        let store = Store::open_in_memory().unwrap();
        apply(&store, &home, "adapter:chrome", vec![named_day.clone()]);
        apply(&store, &home, "adapter:chrome", vec![profile_day.clone()]);
        assert_eq!(live(&store), [profile]);
        let r = store.retire(&actor, "memory", &named_id, &[], "").unwrap();
        store.retirement_apply(&r.id).unwrap();
        let mark = Some(format!("retired:{}", r.id));
        assert_eq!(superseded(&store, profile), mark);
        assert_eq!(superseded(&store, named), mark);
        store.retirement_undo(&actor, &r.id).unwrap();
        assert_eq!(live(&store), [profile]);

        // the retirement first
        let store = Store::open_in_memory().unwrap();
        apply(&store, &home, "adapter:chrome", vec![named_day.clone()]);
        let r = store.retire(&actor, "memory", &named_id, &[], "").unwrap();
        store.retirement_apply(&r.id).unwrap();
        apply(&store, &home, "adapter:chrome", vec![profile_day.clone()]);
        assert_eq!(
            superseded(&store, profile),
            Some(format!("retired:{}", r.id))
        );

        // the retirement before the named day, which a peer sends after
        let older = Store::open_in_memory().unwrap();
        apply(&older, &home, "adapter:chrome", vec![named_day.clone()]);
        let r = older.retire(&actor, "memory", &named_id, &[], "").unwrap();
        let store = Store::open_in_memory().unwrap();
        apply(&store, &home, "adapter:chrome", vec![profile_day.clone()]);
        store
            .retirements_sync_apply(&older.retirements_sync_export("").unwrap())
            .unwrap();
        store.retirement_apply(&r.id).unwrap();
        apply(&store, &home, "peer:older", vec![named_day.clone()]);
        let mark = Some(format!("retired:{}", r.id));
        assert_eq!(superseded(&store, named), mark);
        assert_eq!(superseded(&store, profile), mark);
    }

    /// A private label given to a retired profile day reaches the day
    /// stored under the profile's name that stood behind it when the
    /// retirement took both — however long the profile's name.
    #[test]
    fn test_a_private_label_reaches_a_retired_day_behind_it() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        for name in ["Work".to_string(), "W".repeat(1100)] {
            let named = format!("{name}#2026-11-10");
            let profile = "profile:1#2026-11-10";
            let store = Store::open_in_memory().unwrap();
            let named_day = chrome_day(&named, &name, &["09:00", "10:00"]);
            let profile_day = chrome_day(profile, &name, &["09:00", "10:00", "11:00"]);
            apply(&store, &home, "adapter:chrome", vec![named_day]);
            apply(&store, &home, "adapter:chrome", vec![profile_day]);
            let (named_id, profile_id) = (
                crate::ids::episode_id("chrome", &named),
                crate::ids::episode_id("chrome", profile),
            );
            let actor = Actor::new("human:test");
            let r = store
                .retire(&actor, "memory", &profile_id, &[], "")
                .unwrap();
            store.retirement_apply(&r.id).unwrap();
            let mark = Some(format!("retired:{}", r.id));
            assert_eq!(superseded(&store, &named), mark);
            assert_eq!(superseded(&store, profile), mark);
            assert_eq!(
                other_copies(&store.lock(), &profile_id).unwrap(),
                std::slice::from_ref(&named_id),
                "{} characters",
                name.len()
            );
            store
                .set_episode_scopes(&actor, std::slice::from_ref(&profile_id), "private")
                .unwrap();
            let scope: String = store
                .lock()
                .query_row(
                    "SELECT scope FROM episodes WHERE id = ?1",
                    [&named_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(scope, "private");
        }
    }

    fn scope_of(store: &Store, source_ref: &str) -> (String, Option<String>) {
        store
            .lock()
            .query_row(
                "SELECT scope, scope_changed_at FROM episodes
                 WHERE source = 'chrome' AND source_ref = ?1",
                [source_ref],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    /// A peer's copy that arrives private carries its sender's label, not
    /// a stamp of this node's: it doesn't take a copy the owner later took
    /// out of the vault back in. The sender's stamp, on the scope leg,
    /// decides — a later one does take the record in.
    #[test]
    fn test_a_peer_copy_private_by_its_label_keeps_a_later_decision() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let (held, sent) = ("profile:1#2026-11-10#r2", "profile:1#2026-11-10#r3");
        let store = Store::open_in_memory().unwrap();
        apply(
            &store,
            &home,
            "adapter:chrome",
            vec![chrome_day(held, "Work", &["09:00", "10:00"])],
        );
        // taken out of the vault by the owner, after the sender's copy
        // went in
        store
            .lock()
            .execute(
                "UPDATE episodes SET scope = 'personal', scope_changed_at = '2026-11-12T00:00:00Z'
                 WHERE source_ref = ?1",
                [held],
            )
            .unwrap();
        let mut copy = chrome_day(sent, "Work", &["09:00", "10:00", "11:00"]);
        copy.scope = Some("private".into());
        apply(&store, &home, "peer:other", vec![copy]);
        assert_eq!(
            scope_of(&store, sent),
            ("private".to_string(), Some(UNKNOWN_TIME.to_string()))
        );
        assert_eq!(scope_of(&store, held).0, "personal");

        let sent_id = crate::ids::episode_id("chrome", sent);
        let actor = Actor::new("peer:other");
        let label = |at: &str| vec![(sent_id.clone(), "private".to_string(), at.to_string())];
        store
            .apply_scope_changes(&actor, &label("2026-11-11T00:00:00Z"))
            .unwrap();
        assert_eq!(scope_of(&store, held).0, "personal");
        store
            .apply_scope_changes(&actor, &label("2026-11-13T00:00:00Z"))
            .unwrap();
        assert_eq!(
            scope_of(&store, held),
            (
                "private".to_string(),
                Some("2026-11-13T00:00:00Z".to_string())
            )
        );
    }

    /// A copy a peer sends, with when it was first written.
    fn from_peer(store: &Store, home: &Home, day: EpisodeDraft, written: &str) {
        let origin = super::super::provenance::Origin {
            source: day.source.clone(),
            source_ref: day.source_ref.clone(),
            writer: Some("adapter:chrome".into()),
            at: Some(written.into()),
            ..Default::default()
        };
        store
            .apply_batch_from(
                home,
                &IngestBatch {
                    actor: "peer:other".into(),
                    session_id: None,
                    episodes: vec![day],
                },
                &[origin],
            )
            .unwrap();
    }

    /// A peer's copy private by its sender's label alone (the episode leg
    /// carries no stamp; an earlier version labelled without one, and an
    /// older peer sends no origin) takes the fuller copy here, outside the
    /// vault with no stamp either, into the vault — stamped as of an
    /// unknown time, so a real decision about it still outranks the label.
    #[test]
    fn test_a_peer_copy_private_without_a_stamp_takes_the_record_in() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let (fuller, sent) = ("profile:1#2026-11-10#r3", "profile:1#2026-11-10#r2");
        for written in [None, Some("2026-01-01T00:00:00Z")] {
            let store = Store::open_in_memory().unwrap();
            apply(
                &store,
                &home,
                "adapter:chrome",
                vec![chrome_day(fuller, "Work", &["09:00", "10:00", "11:00"])],
            );
            assert_eq!(scope_of(&store, fuller), ("personal".to_string(), None));
            let mut copy = chrome_day(sent, "Work", &["09:00", "10:00"]);
            copy.scope = Some("private".into());
            match written {
                Some(at) => from_peer(&store, &home, copy, at),
                None => apply(&store, &home, "peer:other", vec![copy]),
            }
            assert_eq!(live(&store), [fuller]);
            for copy in [fuller, sent] {
                assert_eq!(
                    scope_of(&store, copy),
                    ("private".to_string(), Some(UNKNOWN_TIME.to_string())),
                    "{copy}, origin {written:?}"
                );
            }
            let out = "2026-01-02T00:00:00Z";
            store
                .apply_scope_changes(
                    &Actor::new("peer:other"),
                    &[(
                        crate::ids::episode_id("chrome", fuller),
                        "personal".to_string(),
                        out.to_string(),
                    )],
                )
                .unwrap();
            assert_eq!(
                scope_of(&store, fuller),
                ("personal".to_string(), Some(out.to_string()))
            );
        }
    }

    /// A peer's copy made after its day was put into the vault inherits
    /// that older label. It doesn't take back a copy here the owner took
    /// out of the vault in between: it lands with no time of its own, and
    /// the sender's stamp, on the scope leg, is older than the owner's
    /// decision.
    #[test]
    fn test_a_peer_copy_with_an_older_label_keeps_a_decision_made_since() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let (held, sent) = ("profile:1#2026-11-10#r2", "profile:1#2026-11-10#r3");
        let (vaulted, taken_out, made) = (
            "2026-01-01T00:00:00Z",
            "2026-01-02T00:00:00Z",
            "2026-01-03T00:00:00Z",
        );
        let store = Store::open_in_memory().unwrap();
        apply(
            &store,
            &home,
            "adapter:chrome",
            vec![chrome_day(held, "Work", &["09:00", "10:00"])],
        );
        store
            .lock()
            .execute(
                "UPDATE episodes SET scope = 'personal', scope_changed_at = ?2 WHERE source_ref = ?1",
                params![held, taken_out],
            )
            .unwrap();
        let mut copy = chrome_day(sent, "Work", &["09:00", "10:00", "11:00"]);
        copy.scope = Some("private".into());
        from_peer(&store, &home, copy, made);
        store
            .apply_scope_changes(
                &Actor::new("peer:other"),
                &[(
                    crate::ids::episode_id("chrome", sent),
                    "private".to_string(),
                    vaulted.to_string(),
                )],
            )
            .unwrap();
        assert_eq!(
            scope_of(&store, held),
            ("personal".to_string(), Some(taken_out.to_string()))
        );
        assert_eq!(
            scope_of(&store, sent),
            ("private".to_string(), Some(vaulted.to_string()))
        );
    }

    /// On a node holding none of the day yet, a peer's copies arriving one
    /// after the other — the first private, the next private by the first
    /// alone — take no stamp later than their sender's decisions, so the
    /// scope leg's stamps decide: the owner took the fuller copy out of the
    /// vault after the first went in.
    #[test]
    fn test_peer_copies_arriving_in_turn_keep_their_senders_decisions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let (first, fuller) = ("profile:1#2026-11-10#r2", "profile:1#2026-11-10#r3");
        let store = Store::open_in_memory().unwrap();
        let mut copy = chrome_day(first, "Work", &["09:00", "10:00"]);
        copy.scope = Some("private".into());
        from_peer(&store, &home, copy, "2026-01-01T00:00:00Z");
        let mut copy = chrome_day(fuller, "Work", &["09:00", "10:00", "11:00"]);
        copy.scope = Some("personal".into());
        from_peer(&store, &home, copy, "2026-01-01T01:00:00Z");

        let id = |r: &str| crate::ids::episode_id("chrome", r);
        let (private_at, out_at) = ("2026-01-02T00:00:00Z", "2026-01-03T00:00:00Z");
        store
            .apply_scope_changes(
                &Actor::new("peer:other"),
                &[
                    (id(first), "private".to_string(), private_at.to_string()),
                    (id(fuller), "personal".to_string(), out_at.to_string()),
                ],
            )
            .unwrap();
        assert_eq!(
            scope_of(&store, fuller),
            ("personal".to_string(), Some(out_at.to_string()))
        );
        assert_eq!(
            scope_of(&store, first),
            ("private".to_string(), Some(private_at.to_string()))
        );
    }

    /// Every copy a private label reaches is in the mutations log — also
    /// one that a later spread of the same label, with the same stamp,
    /// reaches after it joined the record.
    #[test]
    fn test_every_copy_a_private_label_reaches_is_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let (one, two, three) = (
            "profile:1#2026-11-10#r1",
            "profile:1#2026-11-10#r2",
            "profile:1#2026-11-10#r3",
        );
        apply(
            &store,
            &home,
            "adapter:chrome",
            vec![
                chrome_day(one, "Work", &["09:00"]),
                chrome_day(two, "Work", &["09:00", "10:00"]),
            ],
        );
        let id = |r: &str| crate::ids::episode_id("chrome", r);
        let actor = Actor::new("human:test");
        let stamp = "2026-11-12T00:00:00Z";
        let spread = |store: &Store| {
            let conn = store.lock();
            spread_private(&conn, &actor, &[id(two)]).unwrap()
        };
        store
            .lock()
            .execute(
                "UPDATE episodes SET scope = 'private', scope_changed_at = ?2 WHERE id = ?1",
                params![id(two), stamp],
            )
            .unwrap();
        assert_eq!(spread(&store), 1);
        // a copy of the record outside the vault, as a peer's older build
        // can leave one
        apply(
            &store,
            &home,
            "adapter:chrome",
            vec![chrome_day(three, "Work", &["09:00", "10:00", "11:00"])],
        );
        store
            .lock()
            .execute(
                "UPDATE episodes SET scope = 'personal', scope_changed_at = NULL WHERE id = ?1",
                [id(three)],
            )
            .unwrap();
        assert_eq!(spread(&store), 1);
        let logged: Vec<String> = {
            let conn = store.lock();
            let mut stmt = conn
                .prepare("SELECT payload FROM mutations WHERE kind = 'scope_private_copies'")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<std::result::Result<_, _>>().unwrap()
        };
        for copy in [id(one), id(three)] {
            assert!(
                logged.iter().any(|p| p.contains(&copy)),
                "{copy} not in {logged:?}"
            );
        }
    }

    /// A Chrome day with this header count and these visit lines.
    fn chrome_text(count: usize, lines: &[&str]) -> String {
        let mut body = format!(
            "browsing (Work) on 2026-11-10: {count} visits\n\ntop sites: example.com ({count})\n\n"
        );
        for l in lines {
            body.push_str(l);
            body.push('\n');
        }
        body
    }

    /// Lines alone undercount a day — consecutive visits under one title
    /// share a line, a long day stops at `[truncated]` — so a day holds
    /// another only with at least its count too.
    #[test]
    fn test_a_day_holds_another_only_with_its_visits_counted() {
        let one = "[09:00] Example — example.com";
        let two = "[10:00] Other — example.org";
        // two visits under one title: one line, like a single visit's
        let single = chrome_text(1, &[one]);
        let repeated = chrome_text(2, &[one]);
        assert!(chrome_day_holds(&repeated, &single));
        assert!(!chrome_day_holds(&single, &repeated));
        // a long day cut at the cap holds more than its lines show: what
        // another long day shows past the cut may be among them, what it
        // shows must be; a short day, shown whole, only where each of its
        // lines shows
        let cut = chrome_text(40, &[one, "[truncated]"]);
        let shorter = chrome_text(3, &[one, two]);
        assert!(!chrome_day_holds(&cut, &shorter));
        assert!(!chrome_day_holds(&shorter, &cut));
        assert!(chrome_day_holds(
            &cut,
            &chrome_text(39, &[one, two, "[truncated]"])
        ));
        assert!(chrome_day_holds(
            &chrome_text(41, &[one, two, "[truncated]"]),
            &cut
        ));
        let later = "[11:00] Later — example.net";
        assert!(!chrome_day_holds(
            &chrome_text(40, &[one, later, "[truncated]"]),
            &shorter
        ));
        // no readable count: holds nothing, held by nothing
        let uncounted = format!("browsing (Work) on 2026-11-10: many visits\n\n{one}\n");
        assert!(!chrome_day_holds(&uncounted, &single));
        assert!(!chrome_day_holds(&repeated, &uncounted));

        // a named day of two visits under one title arriving after a
        // profile day of one visit stays in view beside it
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut smaller = chrome_day("profile:1#2026-11-10", "Work", &["09:00"]);
        smaller.body = chrome_text(1, &[one]);
        let mut richer = chrome_day("Work#2026-11-10", "Work", &["09:00"]);
        richer.body = repeated.clone();
        apply(&store, &home, "adapter:chrome", vec![smaller]);
        apply(&store, &home, "peer:other", vec![richer]);
        assert_eq!(live(&store), ["Work#2026-11-10", "profile:1#2026-11-10"]);
    }

    /// A long day shows its first and latest lines: a day holds another
    /// whose lines in between it leaves out, but not one with a line it
    /// shows otherwise, nor one that opens differently. A title an earlier
    /// version wrote across lines reads as the one-line title written now,
    /// and a line inside such a title is no visit of its own.
    #[test]
    fn test_a_day_holds_lines_where_it_leaves_lines_out() {
        let first = "[08:00] Start — example.com";
        let middle = "[12:00] Middle — example.com";
        let latest = "[20:00] Latest — example.com";
        let fitted = chrome_text(90, &[first, "[57 lines not shown]", latest]);
        let earlier = chrome_text(60, &[first, middle, "[truncated]"]);
        assert!(chrome_day_holds(&fitted, &earlier));
        let otherwise = chrome_text(
            60,
            &[
                first,
                "[21:00] Not shown there — example.org",
                "[truncated]",
            ],
        );
        assert!(!chrome_day_holds(&fitted, &otherwise));
        let opens_otherwise = chrome_text(3, &["[13:00] Elsewhere — example.org"]);
        assert!(!chrome_day_holds(&fitted, &opens_otherwise));
        // another profile's short day under the same name, opening with the
        // same visit: shown whole, its visits where the long day leaves
        // lines out are no visits of the long day's — in either direction,
        // and in the store too
        let other_profile = chrome_text(2, &[first, "[12:00] Elsewhere — example.org"]);
        assert!(!chrome_day_holds(&fitted, &other_profile));
        assert!(!chrome_day_holds(
            &chrome_text(90, &[first, "[truncated]"]),
            &other_profile
        ));
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut named = chrome_day("Work#2026-11-10", "Work", &["08:00"]);
        named.body = other_profile;
        let mut long = chrome_day("profile:1#2026-11-10", "Work", &["08:00"]);
        long.body = fitted.clone();
        apply(&store, &home, "adapter:chrome", vec![named]);
        apply(&store, &home, "adapter:chrome", vec![long]);
        assert_eq!(live(&store), ["Work#2026-11-10", "profile:1#2026-11-10"]);

        // a title across lines, as earlier versions wrote it
        let split = chrome_text(2, &[first, "[09:00] Two", "  lines — example.com"]);
        let one_line = chrome_text(2, &[first, "[09:00] Two lines — example.com"]);
        assert!(chrome_day_holds(&one_line, &split));
        assert!(chrome_day_holds(&split, &one_line));
        // a title holding a line shaped like a visit
        let shaped = chrome_text(
            3,
            &[
                first,
                "[09:00] Two",
                "[10:00] Made up — example.com — example.com",
            ],
        );
        let flat = chrome_text(
            3,
            &[
                first,
                "[09:00] Two [10:00] Made up — example.com — example.com",
            ],
        );
        assert!(!chrome_day_holds(&flat, &shaped));
        assert!(!chrome_day_holds(&shaped, &flat));
    }

    /// The lookups an import makes per day or session stay on indexes
    /// that narrow them to the day or session — no table is scanned, and
    /// no index is walked over a whole source, however large the record or
    /// its provenance.
    #[test]
    fn test_copy_lookups_use_indexes() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.lock();
        for (sql, narrowing) in [
            (
                format!(
                    "SELECT {STORED_COLUMNS} FROM episodes
                     WHERE source = ?1 AND (source_ref = ?2 OR {})",
                    growing_revision_sql(2)
                ),
                "sqlite_autoindex_episodes",
            ),
            (profile_days_sql(), "idx_episodes_event_time"),
            (
                "SELECT id FROM episodes WHERE superseded_by = ?1 AND source = ?2".to_string(),
                "idx_episodes_superseded",
            ),
        ] {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let args = vec!["x"; stmt.parameter_count()];
            let plan: Vec<String> = stmt
                .query_map(rusqlite::params_from_iter(args), |r| r.get(3))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            let reads: Vec<&String> = plan
                .iter()
                .filter(|step| step.starts_with("SEARCH") || step.starts_with("SCAN"))
                .collect();
            assert!(
                !reads.is_empty()
                    && reads
                        .iter()
                        .all(|step| step.starts_with("SEARCH") && step.contains(narrowing)),
                "{sql}\n{plan:?}"
            );
        }
    }

    #[test]
    fn test_counts_read_from_text() {
        // the count each adapter writes first
        assert_eq!(
            growing_items("chrome", "browsing (Work) on 2026-03-15: 12 visits\n\n"),
            Some(12)
        );
        assert_eq!(
            growing_items(
                "imessage",
                "messages: 40\nchat: x\ndate: 2026-03-15\n\n[10:00] a: b\n"
            ),
            Some(40)
        );
        assert_eq!(
            growing_items(
                "claude-code",
                "claude code session s\nturns: 9\nworkspace: w\n\nyou: a\n\n"
            ),
            Some(9)
        );
        // text stored before the count was written: its item lines
        assert_eq!(
            growing_items(
                "imessage",
                "chat: x\ndate: 2026-03-15\n\n[10:00] a: b\n[10:01] me: c\nmore\n"
            ),
            Some(2)
        );
        assert_eq!(
            growing_items(
                "claude-code",
                "claude code session s\nworkspace: w\n\nyou: a\n\nclaude: b\n\nyou: c\n\n"
            ),
            Some(3)
        );
        // a message holding lines shaped like messages (a pasted log)
        // weighs the same in a text stored before the count line and in
        // one read since: the read holding one more counts above it
        let pasted: String = (0..400)
            .map(|i| format!("\n[10:{:02}] x: y", i % 60))
            .collect();
        let before = format!("chat: x\ndate: d\n\n[09:00] a: look{pasted}\n[11:00] me: ok\n");
        let since = format!(
            "messages: 3\nchat: x\ndate: d\n\n[09:00] a: look{pasted}\n[11:00] me: ok\n\
             [12:00] a: one more\n"
        );
        assert_eq!(growing_items("imessage", &before), Some(402));
        assert_eq!(growing_items("imessage", &since), Some(403));
        // a count line further down is message text, not the count
        assert_eq!(
            growing_items(
                "imessage",
                "chat: x\nmessages: 999\ndate: d\n\n[10:00] a: b\n"
            ),
            Some(1)
        );
        assert_eq!(
            growing_items("chrome", "browsing (Work) on d: many visits"),
            None
        );
        assert_eq!(growing_items("mbox", "messages: 4"), None);
    }

    #[test]
    fn test_chrome_named_refs_and_holding() {
        let day = "browsing (Work) on 2026-03-15) on 2026-03-15: x) on 2026-03-15: 2 visits\n\n\
                   top sites: a (2)\n\n[09:00] A — a\n[10:00] B — a\n";
        assert_eq!(
            chrome_named_ref("profile:1#2026-03-15", day).as_deref(),
            Some("Work) on 2026-03-15) on 2026-03-15: x#2026-03-15")
        );
        assert_eq!(
            chrome_named_ref("profile:1#2026-03-15#r4", day).as_deref(),
            Some("Work) on 2026-03-15) on 2026-03-15: x#2026-03-15")
        );
        assert_eq!(chrome_named_ref("Work#2026-03-15", day), None);
        assert_eq!(chrome_named_ref("profile:1#2026-03-15", "chat: x"), None);

        let fewer = "browsing (Work) on 2026-03-15: 1 visits\n\n[09:00] A — a\n";
        let other = "browsing (Work) on 2026-03-15: 1 visits\n\n[11:00] C — a\n";
        assert!(chrome_day_holds(day, fewer));
        assert!(!chrome_day_holds(fewer, day));
        assert!(!chrome_day_holds(day, other));
    }
}
