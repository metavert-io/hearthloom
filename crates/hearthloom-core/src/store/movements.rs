//! The movement layer (v60) — ONE durable table for every source that
//! knows a span of travel: the phone's motion segments (`ios-motion`,
//! subject `me`) and vehicles' own drive records (an ember such as the
//! Tesla one, subject `vehicle:<ember>:<id>`). The location layer's
//! sibling (observations.rs), with the same discipline: per-source
//! idempotency (UNIQUE(source, source_ref), INSERT OR IGNORE), and
//! collapsing cross-source redundancy is a PASS's job (daemon trips.rs),
//! never key trickery.
//!
//! The new idea is `subject`: whose movement a row describes. A phone
//! segment is the person's; a car's drive is the CAR's, and becomes the
//! person's only when evidence says they were aboard — owning a car
//! doesn't mean you're the one in it (Jon 2026-10-23).
//!
//! - `vehicle_profile` — per-car attribution settings (synced, LWW):
//!   how to decide (phone evidence / always me / never me), who drives
//!   when you're aboard, household drivers, the scope for drives that
//!   aren't yours, whether to ask on Tend.
//! - `trip_attribution` — the Tend yes/no ("were you in the car?") and
//!   its answer, keyed by the drive's movement id (synced, LWW).
//! - `trip_day` — the minted ledger. Not synced: each node reconciles
//!   the synced inputs itself. Unlike visits, a day's revision suffix is
//!   a FINGERPRINT of its inputs (movements + answers + settings), so two
//!   nodes with the same inputs mint the same refs (same episode ids),
//!   and the pass ASSERTS its set afterwards — a peer's stale revision
//!   landing later is re-superseded instead of winning (the landing rule
//!   in ingest_apply supersedes every other revision regardless of order).

use anyhow::{anyhow, bail, Result};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use super::Store;
use crate::time::now_utc;

/// One incoming movement span.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MovementIn {
    /// Per-source stable id (idempotency with the source name).
    pub key: String,
    /// walking | running | cycling | automotive | stationary | drive
    pub mode: String,
    /// RFC3339 UTC.
    pub start: String,
    pub end: String,
    #[serde(default)]
    pub tz_offset_minutes: i32,
    #[serde(default)]
    pub start_lat: Option<f64>,
    #[serde(default)]
    pub start_lon: Option<f64>,
    #[serde(default)]
    pub end_lat: Option<f64>,
    #[serde(default)]
    pub end_lon: Option<f64>,
    #[serde(default)]
    pub start_place: Option<String>,
    #[serde(default)]
    pub end_place: Option<String>,
    #[serde(default)]
    pub distance_m: Option<f64>,
    /// Source-specific detail kept verbatim (energy_kwh, odometer,
    /// route file, outside temperature, what was playing, guest mode…).
    #[serde(default)]
    pub attrs: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct StoredMovement {
    pub id: String,
    pub source: String,
    pub source_ref: String,
    pub subject: String,
    pub mode: String,
    pub t_start: String,
    pub t_end: String,
    pub tz_offset_minutes: i32,
    pub start_lat: Option<f64>,
    pub start_lon: Option<f64>,
    pub end_lat: Option<f64>,
    pub end_lon: Option<f64>,
    pub start_place: Option<String>,
    pub end_place: Option<String>,
    pub distance_m: Option<f64>,
    pub attrs: Value,
}

pub const MOVEMENT_MODES: &[&str] =
    &["walking", "running", "cycling", "automotive", "stationary", "drive"];

/// Per-car attribution settings.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct VehicleProfile {
    pub vehicle: String,
    pub name: String,
    #[serde(default)]
    pub descriptor: Value,
    /// phone | always_me | never_me
    pub attribution: String,
    /// Who drives when you're aboard: "me", "unknown", or a person's name.
    pub aboard_driver: String,
    /// People who drive this car (names — entity ids differ per node).
    #[serde(default)]
    pub household: Vec<String>,
    /// Scope for drives that aren't yours (work|personal|family|health|private).
    pub others_scope: String,
    /// Ask on Tend about drives the evidence can't settle.
    pub ask: bool,
    pub updated_at: String,
}

pub const ATTRIBUTION_MODES: &[&str] = &["phone", "always_me", "never_me"];
pub const SCOPES: &[&str] = &["work", "personal", "family", "health", "private"];
/// At most this many new Tend questions per car per 7 days.
pub const ASK_CAP_PER_WEEK: i64 = 3;

