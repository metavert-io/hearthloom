//! The unified location-observation layer (v51) — ONE durable table
//! every geolocated signal lands in, whatever its source: photo geotags
//! (dual-written by photos_add_points + backfilled at migration),
//! iOS CLVisit dwells (POST /v1/ingest/location), calendar venues with
//! coordinates, scan capture points, and any later import (Google-Maps
//! timeline). Idempotency is per-source — UNIQUE(source, source_ref),
//! INSERT OR IGNORE — because different sources legitimately carry
//! different refs for the same real-world moment; collapsing THAT
//! redundancy is the visits_finalize pass's job (spatial+temporal
//! clustering), never key trickery.
//!
//! `visit_day` is the minted ledger (the photo_day precedent): what each
//! day's visit episodes were clustered from, so the pass re-mints only
//! when the day's observation union strictly grows (the superset guard —
//! a node that hasn't synced the other source's observations yet can
//! never clobber a richer day with a subset).
//!
//! Peer-synced via /v1/sync/locations on the telemetry-leg pattern
//! (append-only, id-keyed INSERT OR IGNORE, ts watermark) — the phone
//! posts to the tenant, the Mac scan lands on the laptop; neither alone
//! holds the union. visit_day deliberately does NOT sync: each node
//! re-clusters from the synced observations and converges.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// One incoming observation (the HTTP layer deserializes into this).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ObservationIn {
    /// Per-source stable id (idempotency with the source name).
    pub key: String,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub accuracy_m: Option<f64>,
    /// RFC3339 UTC arrival / capture instant.
    pub t_start: String,
    /// RFC3339 UTC departure — dwell sources (CLVisit, calendar) only.
    #[serde(default)]
    pub t_end: Option<String>,
    #[serde(default)]
    pub tz_offset_minutes: i32,
    /// A human place label when the source knows one (photo place,
    /// calendar venue) — the visit pass's naming material.
    #[serde(default)]
    pub place: Option<String>,
}

/// A stored observation, for the visit-consolidation pass.
#[derive(Debug, Clone)]
pub struct StoredObservation {
    pub id: String,
    pub source: String,
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: Option<f64>,
    pub t_start: String,
    pub t_end: Option<String>,
    pub tz_offset_minutes: i32,
    pub place: Option<String>,
}

