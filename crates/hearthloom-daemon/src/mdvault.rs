//! Markdown-vault importer: a folder of .md files (Obsidian, Logseq,
//! Reflect Open — any plain markdown vault) → Hearthloom NOTES
//! (origin='markdown'), idempotent by vault-relative path.
//!
//! The vault is read IN PLACE and never moved or written — someone's
//! live Obsidian vault stays theirs; re-import upserts changed files
//! (the Notion origin_ref discipline, v26). One-way by design: the
//! daemon never writes back into the vault (two-way markdown sync is a
//! conflict-semantics project this deliberately is not).
//!
//! What we handle:
//!   - title = filename stem (the Obsidian convention); a first line
//!     that just repeats it is dropped
//!   - YAML frontmatter: `tags:` lifted into note tags, created/date/
//!     updated lifted into real timestamps; the block leaves the body
//!   - timestamps: frontmatter > daily-note filename (YYYY-MM-DD /
//!     YYYY_MM_DD) > file mtime — a vault carries real dates; use them
//!   - `[[wiki-links]]` survive verbatim — the notes indexer already
//!     builds note_links from them (v36), so the vault's link graph
//!     reassembles itself as its targets import
//!   - `![[image.png]]` embeds and relative markdown image links: the
//!     file is found (same dir, vault root, common attachment dirs) and
//!     copied into notes/media/ (the vault-guarded route); other media
//!     become `[media: name]` placeholders — the Notion rules
//!   - folder path → tags (top 2 segments, slugified), plus frontmatter
//!   - skipped: hidden dirs (.obsidian, .trash, .git), Logseq's config
//!     dir, Excalidraw drawing files (frontmatter-marked JSON, not prose)

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use hearthloom_core::store::NoteImport;
use hearthloom_core::{paths::Home, store::Store, Actor};

const BODY_CAP: usize = 96 * 1024;
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "heic", "bmp"];
/// Where vaults conventionally keep attachments (checked after the
/// note's own directory and the vault root).
const ATTACHMENT_DIRS: &[&str] = &["attachments", "Attachments", "assets", "files", "media"];

#[derive(Debug, Default, serde::Serialize)]
pub struct MdVaultReport {
    pub files: usize,
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub empty_skipped: usize,
    pub excalidraw_skipped: usize,
    pub media_stripped: usize,
    pub images_copied: usize,
    pub image_bytes: u64,
}

/// FNV-1a over the vault-relative path — the single-segment media ref
/// (the /notes-media/{ref}/{file} route can't carry path separators;
/// origin_ref itself stays the readable relative path).
fn media_ref(rel: &Path) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in rel.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("md{h:016x}")
}

/// Parsed YAML frontmatter block: only the fields we lift. Hand-rolled
/// over the common shapes — a vault's frontmatter is human-written and
/// a full YAML parser is a dependency this doesn't need.
#[derive(Debug, Default, PartialEq)]
struct Frontmatter {
    tags: Vec<String>,
    created: Option<String>,
    updated: Option<String>,
    excalidraw: bool,
}

fn slugify(raw: &str) -> String {
    let slug: String = raw
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    slug.chars().take(24).collect()
}

/// "2024-03-15", "2024-03-15 10:30", "2024-03-15T10:30:00", with
/// optional quotes — the shapes Obsidian/Logseq frontmatter carries.
fn parse_fm_date(raw: &str) -> Option<String> {
    let s = raw.trim().trim_matches(['"', '\'']).trim();
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let year: i32 = parts[0].parse().ok()?;
    let month: u32 = parts[1].parse().ok()?;
    let day: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !(1000..=9999).contains(&year) {
        return None;
    }
    let (hour, minute) = match time.split(':').collect::<Vec<_>>().as_slice() {
        [h, m, ..] => (
            h.parse::<u32>().ok()?.min(23),
            m[..2.min(m.len())].parse::<u32>().ok()?.min(59),
        ),
        _ => (12, 0),
    };
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00Z"
    ))
}

