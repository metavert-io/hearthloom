//! Rewind rescue adapter: the lifelogger that got remote-killed
//! (recording disabled 2025-12-19) left years of screen-time segments
//! and call transcripts stranded in a local SQLite at
//! `~/Library/Application Support/com.memoryvault.MemoryVault/`.
//! This reads that frozen database IN PLACE (read-only, the app can
//! never change it again) and rescues:
//!
//!   - `segment` (focused-app sessions, window titles, browser URLs)
//!     → one `rewind_day` episode per day, top apps as Artifact entities
//!   - `transcript_word` + `audio` → one `rewind_transcript` episode per
//!     recorded conversation
//!
//! OCR frame text is deliberately NOT ingested (millions of rows of
//! screen noise — volume without recall value); its presence is noted
//! in the report so nothing is silently ignored.
//!
//! Schema drift is the norm (the teardown documents v0.63; later builds
//! renamed columns and finally encrypted the file as db-enc.sqlite3):
//! every table and column is DISCOVERED via sqlite_master/PRAGMA and
//! anything absent degrades to a skip, never a failure. An encrypted-
//! only install gets an error explaining the one-time sqlcipher export.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use chrono::DateTime;
use hearthloom_core::{EntityRef, EpisodeDraft, EventTime, TimeUncertainty};
use rusqlite::{Connection, OpenFlags};

const BODY_CAP: usize = 48 * 1024;
/// Apple epoch (2001-01-01) → Unix epoch offset in seconds.
const APPLE_EPOCH_OFFSET: i64 = 978_307_200;
const TOP_APPS_PER_DAY: usize = 5;

pub fn default_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
    Ok(Path::new(&home).join("Library/Application Support/com.memoryvault.MemoryVault"))
}

const DECRYPT_HINT: &str =
    "this Rewind install keeps only the ENCRYPTED database (db-enc.sqlite3). \
     Decrypt it once with sqlcipher (the key is the one Rewind stored on your \
     machine — check the login keychain for the app's entry):\n\
       sqlcipher db-enc.sqlite3\n\
       PRAGMA key = '<your key>';\n\
       ATTACH DATABASE 'rewind-plain.sqlite3' AS plain KEY '';\n\
       SELECT sqlcipher_export('plain');\n\
     then point this adapter at rewind-plain.sqlite3.";

/// Timestamps in Rewind drift between epochs across versions; classify
/// by magnitude. Returns Unix seconds.
fn to_unix(raw: f64) -> Option<i64> {
    let v = raw.abs();
    let unix = if (1.0e9..2.5e9).contains(&v) {
        raw as i64 // unix seconds
    } else if (1.0e12..2.5e12).contains(&v) {
        (raw / 1e3) as i64 // unix millis
    } else if (1.0e15..2.5e15).contains(&v) {
        (raw / 1e6) as i64 // unix micros
    } else if (3.0e8..9.5e8).contains(&v) {
        raw as i64 + APPLE_EPOCH_OFFSET // apple seconds
    } else if (3.0e11..9.5e11).contains(&v) {
        (raw / 1e3) as i64 + APPLE_EPOCH_OFFSET // apple millis
    } else {
        return None;
    };
    // sanity: 2015..2035
    (1_420_070_400..2_051_222_400).contains(&unix).then_some(unix)
}

fn tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type IN ('table','view')")?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names)
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare(&format!("PRAGMA table_info({table})")) else {
        return Vec::new();
    };
    stmt.query_map([], |row| row.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// First candidate column that exists (schema drift across versions).
fn pick<'a>(cols: &[String], candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .find(|c| cols.iter().any(|have| have.eq_ignore_ascii_case(c)))
        .copied()
}

/// A cleaned app name from a bundle id ("com.apple.Safari" → "Safari")
/// or an already-clean name.
fn app_name(raw: &str) -> String {
    let last = raw.rsplit('.').next().unwrap_or(raw);
    if last.is_empty() { raw.to_string() } else { last.to_string() }
}

