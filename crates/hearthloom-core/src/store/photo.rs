//! The photo-location union store (v47) — the durable per-photo record
//! both sources feed: the Mac Photos.sqlite scan (daemon-side ingest)
//! and the iOS geotag stream (POST /v1/ingest/photos). Keyed on
//! `<unix-capture-secs>|<filename>` — the identity visible to BOTH — so
//! the two UNION instead of duplicating.
//!
//! `photo_day` is the minted ledger: what each `photos#DAY` episode
//! contains (count + keys hash). The finalize pass re-mints a day under
//! the NEXT revision ref (`photos#DAY#r2`, the supersession pattern in
//! CLAUDE.md) only when the local union STRICTLY GROWS the minted set —
//! a points-poor node (the tenant before its first sync) can never
//! clobber a rich episode with a subset.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;

use super::Store;
use crate::time::now_utc;

/// One incoming photo point (the HTTP layer deserializes into this;
/// the daemon also converts the Mac scan's records to it).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PhotoPointIn {
    pub key: String,
    pub filename: String,
    /// RFC3339 UTC capture instant.
    pub taken_at: String,
    #[serde(default)]
    pub tz_offset_minutes: i32,
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    #[serde(default)]
    pub place: Option<String>,
    /// The key this node's own Mac scan gave the photo before it keyed
    /// photos by their original file name (the library's own name for the
    /// file). A point stored under it moves to `key` (see
    /// [`Store::photos_add_points`]). Set only by the daemon's scan; never
    /// read from a request.
    #[serde(skip)]
    pub earlier_key: Option<String>,
}

/// Two keys of the same photo name the same capture second.
fn same_second(a: &str, b: &str) -> bool {
    matches!((a.split_once('|'), b.split_once('|')), (Some((x, _)), Some((y, _))) if x == y)
}

/// A point here took the place of `key`: it is not stored again — as a
/// point, or as a location observation.
pub(crate) fn replaced(conn: &rusqlite::Connection, key: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM photo_replaced WHERE key = ?1)",
        [key],
        |r| r.get(0),
    )?)
}

/// `consolidation_state` key: the highest export position a removed point
/// held.
const POSITION_FLOOR: &str = "photo_point.removed_position";

/// The next position in the export order (the leg pages by rowid): past
/// every point stored here and every point ever removed — a peer's cursor
/// may already be past a removed point's place, so it is never handed out
/// again.
fn next_position(conn: &rusqlite::Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT MAX(COALESCE((SELECT MAX(rowid) FROM photo_point), 0),
                    COALESCE((SELECT CAST(value AS INTEGER) FROM consolidation_state
                              WHERE key = ?1), 0)) + 1",
        [POSITION_FLOOR],
        |r| r.get(0),
    )?)
}

/// Record that the point under `key` took the place of `earlier`. A new
/// record moves that point to the end of the export order, so peers learn
/// every key it replaced; one recorded already changes nothing.
fn record_replaced(conn: &rusqlite::Connection, earlier: &str, key: &str) -> Result<()> {
    let new = conn.execute(
        "INSERT OR IGNORE INTO photo_replaced (key, by_key) VALUES (?1, ?2)",
        rusqlite::params![earlier, key],
    )?;
    if new > 0 {
        let position = next_position(conn)?;
        conn.execute(
            "UPDATE photo_point SET rowid = ?2 WHERE key = ?1",
            rusqlite::params![key, position],
        )?;
    }
    Ok(())
}

/// The point under `earlier`, and its observation, gone: another key
/// holds the photo now.
fn drop_point(conn: &rusqlite::Connection, earlier: &str) -> Result<()> {
    let position: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM photo_point WHERE key = ?1",
            [earlier],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(position) = position {
        conn.execute(
            "INSERT INTO consolidation_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET
                 value = MAX(CAST(value AS INTEGER), CAST(excluded.value AS INTEGER))",
            rusqlite::params![POSITION_FLOOR, position.to_string()],
        )?;
        conn.execute("DELETE FROM photo_point WHERE key = ?1", [earlier])?;
    }
    conn.execute(
        "DELETE FROM location_observation WHERE id = 'photos|' || ?1",
        [earlier],
    )?;
    Ok(())
}

/// A stored point as its day's minted ledger sees it: (the day, whether
/// the day was minted with it — the point stored no later than the day's
/// mint; a day mints from every point stored at the time).
type Entry = (String, bool);

/// The point under `key`, as an [`Entry`] (`None` when none is stored).
fn point_entry(conn: &rusqlite::Connection, key: &str) -> Result<Option<Entry>> {
    Ok(conn
        .query_row(
            "SELECT pp.day, COALESCE(pp.ingest_time <= pd.minted_at, 0)
             FROM photo_point pp LEFT JOIN photo_day pd ON pd.day = pp.day
             WHERE pp.key = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// One photo's points (`before`) are one point now, the one under `key`
/// (`kept`: its entry; `None` when none is stored). A day minted with n of
/// them counted the photo n times and holds it once now — or not at all,
/// when the point kept is in another day — so its count drops by what it
/// counted beyond that, and the day re-mints as soon as its union really
/// grows. Points stored after a day was minted were never in its count.
/// A count adopted from an episode another node minted (`legacy`) stays:
/// what that count held isn't known here.
///
/// The point kept holds the photo's place in that count from now on: one
/// stored after the mint is dated to the mint, so a later merge of the
/// same photo finds the day counted it.
fn ledger_after_merge(
    conn: &rusqlite::Connection,
    before: impl IntoIterator<Item = Entry>,
    key: &str,
    kept: Option<Entry>,
) -> Result<()> {
    let mut counted: std::collections::BTreeMap<String, i64> = Default::default();
    for (day, minted) in before {
        if minted {
            *counted.entry(day).or_default() += 1;
        }
    }
    if let Some((day, minted)) = kept {
        if let Some(n) = counted.get_mut(&day) {
            *n -= 1;
            if !minted {
                conn.execute(
                    "UPDATE photo_point
                     SET ingest_time = (SELECT minted_at FROM photo_day WHERE day = ?2)
                     WHERE key = ?1 AND day = ?2",
                    rusqlite::params![key, day],
                )?;
            }
        }
    }
    for (day, over) in counted.into_iter().filter(|(_, over)| *over > 0) {
        conn.execute(
            "UPDATE photo_day SET count = MAX(count - ?2, 0)
             WHERE day = ?1 AND keys_hash != 'legacy'",
            rusqlite::params![day, over],
        )?;
    }
    Ok(())
}

/// The photo a Mac scan of an earlier version stored under `earlier` is
/// `p.key` from now on — recorded whether or not anything is stored under
/// `earlier` here (a peer may hold it), so every peer drops its copy and
/// none stores it again. A point stored under `earlier` moves to the new
/// key — or, a point being under the new key already, goes. An observation
/// under `earlier` moves (or goes) the same way also when its point isn't
/// here: the locations leg can bring it before the photos leg brings the
/// point. `false` when no point is stored under `earlier`.
fn take_over(conn: &rusqlite::Connection, earlier: &str, p: &PhotoPointIn) -> Result<bool> {
    let had: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM photo_point WHERE key = ?1)",
        [earlier],
        |r| r.get(0),
    )?;
    let key_here: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM photo_point WHERE key = ?1)",
        [&p.key],
        |r| r.get(0),
    )?;
    if had && key_here {
        let gone = point_entry(conn, earlier)?;
        let kept = point_entry(conn, &p.key)?;
        drop_point(conn, earlier)?;
        ledger_after_merge(conn, gone.into_iter().chain(kept.clone()), &p.key, kept)?;
    } else {
        if had {
            conn.execute(
                "UPDATE photo_point SET key = ?2, filename = ?3 WHERE key = ?1",
                rusqlite::params![earlier, p.key, p.filename],
            )?;
        }
        conn.execute(
            "UPDATE OR IGNORE location_observation SET id = 'photos|' || ?2, source_ref = ?2
             WHERE id = 'photos|' || ?1",
            rusqlite::params![earlier, p.key],
        )?;
        conn.execute(
            "DELETE FROM location_observation WHERE id = 'photos|' || ?1",
            [earlier],
        )?;
    }
    record_replaced(conn, earlier, &p.key)?;
    Ok(had)
}

/// The minted ledger row: (count, keys_hash, episode_id, revision).
pub type PhotoDayState = (i64, String, Option<String>, i64);

