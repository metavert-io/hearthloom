//! Full data export (Jon 2026-07-18): one zip that can RECONSTRUCT the
//! whole system on a fresh node — regardless of which node produced it,
//! peered or not. Contents:
//!
//!   manifest.json           what's inside, what was excluded and WHY
//!   hearthloom.db           a consistent snapshot (VACUUM INTO on the
//!                           live db; transient sessions stripped; the
//!                           owner's login survives so a restore keeps
//!                           their password) — THE restore artifact
//!   tables/<name>.json      the human-readable data hierarchy (every
//!                           table classified in core export_all.rs)
//!   files/<dir>/…           raw episode content, notes, media, config
//!
//! THE ENSHRINED CONTRACT (see CLAUDE.md "Full export"): tables are
//! classified in `hearthloom_core::store::export_all::TABLE_CLASSES`
//! (build-failing test on unclassified tables); directories under
//! ~/.hearthloom are classified in `DIR_CLASSES` below with the same
//! test discipline — and at RUNTIME an unclassified directory is
//! INCLUDED with a manifest warning, never silently dropped. New
//! integrations that put files on disk are therefore exported
//! automatically; opting out requires writing an exclusion reason here.
//!
//! Vault rule: starting an export REQUIRES the vault unlocked — the
//! export contains everything, private included, and the user must
//! consciously be in that state. Runs as a background job with progress
//! (+ETA) polled by the Privacy page; download is a separate click.

use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::AppState;
use hearthloom_core::store::{TableClass, TABLE_CLASSES};

pub enum DirClass {
    Exported,
    Excluded(&'static str),
}

/// Every directory (and special file) at the home root, classified.
/// The test below fails when a KNOWN dir goes unclassified; unknown
/// entries at runtime are exported with a warning (safe default).
pub const DIR_CLASSES: &[(&str, DirClass)] = &[
    ("raw", DirClass::Exported),
    ("notes", DirClass::Exported),
    // `hearthloom eval` suites: the owner's own ground-truth questions.
    ("eval", DirClass::Exported),
    ("config", DirClass::Exported),
    // plugin packages + their data export whole; the per-plugin PAT
    // secret (plugins/<name>/key) is carved out in walk() — secrets
    // never export, and a restored node re-mints keys on reinstall.
    ("plugins", DirClass::Exported),
    (
        "archives",
        DirClass::Excluded("original third-party exports — re-obtainable, often multi-GB"),
    ),
    ("inbox", DirClass::Excluded("transient drop folder")),
    (
        "models",
        DirClass::Excluded("embedding model — re-downloadable"),
    ),
    (
        "mounts",
        DirClass::Excluded("virtual-source mountpoints — data lives at origin"),
    ),
    ("exports", DirClass::Excluded("the export output itself")),
    (
        "BRAIN.md",
        DirClass::Excluded("derived — regenerates from the store"),
    ),
    (
        "SKILL.md",
        DirClass::Excluded("derived — regenerates from the store"),
    ),
    (
        "daemon.log",
        DirClass::Excluded("diagnostics, not record data"),
    ),
    (".env", DirClass::Excluded("secrets never export")),
    (".env.sealed", DirClass::Excluded("secrets never export")),
    (
        ".ember-secrets.key",
        DirClass::Excluded("the key ember secrets are sealed with — secrets never export"),
    ),
    (
        "hearthloom.db",
        DirClass::Excluded("live db — the zip carries a consistent snapshot instead"),
    ),
    ("hearthloom.db-wal", DirClass::Excluded("live db journal")),
    ("hearthloom.db-shm", DirClass::Excluded("live db journal")),
];

const KEY_PROGRESS: &str = "export.progress";
const PAGE: i64 = 1000;

fn set_progress(state: &AppState, v: Value) {
    let _ = state
        .store
        .set_consolidation_state(KEY_PROGRESS, &v.to_string());
}

pub fn progress(state: &AppState) -> Value {
    state
        .store
        .consolidation_state(KEY_PROGRESS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"phase": "idle"}))
}

pub fn export_dir(state: &AppState) -> std::path::PathBuf {
    state.home.root().join("exports")
}