pub fn ingest(path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    // Accept the app-support dir, or a direct path to a (decrypted) db.
    let db = if path.is_dir() {
        let plain = path.join("db.sqlite3");
        if plain.is_file() {
            plain
        } else if path.join("db-enc.sqlite3").is_file() {
            return Err(anyhow!("{DECRYPT_HINT}"));
        } else {
            return Err(anyhow!(
                "no Rewind database under {} (expected db.sqlite3)",
                path.display()
            ));
        }
    } else {
        path.to_path_buf()
    };
    let conn = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let names = match tables(&conn) {
        Ok(n) => n,
        // SQLCipher-encrypted files open but fail on first real read
        Err(e) if e.to_string().contains("not a database") => {
            return Err(anyhow!("{} — {DECRYPT_HINT}", db.display()))
        }
        Err(e) => return Err(e),
    };
    let has = |t: &str| names.iter().any(|n| n.eq_ignore_ascii_case(t));

    let mut drafts = Vec::new();
    if has("segment") {
        drafts.extend(segments_to_days(&conn, scope)?);
    }
    if has("transcript_word") {
        drafts.extend(transcripts(&conn, scope)?);
    }
    if drafts.is_empty() {
        return Err(anyhow!(
            "no usable Rewind data found in {} (tables present: {})",
            db.display(),
            names.join(", ")
        ));
    }
    if has("search_content") || has("searchRanking_content") {
        tracing::info!(
            "rewind: OCR frame text present but deliberately not ingested \
             (screen noise at frame volume; segments + transcripts carry the recall value)"
        );
    }
    Ok(drafts)
}

/// Focused-app sessions → one episode per day: `[HH:MM] App — window`
/// lines plus a top-apps rollup; the day's top apps become Artifact
/// entities (the health-adapter Device pattern) so Ribbons shows when
/// each tool entered and left your life.
fn segments_to_days(conn: &Connection, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let cols = columns(conn, "segment");
    let Some(start) = pick(&cols, &["startDate", "start", "startTime", "createdAt"]) else {
        tracing::warn!("rewind: segment table has no recognizable time column — skipped");
        return Ok(Vec::new());
    };
    let end = pick(&cols, &["endDate", "end", "endTime"]);
    let Some(app) = pick(&cols, &["bundleID", "bundleIdentifier", "bundle_id", "appName", "app"])
    else {
        tracing::warn!("rewind: segment table has no recognizable app column — skipped");
        return Ok(Vec::new());
    };
    let window = pick(&cols, &["windowName", "windowTitle", "title"]);
    let url = pick(&cols, &["browserUrl", "url"]);

    let select = format!(
        "SELECT CAST({start} AS REAL), {}, {app}, {}, {} FROM segment ORDER BY {start}",
        end.map(|c| format!("CAST({c} AS REAL)")).unwrap_or_else(|| "NULL".into()),
        window.unwrap_or("NULL"),
        url.unwrap_or("NULL"),
    );
    let mut stmt = conn.prepare(&select)?;
    type Row = (f64, Option<f64>, Option<String>, Option<String>, Option<String>);
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1).ok(), row.get(2).ok(), row.get(3).ok(), row.get(4).ok()))
        })?
        .collect::<std::result::Result<_, _>>()?;

    // (unix, app, window, url, minutes)
    type Session = (i64, String, String, String, f64);
    let mut buckets: BTreeMap<String, Vec<Session>> = BTreeMap::new();
    for (start_raw, end_raw, app, window, url) in rows {
        let Some(unix) = to_unix(start_raw) else { continue };
        let Some(dt) = DateTime::from_timestamp(unix, 0) else { continue };
        let minutes = end_raw
            .and_then(to_unix)
            .map(|e| ((e - unix).max(0) as f64) / 60.0)
            .unwrap_or(0.0);
        let app = app_name(app.as_deref().unwrap_or("unknown"));
        buckets.entry(dt.format("%Y-%m-%d").to_string()).or_default().push((
            unix,
            app,
            window.unwrap_or_default(),
            url.unwrap_or_default(),
            minutes,
        ));
    }

    let mut drafts = Vec::new();
    for (date, sessions) in buckets {
        let mut minutes_by_app: BTreeMap<String, f64> = BTreeMap::new();
        for (_, app, _, _, minutes) in &sessions {
            *minutes_by_app.entry(app.clone()).or_default() += minutes.max(0.5);
        }
        let mut top: Vec<(String, f64)> = minutes_by_app.into_iter().collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));

        let mut body = format!("screen activity on {date}: {} app sessions\n", sessions.len());
        let top_list: Vec<String> = top
            .iter()
            .take(8)
            .map(|(a, m)| format!("{a} ({}m)", m.round() as i64))
            .collect();
        body.push_str(&format!("top apps: {}\n\n", top_list.join(", ")));
        let mut last_line = String::new();
        for (unix, app, window, url, _) in &sessions {
            let Some(dt) = DateTime::from_timestamp(*unix, 0) else { continue };
            let detail = if !url.is_empty() {
                format!("{window} ({url})")
            } else {
                window.clone()
            };
            let line = format!("[{}] {app} — {}\n", dt.format("%H:%M"), detail.trim());
            if line.trim_end() == last_line {
                continue; // collapse focus-flap runs
            }
            last_line = line.trim_end().to_string();
            if body.len() + line.len() > BODY_CAP {
                body.push_str("[truncated]\n");
                break;
            }
            body.push_str(&line);
        }

        let entities: Vec<EntityRef> = top
            .iter()
            .take(TOP_APPS_PER_DAY)
            .map(|(app, _)| EntityRef {
                entity_type: "Artifact".into(),
                name: app.clone(),
                attributes: vec![],
                role: "used".into(),
            })
            .collect();

        drafts.push(EpisodeDraft {
            source: "rewind".into(),
            source_ref: format!("day#{date}"),
            kind: "rewind_day".into(),
            title: format!(
                "Screen time: {}",
                top.iter().take(3).map(|(a, _)| a.clone()).collect::<Vec<_>>().join(", ")
            ),
            body,
            event_time: EventTime {
                utc: DateTime::from_timestamp(sessions[0].0, 0)
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Hour,
            },
            trust: "self".into(),
            scope: scope.map(String::from),
            entities,
            edges: vec![],
        });
    }
    Ok(drafts)
}

