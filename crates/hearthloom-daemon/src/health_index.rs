//! Numeric sidecar for the Apple Health archive: full-resolution samples
//! plus per-day rollups in a separate SQLite file next to the export —
//! the analytical layer of the health integration. Trends and
//! comparisons are read-only SQL over these tables (the RLM's `vquery`
//! handle), never LLM work and never prose-scraping over episodes.
//!
//! Like the mbox index, this file is DERIVED: not the substrate, not
//! opened by anything but the daemon, rebuildable from the archive at
//! any time (law #4 untouched).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde_json::Value;

const BATCH: usize = 5000;

/// `<export dir>/health.vidx.sqlite`, next to export.xml.
pub fn sidecar_path(export_xml: &Path) -> PathBuf {
    export_xml.with_file_name("health.vidx.sqlite")
}

pub struct SidecarWriter {
    conn: Connection,
    tmp: PathBuf,
    dest: PathBuf,
    pending: usize,
    pub samples: usize,
}

impl SidecarWriter {
    pub fn create(export_xml: &Path) -> Result<Self> {
        let dest = sidecar_path(export_xml);
        let tmp = dest.with_extension("sqlite.building");
        let _ = std::fs::remove_file(&tmp);
        let conn = Connection::open(&tmp)?;
        conn.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
             CREATE TABLE samples(
                 day TEXT NOT NULL, metric TEXT NOT NULL, source TEXT NOT NULL,
                 value REAL, unit TEXT, start TEXT
             );
             CREATE TABLE daily(
                 day TEXT NOT NULL, metric TEXT NOT NULL, source TEXT NOT NULL,
                 n INTEGER, sum REAL, min REAL, max REAL, avg REAL
             );
             BEGIN;",
        )?;
        Ok(SidecarWriter {
            conn,
            tmp,
            dest,
            pending: 0,
            samples: 0,
        })
    }

    pub fn add_sample(&mut self, s: &hearthloom_adapters::apple_health::Sample) -> Result<()> {
        let metric = match (&s.metric[..], &s.category) {
            ("SleepAnalysis", Some(stage)) => format!("Sleep/{stage}"),
            _ => s.metric.clone(),
        };
        self.conn.execute(
            "INSERT INTO samples(day, metric, source, value, unit, start)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![s.day, metric, s.source, s.value, s.unit, s.start],
        )?;
        self.samples += 1;
        self.pending += 1;
        if self.pending >= BATCH {
            self.conn.execute_batch("COMMIT; BEGIN;")?;
            self.pending = 0;
        }
        Ok(())
    }

    /// Write the day rollups (from the adapter's DayTable) and install
    /// the index atomically over any previous one.
    pub fn finish(
        mut self,
        daily: &std::collections::BTreeMap<
            (String, String, String),
            hearthloom_adapters::apple_health::Agg,
        >,
    ) -> Result<PathBuf> {
        for ((day, metric, source), agg) in daily {
            self.conn.execute(
                "INSERT INTO daily(day, metric, source, n, sum, min, max, avg)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    day,
                    metric,
                    source,
                    agg.n as i64,
                    agg.sum,
                    agg.min,
                    agg.max,
                    agg.avg()
                ],
            )?;
            self.pending += 1;
            if self.pending >= BATCH {
                self.conn.execute_batch("COMMIT; BEGIN;")?;
                self.pending = 0;
            }
        }
        self.conn.execute_batch(
            "COMMIT;
             CREATE INDEX idx_samples_metric_day ON samples(metric, day);
             CREATE INDEX idx_daily_metric_day ON daily(metric, day);",
        )?;
        drop(self.conn);
        std::fs::rename(&self.tmp, &self.dest)
            .with_context(|| format!("installing sidecar at {}", self.dest.display()))?;
        Ok(self.dest)
    }
}

/// Read-only SQL over the sidecar: single SELECT/WITH statement, row cap.
/// The caller (virtual_source::query) is responsible for the vault gate.
pub fn query(sidecar: &Path, sql: &str, max_rows: usize) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let head = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if head != "select" && head != "with" {
        return Err(anyhow!(
            "vquery accepts a single read-only SELECT/WITH statement"
        ));
    }
    if trimmed.contains(';') {
        return Err(anyhow!("vquery accepts exactly one statement"));
    }
    if !sidecar.is_file() {
        return Err(anyhow!(
            "no sidecar at {} — run the apple-health ingest first",
            sidecar.display()
        ));
    }
    let conn = Connection::open_with_flags(sidecar, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(trimmed)?;
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let n = columns.len();
    let mut rows_out = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if rows_out.len() >= max_rows {
            break;
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let v = row.get_ref(i)?;
            out.push(match v {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(i) => Value::from(i),
                rusqlite::types::ValueRef::Real(f) => Value::from((f * 1000.0).round() / 1000.0),
                rusqlite::types::ValueRef::Text(t) => {
                    Value::from(String::from_utf8_lossy(t).into_owned())
                }
                rusqlite::types::ValueRef::Blob(_) => Value::from("<blob>"),
            });
        }
        rows_out.push(out);
    }
    Ok((columns, rows_out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_adapters::apple_health::{DayTable, Sample};

    fn sample(day: &str, metric: &str, source: &str, value: f64) -> Sample {
        Sample {
            metric: metric.into(),
            value: Some(value),
            category: None,
            unit: Some("count".into()),
            source: source.into(),
            start: format!("{day} 08:00:00 -0400"),
            day: day.into(),
        }
    }

    #[test]
    fn test_sidecar_roundtrip_and_query_guardrails() {
        let tmp = tempfile::tempdir().unwrap();
        let xml = tmp.path().join("export.xml");
        std::fs::write(&xml, "<HealthData/>").unwrap();

        let mut table = DayTable::default();
        let mut writer = SidecarWriter::create(&xml).unwrap();
        for (d, v) in [("2026-04-01", 8000.0), ("2026-04-02", 12000.0)] {
            let s = sample(d, "StepCount", "Apple Watch", v);
            writer.add_sample(&s).unwrap();
            table.add(&s);
        }
        let sidecar = writer.finish(&table.map).unwrap();

        let (cols, rows) = query(
            &sidecar,
            "SELECT day, sum FROM daily WHERE metric='StepCount' ORDER BY day",
            100,
        )
        .unwrap();
        assert_eq!(cols, vec!["day", "sum"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], serde_json::json!(12000.0));

        // Guardrails: write statements and multi-statements refused.
        assert!(query(&sidecar, "DELETE FROM daily", 10).is_err());
        assert!(query(&sidecar, "SELECT 1; SELECT 2", 10).is_err());
        // Row cap honored.
        let (_, capped) = query(&sidecar, "SELECT * FROM samples", 1).unwrap();
        assert_eq!(capped.len(), 1);
    }
}
