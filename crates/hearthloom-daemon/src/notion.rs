//! Notion importer: an extracted "Markdown & CSV" workspace export →
//! Hearthloom NOTES (origin='notion'), idempotent by page-id.
//!
//! Notion's own exporter already flattens its rich blocks to markdown
//! (callouts → blockquotes, toggles → bullets, columns stacked) — the
//! text survives; only layout is lost. What we handle:
//!   - page-id suffixes in filenames → origin_ref (re-import = upsert)
//!   - folder hierarchy → tags (top 3 segments, slugified)
//!   - media links → `[media: filename]` placeholders (Jon: no rich
//!     media; the export should be "no files" anyway — belt+suspenders)
//!   - databases: a .csv with ≤50 rows becomes ONE markdown-table note;
//!     bigger ones are SKIPPED AND NAMED in the report (no silent caps)
//!   - database rows carry Created:/Updated: property lines — lifted
//!     into the note's real timestamps; PLAIN pages have none in
//!     Notion's export and date from import time (documented loss)

use std::path::{Path, PathBuf};

use anyhow::Result;
use hearthloom_core::store::NoteImport;
use hearthloom_core::{paths::Home, store::Store, Actor};

const CSV_TABLE_MAX_ROWS: usize = 50;
const CSV_TABLE_MAX_COLS: usize = 8;
const CELL_CAP: usize = 200;
const BODY_CAP: usize = 96 * 1024;

const MEDIA_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "heic", "bmp", "tiff", "pdf", "mp4", "mov", "mp3",
    "wav", "m4a", "zip", "pptx", "docx", "xlsx",
];

/// Images are the exception to no-rich-media (Jon 2026-07-10: "some
/// images are going to be important to preserve"): when the export
/// carries the file, it copies into ~/.hearthloom/notes/media/ and the
/// link rewrites to the vault-guarded serving route. Everything else
/// stays a placeholder. No size floor — silent drops lose the one
/// diagram that mattered; the report counts bytes instead.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "heic", "bmp"];

#[derive(Debug, Default, serde::Serialize)]
pub struct NotionReport {
    pub pages: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub tables: usize,
    pub media_stripped: usize,
    pub images_copied: usize,
    pub image_bytes: u64,
    pub empty_skipped: usize,
    /// Databases too big for a table note — named, never silent.
    pub big_csvs: Vec<String>,
}

/// Filename stems end with the 32-hex page id ("My Page 21fc4a5a…").
/// Returns (clean title, Some(id)) or (stem, None).
fn split_page_id(stem: &str) -> (String, Option<String>) {
    if let Some((head, tail)) = stem.rsplit_once(' ') {
        if tail.len() == 32 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            return (head.trim().to_string(), Some(tail.to_lowercase()));
        }
    }
    (stem.trim().to_string(), None)
}

/// Stable fallback ref for files without a page id.
fn path_ref(rel: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in rel.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("path:{h:016x}")
}

/// Folder path → tags: strip page-id suffixes, slugify, top 3.
fn tags_from(rel_dir: &Path) -> Vec<String> {
    let mut tags = Vec::new();
    for component in rel_dir.components().take(3) {
        let raw = component.as_os_str().to_string_lossy();
        let (name, _) = split_page_id(&raw);
        let slug: String = name
            .to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let slug: String = slug.chars().take(24).collect();
        if slug.len() >= 2 && !tags.contains(&slug) {
            tags.push(slug);
        }
    }
    tags
}

/// Minimal percent-decode for export-relative paths ("My%20File.png").
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[derive(Debug, Default)]
struct MediaStats {
    placeholders: usize,
    images_copied: usize,
    image_bytes: u64,
}