/// Recorded conversations: words regrouped into one transcript episode
/// per audio session, speaker turns on `speechSource` changes.
fn transcripts(conn: &Connection, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let cols = columns(conn, "transcript_word");
    let Some(word) = pick(&cols, &["word", "text"]) else {
        return Ok(Vec::new());
    };
    let audio_fk = pick(&cols, &["audioId", "audio_id", "audioRecordingId", "audio"]);
    let offset = pick(&cols, &["timeOffset", "offset", "time"]);
    let speaker = pick(&cols, &["speechSource", "speaker", "source"]);

    // The audio table gives each session its start time; without it the
    // words still bundle by their foreign key, dated by first offset.
    let audio_cols = columns(conn, "audio");
    let audio_start = pick(&audio_cols, &["startTime", "startDate", "createdAt", "start"]);

    let select = format!(
        "SELECT {}, {word}, {}, {} FROM transcript_word ORDER BY {}, {}",
        audio_fk.unwrap_or("1"),
        speaker.unwrap_or("NULL"),
        offset.map(|c| format!("CAST({c} AS REAL)")).unwrap_or_else(|| "0".into()),
        audio_fk.unwrap_or("1"),
        offset.unwrap_or("rowid"),
    );
    let mut stmt = conn.prepare(&select)?;
    type Row = (i64, String, Option<String>, f64);
    let rows: Vec<Row> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0).unwrap_or(1),
                row.get(1)?,
                row.get(2).ok(),
                row.get(3).unwrap_or(0.0),
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let starts: BTreeMap<i64, i64> = match (audio_fk, audio_start) {
        (Some(_), Some(start_col)) => {
            let mut stmt =
                conn.prepare(&format!("SELECT id, CAST({start_col} AS REAL) FROM audio"))?;
            let collected: BTreeMap<i64, i64> = stmt
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?)))?
                .flatten()
                .filter_map(|(id, raw)| to_unix(raw).map(|u| (id, u)))
                .collect();
            collected
        }
        _ => BTreeMap::new(),
    };

    let mut sessions: BTreeMap<i64, Vec<(String, Option<String>)>> = BTreeMap::new();
    for (audio_id, word, speaker, _offset) in rows {
        sessions.entry(audio_id).or_default().push((word, speaker));
    }

    let mut drafts = Vec::new();
    for (audio_id, words) in sessions {
        let mut body = String::new();
        let mut current_speaker: Option<String> = None;
        for (word, speaker) in &words {
            if *speaker != current_speaker {
                current_speaker = speaker.clone();
                let label = speaker.as_deref().unwrap_or("speaker");
                body.push_str(&format!("\n[{label}] "));
            }
            body.push_str(word);
            body.push(' ');
            if body.len() > BODY_CAP {
                body.push_str("[truncated]");
                break;
            }
        }
        let body = body.trim().to_string();
        if body.len() < 40 {
            continue; // a few stray words is not a conversation
        }
        let unix = starts.get(&audio_id).copied();
        drafts.push(EpisodeDraft {
            source: "rewind".into(),
            source_ref: format!("audio#{audio_id}"),
            kind: "rewind_transcript".into(),
            title: format!("Recorded conversation ({} words)", words.len()),
            body,
            event_time: EventTime {
                utc: unix
                    .and_then(|u| DateTime::from_timestamp(u, 0))
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Hour,
            },
            trust: "self".into(),
            scope: scope.map(String::from),
            entities: vec![],
            edges: vec![],
        });
    }
    Ok(drafts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_epoch_classification() {
        // unix seconds / millis / micros, apple seconds / millis — all
        // land on the same instant (2023-06-15T00:00:00Z = 1686787200)
        let unix = 1_686_787_200i64;
        assert_eq!(to_unix(1_686_787_200.0), Some(unix));
        assert_eq!(to_unix(1_686_787_200_000.0), Some(unix));
        assert_eq!(to_unix(1_686_787_200_000_000.0), Some(unix));
        assert_eq!(to_unix((unix - APPLE_EPOCH_OFFSET) as f64), Some(unix));
        assert_eq!(to_unix(((unix - APPLE_EPOCH_OFFSET) as f64) * 1e3), Some(unix));
        // garbage stays out
        assert_eq!(to_unix(0.0), None);
        assert_eq!(to_unix(42.0), None);
    }

    fn seed_db(dir: &Path) -> PathBuf {
        let db = dir.join("db.sqlite3");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE segment (id INTEGER PRIMARY KEY, startDate REAL, endDate REAL,
                                   bundleID TEXT, windowName TEXT, browserUrl TEXT);
             CREATE TABLE audio (id INTEGER PRIMARY KEY, startTime REAL);
             CREATE TABLE transcript_word (id INTEGER PRIMARY KEY, audioId INTEGER,
                                           word TEXT, timeOffset REAL, speechSource TEXT);
             CREATE TABLE search_content (id INTEGER PRIMARY KEY, c0 TEXT);
             -- two days of segments (unix seconds)
             INSERT INTO segment VALUES (1, 1686787200, 1686790800, 'com.apple.Safari', 'Rust docs', 'https://doc.rust-lang.org');
             INSERT INTO segment VALUES (2, 1686790800, 1686794400, 'com.microsoft.VSCode', 'main.rs', NULL);
             INSERT INTO segment VALUES (3, 1686873600, 1686877200, 'com.apple.Safari', 'HN', 'https://news.ycombinator.com');
             -- one conversation
             INSERT INTO audio VALUES (7, 1686787260);
             INSERT INTO transcript_word VALUES (1, 7, 'hello', 0.0, 'me');
             INSERT INTO transcript_word VALUES (2, 7, 'there', 0.4, 'me');
             INSERT INTO transcript_word VALUES (3, 7, 'hi', 1.0, 'others');
             INSERT INTO transcript_word VALUES (4, 7, 'this', 1.5, 'others');
             INSERT INTO transcript_word VALUES (5, 7, 'is', 1.8, 'others');
             INSERT INTO transcript_word VALUES (6, 7, 'a', 2.0, 'others');
             INSERT INTO transcript_word VALUES (7, 7, 'long', 2.2, 'others');
             INSERT INTO transcript_word VALUES (8, 7, 'enough', 2.4, 'others');
             INSERT INTO transcript_word VALUES (9, 7, 'conversation', 2.6, 'others');
             INSERT INTO transcript_word VALUES (10, 7, 'transcript', 2.8, 'others');",
        )
        .unwrap();
        db
    }

    #[test]
    fn test_rescue_segments_and_transcripts() {
        let tmp = tempfile::tempdir().unwrap();
        seed_db(tmp.path());
        let drafts = ingest(tmp.path(), Some("personal")).unwrap();

        let days: Vec<_> = drafts.iter().filter(|d| d.kind == "rewind_day").collect();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].source_ref, "day#2023-06-15");
        assert!(days[0].body.contains("Safari"));
        assert!(days[0].body.contains("Rust docs"));
        assert!(days[0].body.contains("doc.rust-lang.org"));
        // top apps became Artifact entities
        assert!(days[0].entities.iter().any(|e| e.name == "Safari" && e.entity_type == "Artifact"));

        let scripts: Vec<_> = drafts.iter().filter(|d| d.kind == "rewind_transcript").collect();
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0].source_ref, "audio#7");
        assert!(scripts[0].body.contains("[me] hello there"));
        assert!(scripts[0].body.contains("[others] hi this is"));
        assert_eq!(scripts[0].event_time.utc.as_deref(), Some("2023-06-15T00:01:00Z"));

        // idempotency comes from source_ref stability
        let again = ingest(tmp.path(), Some("personal")).unwrap();
        assert_eq!(again.len(), drafts.len());
        assert_eq!(again[0].source_ref, drafts[0].source_ref);
    }

    #[test]
    fn test_encrypted_only_install_gets_instructions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("db-enc.sqlite3"), b"SQLite format 3\0garbage").unwrap();
        let err = ingest(tmp.path(), None).unwrap_err().to_string();
        assert!(err.contains("sqlcipher_export"), "got: {err}");
    }

    #[test]
    fn test_schema_drift_degrades_not_fails() {
        // a future/older schema with renamed time column: segments skip,
        // transcripts still rescue
        let tmp = tempfile::tempdir().unwrap();
        let conn = Connection::open(tmp.path().join("db.sqlite3")).unwrap();
        conn.execute_batch(
            "CREATE TABLE segment (id INTEGER PRIMARY KEY, weirdTime REAL, bundleID TEXT);
             CREATE TABLE audio (id INTEGER PRIMARY KEY, startTime REAL);
             CREATE TABLE transcript_word (id INTEGER PRIMARY KEY, audioId INTEGER,
                                           word TEXT, timeOffset REAL, speechSource TEXT);
             INSERT INTO audio VALUES (1, 1686787260);
             INSERT INTO transcript_word VALUES (1, 1, 'a', 0, 'me');
             INSERT INTO transcript_word VALUES (2, 1, 'sufficiently', 1, 'me');
             INSERT INTO transcript_word VALUES (3, 1, 'wordy', 2, 'me');
             INSERT INTO transcript_word VALUES (4, 1, 'conversation', 3, 'me');
             INSERT INTO transcript_word VALUES (5, 1, 'for', 4, 'me');
             INSERT INTO transcript_word VALUES (6, 1, 'the', 5, 'me');
             INSERT INTO transcript_word VALUES (7, 1, 'length', 6, 'me');
             INSERT INTO transcript_word VALUES (8, 1, 'floor', 7, 'me');",
        )
        .unwrap();
        let drafts = ingest(tmp.path(), None).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].kind, "rewind_transcript");
    }
}