impl Store {
    /// Upsert observations for one source — idempotent by (source, key).
    /// Returns (accepted, duplicates). Batched lock holds (the
    /// refresh_local_sources pattern).
    pub fn observations_add(
        &self,
        source: &str,
        actor: &str,
        rows: &[ObservationIn],
    ) -> Result<(usize, usize)> {
        let (mut accepted, mut duplicates) = (0usize, 0usize);
        for chunk in rows.chunks(500) {
            {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                for o in chunk {
                    let at = chrono::DateTime::parse_from_rfc3339(&o.t_start)
                        .map_err(|_| anyhow!("t_start must be RFC3339: {:?}", o.t_start))?;
                    let local = at.timestamp() + o.tz_offset_minutes as i64 * 60;
                    let day = chrono::DateTime::from_timestamp(local, 0)
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                        .ok_or_else(|| anyhow!("unrepresentable t_start"))?;
                    let changed = tx.execute(
                        "INSERT OR IGNORE INTO location_observation
                             (id, source, source_ref, lat, lon, accuracy_m, t_start,
                              t_end, tz_offset_minutes, place, day, actor, ingest_time)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                        rusqlite::params![
                            format!("{source}|{}", o.key), source, o.key, o.lat, o.lon,
                            o.accuracy_m, o.t_start, o.t_end, o.tz_offset_minutes,
                            o.place, day, actor, now_utc()
                        ],
                    )?;
                    if changed == 1 { accepted += 1 } else { duplicates += 1 }
                }
                tx.commit()?;
            }
            if rows.len() > 500 {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
        Ok((accepted, duplicates))
    }

    /// Closed days (day < today_local) whose observation union grew past
    /// the minted ledger (or were never clustered). Mirrors
    /// photo_days_needing_mint.
    pub fn observation_days_needing_visits(&self, today_local: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT lo.day FROM location_observation lo
             WHERE lo.day < ?1
             GROUP BY lo.day
             HAVING COUNT(lo.id) > COALESCE(
                 (SELECT vd.obs_count FROM visit_day vd WHERE vd.day = lo.day), -1)
             ORDER BY lo.day",
        )?;
        let rows = stmt.query_map([today_local], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// All observations for one local day, time-ordered — the clustering
    /// pass's working set.
    pub fn observations_for_day(&self, day: &str) -> Result<Vec<StoredObservation>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, source, lat, lon, accuracy_m, t_start, t_end,
                    tz_offset_minutes, place
             FROM location_observation WHERE day = ?1 ORDER BY t_start",
        )?;
        let rows = stmt.query_map([day], |row| {
            Ok(StoredObservation {
                id: row.get(0)?,
                source: row.get(1)?,
                lat: row.get(2)?,
                lon: row.get(3)?,
                accuracy_m: row.get(4)?,
                t_start: row.get(5)?,
                t_end: row.get(6)?,
                tz_offset_minutes: row.get(7)?,
                place: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>().map_err(Into::into)
    }

    /// The minted ledger row: (obs_count, keys_hash, revision).
    pub fn visit_day_state(&self, day: &str) -> Result<Option<(i64, String, i64)>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT obs_count, keys_hash, revision FROM visit_day WHERE day = ?1",
            [day],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_visit_day_state(
        &self,
        day: &str,
        obs_count: i64,
        keys_hash: &str,
        revision: i64,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO visit_day (day, obs_count, keys_hash, revision, minted_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(day) DO UPDATE SET obs_count=excluded.obs_count,
                 keys_hash=excluded.keys_hash, revision=excluded.revision,
                 minted_at=excluded.minted_at",
            rusqlite::params![day, obs_count, keys_hash, revision, now_utc()],
        )?;
        Ok(())
    }

    /// Mark a day's leftover visit episodes superseded when a re-cluster
    /// yields FEWER visits than before (two clusters merged): every
    /// unsuperseded `visit:<day>:<idx>…` episode whose revision is below
    /// `revision` gets absorbed into `absorbed_by`. The normal grow case
    /// is handled receiver-side by the #r<n> ref pattern in ingest_apply;
    /// this covers only the shrink case, and both peers converge because
    /// both re-cluster the same synced observations.
    pub fn supersede_stale_visits(
        &self,
        day: &str,
        revision: i64,
        absorbed_by: &str,
    ) -> Result<usize> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE episodes SET superseded_by = ?1
             WHERE source = 'visits' AND superseded_by IS NULL AND id != ?1
               AND (source_ref LIKE 'visit:' || ?2 || ':%')
               AND COALESCE(
                   CAST(substr(source_ref, instr(source_ref, '#r') + 2) AS INTEGER), 1)
                   < ?3
               AND instr(source_ref, '#r') > 0
             ",
            rusqlite::params![absorbed_by, day, revision],
        )? + conn.execute(
            // rev-1 refs carry no #r suffix at all
            "UPDATE episodes SET superseded_by = ?1
             WHERE source = 'visits' AND superseded_by IS NULL AND id != ?1
               AND source_ref LIKE 'visit:' || ?2 || ':%'
               AND instr(source_ref, '#r') = 0 AND ?3 > 1",
            rusqlite::params![absorbed_by, day, revision],
        )?;
        conn.execute(
            "DELETE FROM location_points WHERE episode_id IN (
                 SELECT id FROM episodes WHERE superseded_by = ?1)",
            [absorbed_by],
        )?;
        Ok(n)
    }

    /// Live visit episodes past an episodes-rowid watermark, oldest first:
    /// (rowid, id, source_ref, body). The working set of a pass that reads
    /// each visit once (daemon visits.rs, the calendar-only cleanup) —
    /// private visits too: it decides lifecycle and shows nothing.
    pub fn visits_after(&self, after_rowid: i64, limit: usize) -> Result<Vec<(i64, String, String, String)>> {
        let conn = self.lock();
        // idx_episodes_source (source, kind) walks the visits in rowid
        // order; the body comes from the FTS row by its rowid pointer
        let mut stmt = conn.prepare(
            "SELECT e.rowid, e.id, e.source_ref, f.body
             FROM episodes e JOIN episode_fts f ON f.rowid = e.fts_rowid
             WHERE e.source = 'visits' AND e.kind = 'visit' AND e.rowid > ?1
               AND e.superseded_by IS NULL
             ORDER BY e.rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![after_rowid, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<std::result::Result<_, _>>().map_err(Into::into)
    }

    /// Withdraw visit episodes: each (id, marker) still live is marked
    /// superseded by its marker — a ref no episode carries, computed the
    /// same on every node — and loses its map point, like any superseded
    /// row. Content stays; nothing leaves `episodes`. One mutation per
    /// visit says why. Returns how many were withdrawn.
    pub fn withdraw_visits(&self, actor: &Actor, reason: &str, withdrawals: &[(String, String)]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut n = 0usize;
        for (id, marker) in withdrawals {
            let changed = tx.execute(
                "UPDATE episodes SET superseded_by = ?2
                 WHERE id = ?1 AND source = 'visits' AND superseded_by IS NULL",
                [id, marker],
            )?;
            if changed == 0 {
                continue;
            }
            tx.execute("DELETE FROM location_points WHERE episode_id = ?1", [id])?;
            record_mutation(
                &tx,
                actor,
                "visit_withdrawn",
                &format!("visit_withdrawn:{id}"),
                &json!({"episode": id, "superseded_by": marker, "reason": reason}),
            )?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Peer-sync export: observations past a ts watermark (telemetry
    /// pattern — append-only, id-keyed, replay-safe).
    pub fn observations_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, source, source_ref, lat, lon, accuracy_m, t_start, t_end,
                    tz_offset_minutes, place, day, actor, ingest_time
             FROM location_observation WHERE synced_at > ?1
             ORDER BY synced_at LIMIT ?2",
        )?;
        let items: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?, "source": row.get::<_, String>(1)?,
                    "source_ref": row.get::<_, String>(2)?,
                    "lat": row.get::<_, f64>(3)?, "lon": row.get::<_, f64>(4)?,
                    "accuracy_m": row.get::<_, Option<f64>>(5)?,
                    "t_start": row.get::<_, String>(6)?,
                    "t_end": row.get::<_, Option<String>>(7)?,
                    "tz_offset_minutes": row.get::<_, i32>(8)?,
                    "place": row.get::<_, Option<String>>(9)?,
                    "day": row.get::<_, String>(10)?,
                    "actor": row.get::<_, String>(11)?,
                    "ingest_time": row.get::<_, String>(12)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(&conn, "synced_at", "FROM location_observation WHERE synced_at > ?1", since, limit)?;
        Ok(json!({ "items": items, "next": next }))
    }

    /// Apply a peer's observation page — INSERT OR IGNORE by id. The
    /// receiver re-stamps ingest_time so its own export watermark stays
    /// monotonic.
    pub fn observations_apply(&self, batch: &Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for r in batch.get("items").and_then(|v| v.as_array()).into_iter().flatten() {
            let (Some(id), Some(source), Some(source_ref), Some(lat), Some(lon), Some(t_start), Some(day)) = (
                r.get("id").and_then(|v| v.as_str()),
                r.get("source").and_then(|v| v.as_str()),
                r.get("source_ref").and_then(|v| v.as_str()),
                r.get("lat").and_then(|v| v.as_f64()),
                r.get("lon").and_then(|v| v.as_f64()),
                r.get("t_start").and_then(|v| v.as_str()),
                r.get("day").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            applied += tx.execute(
                "INSERT OR IGNORE INTO location_observation
                     (id, source, source_ref, lat, lon, accuracy_m, t_start,
                      t_end, tz_offset_minutes, place, day, actor, ingest_time)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                rusqlite::params![
                    id, source, source_ref, lat, lon,
                    r.get("accuracy_m").and_then(|v| v.as_f64()),
                    t_start,
                    r.get("t_end").and_then(|v| v.as_str()),
                    r.get("tz_offset_minutes").and_then(|v| v.as_i64()).unwrap_or(0),
                    r.get("place").and_then(|v| v.as_str()),
                    day,
                    r.get("actor").and_then(|v| v.as_str()).unwrap_or("peer"),
                    now_utc()
                ],
            )?;
        }
        tx.commit()?;
        Ok(applied)
    }

    /// The tz offset of the freshest observation — the device-day clock
    /// for the visit pass (the health_latest_tz_offset precedent).
    pub fn observations_latest_tz_offset(&self) -> Result<i32> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT tz_offset_minutes FROM location_observation
                 ORDER BY ingest_time DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(key: &str, t: &str, lat: f64, lon: f64) -> ObservationIn {
        ObservationIn {
            key: key.into(),
            lat,
            lon,
            accuracy_m: Some(20.0),
            t_start: t.into(),
            t_end: None,
            tz_offset_minutes: -240,
            place: None,
        }
    }

    #[test]
    fn test_observations_idempotent_per_source() {
        let store = Store::open_in_memory().unwrap();
        let rows = vec![obs("k1", "2026-08-10T15:00:00Z", 42.36, -71.06)];
        assert_eq!(store.observations_add("ios-visits", "adapter:ios", &rows).unwrap(), (1, 0));
        assert_eq!(store.observations_add("ios-visits", "adapter:ios", &rows).unwrap(), (0, 1));
        // the SAME key under a different source is a different observation
        assert_eq!(store.observations_add("photos", "adapter:photos", &rows).unwrap(), (1, 0));
        assert_eq!(store.observations_for_day("2026-08-10").unwrap().len(), 2);
    }

    #[test]
    fn test_days_needing_visits_and_ledger() {
        let store = Store::open_in_memory().unwrap();
        store
            .observations_add("ios-visits", "a", &[obs("k1", "2026-08-10T15:00:00Z", 42.0, -71.0)])
            .unwrap();
        assert_eq!(
            store.observation_days_needing_visits("2026-08-12").unwrap(),
            vec!["2026-08-10"]
        );
        // today never clusters
        assert!(store.observation_days_needing_visits("2026-08-10").unwrap().is_empty());
        store.set_visit_day_state("2026-08-10", 1, "h", 1).unwrap();
        assert!(store.observation_days_needing_visits("2026-08-12").unwrap().is_empty());
        // a new observation reopens the day
        store
            .observations_add("photos", "a", &[obs("k2", "2026-08-10T16:00:00Z", 42.0, -71.0)])
            .unwrap();
        assert_eq!(
            store.observation_days_needing_visits("2026-08-12").unwrap(),
            vec!["2026-08-10"]
        );
    }

    #[test]
    fn test_peer_export_apply_roundtrip() {
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        a.observations_add("ios-visits", "a", &[obs("k1", "2026-08-10T15:00:00Z", 42.0, -71.0)])
            .unwrap();
        let page = a.observations_since("", 100).unwrap();
        assert_eq!(b.observations_apply(&page).unwrap(), 1);
        assert_eq!(b.observations_apply(&page).unwrap(), 0, "replay is a no-op");
        assert_eq!(b.observations_for_day("2026-08-10").unwrap().len(), 1);
    }
}
