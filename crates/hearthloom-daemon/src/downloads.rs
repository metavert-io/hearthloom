//! Downloads auto-detect + move-into-place (Integrations item 16), and
//! the export-archive detector the inbox zip path shares (item 17).
//!
//! A per-source matcher scans `~/Downloads` for the exports the import
//! cards describe (a `conversations*.json`, an `.mbox`, the extracted
//! ChatGPT/Facebook/LinkedIn/Notion dirs, …). "Place" MOVES the find
//! into where that source ingests from — the inbox drop folder for
//! plain files, `~/.hearthloom/archives/<key>` for extracted archive
//! dirs — or copies it when the caller asks to keep the original.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;

/// The 250MB inbox mbox ceiling (inbox.rs enforces it at ingest time;
/// we warn at detect time so the move isn't a dead end).
const MBOX_WARN_BYTES: u64 = 250 * 1024 * 1024;

#[derive(Serialize)]
pub struct Found {
    /// Matches an import card's `key` in integrations.js.
    pub key: &'static str,
    pub name: String,
    pub path: String,
    pub bytes: u64,
    pub dir: bool,
    /// Where "place" will put it, for display ("inbox" | "archives/…").
    pub dest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warn: Option<String>,
}

/// File-shaped exports go to the inbox drop folder (the watcher ingests
/// them). Returns the import-card key.
pub fn detect_file(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name == "chat.db" {
        return Some("imessage");
    }
    if name == "photos.sqlite" {
        return Some("photos");
    }
    // Apple Health's export is literally named export.zip.
    if name == "export.zip" || name == "apple_health_export.zip" {
        return Some("apple-health");
    }
    if name.starts_with("conversations") && name.ends_with(".json") {
        return Some("chatlog");
    }
    if name.ends_with(".mbox") {
        return Some("mbox");
    }
    if name.ends_with(".ics") {
        return Some("ics");
    }
    None
}

/// Dir-shaped exports (already extracted) go to `archives/<key>`, where
/// their run-now adapters read. Markers are each export's signature
/// layout, so a random folder never matches.
pub fn detect_dir(path: &Path) -> Option<&'static str> {
    if path.join("conversations.json").is_file() && path.join("chat.html").is_file() {
        return Some("openai");
    }
    if path.join("Connections.csv").is_file() || path.join("Positions.csv").is_file() {
        return Some("linkedin");
    }
    if path.join("your_facebook_activity").is_dir() {
        return Some("facebook");
    }
    // The Health export unzipped by hand (or by the inbox): the export
    // dir itself, or the wrapper containing it.
    if path.join("export.xml").is_file() || path.join("apple_health_export/export.xml").is_file()
    {
        return Some("apple-health");
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.starts_with("Export-") {
        return Some("notion");
    }
    None
}

/// Dirs stage into archives/<key> (their adapters read there); files go
/// to the inbox drop folder.
fn dest_label(key: &str, dir: bool) -> String {
    if dir {
        format!("~/.hearthloom/archives/{key}")
    } else {
        "~/.hearthloom/inbox".into()
    }
}

/// Scan `~/Downloads` (top level only) for recognizable exports,
/// newest first, capped.
pub fn scan_downloads() -> Vec<Found> {
    let downloads = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h).join("Downloads"),
        Err(_) => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&downloads) {
        Ok(e) => e,
        Err(_) => return Vec::new(), // no Downloads (cloud node) or no TCC
    };
    let mut found: Vec<(std::time::SystemTime, Found)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let key = if meta.is_dir() {
            detect_dir(&path)
        } else {
            detect_file(&path)
        };
        let Some(key) = key else { continue };
        let bytes = if meta.is_dir() { 0 } else { meta.len() };
        let warn = (key == "mbox" && bytes > MBOX_WARN_BYTES).then(|| {
            format!(
                "{}MB — over the 250MB inbox limit; use the filtered pipeline instead \
                 (hearthloom ingest mbox <file> --include-labels …)",
                bytes / (1024 * 1024)
            )
        });
        found.push((
            meta.modified().unwrap_or(std::time::UNIX_EPOCH),
            Found {
                key,
                name,
                path: path.to_string_lossy().into_owned(),
                bytes,
                dir: meta.is_dir(),
                dest: dest_label(key, meta.is_dir()),
                warn,
            },
        ));
    }
    found.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    found.into_iter().map(|(_, f)| f).take(20).collect()
}

/// Move (or copy, when `keep_copy`) a detected export into place.
/// Re-detects the path so a stale/forged request can't relocate an
/// arbitrary file, and refuses anything already inside `~/.hearthloom`.
pub fn place(
    home: &hearthloom_core::Home,
    path: &str,
    key: &str,
    keep_copy: bool,
) -> Result<(PathBuf, String)> {
    let src = PathBuf::from(path);
    if !src.is_absolute() {
        return Err(anyhow!("path must be absolute"));
    }
    let src = src
        .canonicalize()
        .with_context(|| format!("no such file or folder: {path}"))?;
    if src.starts_with(home.root()) {
        return Err(anyhow!("that's already inside ~/.hearthloom"));
    }
    let detected = if src.is_dir() {
        detect_dir(&src)
    } else {
        detect_file(&src)
    };
    if detected != Some(key) {
        return Err(anyhow!(
            "{} doesn't look like a {key} export{}",
            src.display(),
            detected
                .map(|d| format!(" (detected: {d})"))
                .unwrap_or_default()
        ));
    }
    if src.is_dir() {
        let dest = install_archive_dir(home, &src, key, keep_copy)?;
        Ok((
            dest,
            "staged — press run now on its card to ingest".into(),
        ))
    } else {
        let inbox = home.inbox_dir();
        std::fs::create_dir_all(&inbox)?;
        let name = src
            .file_name()
            .ok_or_else(|| anyhow!("bad path"))?
            .to_string_lossy()
            .into_owned();
        // Never clobber an earlier drop of the same name (inbox.rs manner).
        let mut dest = inbox.join(&name);
        let mut n = 1;
        while dest.exists() {
            dest = inbox.join(format!("{n}-{name}"));
            n += 1;
        }
        move_or_copy(&src, &dest, keep_copy)?;
        Ok((
            dest,
            "in the inbox — the watcher ingests it within a minute".into(),
        ))
    }
}

