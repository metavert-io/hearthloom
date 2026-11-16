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
        let (mut accepted, mut duplicates) = (0usize, 0usize);
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
                    if changed == 1 {
                        accepted += 1
                    } else {
                        duplicates += 1
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
        Ok((accepted, duplicates))
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
        let conn = self.lock();
        let mut stmt = conn.prepare(
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
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
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
        Ok(applied)
    }
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