/// Rewrite media links: an IMAGE whose file exists beside the page is
/// copied into the substrate (notes/media/<ref>/) and its link points
/// at the guarded serving route; anything else (or a missing file)
/// becomes `[media: filename]`. Web and page links survive untouched.
fn rewrite_media_links(
    body: &str,
    page_dir: &Path,
    origin_ref: &str,
    media_root: &Path,
) -> Result<(String, MediaStats)> {
    let mut out = String::with_capacity(body.len());
    let mut stats = MediaStats::default();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // find a "[label](target)" (optionally preceded by '!')
        let bang = bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
        let bracket = bytes[i] == b'[';
        if !(bang || bracket) {
            let c = body[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let open = if bang { i + 1 } else { i };
        let Some(close) = body[open..].find("](").map(|p| open + p) else {
            out.push_str(&body[i..=open.min(bytes.len() - 1)]);
            i = open + 1;
            continue;
        };
        let Some(end) = body[close + 2..].find(')').map(|p| close + 2 + p) else {
            out.push_str(&body[i..close + 2]);
            i = close + 2;
            continue;
        };
        let target = &body[close + 2..end];
        let clean_target = target.split(['?', '#']).next().unwrap_or(target);
        let extension = clean_target.rsplit('.').next().unwrap_or("").to_lowercase();
        let is_media = !clean_target.starts_with("http") // exported files are relative
            && MEDIA_EXTENSIONS.contains(&extension.as_str());
        if is_media {
            let decoded = percent_decode(clean_target);
            let filename = decoded.rsplit('/').next().unwrap_or(&decoded).to_string();
            let source = page_dir.join(&decoded);
            if IMAGE_EXTENSIONS.contains(&extension.as_str()) && source.is_file() {
                // preserve the image: copy into the substrate, point the
                // link at the vault-guarded route
                let dir = media_root.join(origin_ref);
                std::fs::create_dir_all(&dir)?;
                let dest = dir.join(&filename);
                let bytes = std::fs::copy(&source, &dest)?;
                stats.images_copied += 1;
                stats.image_bytes += bytes;
                let encoded = filename.replace(' ', "%20");
                out.push_str(&format!(
                    "![{filename}](/notes-media/{origin_ref}/{encoded})"
                ));
            } else {
                out.push_str(&format!("[media: {filename}]"));
                stats.placeholders += 1;
            }
        } else {
            out.push_str(&body[i..=end]);
        }
        i = end + 1;
    }
    Ok((out, stats))
}

/// "February 4, 2021 7:20 AM" (Notion's property rendering) → RFC 3339.
/// No timezone in the export — stored as-is in UTC form (the same
/// honest compromise every dateless import makes).
fn parse_notion_datetime(s: &str) -> Option<String> {
    let parts: Vec<&str> = s.split([' ', ',']).filter(|p| !p.is_empty()).collect();
    // Month D YYYY [H:MM AM/PM]
    let first = parts.first()?;
    let month = MONTHS_FULL
        .iter()
        .position(|m| first.eq_ignore_ascii_case(m))?
        + 1;
    let day: u32 = parts.get(1)?.parse().ok()?;
    let year: i32 = parts.get(2)?.parse().ok()?;
    let (mut hour, minute) = match parts.get(3).and_then(|t| t.split_once(':')) {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (12, 0),
    };
    match parts.get(4).map(|p| p.to_uppercase()) {
        Some(ref ap) if ap == "PM" && hour != 12 => hour += 12,
        Some(ref ap) if ap == "AM" && hour == 12 => hour = 0,
        _ => {}
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z"
    ))
}

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Pull `Created:` / `Updated:` property lines from the top of a page
/// (Notion writes them for database rows). Returns (body without those
/// lines, created, updated).
fn extract_timestamps(body: &str) -> (String, Option<String>, Option<String>) {
    let mut created = None;
    let mut updated = None;
    let mut kept: Vec<&str> = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if i < 12 {
            let trimmed = line.trim();
            let lower = trimmed.to_lowercase();
            let value = trimmed.split_once(':').map(|x| x.1.trim());
            if (lower.starts_with("created:") || lower.starts_with("created time:"))
                && created.is_none()
            {
                if let Some(ts) = value.and_then(parse_notion_datetime) {
                    created = Some(ts);
                    continue;
                }
            }
            if (lower.starts_with("updated:")
                || lower.starts_with("last edited:")
                || lower.starts_with("last edited time:"))
                && updated.is_none()
            {
                if let Some(ts) = value.and_then(parse_notion_datetime) {
                    updated = Some(ts);
                    continue;
                }
            }
        }
        kept.push(line);
    }
    (kept.join("\n"), created, updated)
}

/// A small database → one markdown-table note body.
fn csv_to_table(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        let cells: Vec<String> = row
            .iter()
            .take(CSV_TABLE_MAX_COLS)
            .map(|c| {
                let mut c = c.replace('|', "\\|").replace('\n', " ");
                if c.len() > CELL_CAP {
                    c.truncate(CELL_CAP);
                    c.push('…');
                }
                c
            })
            .collect();
        out.push_str(&format!("| {} |\n", cells.join(" | ")));
        if i == 0 {
            out.push_str(&format!("|{}\n", " --- |".repeat(cells.len())));
        }
    }
    out
}

