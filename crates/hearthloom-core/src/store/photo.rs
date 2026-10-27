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
    /// pattern) — the Mac scan can push tens of thousands at once.
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
                              device, day, ingest_time)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
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
                            now_utc()
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
            "SELECT rowid, key, filename, at_unix, tz_offset_minutes, lat, lon, place, device
             FROM photo_point WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![after, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                serde_json::json!({
                    "key": row.get::<_, String>(1)?,
                    "filename": row.get::<_, String>(2)?,
                    "at_unix": row.get::<_, i64>(3)?,
                    "tz_offset_minutes": row.get::<_, Option<i32>>(4)?,
                    "lat": row.get::<_, Option<f64>>(5)?,
                    "lon": row.get::<_, Option<f64>>(6)?,
                    "place": row.get::<_, Option<String>>(7)?,
                    "device": row.get::<_, String>(8)?,
                }),
            ))
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

    /// Apply a peer's photo-point page — idempotent by key.
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
            applied += tx.execute(
                "INSERT OR IGNORE INTO photo_point
                     (key, filename, at_unix, tz_offset_minutes, lat, lon, place,
                      device, day, ingest_time)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
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
                    now_utc()
                ],
            )?;
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
