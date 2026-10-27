//! Chrome history adapter: "what was I researching?" One episode per
//! profile per day, built from the `History` SQLite in each Chrome
//! profile. Chrome's data dir is not TCC-protected, so this needs no
//! special permissions; the live database is copied to a temp file first
//! because Chrome holds a lock while running.
//!
//! Chrome profiles often map to life contexts (work vs personal browser
//! profile), so the profile's display name is kept in the title/body for
//! later scope correction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use hearthloom_core::{EpisodeDraft, EventTime, TimeUncertainty};
use rusqlite::{Connection, OpenFlags};

const BODY_CAP: usize = 48 * 1024;
/// WebKit epoch (1601-01-01) → Unix epoch offset in seconds.
const WEBKIT_EPOCH_OFFSET: i64 = 11_644_473_600;

/// Default Chrome data dir.
pub fn default_data_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
    Ok(Path::new(&home).join("Library/Application Support/Google/Chrome"))
}

pub fn ingest(data_dir: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
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

    let mut drafts = Vec::new();
    for profile in profiles {
        match ingest_profile(&profile, scope) {
            Ok(mut profile_drafts) => drafts.append(&mut profile_drafts),
            Err(err) => tracing::warn!("skipping {}: {err}", profile.display()),
        }
    }
    if drafts.is_empty() {
        return Err(anyhow!("no browsing history found"));
    }
    Ok(drafts)
}

/// The human-facing profile name from Preferences, else the dir name.
fn profile_name(profile: &Path) -> String {
    let from_prefs = std::fs::read_to_string(profile.join("Preferences"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|prefs| {
            prefs
                .pointer("/profile/name")
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .filter(|name| !name.is_empty());
    from_prefs.unwrap_or_else(|| {
        profile
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "profile".into())
    })
}

fn ingest_profile(profile: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let name = profile_name(profile);
    // Chrome locks the live db; work on a copy.
    let tmp = tempfile::NamedTempFile::new()?;
    std::fs::copy(profile.join("History"), tmp.path())
        .with_context(|| format!("copying History from {}", profile.display()))?;
    let conn = Connection::open_with_flags(
        tmp.path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    let mut stmt = conn.prepare(
        "SELECT v.visit_time, u.url, u.title
         FROM visits v JOIN urls u ON u.id = v.url
         WHERE u.title IS NOT NULL AND u.title != ''
         ORDER BY v.visit_time",
    )?;
    let rows: Vec<(i64, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    // Bundle per UTC day (Chrome stores UTC; local offset is unknown).
    let mut buckets: BTreeMap<String, Vec<(i64, String, String)>> = BTreeMap::new();
    for (webkit_time, url, title) in rows {
        let unix = webkit_time / 1_000_000 - WEBKIT_EPOCH_OFFSET;
        let Some(dt) = DateTime::from_timestamp(unix, 0) else {
            continue;
        };
        buckets
            .entry(dt.format("%Y-%m-%d").to_string())
            .or_default()
            .push((unix, url, title));
    }

    let mut drafts = Vec::new();
    for (date, visits) in buckets {
        let mut body = format!("browsing ({name}) on {date}: {} visits\n\n", visits.len());
        let mut domains: BTreeMap<String, usize> = BTreeMap::new();
        let mut last_title = String::new();
        let mut lines = String::new();
        for (unix, url, title) in &visits {
            *domains.entry(domain_of(url)).or_default() += 1;
            if *title == last_title {
                continue; // collapse reload/redirect runs
            }
            last_title = title.clone();
            let Some(dt) = DateTime::from_timestamp(*unix, 0) else {
                continue;
            };
            let line = format!("[{}] {} — {}\n", dt.format("%H:%M"), title, domain_of(url));
            if body.len() + lines.len() + line.len() > BODY_CAP {
                lines.push_str("[truncated]\n");
                break;
            }
            lines.push_str(&line);
        }
        let mut top: Vec<(String, usize)> = domains.into_iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        let top_list: Vec<String> = top
            .iter()
            .take(8)
            .map(|(d, n)| format!("{d} ({n})"))
            .collect();
        body.push_str(&format!("top sites: {}\n\n", top_list.join(", ")));
        body.push_str(&lines);

        let first_unix = visits[0].0;
        drafts.push(EpisodeDraft {
            source: "chrome".into(),
            source_ref: format!("{name}#{date}"),
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
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .trim_start_matches("www.")
        .to_string()
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
        assert!(first.source_ref.starts_with("Personal#"));
        assert!(first.title.contains("Personal"));
        // Consecutive same-title visits collapse to one line.
        assert_eq!(first.body.matches("Learn Rust").count(), 1);
        assert!(first.body.contains("rust-lang.org"));
        assert!(first.body.contains("top sites:"));
        assert_eq!(first.event_time.uncertainty, TimeUncertainty::Hour);
    }
}
