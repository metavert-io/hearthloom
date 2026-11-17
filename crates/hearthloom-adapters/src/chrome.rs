//! Chrome history adapter: "what was I researching?" One episode per
//! profile per day, built from the `History` SQLite in each Chrome
//! profile. Chrome's data dir is not TCC-protected, so this needs no
//! special permissions; the live database is copied to a temp file first
//! because Chrome holds a lock while running.
//!
//! Only settled days are read ([`crate::first_unsettled_day`]), so a day
//! still being browsed waits; a later read that finds more visits for a
//! stored day lands as a fuller copy (the store's growing-source
//! revisions; the first line's `<n> visits` counts what a copy holds).
//!
//! A day's ref names its profile by when Chrome made it
//! (`profile:<creation time>#<date>`), so profiles shown under one name,
//! on one machine or two, keep separate days and a renamed profile keeps
//! its own; a profile whose Preferences carry no creation time is named by
//! this machine and its folder's full path (`profile-dir:…`). A day stored
//! by earlier versions under the shown name gives way to the profile's day
//! that holds all of it (`hearthloom_core::store::chrome_named_ref`).
//!
//! Chrome profiles often map to life contexts (work vs personal browser
//! profile), so the profile's display name is kept in the title/body for
//! later scope correction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use hearthloom_core::{EpisodeDraft, EventTime, TimeUncertainty};
use rusqlite::{Connection, OpenFlags};

const BODY_CAP: usize = 48 * 1024;
/// WebKit epoch (1601-01-01) → Unix epoch offset in seconds.
const WEBKIT_EPOCH_OFFSET: i64 = 11_644_473_600;
/// Days a run reads again behind where the last one stopped, so visits
/// that sync in late from the owner's other devices still reach their day
/// (as a fuller copy).
pub const LOOKBACK_DAYS: u64 = 7;

/// Default Chrome data dir.
pub fn default_data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
    Ok(Path::new(&home).join("Library/Application Support/Google/Chrome"))
}

/// Every settled day of every profile under `data_dir` (or of the one
/// profile `data_dir` is).
pub fn ingest(data_dir: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    ingest_at(data_dir, scope, Utc::now())
}

/// [`ingest`] as of `now`: the days settled by then.
pub fn ingest_at(
    data_dir: &Path,
    scope: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Vec<EpisodeDraft>> {
    Ok(ingest_new(data_dir, scope, now, &|_| None)?.drafts)
}

/// What [`ingest_new`] read.
#[derive(Debug)]
pub struct ChromeRead {
    pub drafts: Vec<EpisodeDraft>,
    /// For each profile read: its key ([`ChromeRead`] marks are per
    /// profile) and the first day it hasn't read — the next run's mark.
    pub marks: Vec<(String, NaiveDate)>,
}

/// The settled days of each profile from its mark on: `mark(key)` is the
/// first day a profile's last run didn't read (`None`: read every day).
/// [`LOOKBACK_DAYS`] before the mark are read again. A profile that can't
/// be read is skipped and gets no new mark.
pub fn ingest_new(
    data_dir: &Path,
    scope: Option<&str>,
    now: DateTime<Utc>,
    mark: &dyn Fn(&str) -> Option<NaiveDate>,
) -> Result<ChromeRead> {
    let settled_before = crate::first_unsettled_day(now);
    let mut profiles: Vec<PathBuf> = Vec::new();
    if data_dir.join("History").exists() {
        // Pointed directly at a profile dir.
        profiles.push(data_dir.to_path_buf());
    } else {
        for entry in std::fs::read_dir(data_dir)
            .with_context(|| format!("reading {}", data_dir.display()))?
            .flatten()
        {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if (name == "Default" || name.starts_with("Profile ")) && path.join("History").exists()
            {
                profiles.push(path);
            }
        }
    }
    if profiles.is_empty() {
        return Err(anyhow!(
            "no Chrome profiles with History under {}",
            data_dir.display()
        ));
    }

    // A profile with no settled day yet adds nothing; that's not an error.
    let mut read = ChromeRead {
        drafts: Vec::new(),
        marks: Vec::new(),
    };
    for dir in profiles {
        let profile = Profile::read(&dir);
        let from = mark(&profile.key)
            .map(|day| day - chrono::Days::new(LOOKBACK_DAYS))
            .filter(|from| *from < settled_before);
        match ingest_profile(&dir, &profile, scope, from, settled_before) {
            Ok(mut profile_drafts) => {
                read.drafts.append(&mut profile_drafts);
                read.marks.push((profile.key.clone(), settled_before));
            }
            Err(err) => tracing::warn!("skipping {}: {err}", dir.display()),
        }
    }
    Ok(read)
}

/// This machine, for telling apart profiles Chrome gave no creation time:
/// its hardware UUID on a Mac, its machine id elsewhere, else its host
/// name. `None` when none can be read.
fn machine_id() -> Option<String> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        hardware_uuid()
            .or_else(|| {
                ["/etc/machine-id", "/var/lib/dbus/machine-id"]
                    .iter()
                    .find_map(|path| {
                        std::fs::read_to_string(path)
                            .ok()
                            .map(|id| id.trim().to_string())
                            .filter(|id| !id.is_empty())
                    })
            })
            .or_else(|| {
                std::process::Command::new("hostname")
                    .output()
                    .ok()
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .filter(|host| !host.is_empty())
            })
    })
    .clone()
}

