//! Numeric sidecar for the Apple Health archive: full-resolution samples
//! plus per-day rollups in a separate SQLite file next to the export —
//! the analytical layer of the health integration. Trends and
//! comparisons are read-only SQL over these tables (the RLM's `vquery`
//! handle), never LLM work and never prose-scraping over episodes.
//!
//! Like the mbox index, this file is DERIVED: not the substrate, not
//! opened by anything but the daemon, rebuildable from the archive at
//! any time (law #4 untouched).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use hearthloom_adapters::apple_health::{Agg, DayTable, Sample};
use rusqlite::{Connection, TransactionBehavior};
use serde_json::Value;

const BATCH: usize = 5000;

/// The sidecar layout this build writes and reads (`PRAGMA user_version`).
/// 2: every metric's day rollups are in one unit, named in `daily.unit`,
/// and a sleep stage's rollups are kept per device (`daily.source` names
/// the device for those: two devices' records of one night stay apart).
/// A sidecar an earlier build wrote (0) has no `unit` column and may have
/// averaged values written in different units; it is brought up to this
/// layout from its own samples before it is read again (`query`).
const LAYOUT: i64 = 2;

const INSERT_DAILY: &str = "INSERT INTO daily(day, metric, source, n, sum, min, max, avg, unit)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)";