/// One photo day as the photo-day repair reads it
/// ([`Store::photo_days_for_repair`]).
#[derive(Debug, Clone)]
pub struct PhotoDayRecord {
    pub day: String,
    /// The standing copy: in view, or retired.
    pub id: String,
    pub source_ref: String,
    /// Its revision (the base `photos#<day>` is 1).
    pub revision: i64,
    pub title: String,
    pub scope: String,
    pub retired: bool,
    /// Its text (None: unreadable).
    pub body: Option<String>,
    /// The highest revision stored of the day, in any state.
    pub max_revision: i64,
    /// The minted ledger here.
    pub ledger: Option<PhotoDayState>,
    /// (title, content hash) of each copy of the day an owner repair run
    /// re-minted and was then undone (store/repair.rs): a correction made
    /// and undone isn't made again.
    pub undone: Vec<(String, String)>,
}

/// A stored point, for the finalize pass to rebuild adapter records.
#[derive(Debug, Clone)]
pub struct StoredPhotoPoint {
    pub key: String,
    pub filename: String,
    pub at_unix: i64,
    pub tz_offset_minutes: Option<i32>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub place: Option<String>,
}

/// Order-independent fingerprint of a day's point set.
pub fn keys_hash(keys: &mut [String]) -> String {
    use std::hash::{Hash, Hasher};
    keys.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for k in keys.iter() {
        k.hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

impl Store {
    /// Upsert photo points, idempotent by key. Returns (accepted,
    /// duplicates). Batched lock holds (the refresh_local_sources
    /// pattern) — the Mac scan can push tens of thousands at once. A point
    /// carrying the key an earlier scan stored the photo under takes that
    /// point's place; a key some point here took the place of is not
    /// stored again.
    pub fn photos_add_points(
        &self,
        device: &str,
        points: &[PhotoPointIn],
    ) -> Result<(usize, usize)> {
        let (accepted, duplicates, _) = self.add_points(device, points)?;
        Ok((accepted, duplicates))
    }

    /// [`Store::photos_add_points`] for this node's own feeds (its Mac
    /// scan, the phone's stream): then the day of each point it names that
    /// is stored here — stored or moved now, a key it took the place of
    /// recorded now, or all of it done by an earlier read that stopped
    /// before this step — is checked again for a correction waiting behind
    /// the day's copy in view, as the photos leg does
    /// ([`Store::photo_points_apply_and_settle`]).
    pub fn photos_add_points_and_settle(
        &self,
        home: &crate::paths::Home,
        device: &str,
        points: &[PhotoPointIn],
    ) -> Result<(usize, usize)> {
        let (accepted, duplicates, days) = self.add_points(device, points)?;
        self.settle_photo_days(home, &days)?;
        Ok((accepted, duplicates))
    }

    /// See [`Store::photos_add_points`]: (accepted, duplicates, the days of
    /// the points it names that are stored here, new or not).
    fn add_points(
        &self,
        device: &str,
        points: &[PhotoPointIn],
    ) -> Result<(usize, usize, std::collections::BTreeSet<String>)> {
        let (mut accepted, mut duplicates) = (0usize, 0usize);
        let mut days = std::collections::BTreeSet::new();
        for chunk in points.chunks(500) {
            {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                for p in chunk {
                    if replaced(&tx, &p.key)? {
                        duplicates += 1;
                        continue;
                    }
                    if let Some(earlier) = p
                        .earlier_key
                        .as_deref()
                        .filter(|e| *e != p.key && same_second(e, &p.key))
                    {
                        if take_over(&tx, earlier, p)? {
                            days.extend(point_entry(&tx, &p.key)?.map(|(day, _)| day));
                            duplicates += 1;
                            continue;
                        }
                    }
                    let at_unix = chrono::DateTime::parse_from_rfc3339(&p.taken_at)
                        .map(|dt| dt.timestamp())
                        .map_err(|_| anyhow!("taken_at must be RFC3339: {:?}", p.taken_at))?;
                    let local = at_unix + p.tz_offset_minutes as i64 * 60;
                    let day = chrono::DateTime::from_timestamp(local, 0)
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                        .ok_or_else(|| anyhow!("unrepresentable taken_at"))?;
                    let changed = tx.execute(
                        "INSERT OR IGNORE INTO photo_point
                             (key, filename, at_unix, tz_offset_minutes, lat, lon, place,
                              device, day, ingest_time, rowid)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        rusqlite::params![
                            p.key,
                            p.filename,
                            at_unix,
                            p.tz_offset_minutes,
                            p.lat,
                            p.lon,
                            p.place,
                            device,
                            day,
                            now_utc(),
                            next_position(&tx)?
                        ],
                    )?;
                    // the day of the point kept under this key, stored now
                    // or before: a feed read again after a stop between this
                    // commit and the settling below changes nothing here,
                    // and its days are settled then
                    if changed == 1 {
                        accepted += 1;
                        days.insert(day.clone());
                    } else {
                        duplicates += 1;
                        days.extend(point_entry(&tx, &p.key)?.map(|(day, _)| day));
                    }
                    // v51 dual-write: geotagged points also land in the
                    // unified observation layer the visit pass clusters
                    // (same key, so replays stay no-ops there too).
                    if let (Some(lat), Some(lon)) = (p.lat, p.lon) {
                        tx.execute(
                            "INSERT OR IGNORE INTO location_observation
                                 (id, source, source_ref, lat, lon, accuracy_m,
                                  t_start, t_end, tz_offset_minutes, place, day,
                                  actor, ingest_time)
                             VALUES ('photos|' || ?1, 'photos', ?1, ?2, ?3, NULL,
                                     ?4, NULL, ?5, ?6, ?7, 'adapter:photos', ?8)",
                            rusqlite::params![
                                p.key,
                                lat,
                                lon,
                                p.taken_at,
                                p.tz_offset_minutes,
                                p.place,
                                day,
                                now_utc()
                            ],
                        )?;
                    }
                }
                tx.commit()?;
            }
            if points.len() > 500 {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
        Ok((accepted, duplicates, days))
    }

    /// Each of `days` checked for an owner repair's correction waiting
    /// behind the day's copy in view (`settle_repair_copy`), a hundred days
    /// to a lock hold.
    fn settle_photo_days(
        &self,
        home: &crate::paths::Home,
        days: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        for (i, chunk) in days.iter().collect::<Vec<_>>().chunks(100).enumerate() {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            let mut conn = self.lock();
            let tx = conn.transaction()?;
            for day in chunk {
                settle_repair_copy(&tx, home, &format!("photos#{day}"))?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    /// Days whose stored union no longer matches the minted ledger
    /// (or were never minted), closed before `today_local`.
    pub fn photo_days_needing_mint(&self, today_local: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            // COUNT() can't sit inside a correlated EXISTS; compare the
            // aggregate against a scalar subquery instead (-1 = never minted)
            "SELECT pp.day FROM photo_point pp
             WHERE pp.day < ?1
             GROUP BY pp.day
             HAVING COUNT(pp.key) > COALESCE(
                 (SELECT pd.count FROM photo_day pd WHERE pd.day = pp.day), -1)
             ORDER BY pp.day",
        )?;
        let rows = stmt.query_map([today_local], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn photo_points_for_day(&self, day: &str) -> Result<Vec<StoredPhotoPoint>> {
        points_of_day(&self.lock(), day)
    }

    /// The photos stored here for `day`, each by every name it has had —
    /// what the photo-day repair checks a day's text against
    /// ([`photos_missing`]).
    pub fn photo_day_held(&self, day: &str) -> Result<Vec<HeldPhoto>> {
        held_by_points(&self.lock(), day)
    }

    /// Do the photos stored here for `day` (each by every name it has had)
    /// cover every photo its copy in view shows ([`photos_missing`])? A copy
    /// whose list is cut, or whose text can't be read, isn't covered. True
    /// when no copy is in view.
    pub fn photo_points_hold_day(&self, home: &crate::paths::Home, day: &str) -> Result<bool> {
        let conn = self.lock();
        let held = held_by_points(&conn, day)?;
        for copy in day_copies(&conn, &format!("photos#{day}"))? {
            if copy.superseded_by.is_some() {
                continue;
            }
            let covered = copy
                .body(home)
                .as_deref()
                .and_then(photo_day_lines)
                .is_some_and(|shown| photos_missing(&held, &shown) == 0);
            if !covered {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The highest revision of `day` stored here, in any state (its base
    /// `photos#<day>` is 1); 0 when none is.
    pub fn photo_day_max_revision(&self, day: &str) -> Result<i64> {
        let conn = self.lock();
        Ok(day_copies(&conn, &format!("photos#{day}"))?
            .iter()
            .map(|c| c.revision)
            .max()
            .unwrap_or(0))
    }

    /// The minted ledger row: (count, keys_hash, episode_id, revision).
    pub fn photo_day_state(&self, day: &str) -> Result<Option<PhotoDayState>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT count, keys_hash, episode_id, revision FROM photo_day WHERE day = ?1",
            [day],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_photo_day_state(
        &self,
        day: &str,
        count: i64,
        keys_hash: &str,
        episode_id: Option<&str>,
        revision: i64,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO photo_day (day, count, keys_hash, episode_id, revision, minted_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(day) DO UPDATE SET count=excluded.count,
                 keys_hash=excluded.keys_hash, episode_id=excluded.episode_id,
                 revision=excluded.revision, minted_at=excluded.minted_at",
            rusqlite::params![day, count, keys_hash, episode_id, revision, now_utc()],
        )?;
        Ok(())
    }

    /// The current photos#DAY episode (any revision, unsuperseded):
    /// (id, source_ref, body_photo_count) — the count parsed from the
    /// canonical "photos: N" body line lets the pass adopt LEGACY
    /// episodes minted before v47 without re-minting equal-or-poorer
    /// unions.
    pub fn current_photo_episode(
        &self,
        home: &crate::paths::Home,
        day: &str,
    ) -> Result<Option<(String, String, i64)>> {
        let row: Option<(String, String, Option<String>)> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT id, source_ref, content_ref FROM episodes
                 WHERE source = 'photos' AND superseded_by IS NULL
                   AND (source_ref = 'photos#' || ?1 OR source_ref LIKE 'photos#' || ?1 || '#r%')
                 ORDER BY source_ref DESC LIMIT 1",
                [day],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
        };
        let Some((id, source_ref, content_ref)) = row else {
            return Ok(None);
        };
        let count = content_ref
            .map(|cr| home.root().join(cr))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|body| {
                body.lines().find_map(|l| {
                    l.strip_prefix("photos: ")
                        .and_then(|n| n.trim().parse::<i64>().ok())
                })
            })
            .unwrap_or(0);
        Ok(Some((id, source_ref, count)))
    }

    /// Every photo day with a standing copy (in view, or retired), as the
    /// photo-day repair reads it: that copy (the highest revision standing),
    /// its text, the highest revision stored of the day in any state (a new
    /// revision goes past every one, so it never takes a ref another node
    /// already used), and the day's minted ledger here. Read a page at a
    /// time; texts with the store released.
    pub fn photo_days_for_repair(&self, home: &crate::paths::Home) -> Result<Vec<PhotoDayRecord>> {
        struct Row {
            id: String,
            source_ref: String,
            title: String,
            scope: String,
            superseded_by: Option<String>,
            content_ref: Option<String>,
        }
        let mut days: std::collections::BTreeMap<String, (i64, Option<(i64, Row)>)> =
            Default::default();
        let mut after = String::new();
        loop {
            let page: Vec<Row> = {
                let conn = self.lock();
                let mut stmt = conn.prepare_cached(
                    "SELECT id, source_ref, title, scope, superseded_by, content_ref
                     FROM episodes /* all-episodes: every revision of every photo day decides the next one */
                     WHERE source = 'photos' AND source_ref > ?1 ORDER BY source_ref LIMIT 5000",
                )?;
                let rows = stmt.query_map([&after], |r| {
                    Ok(Row {
                        id: r.get(0)?,
                        source_ref: r.get(1)?,
                        title: r.get(2)?,
                        scope: r.get(3)?,
                        superseded_by: r.get(4)?,
                        content_ref: r.get(5)?,
                    })
                })?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            let Some(last) = page.last() else { break };
            after = last.source_ref.clone();
            let full = page.len() >= 5000;
            for row in page {
                let Some(rest) = row.source_ref.strip_prefix("photos#") else {
                    continue;
                };
                let (day, revision) = match rest.split_once("#r") {
                    Some((day, digits)) => match digits.parse::<i64>() {
                        Ok(n) if n >= 2 && n.to_string() == digits => (day, n),
                        _ => continue,
                    },
                    None => (rest, 1),
                };
                if chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_err() {
                    continue;
                }
                let entry = days.entry(day.to_string()).or_insert((0, None));
                entry.0 = entry.0.max(revision);
                let standing = row
                    .superseded_by
                    .as_deref()
                    .is_none_or(|s| s.starts_with(super::retire::RETIRED_PREFIX));
                if standing && entry.1.as_ref().is_none_or(|(r, _)| revision > *r) {
                    entry.1 = Some((revision, row));
                }
            }
            if !full {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        // the copies undone repair runs re-minted, by day
        let mut undone: std::collections::HashMap<String, Vec<(String, String)>> =
            Default::default();
        {
            let conn = self.lock();
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT e.source_ref, e.title, e.content_hash
                 FROM repairs r
                 JOIN repair_copies c ON c.run = r.id
                 JOIN episode_origin o ON o.episode_id = c.episode_id AND o.writer = '{}'
                 JOIN episodes e ON e.id = c.episode_id /* all-episodes: copies of undone runs, behind others by design */
                 WHERE r.undone_at IS NOT NULL AND e.source = 'photos'",
                super::repair::REPAIR_WRITER
            ))?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (source_ref, title, hash) = row?;
                if let Some((base, _)) = super::repair::revision_of("photos", &source_ref) {
                    if let Some(day) = base.strip_prefix("photos#") {
                        undone
                            .entry(day.to_string())
                            .or_default()
                            .push((title, hash));
                    }
                }
            }
        }
        let mut out = Vec::new();
        for (day, (max_revision, standing)) in days {
            let Some((revision, row)) = standing else {
                continue;
            };
            let ledger = self.photo_day_state(&day)?;
            let undone = undone.remove(&day).unwrap_or_default();
            let body = row
                .content_ref
                .as_deref()
                .and_then(|rel| std::fs::read_to_string(home.root().join(rel)).ok());
            out.push(PhotoDayRecord {
                day,
                id: row.id,
                source_ref: row.source_ref,
                revision,
                title: row.title,
                scope: row.scope,
                retired: row
                    .superseded_by
                    .is_some_and(|s| s.starts_with(super::retire::RETIRED_PREFIX)),
                body,
                max_revision,
                ledger,
                undone,
            });
        }
        Ok(out)
    }

    /// The keys points here took the place of, for each of `keys` — and the
    /// keys those took the place of in turn: `(earlier key, the key in
    /// `keys` that holds the photo now)`.
    pub fn photo_replaced_into(&self, keys: &[String]) -> Result<Vec<(String, String)>> {
        replaced_into(&self.lock(), keys)
    }

    /// Feed for the peer photo-point leg: rows after a rowid cursor.
    pub fn photo_points_export(&self, after: i64, limit: usize) -> Result<serde_json::Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT rowid, key, filename, at_unix, tz_offset_minutes, lat, lon, place, device,
                    (SELECT json_group_array(r.key) FROM photo_replaced r
                     WHERE r.by_key = photo_point.key)
             FROM photo_point WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![after, limit as i64], |row| {
            let mut point = serde_json::json!({
                "key": row.get::<_, String>(1)?,
                "filename": row.get::<_, String>(2)?,
                "at_unix": row.get::<_, i64>(3)?,
                "tz_offset_minutes": row.get::<_, Option<i32>>(4)?,
                "lat": row.get::<_, Option<f64>>(5)?,
                "lon": row.get::<_, Option<f64>>(6)?,
                "place": row.get::<_, Option<String>>(7)?,
                "device": row.get::<_, String>(8)?,
            });
            // additive: the keys this point took the place of
            let replaces: Vec<String> = row
                .get::<_, Option<String>>(9)?
                .and_then(|list| serde_json::from_str(&list).ok())
                .unwrap_or_default();
            if !replaces.is_empty() {
                point["replaces"] = serde_json::json!(replaces);
            }
            Ok((row.get::<_, i64>(0)?, point))
        })?;
        let mut next = after;
        let mut points = Vec::new();
        for row in rows {
            let (rowid, p) = row?;
            next = rowid;
            points.push(p);
        }
        Ok(serde_json::json!({
            "drained": points.is_empty(), "points": points, "next": next,
        }))
    }

    /// Apply a peer's photo-point page — idempotent by key. A point that
    /// took the place of another key drops the point stored under it here;
    /// a key some point here took the place of is not stored again.
    pub fn photo_points_apply(&self, batch: &serde_json::Value) -> Result<usize> {
        Ok(self.apply_points(batch)?.0)
    }

    /// [`Store::photo_points_apply`] for the photos leg: then each day the
    /// page brings points of whose owner-repair correction stands behind
    /// the day's copy in view — it arrived, or its run settled, before this
    /// node held the photos that show it holds that copy — is checked again
    /// (`settle_repair_copy`). Returns how many points were stored.
    pub fn photo_points_apply_and_settle(
        &self,
        home: &crate::paths::Home,
        batch: &serde_json::Value,
    ) -> Result<usize> {
        let (applied, days) = self.apply_points(batch)?;
        self.settle_photo_days(home, &days)?;
        Ok(applied)
    }

    /// Apply a peer's photo-point page: (points stored, the days of the
    /// points it brought — and of a point stored here already under the
    /// same key, which keeps its own day when the peer's time zone differs).
    fn apply_points(
        &self,
        batch: &serde_json::Value,
    ) -> Result<(usize, std::collections::BTreeSet<String>)> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        let mut days = std::collections::BTreeSet::new();
        for p in batch
            .get("points")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(key), Some(filename), Some(at_unix)) = (
                p.get("key").and_then(|v| v.as_str()),
                p.get("filename").and_then(|v| v.as_str()),
                p.get("at_unix").and_then(|v| v.as_i64()),
            ) else {
                continue;
            };
            let tz = p
                .get("tz_offset_minutes")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let local = at_unix + tz * 60;
            let Some(day) = chrono::DateTime::from_timestamp(local, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
            else {
                continue;
            };
            if replaced(&tx, key)? {
                continue;
            }
            days.insert(day.clone());
            // the keys it took the place of (a list; one key on its own read
            // the same)
            let replaces: Vec<&str> = match p.get("replaces") {
                Some(serde_json::Value::Array(keys)) => {
                    keys.iter().filter_map(|v| v.as_str()).collect()
                }
                Some(serde_json::Value::String(one)) => vec![one.as_str()],
                _ => Vec::new(),
            }
            .into_iter()
            .filter(|e| *e != key && same_second(e, key))
            .collect();
            // the photo's points here before: its own, and those it replaced
            let here = point_entry(&tx, key)?;
            // the point kept is this one, in its own day: the keys recorded
            // below are its names from now on
            days.extend(here.as_ref().map(|(day, _)| day.clone()));
            let mut before: Vec<Entry> = here.iter().cloned().collect();
            for earlier in &replaces {
                before.extend(point_entry(&tx, earlier)?);
                drop_point(&tx, earlier)?;
                // the point here already (the phone's, say) says so now, and
                // the nodes this one passes points on to learn it too
                record_replaced(&tx, earlier, key)?;
            }
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO photo_point
                     (key, filename, at_unix, tz_offset_minutes, lat, lon, place,
                      device, day, ingest_time, rowid)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                rusqlite::params![
                    key,
                    filename,
                    at_unix,
                    tz,
                    p.get("lat").and_then(|v| v.as_f64()),
                    p.get("lon").and_then(|v| v.as_f64()),
                    p.get("place").and_then(|v| v.as_str()),
                    p.get("device").and_then(|v| v.as_str()).unwrap_or("peer"),
                    day,
                    now_utc(),
                    next_position(&tx)?
                ],
            )?;
            applied += inserted;
            let kept = here.or_else(|| (inserted > 0).then(|| (day.clone(), false)));
            ledger_after_merge(&tx, before, key, kept)?;
            // v51 dual-write (same as photos_add_points): peer-applied
            // geotags feed the observation layer too, so a day's visit
            // clustering never waits on the /v1/sync/locations leg.
            if let (Some(lat), Some(lon)) = (
                p.get("lat").and_then(|v| v.as_f64()),
                p.get("lon").and_then(|v| v.as_f64()),
            ) {
                tx.execute(
                    "INSERT OR IGNORE INTO location_observation
                         (id, source, source_ref, lat, lon, accuracy_m, t_start,
                          t_end, tz_offset_minutes, place, day, actor, ingest_time)
                     VALUES ('photos|' || ?1, 'photos', ?1, ?2, ?3, NULL,
                             strftime('%Y-%m-%dT%H:%M:%SZ', ?4, 'unixepoch'),
                             NULL, ?5, ?6, ?7, 'adapter:photos', ?8)",
                    rusqlite::params![
                        key,
                        lat,
                        lon,
                        at_unix,
                        tz,
                        p.get("place").and_then(|v| v.as_str()),
                        day,
                        now_utc()
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok((applied, days))
    }
}

fn points_of_day(conn: &rusqlite::Connection, day: &str) -> Result<Vec<StoredPhotoPoint>> {
    let mut stmt = conn.prepare_cached(
        "SELECT key, filename, at_unix, tz_offset_minutes, lat, lon, place
         FROM photo_point WHERE day = ?1 ORDER BY at_unix",
    )?;
    let rows = stmt.query_map([day], |row| {
        Ok(StoredPhotoPoint {
            key: row.get(0)?,
            filename: row.get(1)?,
            at_unix: row.get(2)?,
            tz_offset_minutes: row.get(3)?,
            lat: row.get(4)?,
            lon: row.get(5)?,
            place: row.get(6)?,
        })
    })?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
}

/// See [`Store::photo_replaced_into`].
fn replaced_into(conn: &rusqlite::Connection, keys: &[String]) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached("SELECT key FROM photo_replaced WHERE by_key = ?1")?;
    let mut out = Vec::new();
    for key in keys {
        let mut seen = std::collections::HashSet::from([key.clone()]);
        let mut frontier = vec![key.clone()];
        while let Some(at) = frontier.pop() {
            let earlier: Vec<String> = stmt
                .query_map([&at], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            for e in earlier {
                if seen.insert(e.clone()) && seen.len() <= 64 {
                    out.push((e.clone(), key.clone()));
                    frontier.push(e);
                }
            }
        }
    }
    Ok(out)
}

/* ————— a photo day's copies, and the photos a copy holds ————— */

/// The day ref (`photos#<date>`) of a photo day's copy: its own ref, or a
/// revision's (`photos#<date>#r<n>`). None for any other ref.
pub(crate) fn day_base(source_ref: &str) -> Option<&str> {
    let rest = source_ref.strip_prefix("photos#")?;
    let date = match rest.split_once("#r") {
        Some((date, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => date,
        Some(_) => return None,
        None => rest,
    };
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(&source_ref[.."photos#".len() + date.len()])
}

/// One stored copy of a photo day.
#[derive(Debug, Clone)]
pub(crate) struct DayCopy {
    pub id: String,
    pub scope: String,
    pub scope_changed_at: Option<String>,
    pub superseded_by: Option<String>,
    pub content_ref: Option<String>,
    /// The base is 1.
    pub revision: i64,
}

impl DayCopy {
    fn body(&self, home: &crate::paths::Home) -> Option<String> {
        let rel = self.content_ref.as_deref()?;
        std::fs::read_to_string(home.root().join(rel)).ok()
    }
}

/// Every stored copy of the photo day `base` (`photos#<date>`), in any state.
pub(crate) fn day_copies(conn: &rusqlite::Connection, base: &str) -> Result<Vec<DayCopy>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT id, source_ref, scope, scope_changed_at, superseded_by, content_ref
         FROM episodes /* all-episodes: every copy of one photo day */
         WHERE source = 'photos' AND (source_ref = ?1 OR {})",
        super::growing::growing_revision_sql(1)
    ))?;
    let rows = stmt.query_map([base], |r| {
        let source_ref: String = r.get(1)?;
        Ok(DayCopy {
            id: r.get(0)?,
            revision: source_ref
                .rsplit_once("#r")
                .and_then(|(_, n)| n.parse().ok())
                .unwrap_or(1),
            scope: r.get(2)?,
            scope_changed_at: r.get(3)?,
            superseded_by: r.get(4)?,
            content_ref: r.get(5)?,
        })
    })?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
}

/// The other stored copies of the photo day `episode_id` is a copy of;
/// None for an episode that isn't one. A private label on one reaches them
/// (`growing::spread_private`).
pub(crate) fn other_day_copies(
    conn: &rusqlite::Connection,
    episode_id: &str,
) -> Result<Option<Vec<String>>> {
    let source_ref: Option<String> = conn
        .query_row(
            "SELECT source_ref FROM episodes /* all-episodes: one copy of a photo day */
             WHERE id = ?1 AND source = 'photos'",
            [episode_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(base) = source_ref.as_deref().and_then(day_base) else {
        return Ok(None);
    };
    Ok(Some(
        day_copies(conn, base)?
            .into_iter()
            .map(|c| c.id)
            .filter(|id| id != episode_id)
            .collect(),
    ))
}

/// The day is private, so a copy arriving is too (see
/// `growing::Arriving::private`): the latest label given to any of
/// `copies` is private — or, none having been given one, one of them is.
/// Carries when that label was given, if it says.
pub(crate) fn day_private(copies: &[DayCopy]) -> Option<Option<&str>> {
    match copies
        .iter()
        .filter(|c| c.scope_changed_at.is_some())
        .max_by(|a, b| a.scope_changed_at.cmp(&b.scope_changed_at))
    {
        Some(c) => (c.scope == "private").then_some(c.scope_changed_at.as_deref()),
        None => copies.iter().find(|c| c.scope == "private").map(|_| None),
    }
}

/// One photo as the coverage check reads it: the minute its line shows,
/// and every name it has had (its file's first, then those of the keys it
/// took the place of) — or, a photo known only by the line a day's text
/// shows of it, that line.
#[derive(Debug, Clone, Default)]
pub struct HeldPhoto {
    pub minute: Option<String>,
    pub names: Vec<String>,
    pub line: Option<String>,
}

/// `HH:MM` of a photo in its own time (as a day's text writes it).
fn local_minute(at_unix: i64, tz_offset_minutes: Option<i32>) -> Option<String> {
    let local = at_unix + tz_offset_minutes.unwrap_or(0) as i64 * 60;
    chrono::DateTime::from_timestamp(local, 0).map(|dt| dt.format("%H:%M").to_string())
}

/// The photo lines a day's text shows (`[HH:MM] <rest>`, `rest` being
/// `<file>[ (lat,lon)][ — place]`), each as (`HH:MM`, rest).
fn stamped_lines(body: &str) -> Vec<(&str, &str)> {
    body.lines()
        .filter(|line| {
            let b = line.as_bytes();
            b.len() > 8
                && b[0] == b'['
                && b[1..3].iter().all(u8::is_ascii_digit)
                && b[3] == b':'
                && b[4..6].iter().all(u8::is_ascii_digit)
                && &b[6..8] == b"] "
        })
        .map(|line| (&line[1..6], &line[8..]))
        .collect()
}

/// The photo lines a day's text shows, when its list is whole: None when
/// it is cut (`[truncated]`) or shorter than the count it declares (what
/// it holds can't be read then).
pub fn photo_day_lines(body: &str) -> Option<Vec<(&str, &str)>> {
    if body.lines().any(|l| l == "[truncated]") {
        return None;
    }
    let declared: Option<usize> = body
        .lines()
        .find_map(|l| l.strip_prefix("photos: "))
        .and_then(|n| n.trim().parse().ok());
    let shown = stamped_lines(body);
    (declared == Some(shown.len())).then_some(shown)
}

/// Does `rest` (a photo line after its time) show the file `name`?
fn shows_name(rest: &str, name: &str) -> bool {
    !name.is_empty()
        && rest.strip_prefix(name).is_some_and(|tail| {
            tail.is_empty() || tail.starts_with(" (") || tail.starts_with(" — ")
        })
}

/// The most of `adj`'s left side (each with the right-side slots it may
/// take) that can each take a different slot: (how many, the left side
/// holding each slot).
fn matching(adj: &[Vec<usize>], slots: usize) -> (usize, Vec<Option<usize>>) {
    fn augment(
        u: usize,
        adj: &[Vec<usize>],
        seen: &mut [bool],
        owner: &mut [Option<usize>],
    ) -> bool {
        for &v in &adj[u] {
            if std::mem::replace(&mut seen[v], true) {
                continue;
            }
            let free = match owner[v] {
                None => true,
                Some(w) => augment(w, adj, seen, owner),
            };
            if free {
                owner[v] = Some(u);
                return true;
            }
        }
        false
    }
    let mut owner = vec![None; slots];
    let mut matched = 0;
    for u in 0..adj.len() {
        let mut seen = vec![false; slots];
        if augment(u, adj, &mut seen, &mut owner) {
            matched += 1;
        }
    }
    (matched, owner)
}

/// How many of the `shown` lines (`HH:MM`, rest) no photo of `held`
/// accounts for. A photo accounts for one line per name it has had — the
/// same photo shown under its file's name and under a key it took the
/// place of is one photo shown twice — and a line it is known only by
/// accounts for that one line. Two lines of one name in one minute are two
/// photos: one photo held never accounts for both.
pub fn photos_missing(held: &[HeldPhoto], shown: &[(&str, &str)]) -> usize {
    let mut slots: Vec<(usize, Option<usize>)> = Vec::new();
    for (i, p) in held.iter().enumerate() {
        if p.names.is_empty() {
            if p.line.is_some() {
                slots.push((i, None));
            }
        } else {
            slots.extend((0..p.names.len()).map(|j| (i, Some(j))));
        }
    }
    let adj: Vec<Vec<usize>> = shown
        .iter()
        .map(|(minute, rest)| {
            slots
                .iter()
                .enumerate()
                .filter(|(_, (i, name))| {
                    let p = &held[*i];
                    p.minute.as_deref() == Some(*minute)
                        && match name {
                            Some(j) => shows_name(rest, &p.names[*j]),
                            None => p.line.as_deref() == Some(*rest),
                        }
                })
                .map(|(s, _)| s)
                .collect()
        })
        .collect();
    shown.len() - matching(&adj, slots.len()).0
}

/// The photos stored here for `day`, each by every name it has had.
fn held_by_points(conn: &rusqlite::Connection, day: &str) -> Result<Vec<HeldPhoto>> {
    let points = points_of_day(conn, day)?;
    let keys: Vec<String> = points.iter().map(|p| p.key.clone()).collect();
    let mut names: std::collections::HashMap<&str, Vec<String>> = points
        .iter()
        .map(|p| (p.key.as_str(), vec![crate::one_line(&p.filename)]))
        .collect();
    for (earlier, by) in replaced_into(conn, &keys)? {
        if let (Some(list), Some((_, name))) = (names.get_mut(by.as_str()), earlier.split_once('|'))
        {
            list.push(crate::one_line(name));
        }
    }
    Ok(points
        .iter()
        .map(|p| HeldPhoto {
            minute: local_minute(p.at_unix, p.tz_offset_minutes),
            names: names.remove(p.key.as_str()).unwrap_or_default(),
            line: None,
        })
        .collect())
}

/// The photos a copy of `day` shows (its text `body`), each by every name
/// the photos stored here say it has had: a line is paired with a stored
/// photo of its minute whose file it shows (one photo per line); a line no
/// stored photo pairs with is known only by itself.
fn held_by_text(conn: &rusqlite::Connection, day: &str, body: &str) -> Result<Vec<HeldPhoto>> {
    let lines = stamped_lines(body);
    let stored = held_by_points(conn, day)?;
    let adj: Vec<Vec<usize>> = lines
        .iter()
        .map(|(minute, rest)| {
            stored
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.minute.as_deref() == Some(*minute)
                        && p.names.first().is_some_and(|n| shows_name(rest, n))
                })
                .map(|(j, _)| j)
                .collect()
        })
        .collect();
    let (_, owner) = matching(&adj, stored.len());
    let mut held: Vec<HeldPhoto> = lines
        .iter()
        .map(|(minute, rest)| HeldPhoto {
            minute: Some(minute.to_string()),
            names: Vec::new(),
            line: Some(rest.to_string()),
        })
        .collect();
    for (j, line) in owner.into_iter().enumerate() {
        if let Some(i) = line {
            held[i].names = stored[j].names.clone();
            held[i].line = None;
        }
    }
    Ok(held)
}

/// Does the copy of `day` whose text is `outer` hold every photo the copy
/// `inner` shows? `inner`'s list must be whole ([`photo_day_lines`]).
fn text_holds(conn: &rusqlite::Connection, day: &str, outer: &str, inner: &str) -> Result<bool> {
    let Some(shown) = photo_day_lines(inner) else {
        return Ok(false);
    };
    Ok(photos_missing(&held_by_text(conn, day, outer)?, &shown) == 0)
}

/// Was `episode_id` written by the owner repair (store/repair.rs)?
fn written_by_repair(conn: &rusqlite::Connection, episode_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM episode_origin WHERE episode_id = ?1 AND writer = ?2)",
        rusqlite::params![episode_id, super::repair::REPAIR_WRITER],
        |r| r.get(0),
    )?)
}

/// An owner repair's copy of the photo day `base` arriving from a peer
/// (text `body`, id `episode_id`): the copy of the day in view here that it
/// doesn't hold, if one is — the repaired copy lands behind it. The node
/// that made the copy checked it against its own copy of the day; another
/// node's can show photos that one didn't hold, so each node checks for
/// itself (a copy whose list is cut, or that can't be read, isn't held).
pub(crate) fn unheld_live_copy(
    conn: &rusqlite::Connection,
    home: &crate::paths::Home,
    base: &str,
    episode_id: &str,
    body: &str,
) -> Result<Option<String>> {
    let day = &base["photos#".len()..];
    let mut live: Vec<DayCopy> = day_copies(conn, base)?
        .into_iter()
        .filter(|c| c.superseded_by.is_none() && c.id != episode_id)
        .collect();
    live.sort_by_key(|c| std::cmp::Reverse(c.revision));
    for copy in live {
        let held = match copy.body(home) {
            Some(inner) => text_holds(conn, day, body, &inner)?,
            None => false,
        };
        if !held {
            return Ok(Some(copy.id));
        }
    }
    Ok(None)
}

/// A photo day whose newest copy is an owner repair's that stands behind
/// another copy of the day — it arrived before this node could tell it
/// held that copy (the photos it names, and the keys they took the place
/// of, reach a node on their own leg) — stands once this node can tell:
/// when it holds every copy of the day in view, which go behind it. Checked
/// when the run settles and when the photos leg brings points of the day
/// ([`Store::photo_points_apply_and_settle`]). An undone run's copy, a
/// retired one and one standing behind something outside the day are left.
/// Returns the copies whose state changed.
pub(crate) fn settle_repair_copy(
    conn: &rusqlite::Connection,
    home: &crate::paths::Home,
    base: &str,
) -> Result<usize> {
    let copies = day_copies(conn, base)?;
    let Some(top) = copies.iter().max_by_key(|c| c.revision) else {
        return Ok(0);
    };
    let Some(ahead) = top.superseded_by.as_deref() else {
        return Ok(0);
    };
    if !copies.iter().any(|c| c.id == ahead)
        || !written_by_repair(conn, &top.id)?
        || super::repair::undone_copy(conn, &top.id)?
    {
        return Ok(0);
    }
    let Some(outer) = top.body(home) else {
        return Ok(0);
    };
    let day = &base["photos#".len()..];
    let live: Vec<&DayCopy> = copies
        .iter()
        .filter(|c| c.superseded_by.is_none())
        .collect();
    for copy in &live {
        let held = match copy.body(home) {
            Some(inner) => text_holds(conn, day, &outer, &inner)?,
            None => false,
        };
        if !held {
            return Ok(0);
        }
    }
    conn.execute(
        "UPDATE episodes SET superseded_by = NULL, location_scanned = 0 WHERE id = ?1",
        [&top.id],
    )?;
    for copy in &live {
        conn.execute(
            "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
            rusqlite::params![top.id, copy.id],
        )?;
        conn.execute(
            "DELETE FROM location_points WHERE episode_id = ?1",
            [&copy.id],
        )?;
    }
    Ok(1 + live.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(key_suffix: &str, taken: &str) -> PhotoPointIn {
        PhotoPointIn {
            key: format!("1750000000|{key_suffix}"),
            filename: key_suffix.into(),
            taken_at: taken.into(),
            tz_offset_minutes: -240,
            lat: Some(42.36),
            lon: Some(-71.06),
            place: None,
            earlier_key: None,
        }
    }

    fn at(name: &str) -> PhotoPointIn {
        PhotoPointIn {
            key: format!("1750000000|{name}"),
            ..point(name, "2026-07-20T12:00:00Z")
        }
    }

    /// A Mac scan's point: keyed by the original name, carrying the key
    /// an earlier scan gave it (the library's own file name).
    fn scanned(original: &str, library: &str) -> PhotoPointIn {
        PhotoPointIn {
            earlier_key: Some(format!("1750000000|{library}")),
            ..at(original)
        }
    }

    fn keys(store: &Store) -> Vec<String> {
        let mut keys: Vec<String> = store
            .photo_points_for_day("2026-07-20")
            .unwrap()
            .into_iter()
            .map(|p| p.key)
            .collect();
        keys.sort();
        keys
    }

    fn observations(store: &Store) -> Vec<String> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare("SELECT id FROM location_observation ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.collect::<std::result::Result<_, _>>().unwrap()
    }

    /// A Mac point keyed by its original name joins the phone's point for
    /// the same photo; one an earlier scan stored under the library's name
    /// moves to the original name — so the phone's point for it, sent
    /// later, joins it too — and where both were stored already, one goes.
    #[test]
    fn test_points_keyed_by_original_name_join_and_earlier_keys_move() {
        let store = Store::open_in_memory().unwrap();
        // the phone's points, and an earlier Mac scan's of photos 2 and 4
        store
            .photos_add_points("iPhone", &[at("IMG_1.HEIC"), at("IMG_4.HEIC")])
            .unwrap();
        store
            .photos_add_points("mac", &[at("AAAA-UUID.heic"), at("DDDD-UUID.heic")])
            .unwrap();
        let scan = [
            scanned("IMG_1.HEIC", "BBBB-UUID.heic"),
            scanned("IMG_2.HEIC", "AAAA-UUID.heic"),
            scanned("IMG_3.HEIC", "CCCC-UUID.heic"),
            scanned("IMG_4.HEIC", "DDDD-UUID.heic"),
        ];
        assert_eq!(store.photos_add_points("mac", &scan).unwrap(), (1, 3));
        let expected = [
            "1750000000|IMG_1.HEIC",
            "1750000000|IMG_2.HEIC",
            "1750000000|IMG_3.HEIC",
            "1750000000|IMG_4.HEIC",
        ];
        assert_eq!(keys(&store), expected);
        assert_eq!(
            observations(&store),
            expected.map(|k| format!("photos|{k}"))
        );
        // the phone sends photo 2 later, and the scan runs again: still one
        // point each
        assert_eq!(
            store
                .photos_add_points("iPhone", &[at("IMG_2.HEIC")])
                .unwrap(),
            (0, 1)
        );
        assert_eq!(store.photos_add_points("mac", &scan).unwrap(), (0, 4));
        // an earlier scan's key is never stored again
        assert_eq!(
            store
                .photos_add_points("mac", &[at("AAAA-UUID.heic")])
                .unwrap(),
            (0, 1)
        );
        assert_eq!(keys(&store), expected);
    }

    /// Two nodes that both hold a photo under an earlier scan's key — the
    /// phone sending its point to one of them — end with one point for it
    /// once the Mac scans again and the nodes sync, in either direction,
    /// and a node a page from an older peer reaches doesn't store the old
    /// key again.
    #[test]
    fn test_moved_keys_reach_every_peer() {
        let mac = Store::open_in_memory().unwrap();
        let hub = Store::open_in_memory().unwrap();
        let third = Store::open_in_memory().unwrap();
        let sync = |from: &Store, to: &Store| {
            let page = from.photo_points_export(0, 100).unwrap();
            to.photo_points_apply(&page).unwrap();
        };
        // before: the Mac's scan stored the library's names, and synced
        mac.photos_add_points("mac", &[at("AAAA-UUID.heic"), at("BBBB-UUID.heic")])
            .unwrap();
        sync(&mac, &hub);
        sync(&hub, &third);
        let old_page = hub.photo_points_export(0, 100).unwrap();
        // the phone sends its point for photo 1 to the hub
        hub.photos_add_points("iPhone", &[at("IMG_1.HEIC")])
            .unwrap();
        // the Mac scans with this version
        mac.photos_add_points(
            "mac",
            &[
                scanned("IMG_1.HEIC", "AAAA-UUID.heic"),
                scanned("IMG_2.HEIC", "BBBB-UUID.heic"),
            ],
        )
        .unwrap();
        sync(&mac, &hub);
        sync(&hub, &mac);
        sync(&hub, &third);
        let expected = ["1750000000|IMG_1.HEIC", "1750000000|IMG_2.HEIC"];
        for store in [&mac, &hub, &third] {
            assert_eq!(keys(store), expected);
            assert_eq!(observations(store), expected.map(|k| format!("photos|{k}")));
        }
        // the phone sends photo 2 to the hub afterwards: nothing new
        hub.photos_add_points("iPhone", &[at("IMG_2.HEIC")])
            .unwrap();
        assert_eq!(keys(&hub), expected);
        // a page an older peer still holds: the old keys stay gone
        third.photo_points_apply(&old_page).unwrap();
        assert_eq!(keys(&third), expected);
        // nothing left to tell: a second exchange moves nothing
        let before = hub.photo_points_export(0, 100).unwrap()["next"].clone();
        sync(&mac, &hub);
        sync(&third, &hub);
        assert_eq!(hub.photo_points_export(0, 100).unwrap()["next"], before);
    }

    /// A point moved to the end of the export order, or stored after a
    /// point was removed, lands past every place a removed point held — a
    /// peer whose cursor is already there still receives it.
    #[test]
    fn test_moved_and_new_points_land_past_a_peers_cursor() {
        let store = Store::open_in_memory().unwrap();
        store
            .photos_add_points("iPhone", &[at("IMG_1.HEIC")])
            .unwrap();
        store
            .photos_add_points("mac", &[at("AAAA-UUID.heic")])
            .unwrap();
        let cursor = store.photo_points_export(0, 100).unwrap()["next"]
            .as_i64()
            .unwrap();
        // the scan finds they are one photo: the earlier key's point goes
        store
            .photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        let page = store.photo_points_export(cursor, 100).unwrap();
        let points = page["points"].as_array().unwrap();
        assert_eq!(points.len(), 1, "{page}");
        assert_eq!(points[0]["key"], "1750000000|IMG_1.HEIC");
        assert_eq!(
            points[0]["replaces"],
            serde_json::json!(["1750000000|AAAA-UUID.heic"])
        );

        // the last point stored goes for a peer's point that replaced it
        let store = Store::open_in_memory().unwrap();
        store
            .photos_add_points("iPhone", &[at("IMG_1.HEIC")])
            .unwrap();
        store
            .photos_add_points("mac", &[at("BBBB-UUID.heic")])
            .unwrap();
        let cursor = store.photo_points_export(0, 100).unwrap()["next"]
            .as_i64()
            .unwrap();
        let mac = Store::open_in_memory().unwrap();
        mac.photos_add_points("mac", &[scanned("IMG_2.HEIC", "BBBB-UUID.heic")])
            .unwrap();
        store
            .photo_points_apply(&mac.photo_points_export(0, 100).unwrap())
            .unwrap();
        let page = store.photo_points_export(cursor, 100).unwrap();
        let points = page["points"].as_array().unwrap();
        assert_eq!(points.len(), 1, "{page}");
        assert_eq!(points[0]["key"], "1750000000|IMG_2.HEIC");
    }

    /// A Mac that never stored a photo under its earlier key still says
    /// which key it replaced: a peer holding only that key drops it, and
    /// neither node stores it again from a page sent before.
    #[test]
    fn test_a_scan_names_the_earlier_key_a_peer_holds() {
        let mac = Store::open_in_memory().unwrap();
        let peer = Store::open_in_memory().unwrap();
        peer.photos_add_points("mac", &[at("AAAA-UUID.heic")])
            .unwrap();
        let old_page = peer.photo_points_export(0, 100).unwrap();
        mac.photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        peer.photo_points_apply(&mac.photo_points_export(0, 100).unwrap())
            .unwrap();
        let expected = ["1750000000|IMG_1.HEIC"];
        assert_eq!(keys(&peer), expected);
        assert_eq!(observations(&peer), expected.map(|k| format!("photos|{k}")));
        mac.photo_points_apply(&old_page).unwrap();
        peer.photo_points_apply(&old_page).unwrap();
        assert_eq!(keys(&mac), expected);
        assert_eq!(keys(&peer), expected);
    }

    /// One photo two Macs' libraries named differently: both earlier keys
    /// stay replaced on every node, so a late page carrying either stores
    /// nothing.
    #[test]
    fn test_every_earlier_key_of_a_photo_stays_replaced() {
        let one = Store::open_in_memory().unwrap();
        let two = Store::open_in_memory().unwrap();
        let hub = Store::open_in_memory().unwrap();
        let sync = |from: &Store, to: &Store| {
            to.photo_points_apply(&from.photo_points_export(0, 100).unwrap())
                .unwrap();
        };
        one.photos_add_points("mac-one", &[at("AAAA-UUID.heic")])
            .unwrap();
        two.photos_add_points("mac-two", &[at("BBBB-UUID.heic")])
            .unwrap();
        let (old_one, old_two) = (
            one.photo_points_export(0, 100).unwrap(),
            two.photo_points_export(0, 100).unwrap(),
        );
        sync(&one, &hub);
        sync(&two, &hub);
        one.photos_add_points("mac-one", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        two.photos_add_points("mac-two", &[scanned("IMG_1.HEIC", "BBBB-UUID.heic")])
            .unwrap();
        sync(&one, &hub);
        sync(&two, &hub);
        sync(&hub, &one);
        sync(&hub, &two);
        let expected = ["1750000000|IMG_1.HEIC"];
        for store in [&one, &two, &hub] {
            assert_eq!(keys(store), expected);
            for old in [&old_one, &old_two] {
                store.photo_points_apply(old).unwrap();
            }
            assert_eq!(keys(store), expected);
        }
        let page = hub.photo_points_export(0, 100).unwrap();
        let mut replaced: Vec<String> =
            serde_json::from_value(page["points"][0]["replaces"].clone()).unwrap();
        replaced.sort();
        assert_eq!(
            replaced,
            ["1750000000|AAAA-UUID.heic", "1750000000|BBBB-UUID.heic"]
        );
    }

    /// The locations leg brings back no observation for a key a point took
    /// the place of, whichever leg arrives first.
    #[test]
    fn test_observations_of_replaced_keys_stay_gone() {
        let old = Store::open_in_memory().unwrap();
        old.photos_add_points("mac", &[at("AAAA-UUID.heic")])
            .unwrap();
        let old_observations = old.observations_since("", 100).unwrap();
        let mac = Store::open_in_memory().unwrap();
        mac.photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        let replacement = mac.photo_points_export(0, 100).unwrap();

        let photos_first = Store::open_in_memory().unwrap();
        photos_first.photo_points_apply(&replacement).unwrap();
        photos_first.observations_apply(&old_observations).unwrap();
        let locations_first = Store::open_in_memory().unwrap();
        locations_first
            .observations_apply(&old_observations)
            .unwrap();
        locations_first.photo_points_apply(&replacement).unwrap();
        for store in [&photos_first, &locations_first] {
            assert_eq!(observations(store), ["photos|1750000000|IMG_1.HEIC"]);
        }
    }

    /// The locations leg can bring an earlier key's observation before the
    /// photos leg brings its point: a scan that moves the photo to its new
    /// key leaves one observation of it — with or without the new key's
    /// point here already.
    #[test]
    fn test_a_scan_moves_an_observation_whose_point_is_not_here() {
        let old = Store::open_in_memory().unwrap();
        old.photos_add_points("mac", &[at("AAAA-UUID.heic")])
            .unwrap();
        let old_observations = old.observations_since("", 100).unwrap();
        let expected = ["photos|1750000000|IMG_1.HEIC"];

        let store = Store::open_in_memory().unwrap();
        store.observations_apply(&old_observations).unwrap();
        store
            .photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        assert_eq!(observations(&store), expected);

        let store = Store::open_in_memory().unwrap();
        store
            .photos_add_points("iPhone", &[at("IMG_1.HEIC")])
            .unwrap();
        store.observations_apply(&old_observations).unwrap();
        store
            .photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        assert_eq!(observations(&store), expected);
    }

    /// A day's minted count drops only for the points it was minted with:
    /// a duplicate stored after the day was minted, merged into a point the
    /// day holds — from a peer's page or by a scan — leaves the count, and
    /// the day isn't picked to mint again; one the day was minted with
    /// lowers it, so the next photo re-mints the day.
    #[test]
    fn test_a_merged_duplicate_lowers_only_a_count_it_was_in() {
        const DAY: &str = "2026-07-20";
        // the day minted from the points stored so far
        let mint = |store: &Store, count: i64| {
            store
                .set_photo_day_state(DAY, count, "minted", None, 1)
                .unwrap();
            let conn = store.lock();
            conn.execute(
                "UPDATE photo_point SET ingest_time = '2026-07-21T00:00:00Z'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE photo_day SET minted_at = '2026-07-21T00:00:00Z'",
                [],
            )
            .unwrap();
        };
        let count = |store: &Store| store.photo_day_state(DAY).unwrap().unwrap().0;
        let picked = |store: &Store| {
            !store
                .photo_days_needing_mint("2026-07-28")
                .unwrap()
                .is_empty()
        };
        let mac = Store::open_in_memory().unwrap();
        mac.photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        let replacing = mac.photo_points_export(0, 100).unwrap();

        // minted with photos 1 and 2; an earlier scan's point for photo 1
        // arrives after, then goes — by a peer's page, or by a scan here
        for by_scan in [false, true] {
            let store = Store::open_in_memory().unwrap();
            store
                .photos_add_points("iPhone", &[at("IMG_1.HEIC"), at("IMG_2.HEIC")])
                .unwrap();
            mint(&store, 2);
            store
                .photos_add_points("mac", &[at("AAAA-UUID.heic")])
                .unwrap();
            if by_scan {
                store
                    .photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
                    .unwrap();
            } else {
                store.photo_points_apply(&replacing).unwrap();
            }
            assert_eq!(keys(&store).len(), 2);
            assert_eq!(count(&store), 2, "by scan: {by_scan}");
            assert!(!picked(&store), "by scan: {by_scan}");
        }

        // minted with the duplicate: the count drops, and the next photo
        // re-mints the day
        let store = Store::open_in_memory().unwrap();
        store
            .photos_add_points(
                "iPhone",
                &[at("IMG_1.HEIC"), at("IMG_2.HEIC"), at("AAAA-UUID.heic")],
            )
            .unwrap();
        mint(&store, 3);
        store.photo_points_apply(&replacing).unwrap();
        assert_eq!(count(&store), 2);
        assert!(!picked(&store));
        store
            .photos_add_points("iPhone", &[at("IMG_3.HEIC")])
            .unwrap();
        assert!(picked(&store));
    }

    /// A day minted with one photo under two earlier keys counted it twice.
    /// The point that takes the place of the first one holds the photo's
    /// place in the count, so taking the place of the second lowers the
    /// count to one — from a peer's pages or by scans here, the point
    /// arriving after the mint — and the next photo re-mints the day.
    #[test]
    fn test_a_point_replacing_keys_one_at_a_time_keeps_the_count_right() {
        const DAY: &str = "2026-07-20";
        let count = |store: &Store| store.photo_day_state(DAY).unwrap().unwrap().0;
        let picked = |store: &Store| {
            !store
                .photo_days_needing_mint("2026-07-28")
                .unwrap()
                .is_empty()
        };
        // a Mac whose scans name the two earlier keys one at a time
        let mac = Store::open_in_memory().unwrap();
        mac.photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
            .unwrap();
        let first_page = mac.photo_points_export(0, 100).unwrap();
        mac.photos_add_points("mac", &[scanned("IMG_1.HEIC", "BBBB-UUID.heic")])
            .unwrap();
        let second_page = mac.photo_points_export(0, 100).unwrap();

        for by_scan in [false, true] {
            let store = Store::open_in_memory().unwrap();
            store
                .photos_add_points("mac", &[at("AAAA-UUID.heic"), at("BBBB-UUID.heic")])
                .unwrap();
            // the day minted from both
            store
                .set_photo_day_state(DAY, 2, "minted", None, 1)
                .unwrap();
            {
                let conn = store.lock();
                conn.execute(
                    "UPDATE photo_point SET ingest_time = '2026-07-21T00:00:00Z'",
                    [],
                )
                .unwrap();
                conn.execute(
                    "UPDATE photo_day SET minted_at = '2026-07-21T00:00:00Z'",
                    [],
                )
                .unwrap();
            }
            if by_scan {
                store
                    .photos_add_points("iPhone", &[at("IMG_1.HEIC")])
                    .unwrap();
                store
                    .photos_add_points("mac", &[scanned("IMG_1.HEIC", "AAAA-UUID.heic")])
                    .unwrap();
            } else {
                store.photo_points_apply(&first_page).unwrap();
            }
            assert_eq!(keys(&store).len(), 2, "by scan: {by_scan}");
            assert_eq!(count(&store), 2, "by scan: {by_scan}");
            if by_scan {
                store
                    .photos_add_points("mac", &[scanned("IMG_1.HEIC", "BBBB-UUID.heic")])
                    .unwrap();
            } else {
                store.photo_points_apply(&second_page).unwrap();
            }
            assert_eq!(
                keys(&store),
                ["1750000000|IMG_1.HEIC"],
                "by scan: {by_scan}"
            );
            assert_eq!(count(&store), 1, "by scan: {by_scan}");
            assert!(!picked(&store), "by scan: {by_scan}");
            store
                .photos_add_points("iPhone", &[at("IMG_2.HEIC")])
                .unwrap();
            assert!(picked(&store), "by scan: {by_scan}");
        }
    }

    /// A day minted with one photo under three earlier keys counted it three
    /// times. Each earlier key the point kept takes the place of lowers the
    /// count by what it held beyond the photo, one at a time — from a
    /// peer's pages or by scans here — until the day counts it once. When
    /// the point kept is in another day (its own clock put it there), the
    /// minted day lets go of the photo altogether.
    #[test]
    fn test_a_photo_under_three_earlier_keys_leaves_each_day_counted_right() {
        const DAY: &str = "2026-07-20";
        let libraries = ["AAAA-UUID.heic", "BBBB-UUID.heic", "CCCC-UUID.heic"];
        let count = |store: &Store| store.photo_day_state(DAY).unwrap().unwrap().0;
        // a Mac whose scans name the three earlier keys one at a time
        let mac = Store::open_in_memory().unwrap();
        let pages: Vec<_> = libraries
            .iter()
            .map(|library| {
                mac.photos_add_points("mac", &[scanned("IMG_1.HEIC", library)])
                    .unwrap();
                mac.photo_points_export(0, 100).unwrap()
            })
            .collect();
        // the phone's point for the photo: the same day, or the evening
        // before by its own clock
        let same_day = at("IMG_1.HEIC");
        let day_before = PhotoPointIn {
            taken_at: "2026-07-20T02:00:00Z".into(),
            ..at("IMG_1.HEIC")
        };

        for (phone, counts) in [(&same_day, [3, 2, 1]), (&day_before, [2, 1, 0])] {
            for by_scan in [false, true] {
                let store = Store::open_in_memory().unwrap();
                let earlier: Vec<PhotoPointIn> = libraries.iter().map(|l| at(l)).collect();
                store.photos_add_points("mac", &earlier).unwrap();
                store
                    .set_photo_day_state(DAY, 3, "minted", None, 1)
                    .unwrap();
                {
                    let conn = store.lock();
                    conn.execute(
                        "UPDATE photo_point SET ingest_time = '2026-07-21T00:00:00Z'",
                        [],
                    )
                    .unwrap();
                    conn.execute(
                        "UPDATE photo_day SET minted_at = '2026-07-21T00:00:00Z'",
                        [],
                    )
                    .unwrap();
                }
                store
                    .photos_add_points("iPhone", std::slice::from_ref(phone))
                    .unwrap();
                for (i, library) in libraries.iter().enumerate() {
                    if by_scan {
                        store
                            .photos_add_points("mac", &[scanned("IMG_1.HEIC", library)])
                            .unwrap();
                    } else {
                        store.photo_points_apply(&pages[i]).unwrap();
                    }
                    assert_eq!(
                        count(&store),
                        counts[i],
                        "key {i}, by scan: {by_scan}, phone {}",
                        phone.taken_at
                    );
                }
                let left = if phone.taken_at == same_day.taken_at {
                    vec!["1750000000|IMG_1.HEIC".to_string()]
                } else {
                    vec![]
                };
                assert_eq!(keys(&store), left, "by scan: {by_scan}");
                // nothing to re-mint on that day until it really grows
                assert!(
                    store
                        .photo_days_needing_mint("2026-07-28")
                        .unwrap()
                        .iter()
                        .all(|d| d != DAY),
                    "by scan: {by_scan}"
                );
            }
        }
    }

    #[test]
    fn test_points_idempotent_and_day_bucketing() {
        let store = Store::open_in_memory().unwrap();
        let batch = vec![point("IMG_1.HEIC", "2026-07-20T12:00:00Z")];
        assert_eq!(store.photos_add_points("iPhone", &batch).unwrap(), (1, 0));
        assert_eq!(store.photos_add_points("iPhone", &batch).unwrap(), (0, 1));
        let days = store.photo_days_needing_mint("2026-07-28").unwrap();
        assert_eq!(days, vec!["2026-07-20"]);
        // today never mints
        assert!(store
            .photo_days_needing_mint("2026-07-20")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_export_apply_roundtrip_unions() {
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        a.photos_add_points("mac", &[point("IMG_A.HEIC", "2026-07-20T12:00:00Z")])
            .unwrap();
        b.photos_add_points("iPhone", &[point("IMG_B.HEIC", "2026-07-20T13:00:00Z")])
            .unwrap();
        let page = a.photo_points_export(0, 100).unwrap();
        assert_eq!(b.photo_points_apply(&page).unwrap(), 1);
        // replay is a no-op
        assert_eq!(b.photo_points_apply(&page).unwrap(), 0);
        assert_eq!(b.photo_points_for_day("2026-07-20").unwrap().len(), 2);
    }
}
