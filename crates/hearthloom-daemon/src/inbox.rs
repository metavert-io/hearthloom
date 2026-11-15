//! The inbox drop folder: files placed in `~/.hearthloom/inbox/` are
//! ingested automatically — the zero-ceremony path for export archives
//! (Google Takeout zips, conversations.json, .mbox, .ics). Processed
//! files move to `inbox/processed/`; failures to `inbox/failed/` with a
//! `.error.txt` note. Nothing is ever deleted.
//!
//! mbox files stream through in batches, so a multi-GB Takeout never
//! has to fit in memory.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use hearthloom_core::{IngestBatch, Store};

use crate::AppState;

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const MBOX_BATCH: usize = 500;
/// Auto-ingesting an mbox bigger than this is refused: a full Gmail
/// Takeout is mostly spam/promotions and belongs to the *filtered*
/// pipeline (`hearthloom ingest mbox --include-labels …`), not an
/// unfiltered inbox drop.
const MBOX_AUTO_MAX_BYTES: u64 = 250 * 1024 * 1024;
/// A file must be size-stable for one poll before we touch it (large
/// copies/downloads land incrementally).
const SETTLE: Duration = Duration::from_secs(20);

pub async fn run_loop(state: AppState) {
    loop {
        if let Err(err) = scan_once(&state).await {
            tracing::warn!("inbox scan failed: {err}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn scan_once(state: &AppState) -> Result<()> {
    let inbox = state.home.inbox_dir();
    let entries = match std::fs::read_dir(&inbox) {
        Ok(entries) => entries,
        Err(_) => return Ok(()), // inbox not created yet
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        // Wait for the file to stop growing before ingesting.
        let size_before = entry.metadata().map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(SETTLE).await;
        let size_after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size_before != size_after {
            tracing::info!("inbox: {name} still growing, will retry");
            continue;
        }

        tracing::info!("inbox: processing {name}");
        let state_clone = state.clone();
        let path_clone = path.clone();
        let result = tokio::task::spawn_blocking(move || {
            process_file(
                &state_clone.store,
                &state_clone.home,
                &state_clone.running,
                &path_clone,
            )
        })
        .await?;

        match result {
            Ok(summary) => {
                tracing::info!("inbox: {name}: {summary}");
                move_to(&path, &inbox.join("processed"))?;
            }
            Err(err) => {
                tracing::warn!("inbox: {name} failed: {err:#}");
                let dest = inbox.join("failed");
                move_to(&path, &dest)?;
                let _ =
                    std::fs::write(dest.join(format!("{name}.error.txt")), format!("{err:#}\n"));
            }
        }
    }
    Ok(())
}

fn move_to(path: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("bad path"))?
        .to_string_lossy()
        .into_owned();
    let mut dest = dir.join(&name);
    // Never clobber an earlier drop of the same name.
    let mut n = 1;
    while dest.exists() {
        dest = dir.join(format!("{n}-{name}"));
        n += 1;
    }
    std::fs::rename(path, &dest).with_context(|| format!("moving to {}", dest.display()))?;
    Ok(())
}

/// Ingest one dropped file (blocking context). Returns a human summary.
/// `running`: the single-flight registry — an export is staged only while
/// no import reads its folder (`downloads::install_archive_dir`).
pub(crate) fn process_file(
    store: &Store,
    home: &hearthloom_core::Home,
    running: &crate::Running,
    path: &Path,
) -> Result<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // TCC-protected databases can be Finder-dragged into the inbox
    // (Finder always has access) when the daemon itself doesn't. Every day
    // of it is read — today's too; a later, fuller copy of a day lands as a
    // revision of it.
    if name == "chat.db" {
        return ingest_drafts(
            store,
            home,
            "adapter:imessage",
            hearthloom_adapters::imessage::ingest(path, None)?,
            &[],
        );
    }
    if name == "photos.sqlite" {
        return ingest_drafts(
            store,
            home,
            "adapter:photos",
            hearthloom_adapters::photos::ingest(path, None)?,
            &[],
        );
    }
    match ext.as_str() {
        "mbox" => ingest_mbox(store, home, path),
        "ics" => ingest_drafts(
            store,
            home,
            "adapter:ics",
            hearthloom_adapters::ics::ingest(path, None)?,
            &[],
        ),
        "json" => {
            let read = hearthloom_adapters::chatlog::read(path, None)?;
            ingest_drafts(store, home, "adapter:chatlog", read.drafts, &read.flagged)
        }
        "zip" | "tgz" | "gz" => {
            let dir = extract_archive(path, unpack_budget(home))?;
            ingest_tree(store, home, running, dir.path())
        }
        other => Err(anyhow!(
            "don't know how to ingest .{other} (supported: mbox, ics, json, zip, tgz)"
        )),
    }
}

/// Room an unpacked archive must leave on the disk: the record's own
/// writes need it.
const UNPACK_RESERVE: u64 = 1 << 30;

/// Bytes an archive may unpack to: what the extraction folder's disk and
/// the data home's disk can both spare, keeping `UNPACK_RESERVE` free on
/// each (unlimited only where neither can be measured).
pub(crate) fn unpack_budget(home: &hearthloom_core::Home) -> u64 {
    [std::env::temp_dir(), home.root().to_path_buf()]
        .iter()
        .filter_map(|dir| crate::mbox_text::free_bytes(&dir.join("probe")))
        .map(|free| free.saturating_sub(UNPACK_RESERVE))
        .min()
        .unwrap_or(u64::MAX)
}

/// Extract an archive to a temp dir, in-process on every platform (a Linux
/// node has no `ditto`): a zip, or a gzipped tar. Both write only plain
/// files and folders inside the folder, and no more than `budget` bytes.
pub(crate) fn extract_archive(path: &Path, budget: u64) -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir().context("creating extraction dir")?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "zip" {
        extract_zip(path, dir.path(), budget)?;
    } else {
        extract_tgz(path, dir.path(), budget)?;
    }
    Ok(dir)
}

