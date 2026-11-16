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
        process_file(&store, &home, &file).unwrap();
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
        let summary = process_file(&store, &home, &file).unwrap();
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
        let summary = process_file(&store, &home, &file).unwrap();
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
}