/// The Mac's hardware UUID (`IOPlatformUUID`).
#[cfg(target_os = "macos")]
fn hardware_uuid() -> Option<String> {
    let out = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|line| {
            let (_, rest) = line.split_once("\"IOPlatformUUID\" = \"")?;
            Some(rest.trim_end_matches('"').trim().to_string())
        })
        .filter(|uuid| !uuid.is_empty())
}

#[cfg(not(target_os = "macos"))]
fn hardware_uuid() -> Option<String> {
    None
}

/// A profile as its Preferences describe it.
struct Profile {
    /// The name Chrome shows (titles and bodies), else the dir name.
    name: String,
    /// What its days' refs name it by: `profile:<creation time>` — kept
    /// through renames, and different for each profile on each machine —
    /// or, for a profile whose Preferences carry none,
    /// `profile-dir:<place>:<dir>`, `place` a digest of this machine and
    /// the folder's full path (different for each profile folder on each
    /// machine, however the folders are named).
    key: String,
}

impl Profile {
    fn read(profile: &Path) -> Profile {
        Profile::read_on(profile, machine_id().as_deref())
    }

    fn read_on(profile: &Path, machine: Option<&str>) -> Profile {
        let prefs = std::fs::read_to_string(profile.join("Preferences"))
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
        let pref = |pointer: &str| prefs.as_ref().and_then(|p| p.pointer(pointer));
        let dir = profile
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "profile".into());
        let name = pref("/profile/name")
            .and_then(|n| n.as_str())
            .filter(|name| !name.is_empty())
            .map(String::from)
            .unwrap_or_else(|| dir.clone());
        // Chrome writes this 64-bit count as a string; accept a number too.
        let created = pref("/profile/creation_time")
            .and_then(|t| match t {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => n.as_u64().map(|n| n.to_string()),
                _ => None,
            })
            .filter(|t| {
                (1..=20).contains(&t.len())
                    && t.bytes().all(|b| b.is_ascii_digit())
                    && t.bytes().any(|b| b != b'0')
            });
        // a ref never carries `#` before its date
        let key = match created {
            Some(created) => format!("profile:{created}"),
            None => {
                let path = std::fs::canonicalize(profile).unwrap_or_else(|_| profile.to_path_buf());
                let mut seed = machine.unwrap_or_default().as_bytes().to_vec();
                seed.push(0);
                seed.extend_from_slice(path.to_string_lossy().as_bytes());
                let place = &hearthloom_core::ids::content_hash(&seed)[..12];
                format!("profile-dir:{place}:{}", dir.replace('#', "%23"))
            }
        };
        Profile { name, key }
    }

    /// A day's `source_ref`: the profile's key and the date.
    fn day_ref(&self, date: &str) -> String {
        format!("{}#{date}", self.key)
    }
}

