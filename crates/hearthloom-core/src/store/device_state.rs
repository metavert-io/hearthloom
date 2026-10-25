//! The generic device attention/presence interval stream (v51). One row
//! per state span, idempotent by (source, key) — `ios-focus` (Focus
//! modes: Work / Personal / Do Not Disturb / Sleep) feeds it first; the
//! model is deliberately source-generic (Jon 2026-08-16) so the
//! hpresence ember's app/idle spans can later land in the SAME stream
//! instead of growing a parallel table. A thin context signal — never
//! episodes; Day/Pulse can annotate against it at read time.
//!
//! Peer-synced via /v1/sync/device_state on the telemetry-leg pattern.

use anyhow::Result;
use serde_json::{json, Value};

use super::Store;
use crate::time::now_utc;

/// One incoming state span.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct DeviceStateIn {
    /// Per-source stable id (e.g. `<since>|<state>` on the client).
    pub key: String,
    pub state: String,
    /// RFC3339 UTC span start (the client's `since`).
    pub since: String,
    /// RFC3339 UTC span end — None while the state is current.
    #[serde(default)]
    pub until: Option<String>,
}

impl Store {
    /// Append state spans for one source — idempotent by (source, key).
    /// A replayed key with a t_end fills in a previously open span
    /// (the "state ended" follow-up post); anything else is a no-op.
    pub fn device_states_add(
        &self,
        source: &str,
        device: &str,
        rows: &[DeviceStateIn],
    ) -> Result<(usize, usize)> {
        let (mut accepted, mut duplicates) = (0usize, 0usize);
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        for s in rows {
            let changed = tx.execute(
                "INSERT INTO device_state (source, key, state, t_start, t_end, device, ingest_time)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(source, key) DO UPDATE SET t_end = excluded.t_end
                     WHERE device_state.t_end IS NULL AND excluded.t_end IS NOT NULL",
                rusqlite::params![source, s.key, s.state, s.since, s.until, device, now_utc()],
            )?;
            if changed == 1 { accepted += 1 } else { duplicates += 1 }
        }
        tx.commit()?;
        Ok((accepted, duplicates))
    }

    /// (count, last_ingest_at, days_covered) for one source — the
    /// ingest-status surface.
    pub fn device_state_stats(&self, source: &str) -> Result<(i64, Option<String>, i64)> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COUNT(*), MAX(ingest_time), COUNT(DISTINCT substr(t_start, 1, 10))
             FROM device_state WHERE source = ?1",
            [source],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(Into::into)
    }

    /// Peer-sync export past a ts watermark (telemetry pattern).
    pub fn device_states_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT source, key, state, t_start, t_end, device, ingest_time
             FROM device_state WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2",
        )?;
        let items: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(json!({
                    "source": row.get::<_, String>(0)?, "key": row.get::<_, String>(1)?,
                    "state": row.get::<_, String>(2)?, "t_start": row.get::<_, String>(3)?,
                    "t_end": row.get::<_, Option<String>>(4)?,
                    "device": row.get::<_, String>(5)?,
                    "ingest_time": row.get::<_, String>(6)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(&conn, "synced_at", "FROM device_state WHERE synced_at > ?1", since, limit)?;
        Ok(json!({ "items": items, "next": next }))
    }

    /// Apply a peer's page — same upsert as ingest (idempotent; an
    /// arriving t_end closes a locally-open span).
    pub fn device_states_apply(&self, batch: &Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for r in batch.get("items").and_then(|v| v.as_array()).into_iter().flatten() {
            let (Some(source), Some(key), Some(state), Some(t_start)) = (
                r.get("source").and_then(|v| v.as_str()),
                r.get("key").and_then(|v| v.as_str()),
                r.get("state").and_then(|v| v.as_str()),
                r.get("t_start").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            applied += tx.execute(
                "INSERT INTO device_state (source, key, state, t_start, t_end, device, ingest_time)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)
                 ON CONFLICT(source, key) DO UPDATE SET t_end = excluded.t_end
                     WHERE device_state.t_end IS NULL AND excluded.t_end IS NOT NULL",
                rusqlite::params![
                    source, key, state, t_start,
                    r.get("t_end").and_then(|v| v.as_str()),
                    r.get("device").and_then(|v| v.as_str()).unwrap_or("peer"),
                    now_utc()
                ],
            )?;
        }
        tx.commit()?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_states_idempotent_and_span_close() {
        let store = Store::open_in_memory().unwrap();
        let open = DeviceStateIn {
            key: "2026-08-15T09:00:00Z|Work".into(),
            state: "Work".into(),
            since: "2026-08-15T09:00:00Z".into(),
            until: None,
        };
        assert_eq!(store.device_states_add("ios-focus", "iPhone", std::slice::from_ref(&open)).unwrap(), (1, 0));
        // replaying the open span is a no-op
        assert_eq!(store.device_states_add("ios-focus", "iPhone", std::slice::from_ref(&open)).unwrap(), (0, 1));
        // the closing follow-up fills t_end in place
        let closed = DeviceStateIn { until: Some("2026-08-15T11:30:00Z".into()), ..open };
        assert_eq!(store.device_states_add("ios-focus", "iPhone", std::slice::from_ref(&closed)).unwrap(), (1, 0));
        // and replaying the closed span is again a no-op
        assert_eq!(store.device_states_add("ios-focus", "iPhone", &[closed]).unwrap(), (0, 1));
        let (n, _, days) = store.device_state_stats("ios-focus").unwrap();
        assert_eq!((n, days), (1, 1));
    }

    #[test]
    fn test_peer_roundtrip() {
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        a.device_states_add(
            "ios-focus",
            "iPhone",
            &[DeviceStateIn {
                key: "k1".into(),
                state: "Sleep".into(),
                since: "2026-08-15T03:00:00Z".into(),
                until: Some("2026-08-15T11:00:00Z".into()),
            }],
        )
        .unwrap();
        let page = a.device_states_since("", 100).unwrap();
        assert_eq!(b.device_states_apply(&page).unwrap(), 1);
        assert_eq!(b.device_states_apply(&page).unwrap(), 0);
    }
}