/// Split a leading `---` frontmatter block off the body. Returns
/// (frontmatter, body-without-block); a file without one gets defaults.
fn split_frontmatter(raw: &str) -> (Frontmatter, String) {
    let mut fm = Frontmatter::default();
    if !raw.starts_with("---") {
        return (fm, raw.to_string());
    }
    let mut lines = raw.lines();
    lines.next(); // the opening ---
    let mut consumed = 3 + 1; // "---\n"
    let mut in_tags_list = false;
    let mut closed = false;
    for line in lines {
        consumed += line.len() + 1;
        let trimmed = line.trim();
        if trimmed == "---" {
            closed = true;
            break;
        }
        if in_tags_list {
            if let Some(item) = trimmed.strip_prefix("- ") {
                let slug = slugify(item.trim_start_matches('#'));
                if slug.len() >= 2 && !fm.tags.contains(&slug) {
                    fm.tags.push(slug);
                }
                continue;
            }
            in_tags_list = false;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = value.trim();
        match key.as_str() {
            "tags" | "tag" => {
                if value.is_empty() {
                    in_tags_list = true; // list form on following lines
                } else {
                    for item in value.trim_matches(['[', ']']).split(',') {
                        let slug = slugify(item.trim().trim_start_matches('#'));
                        if slug.len() >= 2 && !fm.tags.contains(&slug) {
                            fm.tags.push(slug);
                        }
                    }
                }
            }
            "created" | "date" | "created_at" | "date created" if fm.created.is_none() => {
                fm.created = parse_fm_date(value);
            }
            "updated" | "modified" | "updated_at" | "date modified" if fm.updated.is_none() => {
                fm.updated = parse_fm_date(value);
            }
            "excalidraw-plugin" => fm.excalidraw = true,
            _ => {}
        }
    }
    if !closed {
        // an unterminated "---" is a horizontal rule, not frontmatter
        return (Frontmatter::default(), raw.to_string());
    }
    (
        fm,
        raw.get(consumed.min(raw.len())..).unwrap_or("").to_string(),
    )
}

/// A daily-note filename is a date: "2024-03-15" (Obsidian default) or
/// "2024_03_15" (Logseq journals).
fn date_from_stem(stem: &str) -> Option<String> {
    let normalized = stem.replace('_', "-");
    parse_fm_date(&normalized).filter(|_| normalized.len() == 10)
}

/// File mtime → RFC 3339 (the vault's own truth when frontmatter and
/// filename say nothing).
fn mtime_of(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = modified.into();
    Some(dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// Find an embedded/linked file: beside the note, at the vault root,
/// or in a conventional attachments dir.
fn resolve_attachment(name: &str, note_dir: &Path, root: &Path) -> Option<PathBuf> {
    let candidates = std::iter::once(note_dir.join(name))
        .chain(std::iter::once(root.join(name)))
        .chain(ATTACHMENT_DIRS.iter().map(|d| root.join(d).join(name)))
        .chain(ATTACHMENT_DIRS.iter().map(|d| note_dir.join(d).join(name)));
    candidates.into_iter().find(|p| p.is_file())
}

struct MediaOut {
    body: String,
    stripped: usize,
    images_copied: usize,
    image_bytes: u64,
}

/// Rewrite `![[embed.ext]]` (Obsidian) and relative markdown image
/// links: images found on disk copy into the substrate and point at the
/// guarded route; other media become `[media: name]`. `[[wiki-links]]`
/// (no extension / .md) and web links pass through untouched.
fn rewrite_media(
    body: &str,
    note_dir: &Path,
    root: &Path,
    media_key: &str,
    media_root: &Path,
) -> Result<MediaOut> {
    let mut out = String::with_capacity(body.len());
    let mut stripped = 0usize;
    let mut images_copied = 0usize;
    let mut image_bytes = 0u64;
    let mut copy_image = |source: &Path, filename: &str, out: &mut String| -> Result<()> {
        let dir = media_root.join(media_key);
        std::fs::create_dir_all(&dir)?;
        let bytes = std::fs::copy(source, dir.join(filename))?;
        images_copied += 1;
        image_bytes += bytes;
        let encoded = filename.replace(' ', "%20");
        out.push_str(&format!(
            "![{filename}](/notes-media/{media_key}/{encoded})"
        ));
        Ok(())
    };

    let mut rest = body;
    while !rest.is_empty() {
        // ![[embed]] — Obsidian's embed syntax
        if let Some(inner_start) = rest.strip_prefix("![[") {
            if let Some(end) = inner_start.find("]]") {
                let target = &inner_start[..end];
                let name = target.split(['|', '#']).next().unwrap_or(target).trim();
                // no dot = no extension (a bare note name, not media)
                let extension = name
                    .rsplit_once('.')
                    .map(|(_, e)| e.to_lowercase())
                    .unwrap_or_default();
                let filename = name.rsplit('/').next().unwrap_or(name).to_string();
                if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
                    match resolve_attachment(name, note_dir, root) {
                        Some(source) => copy_image(&source, &filename, &mut out)?,
                        None => {
                            out.push_str(&format!("[media: {filename}]"));
                            stripped += 1;
                        }
                    }
                } else if extension.is_empty() || extension == "md" {
                    // an embedded NOTE — keep it as a wiki-link so the
                    // link graph gets it
                    out.push_str(&format!("[[{name}]]"));
                } else {
                    out.push_str(&format!("[media: {filename}]"));
                    stripped += 1;
                }
                rest = &inner_start[end + 2..];
                continue;
            }
        }
        // ![alt](relative/img.png) — standard markdown image
        if let Some(after_bang) = rest.strip_prefix("![") {
            if let Some((alt_end, target_end)) = after_bang
                .find("](")
                .and_then(|a| after_bang[a + 2..].find(')').map(|t| (a, a + 2 + t)))
            {
                let target = &after_bang[alt_end + 2..target_end];
                let clean = target.split(['?', '#']).next().unwrap_or(target);
                let extension = clean.rsplit('.').next().unwrap_or("").to_lowercase();
                if !clean.starts_with("http") && IMAGE_EXTENSIONS.contains(&extension.as_str()) {
                    let decoded = clean.replace("%20", " ");
                    let filename = decoded.rsplit('/').next().unwrap_or(&decoded).to_string();
                    match resolve_attachment(&decoded, note_dir, root) {
                        Some(source) => copy_image(&source, &filename, &mut out)?,
                        None => {
                            out.push_str(&format!("[media: {filename}]"));
                            stripped += 1;
                        }
                    }
                    rest = &after_bang[target_end + 1..];
                    continue;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    Ok(MediaOut {
        body: out,
        stripped,
        images_copied,
        image_bytes,
    })
}

/// Folder path → up to 2 slug tags (the vault's own organization).
fn tags_from(rel_dir: &Path) -> Vec<String> {
    let mut tags = Vec::new();
    for component in rel_dir.components().take(2) {
        let slug = slugify(&component.as_os_str().to_string_lossy());
        if slug.len() >= 2 && !tags.contains(&slug) {
            tags.push(slug);
        }
    }
    tags
}

fn skip_dir(name: &str) -> bool {
    name.starts_with('.') || name == "logseq" || name == "node_modules"
}

pub fn import(store: &Store, home: &Home, root: &Path, scope: &str) -> Result<MdVaultReport> {
    if !root.is_dir() {
        return Err(anyhow!("no such directory: {}", root.display()));
    }
    let actor = Actor::new("adapter:markdown".to_string());
    let mut report = MdVaultReport::default();

    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                if !skip_dir(&name) {
                    stack.push(path);
                }
            } else if name.to_lowercase().ends_with(".md") && !name.starts_with('.') {
                files.push(path);
            }
        }
    }
    files.sort();

    let mut since_sleep = 0usize;
    for path in &files {
        let rel = path.strip_prefix(root).unwrap_or(path);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        // ".excalidraw.md" — a drawing, not prose (also caught by
        // frontmatter below for renamed ones)
        if stem.to_lowercase().ends_with(".excalidraw") {
            report.excalidraw_skipped += 1;
            continue;
        }
        let raw = match std::fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => continue, // non-UTF8 straggler: not a note
        };
        let (fm, body) = split_frontmatter(&raw);
        if fm.excalidraw {
            report.excalidraw_skipped += 1;
            continue;
        }
        let title = stem.trim().to_string();
        let origin_ref = rel.to_string_lossy().to_string();
        let mut tags = tags_from(rel.parent().unwrap_or(Path::new("")));
        for tag in &fm.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        // frontmatter > daily-note filename > file mtime
        let created = fm
            .created
            .or_else(|| date_from_stem(stem))
            .or_else(|| mtime_of(path));
        let updated = fm
            .updated
            .clone()
            .or_else(|| mtime_of(path))
            .or_else(|| created.clone());
        let timestamps = created.as_deref().zip(updated.as_deref());

        let media = rewrite_media(
            &body,
            path.parent().unwrap_or(root),
            root,
            &media_ref(rel),
            &home.notes_media_dir(),
        )?;
        report.media_stripped += media.stripped;
        report.images_copied += media.images_copied;
        report.image_bytes += media.image_bytes;
        let mut body = media.body;
        // drop a first line that just repeats the filename-title
        if let Some(first) = body.trim_start().lines().next() {
            if first.trim_start_matches('#').trim() == title {
                let trimmed = body.trim_start();
                body = trimmed
                    .split_once('\n')
                    .map(|x| x.1)
                    .unwrap_or("")
                    .to_string();
            }
        }
        let body = body.trim();
        if title.is_empty() || body.is_empty() {
            report.empty_skipped += 1;
            continue;
        }
        let mut body = body.to_string();
        body.truncate(BODY_CAP);
        report.files += 1;
        match store.import_note(
            &actor,
            home,
            &title,
            &body,
            &tags,
            scope,
            "markdown",
            &origin_ref,
            timestamps,
        )? {
            NoteImport::Created => report.created += 1,
            NoteImport::Updated => report.updated += 1,
            NoteImport::Skipped => report.unchanged += 1,
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
    fn test_frontmatter_tags_dates_and_excalidraw() {
        let (fm, body) = split_frontmatter(
            "---\ntags: [projects, #deep-work]\ncreated: 2023-05-04\nmodified: \"2024-01-02 09:15\"\n---\n# Real\n\ntext\n",
        );
        assert_eq!(fm.tags, vec!["projects", "deep-work"]);
        assert_eq!(fm.created.as_deref(), Some("2023-05-04T12:00:00Z"));
        assert_eq!(fm.updated.as_deref(), Some("2024-01-02T09:15:00Z"));
        assert!(body.starts_with("# Real"));

        // list-form tags
        let (fm, _) = split_frontmatter("---\ntags:\n  - alpha\n  - beta gamma\n---\nx");
        assert_eq!(fm.tags, vec!["alpha", "beta-gamma"]);

        // excalidraw marker
        let (fm, _) = split_frontmatter("---\nexcalidraw-plugin: parsed\n---\n{json}");
        assert!(fm.excalidraw);

        // an unterminated --- is a horizontal rule, not frontmatter
        let (fm, body) = split_frontmatter("---\nnot: frontmatter without closing");
        assert!(fm.tags.is_empty());
        assert!(body.starts_with("---"));

        // daily-note stems
        assert_eq!(
            date_from_stem("2024-03-15").as_deref(),
            Some("2024-03-15T12:00:00Z")
        );
        assert_eq!(
            date_from_stem("2024_03_15").as_deref(),
            Some("2024-03-15T12:00:00Z")
        );
        assert_eq!(date_from_stem("Meeting Notes"), None);
        assert_eq!(date_from_stem("2024-03"), None);
    }

    #[test]
    fn test_media_rewrite_embeds_and_wiki_links_survive() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("attachments")).unwrap();
        std::fs::write(root.join("attachments/pic.png"), b"png").unwrap();

        let out = rewrite_media(
            "See [[Other Note]] and ![[pic.png]] and ![[Embedded Page]] \
             and ![shot](missing/gone.png) and [site](https://x.com)",
            root,
            root,
            "mdabc",
            &root.join("media"),
        )
        .unwrap();
        // the found image copies; the missing one strips; the embedded
        // page becomes a wiki-link; the web link is untouched
        assert_eq!(out.images_copied, 1);
        assert_eq!(out.stripped, 1);
        assert!(out.body.contains("![pic.png](/notes-media/mdabc/pic.png)"));
        assert!(out.body.contains("[[Other Note]]"));
        assert!(out.body.contains("[[Embedded Page]]"));
        assert!(out.body.contains("[media: gone.png]"));
        assert!(out.body.contains("[site](https://x.com)"));
        assert!(root.join("media/mdabc/pic.png").is_file());
    }

    #[test]
    fn test_import_vault_idempotent_and_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        std::fs::create_dir_all(vault.join("Projects")).unwrap();
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        std::fs::write(vault.join(".obsidian/app.json"), "{}").unwrap();
        std::fs::write(
            vault.join("Projects/Hearthloom.md"),
            "---\ntags: [second-brain]\ncreated: 2024-06-01\n---\n# Hearthloom\n\nLinks to [[Design Notes]].\n",
        )
        .unwrap();
        std::fs::write(vault.join("Design Notes.md"), "The design.\n").unwrap();
        std::fs::write(vault.join("Empty.md"), "\n").unwrap();
        std::fs::write(
            vault.join("Drawing.excalidraw.md"),
            "---\nexcalidraw-plugin: parsed\n---\n{}",
        )
        .unwrap();

        let home_dir = tmp.path().join("home");
        let home = Home::at(&home_dir);
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        let report = import(&store, &home, &vault, "personal").unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.created, 2);
        assert_eq!(report.empty_skipped, 1);
        assert_eq!(report.excalidraw_skipped, 1);

        // idempotent re-import
        let again = import(&store, &home, &vault, "personal").unwrap();
        assert_eq!(again.created, 0);
        assert_eq!(again.unchanged, 2);

        // the vault itself was never written
        assert!(vault.join("Projects/Hearthloom.md").is_file());

        // edit at origin → update in place, no duplicate
        std::fs::write(vault.join("Design Notes.md"), "The REVISED design.\n").unwrap();
        let third = import(&store, &home, &vault, "personal").unwrap();
        assert_eq!(third.updated, 1);
        let notes = store.list_notes(Some("REVISED"), 10).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].origin, "markdown");

        // the wiki-link graph reassembled: Hearthloom → Design Notes
        let all = store.list_notes(None, 10).unwrap();
        let linker = all.iter().find(|n| n.title == "Hearthloom").unwrap();
        let links = store.note_links(&linker.id).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "Design Notes");
        // tags: folder + frontmatter
        assert!(linker.tags.contains(&"projects".to_string()));
        assert!(linker.tags.contains(&"second-brain".to_string()));
    }
}
