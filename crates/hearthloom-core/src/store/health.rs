//! The durable live-HealthKit sample store (v44) — the landing table for
//! POST /v1/ingest/health. Unlike the export.xml sidecar
//! (health_index.rs in the daemon: derived, ephemeral, rebuilt on scan),
//! these rows are PRIMARY data: the device stream has no export.xml to
//! rebuild from. The day-finalize pass aggregates them into `health_day`
//! episodes through the SAME apple_health::DayTable the export path uses,
//! so whichever path mints `day:YYYY-MM-DD` first wins (the store's
//! UNIQUE(source, source_ref) + INSERT OR IGNORE) and the bodies match.
//!
//! Dependency note: this crate cannot see hearthloom-adapters, so reads
//! return plain `HealthSampleRow`s; the daemon maps them onto
//! `apple_health::Sample` before feeding the aggregator.

use anyhow::Result;
use chrono::{DateTime, FixedOffset};
use rusqlite::OptionalExtension;

use super::Store;
use crate::time::now_utc;

/// One live sample, as received from the device (and as stored). The
/// HTTP layer deserializes straight into this shape.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HealthSampleRow {
    pub uuid: String,
    /// Short metric name, HK prefixes already stripped by the client
    /// (`StepCount`, `HeartRate`, `SleepAnalysis`, …).
    pub metric: String,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub unit: Option<String>,
    /// Category value for category samples (`AsleepDeep`, …).
    #[serde(default)]
    pub category: Option<String>,
    /// HK source name ("Apple Watch", "iPhone").
    pub source: String,
    /// RFC3339 UTC.
    pub start: String,
    pub end: String,
    pub tz_offset_minutes: i32,
}

/// A stored row read back for aggregation (start/end are RFC3339 UTC as
/// stored; `day` is the precomputed local bucket).
#[derive(Debug, Clone)]
pub struct StoredHealthSample {
    pub metric: String,
    pub value: Option<f64>,
    pub unit: Option<String>,
    pub category: Option<String>,
    pub source: String,
    pub start_utc: String,
    pub end_utc: String,
    pub tz_offset_minutes: i32,
    pub day: String,
}

/// Local calendar day a sample buckets to. SleepAnalysis belongs to the
/// WAKE day (end); everything else to the start day — exactly the export
/// parser's rule (apple_health.rs `record_to_sample`), so live and
/// export aggregate the same night into the same episode.
pub fn local_day(start_utc: &str, end_utc: &str, tz_off_min: i32, metric: &str) -> String {
    let anchor = if metric == "SleepAnalysis" { end_utc } else { start_utc };
    let off = FixedOffset::east_opt(tz_off_min * 60)
        .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
    DateTime::parse_from_rfc3339(anchor)
        .map(|dt| dt.with_timezone(&off).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| anchor[..anchor.len().min(10)].to_string())
}

/// Duration in hours between two RFC3339 instants (≥ 0).
fn hours_between_rfc3339(start: &str, end: &str) -> Option<f64> {
    let s = DateTime::parse_from_rfc3339(start).ok()?;
    let e = DateTime::parse_from_rfc3339(end).ok()?;
    Some((e - s).num_seconds().max(0) as f64 / 3600.0)
}