/// A day's start as a Chrome visit time (WebKit microseconds).
fn webkit_day_start(day: NaiveDate) -> i64 {
    (day.and_time(chrono::NaiveTime::MIN).and_utc().timestamp() + WEBKIT_EPOCH_OFFSET) * 1_000_000
}

/// Copy a profile's live `History` into `dir` under the same name, with
/// its write-ahead log when it has one (committed visits can sit there
/// until Chrome checkpoints). Chrome writes these files while it runs, so
/// the copy is repeated until they hold still across one whole copy: a
/// copy taken mid-write can read as a day with visits missing.
fn snapshot(profile: &Path, dir: &Path) -> Result<PathBuf> {
    const FILES: [&str; 2] = ["History", "History-wal"];
    let stamp = || {
        FILES.map(|f| {
            std::fs::metadata(profile.join(f))
                .ok()
                .map(|m| (m.len(), m.modified().ok()))
        })
    };
    for attempt in 1..=3u64 {
        let before = stamp();
        let mut copied = true;
        for (file, seen) in FILES.iter().zip(&before) {
            let to = dir.join(file);
            if seen.is_none() {
                let _ = std::fs::remove_file(&to);
            } else if let Err(err) = std::fs::copy(profile.join(file), &to) {
                if *file == "History" {
                    return Err(err)
                        .with_context(|| format!("copying History from {}", profile.display()));
                }
                copied = false; // the log went away mid-copy: try again
            }
        }
        if copied && stamp() == before {
            return Ok(dir.join("History"));
        }
        std::thread::sleep(std::time::Duration::from_millis(250 * attempt));
    }
    Err(anyhow!(
        "History in {} kept changing while it was copied; it is read again next time",
        profile.display()
    ))
}