/// The background job. Progress phases: scanning → snapshot → tables →
/// files → ready (or error). ETA from unit throughput (rows + files).
pub async fn run_export(state: AppState) {
    let started = std::time::Instant::now();
    let result = build(&state, started).await;
    match result {
        Ok((path, bytes)) => {
            set_progress(
                &state,
                json!({
                    "phase": "ready", "pct": 100, "eta_seconds": 0,
                    "file": path.file_name().map(|f| f.to_string_lossy().into_owned()),
                    "bytes": bytes,
                    "finished": hearthloom_core::time::now_utc(),
                }),
            );
            crate::provisioner::notify_system(
                &state, "info", "Your export is ready",
                "download it from the Privacy page — it can rebuild your whole record on a fresh node",
                Some("export"),
            );
        }
        Err(err) => {
            set_progress(
                &state,
                json!({"phase": "error", "error": format!("{err:#}")}),
            );
        }
    }
}

async fn build(state: &AppState, started: std::time::Instant) -> Result<(std::path::PathBuf, u64)> {
    set_progress(state, json!({"phase": "scanning", "pct": 0}));
    let dir = export_dir(state);
    std::fs::create_dir_all(&dir)?;
    // keep exactly one export on disk
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let zip_path = dir.join(format!("hearthloom-export-{stamp}.zip"));

    // — pre-scan: units = table rows + files —
    let exported_tables: Vec<&str> = TABLE_CLASSES
        .iter()
        .filter_map(|(n, c)| matches!(c, TableClass::Exported).then_some(*n))
        .collect();
    let mut total_units: i64 = 0;
    let mut table_counts = Vec::new();
    for t in &exported_tables {
        let n = state.store.table_row_count(t).unwrap_or(0);
        table_counts.push((*t, n));
        total_units += n;
    }
    let (file_list, warnings) = collect_files(&state.home);
    total_units += file_list.len() as i64 + 500; // +db snapshot weight

    let store = state.store.clone();
    let home_root = state.home.root().to_path_buf();
    let state2 = state.clone();
    let zip_path2 = zip_path.clone();
    let table_counts2 = table_counts.clone();
    let bytes = tokio::task::spawn_blocking(move || -> Result<u64> {
        let file = std::fs::File::create(&zip_path2)?;
        let mut zip = zip::ZipWriter::new(file);
        let deflate: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);
        let mut done: i64 = 0;
        let tick = |state: &AppState, phase: &str, done: i64| {
            let pct = if total_units > 0 { (done * 100 / total_units).min(99) } else { 0 };
            let elapsed = started.elapsed().as_secs_f64();
            let eta = if done > 0 {
                ((elapsed / done as f64) * (total_units - done).max(0) as f64) as i64
            } else {
                -1
            };
            set_progress(state, json!({
                "phase": phase, "pct": pct, "eta_seconds": eta,
                "done": done, "total": total_units,
            }));
        };

        // 1. the restore artifact: a consistent snapshot of the live db
        tick(&state2, "snapshot", done);
        let snap = home_root.join("exports/.db-snapshot");
        let _ = std::fs::remove_file(&snap);
        store.vacuum_into(&snap)?;
        {
            // strip transient sessions from the SNAPSHOT only; the
            // owner's user row stays so a restore keeps their password
            let conn = rusqlite::Connection::open(&snap)?;
            let _ = conn.execute("DELETE FROM sessions", []);
            let _ = conn.execute("VACUUM", ());
        }
        zip.start_file("hearthloom.db", deflate)?;
        std::io::copy(&mut std::fs::File::open(&snap)?, &mut zip)?;
        let _ = std::fs::remove_file(&snap);
        done += 500;
        tick(&state2, "tables", done);

        // 2. the readable hierarchy: every Exported table as JSON
        for (table, count) in &table_counts2 {
            zip.start_file(format!("tables/{table}.json"), deflate)?;
            zip.write_all(b"[")?;
            let mut offset = 0i64;
            let mut first = true;
            while offset < *count {
                let rows = store.dump_table_page(table, offset, PAGE)?;
                if rows.is_empty() {
                    break;
                }
                for row in &rows {
                    if !first {
                        zip.write_all(b",\n")?;
                    }
                    first = false;
                    zip.write_all(serde_json::to_string(row)?.as_bytes())?;
                }
                offset += rows.len() as i64;
                done += rows.len() as i64;
                tick(&state2, "tables", done);
                std::thread::sleep(std::time::Duration::from_millis(40)); // unfair mutex
            }
            zip.write_all(b"]")?;
        }

        // 3. the files the record references
        for rel in &file_list {
            let src = home_root.join(rel);
            zip.start_file(format!("files/{}", rel.replace('\\', "/")), deflate)?;
            if let Ok(mut f) = std::fs::File::open(&src) {
                std::io::copy(&mut f, &mut zip)?;
            }
            done += 1;
            if done % 200 == 0 {
                tick(&state2, "files", done);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        // 4. the manifest: contents + every exclusion WITH its reason
        let excluded_tables: Vec<Value> = TABLE_CLASSES
            .iter()
            .filter_map(|(n, c)| match c {
                TableClass::Excluded(reason) => Some(json!({"table": n, "reason": reason})),
                _ => None,
            })
            .collect();
        let excluded_dirs: Vec<Value> = DIR_CLASSES
            .iter()
            .filter_map(|(n, c)| match c {
                DirClass::Excluded(reason) => Some(json!({"path": n, "reason": reason})),
                _ => None,
            })
            .collect();
        zip.start_file("manifest.json", deflate)?;
        zip.write_all(
            serde_json::to_string_pretty(&json!({
                "hearthloom_version": env!("CARGO_PKG_VERSION"),
                "exported_at": hearthloom_core::time::now_utc(),
                "restore": "place hearthloom.db beside a fresh install's data dir as \
                            hearthloom.db.incoming (or use the onboarding wizard's \
                            'restore from an export'); files/ maps onto ~/.hearthloom/",
                "tables": table_counts2.iter().map(|(t, n)| json!({"table": t, "rows": n})).collect::<Vec<_>>(),
                "files": file_list.len(),
                "excluded_tables": excluded_tables,
                "excluded_paths": excluded_dirs,
                "warnings": warnings,
            }))?
            .as_bytes(),
        )?;
        let file = zip.finish()?;
        Ok(file.metadata().map(|m| m.len()).unwrap_or(0))
    })
    .await
    .context("export task panicked")??;
    Ok((zip_path, bytes))
}

/// Walk the home root honoring DIR_CLASSES; unknown entries are
/// EXPORTED with a warning — the safe default is never to lose data.
fn collect_files(home: &hearthloom_core::Home) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let root = home.root();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (files, warnings);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match DIR_CLASSES.iter().find(|(n, _)| *n == name) {
            Some((_, DirClass::Excluded(_))) => continue,
            Some((_, DirClass::Exported)) => {}
            None => {
                if name.starts_with('.') {
                    continue; // dotfiles default to excluded (secret-shaped)
                }
                warnings.push(format!(
                    "{name}: not classified in export::DIR_CLASSES — exported by the \
                     safe default; classify it (see CLAUDE.md \"Full export\")"
                ));
            }
        }
        let path = entry.path();
        if path.is_file() {
            files.push(name);
        } else {
            walk(&path, root, &mut files);
        }
    }
    (files, warnings)
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, root, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy().into_owned();
            // plugins/<name>/key = that plugin's PAT secret — the one
            // file inside an Exported dir that must never leave.
            if rel.starts_with("plugins/") && rel.ends_with("/key") && rel.matches('/').count() == 2
            {
                continue;
            }
            // plugins/<name>/secrets/… = that ember's sealed secrets
            // (OAuth tokens, signing keys) — never leave either; an
            // ember re-establishes them after a restore.
            if rel.starts_with("plugins/") && rel.split('/').nth(2) == Some("secrets") {
                continue;
            }
            out.push(rel);
        }
    }
}