impl Store {
    /// Upsert live HealthKit samples. Returns (accepted, duplicates) —
    /// idempotent by HK uuid (re-streamed batches are pure no-ops).
    ///
    /// Duration-valued category metrics (SleepAnalysis stages, mindful
    /// sessions) arrive from the device with no numeric value; the
    /// export parser values them as duration-in-hours, and DayTable::add
    /// SKIPS valueless samples — so the same normalization happens here,
    /// at ingest, and the stored row aggregates identically to export.
    ///
    /// Batches take the store lock in ≤500-row transactions with a gap
    /// between them (the refresh_local_sources pattern) so HTTP handlers
    /// parked on the mutex get through mid-batch.
    pub fn health_add_samples(&self, samples: &[HealthSampleRow]) -> Result<(usize, usize)> {
        let (mut accepted, mut duplicates) = (0usize, 0usize);
        for chunk in samples.chunks(500) {
            {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                for s in chunk {
                    let day = local_day(&s.start, &s.end, s.tz_offset_minutes, &s.metric);
                    let value = match (s.value, s.metric.as_str()) {
                        (None, "SleepAnalysis") | (None, "MindfulSession") => {
                            hours_between_rfc3339(&s.start, &s.end)
                        }
                        (v, _) => v,
                    };
                    let changed = tx.execute(
                        "INSERT OR IGNORE INTO health_sample
                             (uuid, metric, value, unit, category, source,
                              start_utc, end_utc, tz_offset_minutes, day, ingest_time)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        rusqlite::params![
                            s.uuid, s.metric, value, s.unit, s.category, s.source,
                            s.start, s.end, s.tz_offset_minutes, day, now_utc()
                        ],
                    )?;
                    if changed == 1 { accepted += 1 } else { duplicates += 1 }
                }
                tx.commit()?;
            }
            if samples.len() > 500 {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
        Ok((accepted, duplicates))
    }

    /// Distinct closed days (day < today_local) that have live samples
    /// but no `health_day` episode yet, and whose NEWEST sample arrived
    /// at least `grace` ago (late background-delivery batches land
    /// before the day is sealed; INSERT OR IGNORE makes a straggler
    /// merely incomplete, never a duplicate).
    pub fn health_unfinalized_days(
        &self,
        today_local: &str,
        grace_cutoff_utc: &str,
    ) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT hs.day FROM health_sample hs
             WHERE hs.day < ?1
             GROUP BY hs.day
             HAVING MAX(hs.ingest_time) <= ?2
                AND NOT EXISTS ( /* all-episodes: a retired day still counts as written */
                    SELECT 1 FROM episodes e
                    WHERE e.source = 'apple-health'
                      AND e.source_ref = 'day:' || hs.day)
             ORDER BY hs.day",
        )?;
        let rows = stmt.query_map([today_local, grace_cutoff_utc], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The tz offset of the most recently ingested sample — the best
    /// available guess at the DEVICE's current timezone, which is the
    /// clock that decides when a day has closed. 0 (UTC) before any
    /// sample exists.
    pub fn health_latest_tz_offset(&self) -> Result<i32> {
        let conn = self.lock();
        Ok(conn
            .query_row(
                "SELECT tz_offset_minutes FROM health_sample
                 ORDER BY ingest_time DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// All stored samples for one local day, for the finalize pass.
    pub fn health_samples_for_day(&self, day: &str) -> Result<Vec<StoredHealthSample>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT metric, value, unit, category, source,
                    start_utc, end_utc, tz_offset_minutes, day
             FROM health_sample WHERE day = ?1 ORDER BY start_utc",
        )?;
        let rows = stmt.query_map([day], |row| {
            Ok(StoredHealthSample {
                metric: row.get(0)?,
                value: row.get(1)?,
                unit: row.get(2)?,
                category: row.get(3)?,
                source: row.get(4)?,
                start_utc: row.get(5)?,
                end_utc: row.get(6)?,
                tz_offset_minutes: row.get(7)?,
                day: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Per-(metric, source) rollup of one day's LIVE samples — the
    /// "today so far" feed for Pulse. Shapes rows like the export
    /// sidecar's `daily` table (Sleep stages become `Sleep/<stage>`
    /// valued in HOURS, matching apple_health::DayTable) so the client
    /// can apply the same per-metric aggregate it uses on history.
    pub fn health_day_rollup(&self, day: &str) -> Result<Vec<serde_json::Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT CASE WHEN metric='SleepAnalysis' AND category IS NOT NULL
                         THEN 'Sleep/'||category ELSE metric END AS m,
                    source,
                    COUNT(*) AS n,
                    SUM(CASE WHEN metric='SleepAnalysis'
                             THEN (julianday(end_utc)-julianday(start_utc))*24.0
                             ELSE value END) AS sum,
                    MIN(value) AS min, MAX(value) AS max, AVG(value) AS avg
             FROM health_sample WHERE day = ?1 GROUP BY m, source",
        )?;
        let rows = stmt.query_map([day], |row| {
            Ok(serde_json::json!({
                "metric": row.get::<_, String>(0)?,
                "source": row.get::<_, String>(1)?,
                "n": row.get::<_, i64>(2)?,
                "sum": row.get::<_, Option<f64>>(3)?,
                "min": row.get::<_, Option<f64>>(4)?,
                "max": row.get::<_, Option<f64>>(5)?,
                "avg": row.get::<_, Option<f64>>(6)?,
            }))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(uuid: &str, metric: &str, value: Option<f64>, start: &str, end: &str) -> HealthSampleRow {
        HealthSampleRow {
            uuid: uuid.into(),
            metric: metric.into(),
            value,
            unit: Some("count".into()),
            category: None,
            source: "Apple Watch".into(),
            start: start.into(),
            end: end.into(),
            tz_offset_minutes: -240,
        }
    }

    #[test]
    fn test_local_day_buckets_sleep_to_wake_day() {
        // A night starting 23:30 local July 25 (03:30Z Jul 26 at -0400),
        // ending 07:00 local July 26 — sleep buckets to the WAKE day.
        let d = local_day("2026-07-26T03:30:00Z", "2026-07-26T11:00:00Z", -240, "SleepAnalysis");
        assert_eq!(d, "2026-07-26");
        // A step sample at 23:30 local July 25 stays on July 25 even
        // though its UTC instant is already July 26.
        let d = local_day("2026-07-26T03:30:00Z", "2026-07-26T03:31:00Z", -240, "StepCount");
        assert_eq!(d, "2026-07-25");
    }

    #[test]
    fn test_health_add_samples_idempotent_and_sleep_valued() {
        let store = Store::open_in_memory().unwrap();
        let batch = vec![
            sample("u1", "StepCount", Some(120.0), "2026-07-24T12:00:00Z", "2026-07-24T12:05:00Z"),
            HealthSampleRow {
                category: Some("AsleepDeep".into()),
                unit: None,
                ..sample("u2", "SleepAnalysis", None, "2026-07-24T04:00:00Z", "2026-07-24T05:30:00Z")
            },
        ];
        assert_eq!(store.health_add_samples(&batch).unwrap(), (2, 0));
        // re-POST of the same batch: all duplicates, no new rows
        assert_eq!(store.health_add_samples(&batch).unwrap(), (0, 2));
        let (n, sleep_v): (i64, f64) = {
            let conn = store.lock();
            conn.query_row(
                "SELECT COUNT(*),
                        (SELECT value FROM health_sample WHERE uuid='u2')
                 FROM health_sample",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(n, 2);
        // valueless sleep stage normalized to duration hours (1.5h),
        // exactly what the export parser produces for the same segment
        assert!((sleep_v - 1.5).abs() < 1e-9, "got {sleep_v}");
    }

    #[test]
    fn test_health_unfinalized_days_excludes_today_and_minted() {
        let store = Store::open_in_memory().unwrap();
        let batch = vec![
            sample("a", "StepCount", Some(10.0), "2026-07-24T12:00:00Z", "2026-07-24T12:01:00Z"),
            sample("b", "StepCount", Some(10.0), "2026-07-25T12:00:00Z", "2026-07-25T12:01:00Z"),
            sample("c", "StepCount", Some(10.0), "2026-07-26T12:00:00Z", "2026-07-26T12:01:00Z"),
        ];
        store.health_add_samples(&batch).unwrap();
        // today = the 26th local → only the 24th + 25th are closed
        let far_future = "2999-01-01T00:00:00Z";
        let days = store.health_unfinalized_days("2026-07-26", far_future).unwrap();
        assert_eq!(days, vec!["2026-07-24", "2026-07-25"]);
        // a grace cutoff EARLIER than the rows' ingest_time seals nothing
        let days = store.health_unfinalized_days("2026-07-26", "2000-01-01T00:00:00Z").unwrap();
        assert!(days.is_empty(), "grace must hold back fresh days");
        // minting the 24th's episode removes it from the backlog
        {
            let conn = store.lock();
            conn.execute(
                "INSERT INTO episodes (id, source, source_ref, kind, title, actor,
                     time_uncertainty, trust, scope, ingest_time, content_hash)
                 VALUES ('ep1','apple-health','day:2026-07-24','health_day','t','agent:t',
                     'day','imported','health',?1,'h')",
                [now_utc()],
            )
            .unwrap();
        }
        let days = store.health_unfinalized_days("2026-07-26", far_future).unwrap();
        assert_eq!(days, vec!["2026-07-25"]);
    }
}
