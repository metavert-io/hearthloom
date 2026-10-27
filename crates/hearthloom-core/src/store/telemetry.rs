//! Observability (v35): node-tagged system logs + health/metric samples.
//! Best-practice shape: structured events (level/event/message/meta),
//! append-only, pruned on write, and replicated between a user's own
//! peers so either dashboard shows both nodes. These tables are SYSTEM
//! introspection — Ask references them only when the user explicitly
//! asks about Hearthloom itself (rlm.rs gates this in the prompt).

use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};

use super::Store;
use crate::time::now_utc;

/// Keep a week; the UI shows 24h.
const RETAIN_DAYS: i64 = 7;

#[derive(Debug, Clone, Serialize)]
pub struct LogRow {
    pub id: String,
    pub ts: String,
    pub node: String,
    pub level: String,
    pub event: String,
    pub message: String,
    pub meta: Option<Value>,
}

/// One 30-minute timeline bucket: ok-ratio for a check.
#[derive(Debug, Clone, Serialize)]
pub struct HealthBucket {
    pub bucket: String,
    pub ok: i64,
    pub total: i64,
}

impl Store {
    /// Structured system event. Never fails the caller's path (errors
    /// are swallowed at call sites) — logging must not break the app.
    pub fn log_event(
        &self,
        node: &str,
        level: &str,
        event: &str,
        message: &str,
        meta: Option<Value>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO syslog (id, ts, node, level, event, message, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                format!("lg_{}", uuid::Uuid::new_v4().simple()),
                now_utc(),
                node,
                level,
                event,
                message,
                meta.map(|m| m.to_string()),
            ],
        )?;
        Ok(())
    }

    /// A health tick (ok=pass/fail) or numeric sample (value+unit).
    pub fn record_health(
        &self,
        node: &str,
        check: &str,
        ok: Option<bool>,
        value: Option<f64>,
        unit: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR IGNORE INTO health_points (ts, node, check_name, ok, value, unit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![now_utc(), node, check, ok.map(|b| b as i64), value, unit],
        )?;
        Ok(())
    }

    /// Prune telemetry beyond the retention window (called by the
    /// collector, not per write).
    pub fn prune_telemetry(&self) -> Result<()> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(RETAIN_DAYS))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let conn = self.lock();
        conn.execute("DELETE FROM syslog WHERE ts < ?1", [&cutoff])?;
        conn.execute("DELETE FROM health_points WHERE ts < ?1", [&cutoff])?;
        Ok(())
    }

    /// Paged, sortable log view. sort: ts|level|event, order asc|desc.
    pub fn logs(
        &self,
        node: Option<&str>,
        level: Option<&str>,
        sort: &str,
        desc: bool,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<LogRow>, i64)> {
        let sort_col = match sort {
            "level" => "level",
            "event" => "event",
            _ => "ts",
        };
        let dir = if desc { "DESC" } else { "ASC" };
        let conn = self.lock();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM syslog
             WHERE (?1 IS NULL OR node = ?1) AND (?2 IS NULL OR level = ?2)",
            rusqlite::params![node, level],
            |r| r.get(0),
        )?;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, ts, node, level, event, message, meta FROM syslog
             WHERE (?1 IS NULL OR node = ?1) AND (?2 IS NULL OR level = ?2)
             ORDER BY {sort_col} {dir}, ts DESC LIMIT ?3 OFFSET ?4"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![node, level, limit as i64, offset as i64],
            |row| {
                let meta: Option<String> = row.get(6)?;
                Ok(LogRow {
                    id: row.get(0)?,
                    ts: row.get(1)?,
                    node: row.get(2)?,
                    level: row.get(3)?,
                    event: row.get(4)?,
                    message: row.get(5)?,
                    meta: meta.and_then(|m| serde_json::from_str(&m).ok()),
                })
            },
        )?;
        Ok((rows.collect::<std::result::Result<Vec<_>, _>>()?, total))
    }

    /// 24h timeline: 30-min ok-ratio buckets per check for one node.
    pub fn health_timeline(&self, node: &str, hours: i64) -> Result<Value> {
        let since = (chrono::Utc::now() - chrono::Duration::hours(hours))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT check_name,
                    substr(ts, 1, 15) || CASE WHEN substr(ts,15,1) < '3' THEN '0' ELSE '3' END,
                    SUM(COALESCE(ok, 1)), COUNT(*)
             FROM health_points
             WHERE node = ?1 AND ts > ?2 AND ok IS NOT NULL
             GROUP BY 1, 2 ORDER BY 2",
        )?;
        let rows: Vec<(String, String, i64, i64)> = stmt
            .query_map(rusqlite::params![node, since], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut out: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
        for (check, bucket, ok, total) in rows {
            out.entry(check).or_default().push(json!({
                "bucket": bucket, "ok": ok, "total": total
            }));
        }
        Ok(json!(out))
    }

    /// Numeric series (cpu/mem/disk…) for charting, hourly averages.
    pub fn telemetry_series(&self, node: &str, check: &str, hours: i64) -> Result<Vec<Value>> {
        let since = (chrono::Utc::now() - chrono::Duration::hours(hours))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT substr(ts, 1, 13), ROUND(AVG(value), 1)
             FROM health_points
             WHERE node = ?1 AND check_name = ?2 AND ts > ?3 AND value IS NOT NULL
             GROUP BY 1 ORDER BY 1",
        )?;
        let rows = stmt.query_map(rusqlite::params![node, check, since], |row| {
            Ok(json!({"hour": row.get::<_, String>(0)?, "value": row.get::<_, f64>(1)?}))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Append-only telemetry exchange between own peers: rows after ts.
    pub fn telemetry_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, node, level, event, message, meta FROM syslog
             WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2",
        )?;
        let logs: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?, "ts": row.get::<_, String>(1)?,
                    "node": row.get::<_, String>(2)?, "level": row.get::<_, String>(3)?,
                    "event": row.get::<_, String>(4)?, "message": row.get::<_, String>(5)?,
                    "meta": row.get::<_, Option<String>>(6)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = conn.prepare(
            "SELECT ts, node, check_name, ok, value, unit FROM health_points
             WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2",
        )?;
        let points: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(json!({
                    "ts": row.get::<_, String>(0)?, "node": row.get::<_, String>(1)?,
                    "check": row.get::<_, String>(2)?, "ok": row.get::<_, Option<i64>>(3)?,
                    "value": row.get::<_, Option<f64>>(4)?,
                    "unit": row.get::<_, Option<String>>(5)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        // Both pages advance together: the cursor is the SMALLER of the two
        // page ends when a page came back full (the rest of that table is
        // still to come), else the larger.
        let logs_end = super::page_cursor(
            &conn,
            "synced_at",
            "FROM syslog WHERE synced_at > ?1",
            since,
            limit,
        )?;
        let points_end = super::page_cursor(
            &conn,
            "synced_at",
            "FROM health_points WHERE synced_at > ?1",
            since,
            limit,
        )?;
        let max_ts = match (logs.len() >= limit, points.len() >= limit) {
            (true, false) => logs_end,
            (false, true) => points_end,
            (true, true) => logs_end.min(points_end),
            (false, false) => logs_end.max(points_end),
        };
        Ok(json!({"logs": logs, "points": points, "next": max_ts}))
    }

    /// Apply a peer's telemetry — INSERT OR IGNORE everywhere (ids and
    /// natural keys dedupe), so replay is free.
    pub fn apply_telemetry(&self, batch: &Value) -> Result<usize> {
        let conn = self.lock();
        let mut applied = 0usize;
        for l in batch
            .get("logs")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            applied += conn.execute(
                "INSERT OR IGNORE INTO syslog (id, ts, node, level, event, message, meta)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    l.get("id").and_then(|v| v.as_str()),
                    l.get("ts").and_then(|v| v.as_str()),
                    l.get("node").and_then(|v| v.as_str()),
                    l.get("level").and_then(|v| v.as_str()),
                    l.get("event").and_then(|v| v.as_str()),
                    l.get("message").and_then(|v| v.as_str()),
                    l.get("meta").and_then(|v| v.as_str()),
                ],
            )?;
        }
        for p in batch
            .get("points")
            .and_then(|v| v.as_array())
            .unwrap_or(&vec![])
        {
            applied += conn.execute(
                "INSERT OR IGNORE INTO health_points (ts, node, check_name, ok, value, unit)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    p.get("ts").and_then(|v| v.as_str()),
                    p.get("node").and_then(|v| v.as_str()),
                    p.get("check").and_then(|v| v.as_str()),
                    p.get("ok").and_then(|v| v.as_i64()),
                    p.get("value").and_then(|v| v.as_f64()),
                    p.get("unit").and_then(|v| v.as_str()),
                ],
            )?;
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_health_roundtrip_and_exchange() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.db")).unwrap();
        store
            .log_event(
                "local",
                "info",
                "startup",
                "daemon started",
                Some(json!({"v": "0.1.0"})),
            )
            .unwrap();
        store
            .record_health("local", "daemon", Some(true), None, None)
            .unwrap();
        store
            .record_health("local", "mem", None, Some(512.0), Some("mb"))
            .unwrap();
        let (rows, total) = store.logs(Some("local"), None, "ts", true, 0, 10).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].event, "startup");
        // exchange: replay-safe
        let batch = store.telemetry_since("", 1000).unwrap();
        let peer = Store::open(&tmp.path().join("p.db")).unwrap();
        assert!(peer.apply_telemetry(&batch).unwrap() >= 2);
        assert_eq!(peer.apply_telemetry(&batch).unwrap(), 0);
        let (peer_rows, _) = peer.logs(Some("local"), None, "ts", true, 0, 10).unwrap();
        assert_eq!(peer_rows.len(), 1); // node tag survives the hop
    }
}