/// A profile's days from `from` (every day when `None`) up to, not
/// including, `settled_before`.
fn ingest_profile(
    profile_dir: &Path,
    profile: &Profile,
    scope: Option<&str>,
    from: Option<NaiveDate>,
    settled_before: NaiveDate,
) -> Result<Vec<EpisodeDraft>> {
    let name = &profile.name;
    // Chrome locks the live db; work on a copy.
    let tmp = tempfile::tempdir()?;
    let copy = snapshot(profile_dir, tmp.path())?;
    let conn = Connection::open_with_flags(
        &copy,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    let mut stmt = conn.prepare(
        "SELECT v.visit_time, u.url, u.title
         FROM visits v JOIN urls u ON u.id = v.url
         WHERE u.title IS NOT NULL AND u.title != ''
           AND v.visit_time >= ?1 AND v.visit_time < ?2
         ORDER BY v.visit_time",
    )?;
    let rows: Vec<(i64, String, String)> = stmt
        .query_map(
            [
                from.map(webkit_day_start).unwrap_or(i64::MIN),
                webkit_day_start(settled_before),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?
        .collect::<std::result::Result<_, _>>()?;

    // Bundle per UTC day (Chrome stores UTC; local offset is unknown).
    let mut buckets: BTreeMap<String, Vec<(i64, String, String)>> = BTreeMap::new();
    for (webkit_time, url, title) in rows {
        let unix = webkit_time.div_euclid(1_000_000) - WEBKIT_EPOCH_OFFSET;
        let Some(dt) = DateTime::from_timestamp(unix, 0) else {
            continue;
        };
        let date = dt.format("%Y-%m-%d").to_string();
        buckets.entry(date).or_default().push((unix, url, title));
    }

    let mut drafts = Vec::new();
    for (date, visits) in buckets {
        let mut body = format!("browsing ({name}) on {date}: {} visits\n\n", visits.len());
        let mut domains: BTreeMap<String, usize> = BTreeMap::new();
        let mut last_title = String::new();
        let mut lines = Vec::new();
        for (unix, url, title) in &visits {
            *domains.entry(domain_of(url)).or_default() += 1;
            if *title == last_title {
                continue; // collapse reload/redirect runs
            }
            last_title = title.clone();
            let Some(dt) = DateTime::from_timestamp(*unix, 0) else {
                continue;
            };
            // a title is the page's words: on one line, so none reads as
            // a visit of its own
            lines.push(format!(
                "[{}] {} — {}\n",
                dt.format("%H:%M"),
                hearthloom_core::one_line(title),
                domain_of(url)
            ));
        }
        let mut top: Vec<(String, usize)> = domains.into_iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let top_list: Vec<String> = top
            .iter()
            .take(8)
            .map(|(d, n)| format!("{d} ({n})"))
            .collect();
        body.push_str(&format!("top sites: {}\n\n", top_list.join(", ")));
        // a long day keeps its first and its latest visits: a fuller read
        // shows what it gained
        let body = crate::fit_entries(body, &lines, BODY_CAP, |n| {
            format!("[{n} lines not shown]\n")
        });

        let first_unix = visits[0].0;
        drafts.push(EpisodeDraft {
            source: "chrome".into(),
            source_ref: profile.day_ref(&date),
            kind: "browsing".into(),
            title: format!(
                "Browsing ({name}): {}",
                top_list
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            body,
            event_time: EventTime {
                utc: DateTime::from_timestamp(first_unix, 0)
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

fn domain_of(url: &str) -> String {
    hearthloom_core::one_line(
        url.split("://")
            .nth(1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or(url)
            .trim_start_matches("www."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_history() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("Default");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join("Preferences"),
            r#"{"profile": {"name": "Personal"}}"#,
        )
        .unwrap();
        let conn = Connection::open(profile.join("History")).unwrap();
        conn.execute_batch(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT, visit_count INTEGER);
             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER);
             INSERT INTO urls VALUES (1, 'https://www.rust-lang.org/learn', 'Learn Rust', 3);
             INSERT INTO urls VALUES (2, 'https://news.ycombinator.com/', 'Hacker News', 5);
             -- 2026-03-15 ~14:00 UTC in WebKit microseconds:
             INSERT INTO visits VALUES (1, 1, (11644473600 + 1773585600) * 1000000);
             INSERT INTO visits VALUES (2, 1, (11644473600 + 1773585660) * 1000000);
             INSERT INTO visits VALUES (3, 2, (11644473600 + 1773589200) * 1000000);
             -- next day:
             INSERT INTO visits VALUES (4, 2, (11644473600 + 1773672000) * 1000000);",
        )
        .unwrap();

        let drafts = ingest(&profile, None).unwrap();
        assert_eq!(drafts.len(), 2, "two days → two episodes");
        let first = &drafts[0];
        assert_eq!(first.kind, "browsing");
        assert!(first
            .source_ref
            .starts_with(&format!("{}#", key_of(&profile))));
        assert!(first.title.contains("Personal"));
        // Consecutive same-title visits collapse to one line.
        assert_eq!(first.body.matches("Learn Rust").count(), 1);
        assert!(first.body.contains("rust-lang.org"));
        assert!(first.body.contains("top sites:"));
        assert_eq!(first.event_time.uncertainty, TimeUncertainty::Hour);
    }

    /// A profile dir with these Preferences and one visit per unix time.
    fn profile_with(dir: &Path, prefs: &str, visits: &[i64]) -> Connection {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("Preferences"), prefs).unwrap();
        let conn = Connection::open(dir.join("History")).unwrap();
        conn.execute_batch(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT);
             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER);
             INSERT INTO urls VALUES (1, 'https://example.com/a', 'Example');",
        )
        .unwrap();
        for unix in visits {
            conn.execute(
                "INSERT INTO visits (url, visit_time) VALUES (1, (?1 + 11644473600) * 1000000)",
                [unix],
            )
            .unwrap();
        }
        conn
    }

    fn unix(t: &str) -> i64 {
        t.parse::<DateTime<Utc>>().unwrap().timestamp()
    }

    /// The key a profile folder's days carry on this machine.
    fn key_of(dir: &Path) -> String {
        Profile::read(dir).key
    }

    fn refs(drafts: &[EpisodeDraft]) -> Vec<String> {
        let mut refs: Vec<String> = drafts.iter().map(|d| d.source_ref.clone()).collect();
        refs.sort();
        refs
    }

    /// A day is read only once it has settled: a day still being browsed
    /// waits.
    #[test]
    fn test_only_settled_days_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("Default");
        let now = Utc::now().timestamp();
        profile_with(
            &profile,
            r#"{"profile": {"name": "Personal"}}"#,
            &[
                unix("2026-03-15T14:00:00Z"),
                unix("2026-03-16T09:00:00Z"),
                now,
            ],
        );
        // a visit made now adds nothing today
        assert!(refs(&ingest(&profile, None).unwrap())
            .iter()
            .all(|r| !r.ends_with(&Utc::now().format("%Y-%m-%d").to_string())));

        let at = |t: &str| ingest_at(&profile, None, t.parse().unwrap()).unwrap();
        let key = key_of(&profile);
        // the evening of the 16th: neither day has settled
        assert!(at("2026-03-16T20:00:00Z").is_empty());
        // the 15th settles a whole day after it ends
        assert_eq!(
            refs(&at("2026-03-17T00:00:00Z")),
            [format!("{key}#2026-03-15")]
        );
        assert_eq!(
            refs(&at("2026-03-18T00:00:00Z")),
            [format!("{key}#2026-03-15"), format!("{key}#2026-03-16")]
        );
    }

    /// A run given its last marks reads only the days from each profile's
    /// mark (less the lookback) on, and hands back the next marks.
    #[test]
    fn test_a_run_reads_from_where_the_last_one_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let days: Vec<i64> = (1..=20)
            .map(|d| unix(&format!("2026-03-{d:02}T12:00:00Z")))
            .collect();
        profile_with(
            &tmp.path().join("Default"),
            r#"{"profile": {"name": "Personal", "creation_time": "13300000000000001"}}"#,
            &days,
        );
        let now: DateTime<Utc> = "2026-03-22T06:00:00Z".parse().unwrap();
        let first = ingest_new(tmp.path(), None, now, &|_| None).unwrap();
        assert_eq!(first.drafts.len(), 20);
        let day = |d: &str| d.parse::<NaiveDate>().unwrap();
        assert_eq!(
            first.marks,
            [("profile:13300000000000001".to_string(), day("2026-03-21"))]
        );

        // the next run, a day later: the lookback and the newly settled day
        let marks = first.marks.clone();
        let mark = move |key: &str| marks.iter().find(|(k, _)| k == key).map(|(_, d)| *d);
        let later: DateTime<Utc> = "2026-03-23T06:00:00Z".parse().unwrap();
        let second = ingest_new(tmp.path(), None, later, &mark).unwrap();
        assert_eq!(
            refs(&second.drafts),
            (14..=20)
                .map(|d| format!("profile:13300000000000001#2026-03-{d:02}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(second.marks[0].1, day("2026-03-22"));
        // a profile with no mark yet reads every day
        assert_eq!(
            ingest_new(tmp.path(), None, later, &|_| None)
                .unwrap()
                .drafts
                .len(),
            20
        );
    }

    #[test]
    fn test_a_day_counts_its_visits_and_names_its_earlier_ref() {
        let tmp = tempfile::tempdir().unwrap();
        profile_with(
            &tmp.path().join("Default"),
            r#"{"profile": {"name": "Work) on 2026-03-15: x", "creation_time": "13300000000000001"}}"#,
            &[unix("2026-03-15T12:00:00Z"), unix("2026-03-15T13:00:00Z")],
        );
        let drafts = ingest_at(tmp.path(), None, "2026-03-20T00:00:00Z".parse().unwrap()).unwrap();
        assert_eq!(drafts.len(), 1);
        let d = &drafts[0];
        assert_eq!(
            hearthloom_core::store::growing_items("chrome", &d.body),
            Some(2)
        );
        assert_eq!(
            hearthloom_core::store::chrome_named_ref(&d.source_ref, &d.body).as_deref(),
            Some("Work) on 2026-03-15: x#2026-03-15")
        );
    }

    /// A long day keeps its first and its latest visits, so a read with
    /// more shows what it gained and holds the read before it; a page's
    /// title stays on one line.
    #[test]
    fn test_a_long_day_keeps_its_latest_visits_and_titles_on_one_line() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("Default");
        let conn = profile_with(
            &profile,
            r#"{"profile": {"name": "Work", "creation_time": "13300000000000001"}}"#,
            &[],
        );
        let start = unix("2026-03-15T06:00:00Z");
        let add = |id: i64, title: &str| {
            conn.execute(
                "INSERT INTO urls VALUES (?1, ?2, ?3)",
                rusqlite::params![id + 1, format!("https://example.com/{id}"), title],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO visits (url, visit_time) VALUES (?1, (?2 + 11644473600) * 1000000)",
                rusqlite::params![id + 1, start + id * 60],
            )
            .unwrap();
        };
        let words = "x".repeat(200);
        for id in 1..=400 {
            add(id, &format!("page {id} {words}"));
        }
        add(401, "Split\n[23:59] Not a visit — example.org");
        let read = || ingest_at(&profile, None, "2026-03-20T00:00:00Z".parse().unwrap()).unwrap();
        let before = read();
        let body = &before[0].body;
        assert!(body.len() <= BODY_CAP);
        assert!(body.contains("page 1 ") && body.contains("page 400 "));
        assert!(body.contains(" lines not shown]\n"));
        assert!(body.contains("] Split [23:59] Not a visit — example.org — example.com\n"));
        assert!(!body.lines().any(|l| l.starts_with("[23:59]")));

        add(402, "the latest page");
        let after = read();
        assert!(after[0].body.contains("the latest page"));
        assert_eq!(
            hearthloom_core::store::growing_items("chrome", &after[0].body),
            Some(402)
        );
        assert!(hearthloom_core::store::chrome_day_holds(
            &after[0].body,
            body
        ));
    }

    /// A day's ref names its profile, not its shown name: profiles shown
    /// under one name keep apart on every date, a profile without a
    /// creation time is told apart by its folder, and a rename keeps the
    /// refs.
    #[test]
    fn test_days_are_keyed_by_the_profile_not_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        let days = [unix("2026-11-12T12:00:00Z"), unix("2026-12-05T12:00:00Z")];
        let prefs = |name: &str, created: &str| {
            format!(r#"{{"profile": {{"name": "{name}", "creation_time": "{created}"}}}}"#)
        };
        profile_with(
            &tmp.path().join("Default"),
            &prefs("Personal", "13300000000000001"),
            &days,
        );
        profile_with(
            &tmp.path().join("Profile 1"),
            &prefs("Personal", "13300000000000002"),
            &days,
        );
        profile_with(
            &tmp.path().join("Profile 2"),
            r#"{"profile": {"name": "Personal"}}"#,
            &days,
        );
        profile_with(
            &tmp.path().join("Profile 3"),
            r#"{"profile": {"name": "Personal"}}"#,
            &days,
        );
        let now = "2027-01-10T00:00:00Z".parse().unwrap();
        let (two, three) = (
            key_of(&tmp.path().join("Profile 2")),
            key_of(&tmp.path().join("Profile 3")),
        );
        assert!(two.starts_with("profile-dir:") && two.ends_with(":Profile 2"));
        let mut expected = vec![
            format!("{two}#2026-11-12"),
            format!("{two}#2026-12-05"),
            format!("{three}#2026-11-12"),
            format!("{three}#2026-12-05"),
            "profile:13300000000000001#2026-11-12".to_string(),
            "profile:13300000000000001#2026-12-05".to_string(),
            "profile:13300000000000002#2026-11-12".to_string(),
            "profile:13300000000000002#2026-12-05".to_string(),
        ];
        expected.sort();
        assert_eq!(refs(&ingest_at(tmp.path(), None, now).unwrap()), expected);

        // renamed: the days keep their refs, the name shows in the text
        std::fs::write(
            tmp.path().join("Profile 1/Preferences"),
            prefs("Work", "13300000000000002"),
        )
        .unwrap();
        let drafts = ingest_at(&tmp.path().join("Profile 1"), None, now).unwrap();
        assert_eq!(
            refs(&drafts),
            [
                "profile:13300000000000002#2026-11-12",
                "profile:13300000000000002#2026-12-05"
            ]
        );
        assert!(drafts.iter().all(|d| d.title.contains("Work")));
    }

    /// Visits committed to the write-ahead log, not yet checkpointed into
    /// the main file, are read too.
    #[test]
    fn test_visits_in_the_write_ahead_log_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path().join("Default");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join("Preferences"),
            r#"{"profile": {"name": "Personal"}}"#,
        )
        .unwrap();
        let conn = Connection::open(profile.join("History")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE urls (id INTEGER PRIMARY KEY, url TEXT, title TEXT);
             CREATE TABLE visits (id INTEGER PRIMARY KEY, url INTEGER, visit_time INTEGER);
             INSERT INTO urls VALUES (1, 'https://example.com/a', 'Example');",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO visits (url, visit_time) VALUES (1, (?1 + 11644473600) * 1000000)",
            [unix("2026-03-15T14:00:00Z")],
        )
        .unwrap();
        assert!(profile.join("History-wal").exists());

        // the writer is still open, as Chrome is while it runs
        let drafts = ingest(&profile, None).unwrap();
        assert_eq!(refs(&drafts), [format!("{}#2026-03-15", key_of(&profile))]);
        drop(conn);
    }

    /// Profiles Chrome gave no creation time are told apart by machine and
    /// by folder: two Macs' `Default` profiles, and two Chrome data
    /// folders' `Default` profiles on one Mac, never share a day's ref;
    /// reading the same folder again gives the same one.
    #[test]
    fn test_profiles_without_a_creation_time_keep_apart_across_folders_and_machines() {
        let tmp = tempfile::tempdir().unwrap();
        let prefs = r#"{"profile": {"name": "Person 1"}}"#;
        let day = [unix("2026-03-15T12:00:00Z")];
        let chrome = tmp.path().join("Google/Chrome/Default");
        let beta = tmp.path().join("Google/Chrome Beta/Default");
        profile_with(&chrome, prefs, &day);
        profile_with(&beta, prefs, &day);
        let one = Profile::read_on(&chrome, Some("machine-one")).key;
        assert_ne!(one, Profile::read_on(&beta, Some("machine-one")).key);
        assert_ne!(one, Profile::read_on(&chrome, Some("machine-two")).key);
        assert_eq!(one, Profile::read_on(&chrome, Some("machine-one")).key);
        // and through the reader itself
        let now = "2026-03-20T00:00:00Z".parse().unwrap();
        let (a, b) = (
            refs(&ingest_at(&chrome, None, now).unwrap()),
            refs(&ingest_at(&beta, None, now).unwrap()),
        );
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_ne!(a, b);
        assert_eq!(a, refs(&ingest_at(&chrome, None, now).unwrap()));
    }
}