pub fn import(store: &Store, home: &Home, root: &Path, scope: &str) -> Result<NotionReport> {
    let actor = Actor::new("adapter:notion".to_string());
    let mut report = NotionReport::default();
    // Descend past the export's wrapper directory ("Export-<uuid>/") so
    // it never becomes a tag on every note.
    let mut root = root.to_path_buf();
    loop {
        let entries: Vec<PathBuf> = std::fs::read_dir(&root)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                !p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .collect();
        match entries.as_slice() {
            [only] if only.is_dir() => root = only.clone(),
            _ => break,
        }
    }
    let root = root.as_path();
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    let csv_names: Vec<String> = files
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .collect();

    let mut since_sleep = 0usize;
    for path in &files {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let (title, page_id) = split_page_id(stem);
        let origin_ref = page_id.unwrap_or_else(|| path_ref(rel));
        let tags = tags_from(rel.parent().unwrap_or(Path::new("")));

        let outcome = match extension.as_str() {
            "md" => {
                let raw = std::fs::read_to_string(path)?;
                // Database rows carry Created:/Updated: property lines —
                // the real dates. Lift them out of the body into the
                // note's timestamps (plain pages have none: import time).
                let (raw, created, updated) = extract_timestamps(&raw);
                let timestamps = match (&created, &updated) {
                    (Some(c), Some(u)) => Some((c.as_str(), u.as_str())),
                    (Some(c), None) => Some((c.as_str(), c.as_str())),
                    (None, Some(u)) => Some((u.as_str(), u.as_str())),
                    (None, None) => None,
                };
                let (mut body, stats) = rewrite_media_links(
                    &raw,
                    path.parent().unwrap_or(root),
                    &origin_ref,
                    &home.notes_media_dir(),
                )?;
                report.media_stripped += stats.placeholders;
                report.images_copied += stats.images_copied;
                report.image_bytes += stats.image_bytes;
                // drop a first line that just repeats the title
                if let Some(first) = body.lines().next() {
                    if first.trim_start_matches('#').trim() == title {
                        body = body.split_once('\n').map(|x| x.1).unwrap_or("").to_string();
                    }
                }
                let body = body.trim();
                if title.is_empty() || body.is_empty() {
                    report.empty_skipped += 1;
                    continue;
                }
                let mut body = body.to_string();
                body.truncate(BODY_CAP);
                report.pages += 1;
                Some(store.import_note(
                    &actor,
                    home,
                    &title,
                    &body,
                    &tags,
                    scope,
                    "notion",
                    &origin_ref,
                    timestamps,
                )?)
            }
            "csv" => {
                // Notion emits "Db.csv" AND "Db_all.csv" — keep one
                if stem.ends_with("_all")
                    && csv_names
                        .iter()
                        .any(|n| n == &format!("{}.csv", stem.trim_end_matches("_all")))
                {
                    continue;
                }
                let raw = std::fs::read_to_string(path)?;
                let rows = hearthloom_adapters::linkedin::parse_csv(&raw);
                if rows.len() <= 1 {
                    report.empty_skipped += 1;
                    continue;
                }
                if rows.len() - 1 > CSV_TABLE_MAX_ROWS {
                    report
                        .big_csvs
                        .push(format!("{title} ({} rows)", rows.len() - 1));
                    continue;
                }
                let body = csv_to_table(&rows);
                report.tables += 1;
                Some(store.import_note(
                    &actor,
                    home,
                    &title,
                    &body,
                    &tags,
                    scope,
                    "notion",
                    &origin_ref,
                    None,
                )?)
            }
            _ => {
                // media that slipped past "no files" — never imported
                report.media_stripped += 1;
                continue;
            }
        };
        match outcome {
            Some(NoteImport::Created) => report.created += 1,
            Some(NoteImport::Updated) => report.updated += 1,
            Some(NoteImport::Skipped) => report.unchanged += 1,
            None => {}
        }
        since_sleep += 1;
        if since_sleep >= 25 {
            since_sleep = 0;
            std::thread::sleep(std::time::Duration::from_millis(40)); // unfair mutex
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_id_tags_and_media() {
        let (title, id) = split_page_id("My Page 21fc4a5a8df3411caa11aa22bb33cc44");
        assert_eq!(title, "My Page");
        assert_eq!(id.as_deref(), Some("21fc4a5a8df3411caa11aa22bb33cc44"));
        assert_eq!(split_page_id("Plain title").1, None);

        assert_eq!(
            tags_from(Path::new(
                "Work 21fc4a5a8df3411caa11aa22bb33cc44/Game Design"
            )),
            vec!["work", "game-design"]
        );

        // no files on disk → everything becomes a placeholder
        let tmp = tempfile::tempdir().unwrap();
        let (body, stats) = rewrite_media_links(
            "See ![shot](img/Screen%20Shot.png) and [deck](files/Deck.pdf) \
             but keep [site](https://example.com) and [page](Other%20Page.md).",
            tmp.path(),
            "ref1",
            &tmp.path().join("media"),
        )
        .unwrap();
        assert_eq!(stats.placeholders, 2);
        assert_eq!(stats.images_copied, 0);
        assert!(body.contains("[media: Screen Shot.png]"));
        assert!(body.contains("[media: Deck.pdf]"));
        assert!(body.contains("[site](https://example.com)"));
        assert!(body.contains("[page](Other%20Page.md)"));

        // the image EXISTS → copied into the substrate, link rewritten;
        // the pdf stays a placeholder (images are the only exception)
        std::fs::create_dir_all(tmp.path().join("img")).unwrap();
        std::fs::write(tmp.path().join("img/Screen Shot.png"), b"png-bytes").unwrap();
        let (body, stats) = rewrite_media_links(
            "![shot](img/Screen%20Shot.png) [deck](files/Deck.pdf)",
            tmp.path(),
            "ref1",
            &tmp.path().join("media"),
        )
        .unwrap();
        assert_eq!(stats.images_copied, 1);
        assert_eq!(stats.placeholders, 1);
        assert!(body.contains("![Screen Shot.png](/notes-media/ref1/Screen%20Shot.png)"));
        assert!(tmp.path().join("media/ref1/Screen Shot.png").is_file());

        // property-line timestamps lift out of the body
        let (body, created, updated) = extract_timestamps(
            "# T\n\nCreated: February 4, 2021 7:20 AM\nUpdated: December 31, 2022 12:05 PM\n\nreal text\n");
        assert_eq!(created.as_deref(), Some("2021-02-04T07:20:00Z"));
        assert_eq!(updated.as_deref(), Some("2022-12-31T12:05:00Z"));
        assert!(!body.contains("Created:"));
        assert!(body.contains("real text"));
        assert_eq!(
            parse_notion_datetime("June 1, 2015").as_deref(),
            Some("2015-06-01T12:00:00Z")
        );
    }

    #[test]
    fn test_import_end_to_end_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let export = tmp.path().join("export");
        std::fs::create_dir_all(export.join("Projects abcdefabcdefabcdefabcdefabcdef12")).unwrap();
        std::fs::write(
            export.join("Projects abcdefabcdefabcdefabcdefabcdef12/Roadmap 11112222333344445555666677778888.md"),
            "# Roadmap\n\nShip the thing. ![img](media/pic.png)\n",
        )
        .unwrap();
        std::fs::write(
            export.join("Small DB 99998888777766665555444433332222.csv"),
            "Name,Status\nAlpha,Done\nBeta,Open\n",
        )
        .unwrap();

        let home_dir = tmp.path().join("home");
        let home = Home::at(&home_dir);
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        let report = import(&store, &home, &export, "personal").unwrap();
        assert_eq!(report.pages, 1);
        assert_eq!(report.tables, 1);
        assert_eq!(report.created, 2);
        assert_eq!(report.media_stripped, 1);

        // re-import: everything unchanged, nothing duplicated
        let again = import(&store, &home, &export, "personal").unwrap();
        assert_eq!(again.created, 0);
        assert_eq!(again.updated, 0);
        assert_eq!(again.unchanged, 2);

        // edit the page at origin → update in place
        std::fs::write(
            export.join("Projects abcdefabcdefabcdefabcdefabcdef12/Roadmap 11112222333344445555666677778888.md"),
            "# Roadmap\n\nShip the OTHER thing.\n",
        )
        .unwrap();
        let third = import(&store, &home, &export, "personal").unwrap();
        assert_eq!(third.updated, 1);
        let notes = store.list_notes(Some("OTHER"), 10).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].origin, "notion");
        assert_eq!(notes[0].tags, vec!["projects"]);
    }
}