fn day_of(t: &str, tz_offset_minutes: i32) -> Result<String> {
    let at = chrono::DateTime::parse_from_rfc3339(t).map_err(|_| anyhow!("time must be RFC3339: {t:?}"))?;
    let local = at.timestamp() + tz_offset_minutes as i64 * 60;
    chrono::DateTime::from_timestamp(local, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .ok_or_else(|| anyhow!("unrepresentable time"))
}

fn row_to_movement(row: &rusqlite::Row) -> rusqlite::Result<StoredMovement> {
    let attrs: String = row.get(15)?;
    Ok(StoredMovement {
        id: row.get(0)?,
        source: row.get(1)?,
        source_ref: row.get(2)?,
        subject: row.get(3)?,
        mode: row.get(4)?,
        t_start: row.get(5)?,
        t_end: row.get(6)?,
        tz_offset_minutes: row.get(7)?,
        start_lat: row.get(8)?,
        start_lon: row.get(9)?,
        end_lat: row.get(10)?,
        end_lon: row.get(11)?,
        start_place: row.get(12)?,
        end_place: row.get(13)?,
        distance_m: row.get(14)?,
        attrs: serde_json::from_str(&attrs).unwrap_or_else(|_| json!({})),
    })
}

const MOVEMENT_COLS: &str = "id, source, source_ref, subject, mode, t_start, t_end,
    tz_offset_minutes, start_lat, start_lon, end_lat, end_lon, start_place,
    end_place, distance_m, attrs";

fn profile_from_row(row: &rusqlite::Row) -> rusqlite::Result<VehicleProfile> {
    let descriptor: String = row.get(2)?;
    let household: String = row.get(5)?;
    Ok(VehicleProfile {
        vehicle: row.get(0)?,
        name: row.get(1)?,
        descriptor: serde_json::from_str(&descriptor).unwrap_or_else(|_| json!({})),
        attribution: row.get(3)?,
        aboard_driver: row.get(4)?,
        household: serde_json::from_str(&household).unwrap_or_default(),
        others_scope: row.get(6)?,
        ask: row.get::<_, i64>(7)? != 0,
        updated_at: row.get(8)?,
    })
}

const PROFILE_COLS: &str =
    "vehicle, name, descriptor, attribution, aboard_driver, household, others_scope, ask, updated_at";

impl Store {
    /// Add movement spans for one source + subject — idempotent by
    /// (source, key). Returns (accepted, duplicates).
    pub fn movements_add(
        &self,
        source: &str,
        subject: &str,
        actor: &str,
        rows: &[MovementIn],
    ) -> Result<(usize, usize)> {
        if subject != "me" && !subject.starts_with("vehicle:") {
            bail!("subject must be \"me\" or \"vehicle:…\"");
        }
        let (mut accepted, mut duplicates) = (0usize, 0usize);
        for chunk in rows.chunks(500) {
            {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                for m in chunk {
                    if !MOVEMENT_MODES.contains(&m.mode.as_str()) {
                        bail!("mode must be one of {}", MOVEMENT_MODES.join("|"));
                    }
                    if m.key.is_empty() || m.key.len() > 200 {
                        bail!("key must be 1-200 chars");
                    }
                    let s = chrono::DateTime::parse_from_rfc3339(&m.start)
                        .map_err(|_| anyhow!("start must be RFC3339: {:?}", m.start))?;
                    let e = chrono::DateTime::parse_from_rfc3339(&m.end)
                        .map_err(|_| anyhow!("end must be RFC3339: {:?}", m.end))?;
                    if e < s {
                        bail!("end is before start for {:?}", m.key);
                    }
                    let day = day_of(&m.start, m.tz_offset_minutes)?;
                    let attrs = m.attrs.clone().unwrap_or_else(|| json!({}));
                    let changed = tx.execute(
                        "INSERT OR IGNORE INTO movement_observation
                             (id, source, source_ref, subject, mode, t_start, t_end,
                              tz_offset_minutes, start_lat, start_lon, end_lat, end_lon,
                              start_place, end_place, distance_m, attrs, day, actor, ingest_time)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                        rusqlite::params![
                            format!("{source}|{}", m.key), source, m.key, subject, m.mode,
                            m.start, m.end, m.tz_offset_minutes, m.start_lat, m.start_lon,
                            m.end_lat, m.end_lon, m.start_place, m.end_place, m.distance_m,
                            attrs.to_string(), day, actor, now_utc()
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

    /// Closed days with a vehicle movement that need (re)reconciling: never
    /// minted, movement count changed, marked dirty (an answer or a
    /// settings change), or a live trip episode that isn't the minted set
    /// (a peer's stale revision landed). Phone-only days stay the phone's.
    pub fn movement_days_needing_trips(&self, today_local: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT d.day FROM (
                 SELECT mo.day AS day, COUNT(mo.id) AS n,
                        SUM(mo.subject LIKE 'vehicle:%') AS vehicles
                 FROM movement_observation mo WHERE mo.day < ?1 GROUP BY mo.day
             ) d
             LEFT JOIN trip_day td ON td.day = d.day
             WHERE d.vehicles > 0
               AND (td.day IS NULL OR td.dirty = 1 OR d.n != td.obs_count
                    OR EXISTS (
                        SELECT 1 FROM episodes e
                        WHERE e.source = 'trips' AND e.superseded_by IS NULL
                          AND e.source_ref LIKE 'trip:' || d.day || ':%'
                          AND e.id NOT IN (SELECT j.value FROM json_each(td.minted) j)))
             ORDER BY d.day",
        )?;
        let rows = stmt.query_map([today_local], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Every movement of one local day, time-ordered.
    pub fn movements_for_day(&self, day: &str) -> Result<Vec<StoredMovement>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MOVEMENT_COLS} FROM movement_observation WHERE day = ?1 ORDER BY t_start"
        ))?;
        let rows = stmt.query_map([day], row_to_movement)?;
        rows.collect::<std::result::Result<_, _>>().map_err(Into::into)
    }

    /// Has the person's phone reported ANY motion since `since`? Without
    /// a phone feed, "the phone didn't see you in the car" is no evidence
    /// at all, and attribution falls back to the car's default.
    pub fn phone_motion_since(&self, since: &str) -> Result<bool> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT 1 FROM movement_observation WHERE subject = 'me' AND t_start >= ?1 LIMIT 1",
                [since],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// (fingerprint, minted episode ids) of a day's last reconciliation.
    pub fn trip_day_state(&self, day: &str) -> Result<Option<(String, Vec<String>)>> {
        let conn = self.lock();
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT fingerprint, minted FROM trip_day WHERE day = ?1",
                [day],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(fp, minted)| (fp, serde_json::from_str(&minted).unwrap_or_default())))
    }

    pub fn set_trip_day_state(&self, day: &str, obs_count: i64, fingerprint: &str, minted: &[String]) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO trip_day (day, obs_count, fingerprint, minted, dirty, minted_at)
             VALUES (?1,?2,?3,?4,0,?5)
             ON CONFLICT(day) DO UPDATE SET obs_count=excluded.obs_count,
                 fingerprint=excluded.fingerprint, minted=excluded.minted,
                 dirty=0, minted_at=excluded.minted_at",
            rusqlite::params![day, obs_count, fingerprint, serde_json::to_string(minted)?, now_utc()],
        )?;
        Ok(())
    }

    /// Make exactly `intended` the day's live trip episodes: revive any of
    /// them a stale peer revision superseded (never a retired one), and
    /// supersede every other live trip episode of the day into the first.
    pub fn assert_trip_day(&self, day: &str, intended: &[String]) -> Result<usize> {
        let ids = serde_json::to_string(intended)?;
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut n = tx.execute(
            "UPDATE episodes SET superseded_by = NULL
             WHERE id IN (SELECT value FROM json_each(?1))
               AND superseded_by IS NOT NULL /* all-episodes: reviving our own set */
               AND superseded_by NOT LIKE 'retired:%'",
            [&ids],
        )?;
        let absorbed_by = intended.first().cloned().unwrap_or_else(|| format!("trips:none:{day}"));
        n += tx.execute(
            "UPDATE episodes SET superseded_by = ?3
             WHERE source = 'trips' AND superseded_by IS NULL
               AND source_ref LIKE 'trip:' || ?1 || ':%'
               AND id NOT IN (SELECT value FROM json_each(?2))",
            rusqlite::params![day, ids, absorbed_by],
        )?;
        tx.execute(
            "DELETE FROM location_points WHERE episode_id IN (
                 SELECT id FROM episodes WHERE source = 'trips' AND superseded_by IS NOT NULL
                   AND source_ref LIKE 'trip:' || ?1 || ':%') /* all-episodes: dropping their points */",
            [day],
        )?;
        tx.commit()?;
        Ok(n)
    }

    pub fn movements_latest_tz_offset(&self) -> Result<i32> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT tz_offset_minutes FROM movement_observation ORDER BY ingest_time DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /* ————— reconciliation side effects (daemon trips.rs) ————— */

    /// Re-point the phone's own trip episodes for a day: first release
    /// every `ios-motion` episode a previous reconciliation of this day
    /// had absorbed (an answer may have changed whose drive it was),
    /// then absorb the new set. Retired episodes (`retired:…`) are never
    /// touched — only pointers at this day's `trips` episodes move.
    pub fn repoint_phone_trips(&self, day: &str, absorb: &[(String, String)]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            // all-episodes: releasing supersession must see superseded rows
            "UPDATE episodes SET superseded_by = NULL
             WHERE source = 'ios-motion' /* all-episodes: releases pointers */
               AND superseded_by IN (
                   SELECT id FROM episodes
                   WHERE source = 'trips' AND source_ref LIKE 'trip:' || ?1 || ':%')",
            [day],
        )?;
        let mut n = 0usize;
        for (phone_episode, trip_episode) in absorb {
            n += tx.execute(
                "UPDATE episodes SET superseded_by = ?2
                 WHERE id = ?1 AND source = 'ios-motion' AND superseded_by IS NULL",
                rusqlite::params![phone_episode, trip_episode],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Drop this node's trip-derived location points for drives no longer
    /// attributed to the person (each node derives and drops its own).
    pub fn delete_trip_observations(&self, keys: &[String]) -> Result<usize> {
        let conn = self.lock();
        let mut n = 0usize;
        for key in keys {
            n += conn.execute(
                "DELETE FROM location_observation WHERE source = 'trips' AND source_ref = ?1",
                [key],
            )?;
        }
        Ok(n)
    }

    /// The trip-derived location points currently on file for a day.
    pub fn trip_observation_keys(&self, day: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT source_ref FROM location_observation WHERE source = 'trips' AND day = ?1",
        )?;
        let rows = stmt.query_map([day], |row| row.get(0))?;
        rows.collect::<std::result::Result<_, _>>().map_err(Into::into)
    }

    /* ————— vehicle profiles ————— */

    /// Make sure a car has a profile (defaults: decide by phone evidence,
    /// you drive when aboard, others' drives are `family`, ask on Tend).
    /// Never overwrites the user's settings; refreshes name/descriptor.
    pub fn vehicle_profile_ensure(&self, vehicle: &str, name: &str, descriptor: &Value) -> Result<()> {
        let conn = self.lock();
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT name, descriptor FROM vehicle_profile WHERE vehicle = ?1",
                [vehicle],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let desc = descriptor.to_string();
        match existing {
            None => {
                conn.execute(
                    "INSERT INTO vehicle_profile (vehicle, name, descriptor, attribution,
                         aboard_driver, household, others_scope, ask, updated_at)
                     VALUES (?1, ?2, ?3, 'phone', 'me', '[]', 'family', 1, ?4)",
                    rusqlite::params![vehicle, name, desc, now_utc()],
                )?;
            }
            Some((n, d)) if n != name || d != desc => {
                conn.execute(
                    "UPDATE vehicle_profile SET name = ?2, descriptor = ?3, updated_at = ?4 WHERE vehicle = ?1",
                    rusqlite::params![vehicle, name, desc, now_utc()],
                )?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    pub fn vehicle_profiles(&self) -> Result<Vec<VehicleProfile>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!("SELECT {PROFILE_COLS} FROM vehicle_profile ORDER BY name"))?;
        let rows = stmt.query_map([], profile_from_row)?;
        rows.collect::<std::result::Result<_, _>>().map_err(Into::into)
    }

    pub fn vehicle_profile(&self, vehicle: &str) -> Result<Option<VehicleProfile>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {PROFILE_COLS} FROM vehicle_profile WHERE vehicle = ?1"),
            [vehicle],
            profile_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The user's settings edit (validated). Re-opens reconciliation of
    /// every day with this car's drives — settings change whose drive
    /// each one was.
    pub fn vehicle_profile_set(&self, p: &VehicleProfile) -> Result<()> {
        if !ATTRIBUTION_MODES.contains(&p.attribution.as_str()) {
            bail!("attribution must be one of {}", ATTRIBUTION_MODES.join("|"));
        }
        if !SCOPES.contains(&p.others_scope.as_str()) {
            bail!("scope must be one of {}", SCOPES.join("|"));
        }
        let driver = p.aboard_driver.trim();
        if driver.is_empty() || driver.len() > 120 {
            bail!("aboard_driver is \"me\", \"unknown\", or a person's name");
        }
        let household: Vec<String> = p
            .household
            .iter()
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty() && h.len() <= 120)
            .take(12)
            .collect();
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE vehicle_profile SET attribution = ?2, aboard_driver = ?3, household = ?4,
                 others_scope = ?5, ask = ?6, updated_at = ?7 WHERE vehicle = ?1",
            rusqlite::params![
                p.vehicle, p.attribution, driver, serde_json::to_string(&household)?,
                p.others_scope, p.ask as i64, now_utc()
            ],
        )?;
        if n == 0 {
            bail!("no vehicle {:?}", p.vehicle);
        }
        tx.execute(
            "UPDATE trip_day SET dirty = 1 WHERE day IN (
                 SELECT DISTINCT day FROM movement_observation WHERE subject = ?1)",
            [&p.vehicle],
        )?;
        tx.commit()?;
        Ok(())
    }

    /* ————— the Tend question: were you in the car? ————— */

    /// (answer, driver) for a drive, if answered.
    pub fn trip_answer(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT answer FROM trip_attribution WHERE key = ?1 AND status = 'answered'",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Ask about a drive unless already asked, answered or dismissed, and
    /// under the per-car weekly cap. Returns whether a question opened.
    pub fn trip_question_open(&self, key: &str, vehicle: &str, day: &str, summary: &str) -> Result<bool> {
        let conn = self.lock();
        let exists: Option<i64> = conn
            .query_row("SELECT 1 FROM trip_attribution WHERE key = ?1", [key], |row| row.get(0))
            .optional()?;
        if exists.is_some() {
            return Ok(false);
        }
        let week_ago = (chrono::Utc::now() - chrono::Duration::days(7))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let recent: i64 = conn.query_row(
            "SELECT COUNT(*) FROM trip_attribution WHERE vehicle = ?1 AND asked_at >= ?2",
            rusqlite::params![vehicle, week_ago],
            |row| row.get(0),
        )?;
        if recent >= ASK_CAP_PER_WEEK {
            return Ok(false);
        }
        let now = now_utc();
        conn.execute(
            "INSERT INTO trip_attribution (key, vehicle, day, summary, status, answer, asked_at, answered_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', NULL, ?5, NULL, ?5)",
            rusqlite::params![key, vehicle, day, summary, now],
        )?;
        Ok(true)
    }

    /// Answer a question: `yes` (I was in the car) / `no` (I wasn't) /
    /// `dismiss` (stop asking; the evidence-based default stands).
    pub fn trip_question_answer(&self, key: &str, answer: &str) -> Result<()> {
        let (status, value) = match answer {
            "yes" => ("answered", Some("aboard")),
            "no" => ("answered", Some("not_me")),
            "dismiss" => ("dismissed", None),
            _ => bail!("answer is yes, no or dismiss"),
        };
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let day: Option<String> = tx
            .query_row("SELECT day FROM trip_attribution WHERE key = ?1", [key], |row| row.get(0))
            .optional()?;
        let Some(day) = day else { bail!("no question {key:?}") };
        let now = now_utc();
        tx.execute(
            "UPDATE trip_attribution SET status = ?2, answer = ?3, answered_at = ?4, updated_at = ?4 WHERE key = ?1",
            rusqlite::params![key, status, value, now],
        )?;
        // the day re-reconciles with the answer
        tx.execute("UPDATE trip_day SET dirty = 1 WHERE day = ?1", [&day])?;
        tx.commit()?;
        Ok(())
    }

    pub fn trip_questions_pending(&self) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ta.key, ta.vehicle, ta.day, ta.summary, ta.asked_at, vp.name
             FROM trip_attribution ta LEFT JOIN vehicle_profile vp ON vp.vehicle = ta.vehicle
             WHERE ta.status = 'pending' ORDER BY ta.day DESC LIMIT 50",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "key": row.get::<_, String>(0)?,
                "vehicle": row.get::<_, String>(1)?,
                "day": row.get::<_, String>(2)?,
                "summary": row.get::<_, String>(3)?,
                "asked_at": row.get::<_, Option<String>>(4)?,
                "vehicle_name": row.get::<_, Option<String>>(5)?,
            }))
        })?;
        rows.collect::<std::result::Result<_, _>>().map_err(Into::into)
    }

    pub fn trip_questions_pending_count(&self) -> Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM trip_attribution WHERE status = 'pending'", [], |row| row.get(0))?)
    }

    /* ————— peer sync ————— */

    /// Movements past a watermark (telemetry pattern: append-only).
    pub fn movements_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MOVEMENT_COLS}, day, actor FROM movement_observation
             WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2"
        ))?;
        let items: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                let m = row_to_movement(row)?;
                Ok(json!({
                    "id": m.id, "source": m.source, "source_ref": m.source_ref,
                    "subject": m.subject, "mode": m.mode, "t_start": m.t_start,
                    "t_end": m.t_end, "tz_offset_minutes": m.tz_offset_minutes,
                    "start_lat": m.start_lat, "start_lon": m.start_lon,
                    "end_lat": m.end_lat, "end_lon": m.end_lon,
                    "start_place": m.start_place, "end_place": m.end_place,
                    "distance_m": m.distance_m, "attrs": m.attrs,
                    "day": row.get::<_, String>(16)?, "actor": row.get::<_, String>(17)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(&conn, "synced_at", "FROM movement_observation WHERE synced_at > ?1", since, limit)?;
        Ok(json!({ "items": items, "next": next }))
    }

    pub fn movements_apply(&self, batch: &Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for r in batch.get("items").and_then(|v| v.as_array()).into_iter().flatten() {
            let s = |k: &str| r.get(k).and_then(|v| v.as_str());
            let f = |k: &str| r.get(k).and_then(|v| v.as_f64());
            let (Some(id), Some(source), Some(source_ref), Some(subject), Some(mode), Some(t_start), Some(t_end), Some(day)) = (
                s("id"), s("source"), s("source_ref"), s("subject"), s("mode"), s("t_start"), s("t_end"), s("day"),
            ) else {
                continue;
            };
            if !(subject == "me" || subject.starts_with("vehicle:")) || !MOVEMENT_MODES.contains(&mode) {
                continue;
            }
            applied += tx.execute(
                "INSERT OR IGNORE INTO movement_observation
                     (id, source, source_ref, subject, mode, t_start, t_end,
                      tz_offset_minutes, start_lat, start_lon, end_lat, end_lon,
                      start_place, end_place, distance_m, attrs, day, actor, ingest_time)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
                rusqlite::params![
                    id, source, source_ref, subject, mode, t_start, t_end,
                    r.get("tz_offset_minutes").and_then(|v| v.as_i64()).unwrap_or(0),
                    f("start_lat"), f("start_lon"), f("end_lat"), f("end_lon"),
                    s("start_place"), s("end_place"), f("distance_m"),
                    r.get("attrs").cloned().unwrap_or_else(|| json!({})).to_string(),
                    day, s("actor").unwrap_or("peer"), now_utc()
                ],
            )?;
        }
        tx.commit()?;
        Ok(applied)
    }

    /// Profiles + Tend answers past a watermark (one LWW leg for both).
    pub fn vehicles_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {PROFILE_COLS} FROM vehicle_profile WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2"
        ))?;
        let profiles: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(serde_json::to_value(profile_from_row(row)?).unwrap_or(Value::Null))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut stmt = conn.prepare(
            "SELECT key, vehicle, day, summary, status, answer, asked_at, answered_at, updated_at
             FROM trip_attribution WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2",
        )?;
        let answers: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(json!({
                    "key": row.get::<_, String>(0)?, "vehicle": row.get::<_, String>(1)?,
                    "day": row.get::<_, String>(2)?, "summary": row.get::<_, String>(3)?,
                    "status": row.get::<_, String>(4)?, "answer": row.get::<_, Option<String>>(5)?,
                    "asked_at": row.get::<_, Option<String>>(6)?,
                    "answered_at": row.get::<_, Option<String>>(7)?,
                    "updated_at": row.get::<_, String>(8)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let a = super::page_cursor(&conn, "synced_at", "FROM vehicle_profile WHERE synced_at > ?1", since, limit)?;
        let b = super::page_cursor(&conn, "synced_at", "FROM trip_attribution WHERE synced_at > ?1", since, limit)?;
        // the slower table bounds the cursor so neither skips rows
        let next = match (profiles.len() >= limit, answers.len() >= limit) {
            (true, true) => a.min(b),
            (true, false) => a,
            (false, true) => b,
            (false, false) => a.max(b),
        };
        Ok(json!({ "profiles": profiles, "answers": answers, "next": next }))
    }

    /// LWW by updated_at; an applied answer or profile change re-opens
    /// the affected days' reconciliation on this node.
    pub fn vehicles_apply(&self, batch: &Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for p in batch.get("profiles").and_then(|v| v.as_array()).into_iter().flatten() {
            let Ok(p) = serde_json::from_value::<VehicleProfile>(p.clone()) else { continue };
            let local: Option<String> = tx
                .query_row("SELECT updated_at FROM vehicle_profile WHERE vehicle = ?1", [&p.vehicle], |row| row.get(0))
                .optional()?;
            if local.as_deref().is_some_and(|l| l >= p.updated_at.as_str()) {
                continue;
            }
            applied += tx.execute(
                "INSERT INTO vehicle_profile (vehicle, name, descriptor, attribution, aboard_driver,
                     household, others_scope, ask, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(vehicle) DO UPDATE SET name=excluded.name, descriptor=excluded.descriptor,
                     attribution=excluded.attribution, aboard_driver=excluded.aboard_driver,
                     household=excluded.household, others_scope=excluded.others_scope,
                     ask=excluded.ask, updated_at=excluded.updated_at",
                rusqlite::params![
                    p.vehicle, p.name, p.descriptor.to_string(), p.attribution, p.aboard_driver,
                    serde_json::to_string(&p.household)?, p.others_scope, p.ask as i64, p.updated_at
                ],
            )?;
            tx.execute(
                "UPDATE trip_day SET dirty = 1 WHERE day IN (
                     SELECT DISTINCT day FROM movement_observation WHERE subject = ?1)",
                [&p.vehicle],
            )?;
        }
        for a in batch.get("answers").and_then(|v| v.as_array()).into_iter().flatten() {
            let s = |k: &str| a.get(k).and_then(|v| v.as_str());
            let (Some(key), Some(vehicle), Some(day), Some(status), Some(updated_at)) =
                (s("key"), s("vehicle"), s("day"), s("status"), s("updated_at"))
            else {
                continue;
            };
            let local: Option<(String, String)> = tx
                .query_row(
                    "SELECT updated_at, status FROM trip_attribution WHERE key = ?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            // LWW; stamps are second-resolution, so on a tie a decided
            // question (answered/dismissed) beats one still pending
            let rank = |st: &str| if st == "pending" { 0 } else { 1 };
            if let Some((l_at, l_status)) = &local {
                if l_at.as_str() > updated_at
                    || (l_at.as_str() == updated_at && rank(l_status) >= rank(status))
                {
                    continue;
                }
            }
            applied += tx.execute(
                "INSERT INTO trip_attribution (key, vehicle, day, summary, status, answer, asked_at, answered_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(key) DO UPDATE SET status=excluded.status, answer=excluded.answer,
                     answered_at=excluded.answered_at, updated_at=excluded.updated_at",
                rusqlite::params![
                    key, vehicle, day, s("summary").unwrap_or(""), status, s("answer"),
                    s("asked_at"), s("answered_at"), updated_at
                ],
            )?;
            if status == "answered" {
                tx.execute("UPDATE trip_day SET dirty = 1 WHERE day = ?1", [day])?;
            }
        }
        tx.commit()?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(&tmp.path().join("h.db")).unwrap();
        (s, tmp)
    }

    fn mv(key: &str, mode: &str, start: &str, end: &str) -> MovementIn {
        MovementIn {
            key: key.into(),
            mode: mode.into(),
            start: start.into(),
            end: end.into(),
            tz_offset_minutes: -240,
            start_lat: None,
            start_lon: None,
            end_lat: None,
            end_lon: None,
            start_place: None,
            end_place: None,
            distance_m: None,
            attrs: None,
        }
    }

    #[test]
    fn test_movements_idempotent_and_validated() {
        let (s, _t) = store();
        let v = "vehicle:htesla:abc";
        let rows = [mv("d1", "drive", "2026-10-20T12:00:00Z", "2026-10-20T12:30:00Z")];
        assert_eq!(s.movements_add("plugin:htesla", v, "plugin:htesla", &rows).unwrap(), (1, 0));
        assert_eq!(s.movements_add("plugin:htesla", v, "plugin:htesla", &rows).unwrap(), (0, 1));
        assert!(s.movements_add("x", "someone", "a", &rows).is_err(), "subject vocabulary");
        assert!(s.movements_add("x", "me", "a", &[mv("k", "teleport", "2026-10-20T12:00:00Z", "2026-10-20T12:01:00Z")]).is_err());
        assert!(s.movements_add("x", "me", "a", &[mv("k", "walking", "2026-10-20T12:00:00Z", "2026-10-20T11:00:00Z")]).is_err());
        let day = s.movements_for_day("2026-10-20").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].subject, v);
    }

    #[test]
    fn test_days_need_a_vehicle_and_regrow() {
        let (s, _t) = store();
        s.movements_add("ios-motion", "me", "a", &[mv("p1", "automotive", "2026-10-20T12:00:00Z", "2026-10-20T12:30:00Z")]).unwrap();
        assert!(s.movement_days_needing_trips("2026-10-22").unwrap().is_empty(), "phone-only days stay the phone's");
        s.movements_add("plugin:htesla", "vehicle:htesla:a", "a", &[mv("d1", "drive", "2026-10-20T12:01:00Z", "2026-10-20T12:29:00Z")]).unwrap();
        assert_eq!(s.movement_days_needing_trips("2026-10-22").unwrap(), vec!["2026-10-20"]);
        assert!(s.movement_days_needing_trips("2026-10-20").unwrap().is_empty(), "today isn't closed");
        s.set_trip_day_state("2026-10-20", 2, "fp", &[]).unwrap();
        assert!(s.movement_days_needing_trips("2026-10-22").unwrap().is_empty());
        s.movements_add("ios-motion", "me", "a", &[mv("p2", "walking", "2026-10-20T13:00:00Z", "2026-10-20T13:10:00Z")]).unwrap();
        assert_eq!(s.movement_days_needing_trips("2026-10-22").unwrap(), vec!["2026-10-20"], "a new input re-opens");
    }

    #[test]
    fn test_profile_defaults_edits_and_lww_sync() {
        let (a, _ta) = store();
        let (b, _tb) = store();
        a.vehicle_profile_ensure("vehicle:htesla:a", "Model Y", &json!({"make": "Tesla"})).unwrap();
        let p = a.vehicle_profile("vehicle:htesla:a").unwrap().unwrap();
        assert_eq!((p.attribution.as_str(), p.aboard_driver.as_str(), p.others_scope.as_str(), p.ask),
                   ("phone", "me", "family", true));
        let mut edit = p.clone();
        edit.household = vec!["Alex".into(), " ".into()];
        edit.others_scope = "private".into();
        a.vehicle_profile_set(&edit).unwrap();
        assert!(a.vehicle_profile_set(&VehicleProfile { attribution: "maybe".into(), ..edit.clone() }).is_err());
        // ensure never clobbers settings
        a.vehicle_profile_ensure("vehicle:htesla:a", "Model Y", &json!({"make": "Tesla"})).unwrap();
        let p = a.vehicle_profile("vehicle:htesla:a").unwrap().unwrap();
        assert_eq!(p.household, vec!["Alex".to_string()]);
        assert_eq!(p.others_scope, "private");
        // sync to b
        let page = a.vehicles_since("", 100).unwrap();
        assert_eq!(b.vehicles_apply(&page).unwrap(), 1);
        assert_eq!(b.vehicle_profile("vehicle:htesla:a").unwrap().unwrap().others_scope, "private");
        assert_eq!(b.vehicles_apply(&page).unwrap(), 0, "unchanged rows don't count");
    }

    #[test]
    fn test_question_cap_answer_and_sync() {
        let (a, _ta) = store();
        let (b, _tb) = store();
        let v = "vehicle:htesla:a";
        a.vehicle_profile_ensure(v, "Model Y", &json!({})).unwrap();
        for i in 0..5 {
            a.trip_question_open(&format!("k{i}"), v, "2026-10-20", "a drive").unwrap();
        }
        assert_eq!(a.trip_questions_pending_count().unwrap(), ASK_CAP_PER_WEEK, "weekly cap");
        assert!(!a.trip_question_open("k0", v, "2026-10-20", "again").unwrap(), "asked once");
        a.movements_add("plugin:htesla", v, "a", &[mv("k0", "drive", "2026-10-20T12:00:00Z", "2026-10-20T12:30:00Z")]).unwrap();
        a.set_trip_day_state("2026-10-20", 1, "fp", &[]).unwrap();
        assert!(a.movement_days_needing_trips("2026-10-22").unwrap().is_empty());
        a.trip_question_answer("k0", "no").unwrap();
        assert_eq!(a.trip_answer("k0").unwrap().as_deref(), Some("not_me"));
        assert_eq!(a.movement_days_needing_trips("2026-10-22").unwrap(), vec!["2026-10-20"], "an answer re-opens the day");
        assert!(a.trip_question_answer("k1", "perhaps").is_err());
        b.vehicles_apply(&a.vehicles_since("", 100).unwrap()).unwrap();
        assert_eq!(b.trip_answer("k0").unwrap().as_deref(), Some("not_me"), "answers sync");
    }

    #[test]
    fn test_movements_sync_roundtrip() {
        let (a, _ta) = store();
        let (b, _tb) = store();
        let mut d = mv("d1", "drive", "2026-10-20T12:00:00Z", "2026-10-20T12:30:00Z");
        d.attrs = Some(json!({"energy_kwh": 3.2}));
        a.movements_add("plugin:htesla", "vehicle:htesla:a", "plugin:htesla", &[d]).unwrap();
        let page = a.movements_since("", 100).unwrap();
        assert_eq!(b.movements_apply(&page).unwrap(), 1);
        assert_eq!(b.movements_apply(&page).unwrap(), 0);
        let got = b.movements_for_day("2026-10-20").unwrap();
        assert_eq!(got[0].attrs["energy_kwh"], json!(3.2));
    }
}