fn insert_rollup(
    conn: &Connection,
    table: &DayTable,
    (day, metric, source): &(String, String, String),
    agg: &Agg,
) -> Result<()> {
    conn.prepare_cached(INSERT_DAILY)?
        .execute(rusqlite::params![
            day,
            metric,
            source,
            agg.n as i64,
            agg.sum,
            agg.min,
            agg.max,
            agg.avg(),
            table.unit(metric)
        ])?;
    Ok(())
}

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
        conn.pragma_update(None, "user_version", LAYOUT)?;
        conn.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;
             CREATE TABLE samples(
                 day TEXT NOT NULL, metric TEXT NOT NULL, source TEXT NOT NULL,
                 value REAL, unit TEXT, start TEXT
             );
             CREATE TABLE daily(
                 day TEXT NOT NULL, metric TEXT NOT NULL, source TEXT NOT NULL,
                 n INTEGER, sum REAL, min REAL, max REAL, avg REAL, unit TEXT
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

    pub fn add_sample(&mut self, s: &Sample) -> Result<()> {
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

    /// Write the day rollups (from the adapter's DayTable, each metric's
    /// values in the one unit the table aggregated them in, recorded
    /// beside them) and install the index atomically over any previous one.
    pub fn finish(mut self, daily: &DayTable) -> Result<PathBuf> {
        for (key, agg) in &daily.map {
            insert_rollup(&self.conn, daily, key, agg)?;
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

/// Bring a sidecar an earlier build wrote up to `LAYOUT`: its day rollups
/// are derived again from its own samples, read in the order the scan
/// wrote them (so each metric gets the unit a fresh scan would give it),
/// and replaced in one transaction. Source names stay as that build
/// recorded them, and its samples don't name the device, so the sleep of
/// two devices sharing a source name stays together; a new import of the
/// export rebuilds the sidecar whole.
pub fn upgrade(sidecar: &Path) -> Result<()> {
    let mut conn = Connection::open(sidecar)?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let layout: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if layout >= LAYOUT {
        return Ok(());
    }
    let mut table = DayTable::default();
    {
        let mut stmt = tx.prepare(
            "SELECT day, metric, source, value, unit, start FROM samples ORDER BY rowid",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            // The sidecar stores sleep stages as `Sleep/<stage>`; the
            // table folds them from the record's metric and category.
            let stored: String = row.get(1)?;
            let (metric, category) = match stored.strip_prefix("Sleep/") {
                Some(stage) => ("SleepAnalysis".to_string(), Some(stage.to_string())),
                None => (stored, None),
            };
            let source: String = row.get(2)?;
            table.add(&Sample {
                metric,
                value: row.get(3)?,
                category,
                unit: row.get(4)?,
                device: source.clone(),
                source,
                start: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                day: row.get(0)?,
            });
        }
    }
    if tx.prepare("SELECT unit FROM daily LIMIT 0").is_err() {
        tx.execute_batch("ALTER TABLE daily ADD COLUMN unit TEXT")?;
    }
    tx.execute("DELETE FROM daily", [])?;
    for (key, agg) in &table.map {
        insert_rollup(&tx, &table, key, agg)?;
    }
    tx.pragma_update(None, "user_version", LAYOUT)?;
    tx.commit()?;
    Ok(())
}

/// Sidecars being brought up to date, or that couldn't be (why).
enum Upgrade {
    Running,
    Failed(String),
}

static UPGRADES: Mutex<BTreeMap<PathBuf, Upgrade>> = Mutex::new(BTreeMap::new());

/// Start bringing an earlier build's sidecar up to date (once per daemon
/// run) and say why it can't be read yet. Its old rollups aren't served
/// meanwhile: they may average values written in different units.
fn not_yet_current(sidecar: &Path) -> anyhow::Error {
    let mut upgrades = UPGRADES.lock().unwrap_or_else(|e| e.into_inner());
    match upgrades.get(sidecar) {
        Some(Upgrade::Failed(why)) => {
            return anyhow!(
                "the health numbers at {} were indexed by an earlier version and couldn't \
                 be brought up to date ({why}); run the Apple Health import again to rebuild them",
                sidecar.display()
            )
        }
        Some(Upgrade::Running) => {}
        None => {
            let path = sidecar.to_path_buf();
            let spawned = std::thread::Builder::new()
                .name("health-sidecar-upgrade".into())
                .spawn(move || {
                    let started = std::time::Instant::now();
                    let result = upgrade(&path);
                    let mut upgrades = UPGRADES.lock().unwrap_or_else(|e| e.into_inner());
                    match result {
                        Ok(()) => {
                            tracing::info!(
                                "health sidecar {}: day rollups brought up to date in {:.1?}",
                                path.display(),
                                started.elapsed()
                            );
                            upgrades.remove(&path);
                        }
                        Err(err) => {
                            tracing::warn!(
                                "health sidecar {}: couldn't bring it up to date: {err:#}",
                                path.display()
                            );
                            upgrades.insert(path, Upgrade::Failed(format!("{err:#}")));
                        }
                    }
                });
            let state = match spawned {
                Ok(_) => Upgrade::Running,
                Err(err) => Upgrade::Failed(err.to_string()),
            };
            upgrades.insert(sidecar.to_path_buf(), state);
        }
    }
    anyhow!(
        "the health numbers are being brought up to date (they were indexed by an earlier \
         version); try again in a minute"
    )
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
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    let layout: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if layout < LAYOUT {
        return Err(not_yet_current(sidecar));
    }
    let mut stmt = conn.prepare(trimmed)?;
    let columns: Vec<String> = stmt.column_names().map(|c| c.to_string()).collect();
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
            source: hearthloom_adapters::apple_health::normalize_source(source),
            device: source.into(),
            start: format!("{day} 08:00:00 -0400"),
            day: day.into(),
        }
    }

    /// Two devices that share a source name, each recording the same
    /// night: the sidecar keeps their nights apart, so Pulse's sleep
    /// series (each tracker's asleep stages summed, the longest taken)
    /// reads one night of eight hours.
    #[test]
    fn test_sleep_rollups_are_per_device() {
        let tmp = tempfile::tempdir().unwrap();
        let xml = tmp.path().join("export.xml");
        std::fs::write(&xml, "<HealthData/>").unwrap();
        let mut table = DayTable::default();
        let mut writer = SidecarWriter::create(&xml).unwrap();
        for device in ["Ada’s Apple Watch", "Ada’s Apple Watch Ultra"] {
            for (stage, hours) in [("AsleepCore", 5.0), ("AsleepDeep", 3.0)] {
                let s = Sample {
                    metric: "SleepAnalysis".into(),
                    category: Some(stage.into()),
                    unit: None,
                    ..sample("2026-04-03", "SleepAnalysis", device, hours)
                };
                writer.add_sample(&s).unwrap();
                table.add(&s);
            }
        }
        let sidecar = writer.finish(&table).unwrap();
        let pulse_sleep = "SELECT day, MAX(s) v FROM (SELECT day, source, SUM(sum) s FROM daily \
                           WHERE metric LIKE 'Sleep/Asleep%' GROUP BY day, source) GROUP BY day";
        let (_, rows) = query(&sidecar, pulse_sleep, 10).unwrap();
        assert_eq!(
            rows,
            vec![vec![
                serde_json::json!("2026-04-03"),
                serde_json::json!(8.0)
            ]]
        );
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
        let sidecar = writer.finish(&table).unwrap();

        let (cols, rows) = query(
            &sidecar,
            "SELECT day, sum, unit FROM daily WHERE metric='StepCount' ORDER BY day",
            100,
        )
        .unwrap();
        assert_eq!(cols, vec!["day", "sum", "unit"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], serde_json::json!(12000.0));
        assert_eq!(rows[1][2], serde_json::json!("count"));

        // Guardrails: write statements and multi-statements refused.
        assert!(query(&sidecar, "DELETE FROM daily", 10).is_err());
        assert!(query(&sidecar, "SELECT 1; SELECT 2", 10).is_err());
        // Row cap honored.
        let (_, capped) = query(&sidecar, "SELECT * FROM samples", 1).unwrap();
        assert_eq!(capped.len(), 1);
    }

    /// A metric written in two units rolls up in one, and the rollup says
    /// which; the raw samples keep the unit each was written in.
    #[test]
    fn test_rollups_carry_their_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let xml = tmp.path().join("export.xml");
        std::fs::write(&xml, "<HealthData/>").unwrap();
        let mut table = DayTable::default();
        let mut writer = SidecarWriter::create(&xml).unwrap();
        for (value, unit) in [(70.0, "kg"), (154.0, "lb")] {
            let s = Sample {
                unit: Some(unit.into()),
                ..sample("2026-04-01", "BodyMass", "Withings", value)
            };
            writer.add_sample(&s).unwrap();
            table.add(&s);
        }
        let sidecar = writer.finish(&table).unwrap();
        let (_, rows) = query(
            &sidecar,
            "SELECT n, ROUND(avg, 2), unit FROM daily WHERE metric='BodyMass'",
            10,
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                serde_json::json!(2),
                serde_json::json!(69.93),
                serde_json::json!("kg")
            ]]
        );
        let (_, units) = query(&sidecar, "SELECT unit FROM samples ORDER BY value", 10).unwrap();
        assert_eq!(
            units,
            vec![vec![serde_json::json!("kg")], vec![serde_json::json!("lb")]]
        );
    }

    /// A sidecar in the earlier layout (no `daily.unit`, rollups that
    /// averaged kg with lb) isn't read as it was: it is brought up to
    /// date from its samples, and then reads with one unit per metric.
    #[test]
    fn test_an_earlier_sidecar_is_brought_up_to_date() {
        let tmp = tempfile::tempdir().unwrap();
        let sidecar = tmp.path().join("health.vidx.sqlite");
        let conn = Connection::open(&sidecar).unwrap();
        conn.execute_batch(
            "CREATE TABLE samples(
                 day TEXT NOT NULL, metric TEXT NOT NULL, source TEXT NOT NULL,
                 value REAL, unit TEXT, start TEXT
             );
             CREATE TABLE daily(
                 day TEXT NOT NULL, metric TEXT NOT NULL, source TEXT NOT NULL,
                 n INTEGER, sum REAL, min REAL, max REAL, avg REAL
             );
             INSERT INTO samples VALUES
                 ('2026-04-01', 'BodyMass', 'Withings', 70.0, 'kg', '2026-04-01 07:00:00 -0400'),
                 ('2026-04-01', 'BodyMass', 'Withings', 154.0, 'lb', '2026-04-01 19:00:00 -0400'),
                 ('2026-04-01', 'Sleep/AsleepCore', 'Apple Watch', 6.5, NULL, '2026-03-31 23:00:00 -0400');
             INSERT INTO daily VALUES
                 ('2026-04-01', 'BodyMass', 'Withings', 2, 224.0, 70.0, 154.0, 112.0),
                 ('2026-04-01', 'Sleep/AsleepCore', 'Apple Watch', 1, 6.5, 6.5, 6.5, 6.5);
             CREATE INDEX idx_daily_metric_day ON daily(metric, day);",
        )
        .unwrap();
        drop(conn);

        let err = query(&sidecar, "SELECT avg FROM daily", 10).unwrap_err();
        assert!(
            format!("{err:#}").contains("being brought up to date"),
            "{err:#}"
        );

        let sql = "SELECT metric, n, ROUND(avg, 2), unit FROM daily ORDER BY metric";
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let rows = loop {
            match query(&sidecar, sql, 10) {
                Ok((_, rows)) => break rows,
                Err(err) => {
                    assert!(std::time::Instant::now() < deadline, "{err:#}");
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        };
        assert_eq!(
            rows,
            vec![
                vec![
                    serde_json::json!("BodyMass"),
                    serde_json::json!(2),
                    serde_json::json!(69.93),
                    serde_json::json!("kg")
                ],
                vec![
                    serde_json::json!("Sleep/AsleepCore"),
                    serde_json::json!(1),
                    serde_json::json!(6.5),
                    serde_json::Value::Null
                ],
            ]
        );
        // Brought up to date once; a second pass changes nothing.
        upgrade(&sidecar).unwrap();
        assert_eq!(query(&sidecar, sql, 10).unwrap().1, rows);
    }
}