/// Install an extracted export dir as `archives/<key>`. An existing
/// symlink there (hand-staging, the old way) is replaced; a real dir is
/// never clobbered. Shared with the inbox zip path (item 17).
pub fn install_archive_dir(
    home: &hearthloom_core::Home,
    src: &Path,
    key: &str,
    keep_copy: bool,
) -> Result<PathBuf> {
    let archives = home.root().join("archives");
    std::fs::create_dir_all(&archives)?;
    let dest = archives.join(key);
    if dest.is_symlink() {
        std::fs::remove_file(&dest).context("replacing the staging symlink")?;
    } else if dest.exists() {
        return Err(anyhow!(
            "~/.hearthloom/archives/{key} already exists — run it from its card, \
             or remove it first if this export should replace it"
        ));
    }
    move_or_copy(src, &dest, keep_copy)?;
    Ok(dest)
}

/// rename when possible; ditto (ships with macOS, handles files AND
/// dirs) across devices or when the original should be kept.
fn move_or_copy(src: &Path, dest: &Path, keep_copy: bool) -> Result<()> {
    if !keep_copy && std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    let status = std::process::Command::new("ditto")
        .arg(src)
        .arg(dest)
        .status()
        .context("running ditto")?;
    if !status.success() {
        return Err(anyhow!("copy failed ({status})"));
    }
    if !keep_copy {
        if src.is_dir() {
            std::fs::remove_dir_all(src)?;
        } else {
            std::fs::remove_file(src)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detectors_match_export_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // files
        for (name, key) in [
            ("chat.db", "imessage"),
            ("Photos.sqlite", "photos"),
            ("export.zip", "apple-health"),
            ("conversations (1).json", "chatlog"),
            ("takeout.mbox", "mbox"),
            ("calendar.ics", "ics"),
        ] {
            let p = root.join(name);
            std::fs::write(&p, b"x").unwrap();
            assert_eq!(detect_file(&p), Some(key), "{name}");
        }
        assert_eq!(detect_file(&root.join("random.txt")), None);
        // dirs
        let openai = root.join("chatgpt-export");
        std::fs::create_dir(&openai).unwrap();
        std::fs::write(openai.join("conversations.json"), b"[]").unwrap();
        std::fs::write(openai.join("chat.html"), b"").unwrap();
        assert_eq!(detect_dir(&openai), Some("openai"));
        let li = root.join("linkedin-export");
        std::fs::create_dir(&li).unwrap();
        std::fs::write(li.join("Connections.csv"), b"").unwrap();
        assert_eq!(detect_dir(&li), Some("linkedin"));
        let fb = root.join("dyi");
        std::fs::create_dir_all(fb.join("your_facebook_activity")).unwrap();
        assert_eq!(detect_dir(&fb), Some("facebook"));
        let notion = root.join("Export-abc123");
        std::fs::create_dir(&notion).unwrap();
        assert_eq!(detect_dir(&notion), Some("notion"));
        assert_eq!(detect_dir(&root.join("nope")), None);
    }

    #[test]
    fn test_place_moves_file_to_inbox_and_dir_to_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("hl"));
        home.init().unwrap();
        let dl = tmp.path().join("Downloads");
        std::fs::create_dir(&dl).unwrap();

        // file → inbox (move: original gone)
        let ics = dl.join("cal.ics");
        std::fs::write(&ics, b"BEGIN:VCALENDAR").unwrap();
        let (dest, _) = place(&home, ics.to_str().unwrap(), "ics", false).unwrap();
        assert!(dest.starts_with(home.inbox_dir()) && dest.exists());
        assert!(!ics.exists());

        // dir → archives (keep_copy: original stays)
        let li = dl.join("linkedin");
        std::fs::create_dir(&li).unwrap();
        std::fs::write(li.join("Connections.csv"), b"a,b").unwrap();
        let (dest, _) = place(&home, li.to_str().unwrap(), "linkedin", true).unwrap();
        assert!(dest.join("Connections.csv").exists());
        assert!(li.exists(), "keep_copy must leave the original");

        // wrong key refused; real existing archive dir never clobbered
        assert!(place(&home, li.to_str().unwrap(), "facebook", true).is_err());
        assert!(place(&home, li.to_str().unwrap(), "linkedin", true)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
    }

    #[test]
    fn test_place_replaces_staging_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("hl"));
        home.init().unwrap();
        let export = tmp.path().join("chatgpt");
        std::fs::create_dir(&export).unwrap();
        std::fs::write(export.join("conversations.json"), b"[]").unwrap();
        std::fs::write(export.join("chat.html"), b"").unwrap();
        let archives = home.root().join("archives");
        std::fs::create_dir_all(&archives).unwrap();
        std::os::unix::fs::symlink(&export, archives.join("openai")).unwrap();
        let (dest, _) = place(&home, export.to_str().unwrap(), "openai", false).unwrap();
        assert!(!dest.is_symlink() && dest.join("conversations.json").exists());
    }
}