/// `name` as a path inside the extraction folder, when it stays inside: no
/// root, no `..`, and not under the Finder's `__MACOSX` resource forks.
fn enclosed(name: &Path) -> Option<std::path::PathBuf> {
    let mut out = std::path::PathBuf::new();
    for part in name.components() {
        match part {
            std::path::Component::Normal(p) => out.push(p),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    let resource_forks = out
        .components()
        .next()
        .is_some_and(|first| first.as_os_str() == "__MACOSX");
    (!out.as_os_str().is_empty() && !resource_forks).then_some(out)
}

/// "N MB" for messages.
fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// Unpack a gzipped tar into `dest`, under the zip's rules: only plain
/// files and folders, each inside `dest` — a link (symbolic or hard), a
/// pipe, a device and anything else is skipped. A tar states no total and
/// gzip doesn't say how far it expands, so the bytes are counted as they
/// are written: a member whose stated size would pass `budget` stops the
/// unpacking before it is written, and no member writes more than it
/// states.
fn extract_tgz(path: &Path, dest: &Path, budget: u64) -> Result<()> {
    use std::io::Read;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut written = 0u64;
    for entry in archive.entries().context("reading the archive")? {
        let mut entry = entry.context("reading the archive")?;
        let kind = entry.header().entry_type();
        let Some(rel) = entry.path().ok().and_then(|name| enclosed(&name)) else {
            continue;
        };
        let out = dest.join(&rel);
        if kind.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let size = entry.header().size().context("reading the archive")?;
        if written.saturating_add(size) > budget {
            return Err(anyhow!(
                "the archive unpacks to more than the {} MB the disk can spare ({} at {} MB)",
                mb(budget),
                rel.display(),
                mb(size)
            ));
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut writer =
            std::fs::File::create(&out).with_context(|| format!("writing {}", rel.display()))?;
        let copied = std::io::copy(&mut (&mut entry).take(size.saturating_add(1)), &mut writer)
            .with_context(|| format!("unpacking {}", rel.display()))?;
        if copied > size {
            return Err(anyhow!(
                "{} unpacks to more than the archive states",
                rel.display()
            ));
        }
        written += copied;
    }
    Ok(())
}

/// Unpack a zip into `dest`. Only plain files and folders are written, each
/// inside `dest`: an entry whose name would land elsewhere, a link, and the
/// Finder's `__MACOSX` resource-fork folder are skipped. The sizes the zip
/// states must fit in `budget`, and no entry writes more than its stated
/// size.
fn extract_zip(path: &Path, dest: &Path, budget: u64) -> Result<()> {
    use std::io::Read;
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("reading the zip")?;
    let stated: u64 = (0..archive.len())
        .filter_map(|i| archive.by_index_raw(i).ok().map(|entry| entry.size()))
        .fold(0, u64::saturating_add);
    if stated > budget {
        return Err(anyhow!(
            "the zip unpacks to {} MB and the disk can spare {} MB",
            mb(stated),
            mb(budget)
        ));
    }
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let resource_forks = rel
            .components()
            .next()
            .is_some_and(|first| first.as_os_str() == "__MACOSX");
        if resource_forks || entry.is_symlink() {
            continue;
        }
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let size = entry.size();
        let mut writer =
            std::fs::File::create(&out).with_context(|| format!("writing {}", rel.display()))?;
        let written = std::io::copy(&mut (&mut entry).take(size.saturating_add(1)), &mut writer)
            .with_context(|| format!("unpacking {}", rel.display()))?;
        if written > size {
            return Err(anyhow!(
                "{} unpacks to more than the zip states",
                rel.display()
            ));
        }
    }
    Ok(())
}

/// Walk an extracted archive. A folder that is a recognizable export
/// (ChatGPT, Facebook, LinkedIn, Notion, Apple Health — `detect_dir`), at
/// whatever depth the archive wraps it, is STAGED whole into
/// `archives/<key>`, where its run-now adapter reads it with everything
/// beside its conversation files (images, the owner's `user.json`); the
/// walk doesn't go into it. Loose `.mbox`, `.ics` and `conversations.json`
/// files elsewhere are ingested. Links are not followed. The archive fails
/// when nothing in it could be staged or ingested.
fn ingest_tree(
    store: &Store,
    home: &hearthloom_core::Home,
    running: &crate::Running,
    root: &Path,
) -> Result<String> {
    let mut summaries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut found = 0usize;
    let mut done = 0usize;
    while let Some(dir) = stack.pop() {
        if let Some(key) = crate::downloads::detect_dir(&dir) {
            found += 1;
            match crate::downloads::install_archive_dir(home, running, &dir, key, false) {
                Ok(dest) => {
                    done += 1;
                    summaries.push(format!(
                        "staged the {key} export at {} — press run now on its Integrations card",
                        dest.display()
                    ));
                }
                Err(err) => summaries.push(format!("{key} export: FAILED: {err:#}")),
            }
            continue;
        }
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            let ingestable =
                name.ends_with(".mbox") || name.ends_with(".ics") || name == "conversations.json";
            if !ingestable {
                continue;
            }
            found += 1;
            match process_file(store, home, running, &path) {
                Ok(summary) => {
                    done += 1;
                    summaries.push(format!(
                        "{}: {summary}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                Err(err) => summaries.push(format!(
                    "{}: FAILED: {err}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
    }
    if found == 0 {
        return Err(anyhow!(
            "archive contained no ingestable files (.mbox, .ics, conversations.json) \
             or recognizable exports"
        ));
    }
    if done == 0 {
        return Err(anyhow!(summaries.join("; ")));
    }
    Ok(summaries.join("; "))
}

fn ingest_mbox(store: &Store, home: &hearthloom_core::Home, path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)?.len();
    if size > MBOX_AUTO_MAX_BYTES {
        return Err(anyhow!(
            "mbox is {}MB (limit {}MB for unfiltered auto-ingest) — a Takeout-scale \
             archive is mostly bulk mail; run it through the filtered pipeline instead: \
             hearthloom ingest mbox <file> --include-labels 'Sent,Category Personal,Starred'",
            size / (1024 * 1024),
            MBOX_AUTO_MAX_BYTES / (1024 * 1024)
        ));
    }
    let mut new = 0usize;
    let mut existing = 0usize;
    let total = hearthloom_adapters::mbox::ingest_batched(path, None, MBOX_BATCH, |batch| {
        let report = store.apply_batch(
            home,
            &IngestBatch {
                actor: "adapter:mbox".into(),
                session_id: None,
                episodes: batch,
            },
        )?;
        new += report.episodes_new;
        existing += report.episodes_existing;
        Ok(())
    })?;
    Ok(format!(
        "{total} messages parsed, {new} new episodes ({existing} already present)"
    ))
}

/// `flagged`: records the file reads as private without a draft of them
/// (`Store::label_private_on_reimport`).
fn ingest_drafts(
    store: &Store,
    home: &hearthloom_core::Home,
    actor: &str,
    drafts: Vec<hearthloom_core::EpisodeDraft>,
    flagged: &[(String, String)],
) -> Result<String> {
    let count = drafts.len();
    // what the export reads as private, a copy stored before becomes too
    let private: Vec<hearthloom_core::EpisodeDraft> = drafts
        .iter()
        .filter(|d| d.scope.as_deref() == Some("private"))
        .cloned()
        .collect();
    // a day stored before lands again only when fuller
    let drafts = crate::local_import::prepare(store, home, drafts)?;
    let mut new = 0usize;
    let mut existing = count - drafts.len();
    for chunk in drafts.chunks(500) {
        let report = store.apply_batch(
            home,
            &IngestBatch {
                actor: actor.into(),
                session_id: None,
                episodes: chunk.to_vec(),
            },
        )?;
        new += report.episodes_new;
        existing += report.episodes_existing;
    }
    let relabeled =
        store.label_private_on_reimport(&hearthloom_core::Actor::new(actor), &private, flagged)?;
    Ok(format!(
        "{count} items parsed, {new} new episodes ({existing} already present){}",
        if relabeled > 0 {
            format!(", {relabeled} stored before now private")
        } else {
            String::new()
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zip with `entries` (name, contents; a name ending in `/` is a
    /// folder, contents starting `->` a link to that target).
    fn write_zip(path: &Path, entries: &[(&str, &str)]) {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, contents) in entries {
            if name.ends_with('/') {
                zip.add_directory(*name, options).unwrap();
            } else if let Some(target) = contents.strip_prefix("->") {
                zip.add_symlink(*name, target, options).unwrap();
            } else {
                zip.start_file(*name, options).unwrap();
                zip.write_all(contents.as_bytes()).unwrap();
            }
        }
        zip.finish().unwrap();
    }

    /// Zips unpack in-process on every platform, writing only plain files
    /// and folders inside the extraction folder.
    #[test]
    fn test_a_zip_unpacks_only_files_inside_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("export.zip");
        write_zip(
            &zip_path,
            &[
                ("export/", ""),
                ("export/calendar.ics", "BEGIN:VCALENDAR"),
                ("export/deep/notes.txt", "kept"),
                ("../hearthloom-unzip-outside.txt", "not written"),
                ("/hearthloom-unzip-absolute.txt", "inside, if anywhere"),
                ("export/link", "->/etc/hosts"),
                ("__MACOSX/export/._calendar.ics", "resource fork"),
            ],
        );
        let dir = extract_archive(&zip_path, u64::MAX).unwrap();
        let root = dir.path();
        assert_eq!(
            std::fs::read_to_string(root.join("export/calendar.ics")).unwrap(),
            "BEGIN:VCALENDAR"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("export/deep/notes.txt")).unwrap(),
            "kept"
        );
        assert!(!root
            .parent()
            .unwrap()
            .join("hearthloom-unzip-outside.txt")
            .exists());
        assert!(!Path::new("/hearthloom-unzip-absolute.txt").exists());
        assert!(std::fs::symlink_metadata(root.join("export/link")).is_err());
        assert!(!root.join("__MACOSX").exists());
        // an absolute name may be kept, but only inside the folder
        let top: Vec<String> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            top.iter()
                .all(|name| name == "export" || name == "hearthloom-unzip-absolute.txt"),
            "{top:?}"
        );
    }

    /// One entry of a test tgz.
    enum Member<'a> {
        File(&'a str, &'a [u8]),
        Zeros(&'a str, usize),
        Link(&'a str, &'a str),
        HardLink(&'a str, &'a str),
        Fifo(&'a str),
        /// a name written as it is, `..` and all
        RawName(&'a str, &'a [u8]),
    }

    fn write_tgz(path: &Path, members: &[Member]) {
        use std::io::Read;
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        for member in members {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            match member {
                Member::File(name, data) => {
                    header.set_size(data.len() as u64);
                    header.set_cksum();
                    tar.append_data(&mut header, name, *data).unwrap();
                }
                Member::Zeros(name, n) => {
                    header.set_size(*n as u64);
                    header.set_cksum();
                    tar.append_data(&mut header, name, std::io::repeat(0).take(*n as u64))
                        .unwrap();
                }
                Member::Link(name, target) | Member::HardLink(name, target) => {
                    header.set_entry_type(if matches!(member, Member::Link(..)) {
                        tar::EntryType::Symlink
                    } else {
                        tar::EntryType::Link
                    });
                    header.set_size(0);
                    tar.append_link(&mut header, name, target).unwrap();
                }
                Member::Fifo(name) => {
                    header.set_entry_type(tar::EntryType::Fifo);
                    header.set_size(0);
                    header.set_cksum();
                    tar.append_data(&mut header, name, std::io::empty())
                        .unwrap();
                }
                Member::RawName(name, data) => {
                    let field = &mut header.as_old_mut().name;
                    field[..name.len()].copy_from_slice(name.as_bytes());
                    header.set_size(data.len() as u64);
                    header.set_cksum();
                    tar.append(&header, *data).unwrap();
                }
            }
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    /// A tgz unpacks like a zip: only plain files and folders, inside the
    /// folder — so a recognized export stages without a link to follow or
    /// a pipe to block on, and its importer reads only what is there.
    #[test]
    fn test_a_tgz_unpacks_only_files_inside_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let chunk = serde_json::json!([{
            "conversation_id": "c1", "title": "Trip", "create_time": 1720992777.0,
            "mapping": {"n1": {"message": {"author": {"role": "user"},
                "create_time": 1720992778.0, "content": {"parts": ["Lisbon in May"]}}}}
        }])
        .to_string();
        let drop = tmp.path().join("chatgpt.tgz");
        write_tgz(
            &drop,
            &[
                Member::File("chatgpt/conversations-000.json", chunk.as_bytes()),
                Member::Fifo("chatgpt/user.json"),
                Member::Link("chatgpt/conversations-001.json", "/etc/hosts"),
                Member::HardLink("chatgpt/conversations-002.json", "/etc/hosts"),
                Member::RawName("../hearthloom-untar-outside.txt", b"not written"),
            ],
        );
        let summary = process_file(&store, &home, &Default::default(), &drop).unwrap();
        assert!(summary.contains("staged the openai export"), "{summary}");
        let staged = home.root().join("archives/openai");
        crate::downloads::only_plain_files(&staged).unwrap();
        let names: Vec<String> = std::fs::read_dir(&staged)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["conversations-000.json"]);
        assert!(!std::env::temp_dir()
            .join("hearthloom-untar-outside.txt")
            .exists());
        let drafts = hearthloom_adapters::openai::ingest(&staged, None, None).unwrap();
        assert_eq!(drafts.len(), 1);
    }

    /// A tgz that would unpack past what the disk can spare stops before
    /// the member that passes the limit is written.
    #[test]
    fn test_a_tgz_never_unpacks_past_its_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let drop = tmp.path().join("big.tgz");
        let mib = 1024 * 1024;
        write_tgz(
            &drop,
            &[
                Member::File("export/small.txt", b"kept"),
                Member::Zeros("export/huge.bin", 8 * mib),
            ],
        );
        assert!(std::fs::metadata(&drop).unwrap().len() < mib as u64);
        let dest = tempfile::tempdir().unwrap();
        let err = extract_tgz(&drop, dest.path(), mib as u64)
            .unwrap_err()
            .to_string();
        assert!(err.contains("can spare"), "{err}");
        assert!(dest.path().join("export/small.txt").is_file());
        assert!(!dest.path().join("export/huge.bin").exists());
        // with room, all of it
        let dest = tempfile::tempdir().unwrap();
        extract_tgz(&drop, dest.path(), 16 * mib as u64).unwrap();
        assert_eq!(
            std::fs::metadata(dest.path().join("export/huge.bin"))
                .unwrap()
                .len(),
            8 * mib as u64
        );
    }

    /// A zipped ChatGPT export in chunks (no single conversations.json) is
    /// staged for its adapter, which then reads every chunk.
    #[test]
    fn test_a_chunked_chatgpt_zip_is_staged_for_its_adapter() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let conversation = |id: &str, text: &str| {
            serde_json::json!([{
                "conversation_id": id, "title": id, "create_time": 1720992777.0,
                "mapping": {"n1": {"message": {"author": {"role": "user"},
                    "create_time": 1720992778.0, "content": {"parts": [text]}}}}
            }])
            .to_string()
        };
        let first = conversation("c1", "first chunk");
        let second = conversation("c2", "second chunk");
        let drop = tmp.path().join("chatgpt-export.zip");
        write_zip(
            &drop,
            &[
                ("chatgpt/user.json", r#"{"email":"me@example.com"}"#),
                ("chatgpt/conversations-000.json", &first),
                ("chatgpt/conversations-001.json", &second),
            ],
        );
        let summary = process_file(&store, &home, &Default::default(), &drop).unwrap();
        assert!(summary.contains("staged the openai export"), "{summary}");
        let staged = home.root().join("archives/openai");
        let drafts = hearthloom_adapters::openai::ingest(&staged, None, None).unwrap();
        let mut ids: Vec<&str> = drafts.iter().map(|d| d.source_ref.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["c1", "c2"]);
    }

    /// An export wrapped in more than one folder is still staged whole for
    /// its own adapter — images and the owner's `user.json` with it — and
    /// none of its conversation files is read by the single-file importer.
    #[test]
    fn test_a_doubly_wrapped_chatgpt_export_is_staged_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let chunk = serde_json::json!([{
            "conversation_id": "c1", "title": "Cats", "create_time": 1720992777.0,
            "mapping": {"n1": {"message": {"author": {"role": "user"},
                "create_time": 1720992778.0,
                "content": {"content_type": "multimodal_text", "parts": [
                    "Look at this",
                    {"content_type": "image_asset_pointer",
                     "asset_pointer": "file-service://file-abc"}
                ]}}}}
        }])
        .to_string();
        let drop = tmp.path().join("backup.zip");
        write_zip(
            &drop,
            &[
                ("backup/chatgpt/user.json", r#"{"email":"Me@Example.com"}"#),
                (
                    "backup/chatgpt/conversation_asset_file_names.json",
                    r#"{"file-abc.dat":"cat.jpeg"}"#,
                ),
                ("backup/chatgpt/file-abc.dat", "pretend-jpeg"),
                ("backup/chatgpt/conversations-000.json", &chunk),
            ],
        );
        let summary = process_file(&store, &home, &Default::default(), &drop).unwrap();
        assert!(summary.contains("staged the openai export"), "{summary}");
        let (_, rows) = store
            .read_query(
                "SELECT COUNT(*) FROM episodes /* all-episodes: none at all */",
                1,
            )
            .unwrap();
        assert_eq!(rows[0][0], 0, "nothing ingested by another importer");

        let staged = home.root().join("archives/openai");
        let media = home.raw_dir().join("openai/media");
        let drafts = hearthloom_adapters::openai::ingest(&staged, Some(&media), None).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].source, "openai");
        assert!(
            drafts[0].body.contains("[image: cat.jpeg]"),
            "{}",
            drafts[0].body
        );
        assert!(media.join("c1/cat.jpeg").is_file());
        assert!(
            drafts[0]
                .entities
                .iter()
                .any(|e| e.attributes.iter().any(|(_, v)| v == "me@example.com")),
            "the owner from user.json"
        );

        // staging again over the same export fails the drop, not just a line
        let err = process_file(&store, &home, &Default::default(), &drop)
            .unwrap_err()
            .to_string();
        assert!(err.contains("already exists"), "{err}");
    }

    /// A conversation stored outside the vault and read again as "do not
    /// remember" goes into the vault: its label changes, its content
    /// doesn't — the copy under the export's other source too.
    #[test]
    fn test_a_reimport_flagged_do_not_remember_vaults_the_stored_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let export = |flagged: bool| {
            serde_json::json!([{
                "conversation_id": "c9", "title": "Clinic visit",
                "create_time": 1720992777.0, "is_do_not_remember": flagged,
                "mapping": {"n1": {"message": {"author": {"role": "user"},
                    "create_time": 1720992778.0, "content": {"parts": ["about the results"]}}}}
            }])
            .to_string()
        };
        let drop_dir = tmp.path().join("drop");
        std::fs::create_dir_all(&drop_dir).unwrap();
        let file = drop_dir.join("conversations.json");
        let scope_and_hash = |source: &str| -> (String, String) {
            let (_, rows) = store
                .read_query(
                    &format!(
                        "SELECT scope, content_hash FROM episodes /* all-episodes: a label check */
                         WHERE source = '{source}' AND source_ref = 'c9'"
                    ),
                    1,
                )
                .unwrap();
            (
                rows[0][0].as_str().unwrap().to_string(),
                rows[0][1].as_str().unwrap().to_string(),
            )
        };

        // stored outside the vault (unflagged, or by a reader that didn't
        // know the flag); the full export holds the same conversation
        std::fs::write(&file, export(false)).unwrap();
        process_file(&store, &home, &Default::default(), &file).unwrap();
        let mut openai_copy = hearthloom_adapters::chatlog::ingest(&file, None).unwrap()[0].clone();
        openai_copy.source = "openai".into();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:openai".into(),
                    session_id: None,
                    episodes: vec![openai_copy],
                },
            )
            .unwrap();
        let (scope, hash) = scope_and_hash("chatlog-chatgpt");
        assert_ne!(scope, "private");
        store.vault_unlock(15); // the read below sees private rows too

        std::fs::write(&file, export(true)).unwrap();
        let summary = process_file(&store, &home, &Default::default(), &file).unwrap();
        assert!(summary.contains("2 stored before now private"), "{summary}");
        assert_eq!(scope_and_hash("chatlog-chatgpt"), ("private".into(), hash));
        assert_eq!(scope_and_hash("openai").0, "private");

        // an import that reads nothing as private changes no label
        store.vault_lock();
        let other = store
            .label_private_on_reimport(
                &hearthloom_core::Actor::new("adapter:chatlog"),
                &hearthloom_adapters::chatlog::ingest(&drop_dir.join("conversations.json"), None)
                    .unwrap()
                    .into_iter()
                    .map(|mut d| {
                        d.scope = None;
                        d
                    })
                    .collect::<Vec<_>>(),
                &[],
            )
            .unwrap();
        assert_eq!(other, 0);
    }

    /// A conversation flagged "do not remember" whose kept branch holds no
    /// turns gets no draft — and a copy an earlier reader stored of it
    /// (from another branch) still goes into the vault, under both of the
    /// export's sources, while the file's other conversations import.
    #[test]
    fn test_a_flagged_conversation_without_turns_still_vaults_the_stored_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        // what an earlier reader stored: every node's turns, outside the vault
        let stored = |source: &str| hearthloom_core::EpisodeDraft {
            source: source.into(),
            source_ref: "c9".into(),
            kind: "conversation".into(),
            title: "Clinic visit".into(),
            body: "conversation: Clinic visit\nprovider: chatgpt\n\nyou: about the results\n\n"
                .into(),
            event_time: hearthloom_core::EventTime::unknown(),
            trust: "self".into(),
            scope: None,
            entities: vec![],
            edges: vec![],
        };
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:chatlog".into(),
                    session_id: None,
                    episodes: vec![stored("chatlog-chatgpt"), stored("openai")],
                },
            )
            .unwrap();
        let scope_of = |source: &str, id: &str| -> Option<String> {
            store.vault_unlock(15);
            let (_, rows) = store
                .read_query(
                    &format!(
                        "SELECT scope FROM episodes /* all-episodes: a label check */
                         WHERE source = '{source}' AND source_ref = '{id}'"
                    ),
                    1,
                )
                .unwrap();
            store.vault_lock();
            rows.first().map(|r| r[0].as_str().unwrap().to_string())
        };
        assert!(scope_of("chatlog-chatgpt", "c9").is_some_and(|s| s != "private"));

        // the export now flags it, and the branch it ended on is only the
        // system's turn; another conversation sits beside it
        let export = serde_json::json!([
            {
                "conversation_id": "c9", "title": "Clinic visit",
                "create_time": 1720992777.0, "is_do_not_remember": true,
                "current_node": "sys",
                "mapping": {
                    "sys": {"message": {"author": {"role": "system"},
                        "content": {"parts": ["You are a helpful assistant."]}}},
                    "n1": {"parent": "sys", "message": {"author": {"role": "user"},
                        "content": {"parts": ["about the results"]}}}
                }
            },
            {
                "conversation_id": "c10", "title": "Garden plans",
                "create_time": 1720992800.0,
                "mapping": {"m1": {"message": {"author": {"role": "user"},
                    "content": {"parts": ["what to plant in spring"]}}}}
            }
        ]);
        let drop_dir = tmp.path().join("drop");
        std::fs::create_dir_all(&drop_dir).unwrap();
        let file = drop_dir.join("conversations.json");
        std::fs::write(&file, export.to_string()).unwrap();
        let summary = process_file(&store, &home, &Default::default(), &file).unwrap();
        assert!(summary.contains("1 new episodes"), "{summary}");
        assert!(summary.contains("2 stored before now private"), "{summary}");
        assert_eq!(
            scope_of("chatlog-chatgpt", "c9").as_deref(),
            Some("private")
        );
        assert_eq!(scope_of("openai", "c9").as_deref(), Some("private"));
        assert!(scope_of("chatlog-chatgpt", "c10").is_some_and(|s| s != "private"));
    }

    #[test]
    fn test_oversized_mbox_refused_before_parsing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let path = tmp.path().join("takeout.mbox");
        // Sparse file: reports the oversized length without writing it.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MBOX_AUTO_MAX_BYTES + 1).unwrap();
        let err = ingest_mbox(&store, &home, &path).unwrap_err().to_string();
        assert!(err.contains("filtered pipeline"), "got: {err}");
    }

    /// A Messages database dropped into the inbox is a copy that won't
    /// change again: its recent days land with the rest, and a later,
    /// fuller drop adds what that day gained.
    #[test]
    fn test_a_dropped_messages_database_lands_its_recent_days() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let drop_dir = tmp.path().join("drop");
        std::fs::create_dir_all(&drop_dir).unwrap();
        let db = drop_dir.join("chat.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, date INTEGER, text TEXT,
                 is_from_me INTEGER, handle_id INTEGER);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT, display_name TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             INSERT INTO handle VALUES (1, '+15551234567');
             INSERT INTO chat VALUES (1, '+15551234567', '');",
        )
        .unwrap();
        let apple_ns = |unix: i64| (unix - 978_307_200) * 1_000_000_000;
        let now = chrono::Utc::now().timestamp();
        let add = |id: i64, at: i64, text: &str| {
            conn.execute(
                "INSERT INTO message VALUES (?1, ?2, ?3, 0, 1)",
                rusqlite::params![id, apple_ns(at), text],
            )
            .unwrap();
            conn.execute("INSERT INTO chat_message_join VALUES (1, ?1)", [id])
                .unwrap();
        };
        add(1, now - 86_400, "yesterday's");
        add(2, now - 60, "just now");

        process_file(&store, &home, &Default::default(), &db).unwrap();
        let bodies = || -> String {
            let (_, rows) = store
                .read_query(
                    "SELECT content_ref FROM episodes
                     WHERE source = 'imessage' AND superseded_by IS NULL",
                    10,
                )
                .unwrap();
            rows.iter()
                .map(|r| std::fs::read_to_string(home.root().join(r[0].as_str().unwrap())).unwrap())
                .collect()
        };
        assert!(bodies().contains("yesterday's"));
        assert!(bodies().contains("just now"));

        add(3, now - 30, "and later");
        process_file(&store, &home, &Default::default(), &db).unwrap();
        let after = bodies();
        assert!(after.contains("and later"));
        assert_eq!(
            after.matches("just now").count(),
            1,
            "one live copy of the day"
        );
    }
}