/// Restore-side boot hook: a fresh (or restoring) node finding
/// `hearthloom.db.incoming` swaps it into place BEFORE the store opens.
/// Crash-safe: the swap is rename-based; the wizard writes .incoming
/// then exits the daemon, and launchd/Fly bring it back through here.
pub fn apply_incoming_restore(home: &hearthloom_core::Home) -> Result<()> {
    let incoming = home.root().join("hearthloom.db.incoming");
    if !incoming.exists() {
        return Ok(());
    }
    let db = home.db_path();
    let backup = home.root().join(format!(
        "hearthloom.db.pre-restore-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    if db.exists() {
        std::fs::rename(&db, &backup).context("backing up the current db")?;
    }
    let _ = std::fs::remove_file(home.root().join("hearthloom.db-wal"));
    let _ = std::fs::remove_file(home.root().join("hearthloom.db-shm"));
    std::fs::rename(&incoming, &db).context("placing the restored db")?;
    tracing::info!(
        "restore applied: {} → hearthloom.db (previous db kept as {})",
        "hearthloom.db.incoming",
        backup.display()
    );
    Ok(())
}

/// Fresh-node restore from an uploaded export zip: only allowed while
/// the record is EMPTY (the wizard's job — no exotic merge-into-existing
/// cases). Extracts files/, stages the db as .incoming, then exits so
/// the supervisor restarts us through apply_incoming_restore.
/// Where an export entry lands on restore, or None to skip it. The
/// database is staged beside the live one, `files/…` lands under the home
/// root, and the rest (manifest, tables/) is informational. zip-slip guard:
/// plain relative segments only. `..` escapes the home root, and so does
/// an absolute name ("files//etc/x"): joining one replaces the root.
fn restore_dest(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    if name == "hearthloom.db" {
        return Some(root.join("hearthloom.db.incoming"));
    }
    let rel = Path::new(name.strip_prefix("files/")?);
    rel.components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
        .then(|| root.join(rel))
}

pub fn stage_restore(state: &AppState, zip_path: &Path) -> Result<Value> {
    if state.store.sync_head().unwrap_or(0) > 0 {
        return Err(anyhow!(
            "this record already has memories — restore only targets a fresh node"
        ));
    }
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let root = state.home.root().to_path_buf();
    let mut extracted = 0usize;
    let mut has_db = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name()?.to_string();
        let Some(dest) = restore_dest(&root, &name) else {
            continue;
        };
        has_db |= name == "hearthloom.db";
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&dest)?;
        std::io::copy(&mut entry, &mut out)?;
        extracted += 1;
    }
    if !has_db {
        return Err(anyhow!(
            "that zip has no hearthloom.db — not a Hearthloom export"
        ));
    }
    Ok(json!({
        "staged": true,
        "extracted": extracted,
        "note": "restarting to complete the restore — sign in with the credentials \
                 the exported record used",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_restore_lands_only_inside_the_home() {
        let root = Path::new("/srv/hl");
        assert_eq!(
            restore_dest(root, "hearthloom.db"),
            Some(root.join("hearthloom.db.incoming"))
        );
        assert_eq!(
            restore_dest(root, "files/notes/a.md"),
            Some(root.join("notes/a.md"))
        );
        assert_eq!(restore_dest(root, "manifest.json"), None);
        for evil in [
            "files/../../etc/x",
            "files//etc/passwd",
            "files/./x",
            "files/a/../../b",
        ] {
            assert_eq!(restore_dest(root, evil), None, "{evil:?} must be skipped");
        }
    }

    #[test]
    fn test_walk_skips_ember_keys_and_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (rel, body) in [
            ("plugins/demo/key", "pat"),
            ("plugins/demo/secrets/client_secret.sealed", "sealed"),
            ("plugins/demo/data/state.json", "{}"),
            ("plugins/demo/pkg/hearthember.json", "{}"),
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let mut out = Vec::new();
        walk(&root.join("plugins"), root, &mut out);
        out.sort();
        assert_eq!(
            out,
            vec![
                "plugins/demo/data/state.json",
                "plugins/demo/pkg/hearthember.json"
            ]
        );
        assert!(DIR_CLASSES
            .iter()
            .any(|(n, c)| *n == hearthloom_core::secrets::NODE_SEAL_KEY_FILE
                && matches!(c, DirClass::Excluded(_))));
    }

    #[test]
    fn test_known_home_entries_are_classified() {
        // the dir-side contract: everything Home creates must appear in
        // DIR_CLASSES (unknowns at runtime export with a warning, but a
        // KNOWN layout dir must be classified deliberately)
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("h"));
        home.init().unwrap();
        let classified: Vec<&str> = DIR_CLASSES.iter().map(|(n, _)| *n).collect();
        for entry in std::fs::read_dir(home.root()).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            assert!(
                classified.contains(&name.as_str()),
                "home dir {name:?} is not classified in export::DIR_CLASSES — \
                 add it as Exported or Excluded(reason) (CLAUDE.md \"Full export\")"
            );
        }
    }

    #[test]
    fn test_collect_respects_classes_and_flags_unknowns() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::Home::at(tmp.path().join("h"));
        home.init().unwrap();
        std::fs::write(home.root().join("raw/x.txt"), "raw content").unwrap();
        std::fs::create_dir_all(home.root().join("archives")).unwrap();
        std::fs::write(home.root().join("archives/huge.zip"), "nope").unwrap();
        std::fs::create_dir_all(home.root().join("somenewfeature")).unwrap();
        std::fs::write(home.root().join("somenewfeature/data.bin"), "keep me").unwrap();
        let (files, warnings) = collect_files(&home);
        assert!(files.iter().any(|f| f == "raw/x.txt"));
        assert!(
            !files.iter().any(|f| f.starts_with("archives/")),
            "excluded dir leaked"
        );
        // the safe default: unknown dirs EXPORT, loudly
        assert!(files.iter().any(|f| f == "somenewfeature/data.bin"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("somenewfeature"));
    }
}
