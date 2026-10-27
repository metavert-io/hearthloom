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
            process_file(&state_clone.store, &state_clone.home, &path_clone)
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
fn process_file(store: &Store, home: &hearthloom_core::Home, path: &Path) -> Result<String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // TCC-protected databases can be Finder-dragged into the inbox
    // (Finder always has access) when the daemon itself doesn't.
    if name == "chat.db" {
        return ingest_drafts(
            store,
            home,
            "adapter:imessage",
            hearthloom_adapters::imessage::ingest(path, None)?,
        );
    }
    if name == "photos.sqlite" {
        return ingest_drafts(
            store,
            home,
            "adapter:photos",
            hearthloom_adapters::photos::ingest(path, None)?,
        );
    }
    match ext.as_str() {
        "mbox" => ingest_mbox(store, home, path),
        "ics" => ingest_drafts(
            store,
            home,
            "adapter:ics",
            hearthloom_adapters::ics::ingest(path, None)?,
        ),
        "json" => ingest_drafts(
            store,
            home,
            "adapter:chatlog",
            hearthloom_adapters::chatlog::ingest(path, None)?,
        ),
        "zip" | "tgz" | "gz" => {
            let dir = extract_archive(path)?;
            // A recognizable export archive (ChatGPT full export, Facebook
            // DYI, LinkedIn, Notion) is STAGED into archives/<key> — its
            // run-now adapter owns the parse. This makes upload-a-zip the
            // full path for archive-dir sources (the primary route on
            // cloud nodes, item 17). Anything else tree-ingests as before.
            if let Some((root, key)) = detect_export_root(dir.path()) {
                let dest = crate::downloads::install_archive_dir(home, &root, key, false)?;
                return Ok(format!(
                    "staged the {key} export at {} — press run now on its Integrations card",
                    dest.display()
                ));
            }
            let summary = ingest_tree(store, home, dir.path())?;
            Ok(summary)
        }
        other => Err(anyhow!(
            "don't know how to ingest .{other} (supported: mbox, ics, json, zip, tgz)"
        )),
    }
}

/// Find an export-archive signature at the extraction root or one level
/// down (zips wrap their content in a single dir as often as not).
fn detect_export_root(root: &Path) -> Option<(std::path::PathBuf, &'static str)> {
    if let Some(key) = crate::downloads::detect_dir(root) {
        return Some((root.to_path_buf(), key));
    }
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(key) = crate::downloads::detect_dir(&path) {
                return Some((path, key));
            }
        }
    }
    None
}

/// Extract an archive to a temp dir using the system tools (ditto knows
/// zip; tar knows tgz) — both ship with macOS.
fn extract_archive(path: &Path) -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir().context("creating extraction dir")?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let status = if ext == "zip" {
        std::process::Command::new("ditto")
            .arg("-x")
            .arg("-k")
            .arg(path)
            .arg(dir.path())
            .status()
    } else {
        std::process::Command::new("tar")
            .arg("-xzf")
            .arg(path)
            .arg("-C")
            .arg(dir.path())
            .status()
    }
    .context("running extractor")?;
    if !status.success() {
        return Err(anyhow!("archive extraction failed ({status})"));
    }
    Ok(dir)
}

/// Walk an extracted archive and ingest every recognizable file.
fn ingest_tree(store: &Store, home: &hearthloom_core::Home, root: &Path) -> Result<String> {
    let mut summaries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut found = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            let ingestable =
                name.ends_with(".mbox") || name.ends_with(".ics") || name == "conversations.json";
            if !ingestable {
                continue;
            }
            found += 1;
            match process_file(store, home, &path) {
                Ok(summary) => summaries.push(format!(
                    "{}: {summary}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )),
                Err(err) => summaries.push(format!(
                    "{}: FAILED: {err}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                )),
            }
        }
    }
    if found == 0 {
        return Err(anyhow!(
            "archive contained no ingestable files (.mbox, .ics, conversations.json)"
        ));
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

fn ingest_drafts(
    store: &Store,
    home: &hearthloom_core::Home,
    actor: &str,
    drafts: Vec<hearthloom_core::EpisodeDraft>,
) -> Result<String> {
    let count = drafts.len();
    let mut new = 0usize;
    let mut existing = 0usize;
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
    Ok(format!(
        "{count} items parsed, {new} new episodes ({existing} already present)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
