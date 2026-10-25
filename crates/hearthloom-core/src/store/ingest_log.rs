//! The raw-ingest audit trail (v48) — every accepted device batch on
//! /v1/ingest/health and /v1/ingest/photos appends one row here. This is
//! the iOS companion's top-to-bottom verification surface: raw samples
//! land in health_sample / photo_point but don't mint episodes until the
//! daily finalize passes, so without this log there is no way to see
//! whether the phone's stream is actually landing. Append-only, never
//! pruned; `GET /v1/ingest/recent` pages it seq-DESC (stable — rows are
//! never deleted), `GET /v1/ingest/status` aggregates the sample stores
//! themselves and takes the latest actor per source from here.

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use super::Store;
use crate::time::now_utc;

impl Store {
    /// Append one audit row for an accepted raw batch. Zero-accepted
    /// batches log too — an all-duplicates POST is still evidence the
    /// device is streaming.
    pub fn ingest_log_append(
        &self,
        actor: &str,
        source: &str,
        kind: &str,
        accepted_count: usize,
        r#ref: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO ingest_log (ts, actor, source, kind, accepted_count, ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![now_utc(), actor, source, kind, accepted_count as i64, r#ref],
        )?;
        Ok(())
    }

    /// Per-source raw-ingestion summary — counts over the durable sample
    /// stores (NOT the log: the stores are the truth about what landed),
    /// latest actor/ingest-time from wherever it is freshest.
    pub fn ingest_status(&self) -> Result<Value> {
        let conn = self.conn.lock();
        let mut sources = Vec::new();
        // (source label, sample-store aggregate query, default actor).
        // Every device feed gets a row (the iOS Sync-status screen
        // renders whatever sources[] it is given); episode-backed feeds
        // count via idx_episodes_source, never a table scan.
        let legs: [(&str, &str, &str); 10] = [
            (
                "apple-health",
                "SELECT COUNT(*), MAX(ingest_time), COUNT(DISTINCT day) FROM health_sample",
                "adapter:ios-healthkit",
            ),
            (
                "photos",
                "SELECT COUNT(*), MAX(ingest_time), COUNT(DISTINCT day) FROM photo_point",
                "adapter:ios-photos",
            ),
            (
                "contacts",
                "SELECT COUNT(*), MAX(ingest_time),
                        COUNT(DISTINCT substr(updated_at, 1, 10)) FROM contact_card",
                "adapter:ios-contacts",
            ),
            (
                "calendar",
                "SELECT COUNT(*), MAX(ingest_time),
                        COUNT(DISTINCT substr(COALESCE(event_time, ingest_time), 1, 10))
                 FROM episodes WHERE source = 'ics' /* all-episodes: ingestion health counts what arrived */",
                "adapter:ios-calendar",
            ),
            (
                "reminders",
                "SELECT COUNT(*), MAX(updated_at),
                        COUNT(DISTINCT substr(created_at, 1, 10))
                 FROM tasks WHERE id LIKE 'tk_rem_%'",
                "adapter:ios-reminders",
            ),
            (
                "ios-visits",
                "SELECT COUNT(*), MAX(ingest_time), COUNT(DISTINCT day)
                 FROM location_observation WHERE source = 'ios-visits'",
                "adapter:ios-location",
            ),
            (
                "ios-motion",
                "SELECT COUNT(*), MAX(ingest_time),
                        COUNT(DISTINCT substr(COALESCE(event_time, ingest_time), 1, 10))
                 FROM episodes WHERE source = 'ios-motion' /* all-episodes: ingestion health counts what arrived */",
                "adapter:ios-motion",
            ),
            (
                "ios-media",
                "SELECT COUNT(*), MAX(ingest_time),
                        COUNT(DISTINCT substr(COALESCE(event_time, ingest_time), 1, 10))
                 FROM episodes WHERE source = 'ios-media' /* all-episodes: ingestion health counts what arrived */",
                "adapter:ios-media",
            ),
            (
                "ios-scan",
                "SELECT COUNT(*), MAX(ingest_time),
                        COUNT(DISTINCT substr(COALESCE(event_time, ingest_time), 1, 10))
                 FROM episodes WHERE source = 'ios-scan' /* all-episodes: ingestion health counts what arrived */",
                "adapter:ios-scan",
            ),
            (
                "ios-focus",
                "SELECT COUNT(*), MAX(ingest_time), COUNT(DISTINCT substr(t_start, 1, 10))
                 FROM device_state WHERE source = 'ios-focus'",
                "adapter:ios-focus",
            ),
        ];
        for (source, sql, default_actor) in legs {
            let (count, last_ingest_at, days): (i64, Option<String>, i64) =
                conn.query_row(sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let actor: Option<String> = conn
                .query_row(
                    "SELECT actor FROM ingest_log WHERE source = ?1
                     ORDER BY seq DESC LIMIT 1",
                    [source],
                    |r| r.get(0),
                )
                .optional()?;
            sources.push(json!({
                "source": source,
                "actor": actor.unwrap_or_else(|| default_actor.to_string()),
                "count": count,
                "last_ingest_at": last_ingest_at,
                "days_covered": days,
            }));
        }
        Ok(json!({ "sources": sources }))
    }

    /// Paged audit trail of accepted batches, newest first.
    pub fn ingest_recent(&self, offset: usize, limit: usize) -> Result<Value> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT ts, source, kind, accepted_count, ref FROM ingest_log
             ORDER BY seq DESC LIMIT ?1 OFFSET ?2",
        )?;
        let items: Vec<Value> = stmt
            .query_map(
                rusqlite::params![limit.min(500) as i64, offset as i64],
                |r| {
                    Ok(json!({
                        "ts": r.get::<_, String>(0)?,
                        "source": r.get::<_, String>(1)?,
                        "kind": r.get::<_, String>(2)?,
                        "count": r.get::<_, i64>(3)?,
                        "ref": r.get::<_, Option<String>>(4)?,
                    }))
                },
            )?
            .collect::<std::result::Result<_, _>>()?;
        Ok(json!({ "items": items }))
    }
}
