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
        DirClass::Excluded(
            "mail archives — tens of GB; included when the export asks for them (--with-archives), \
             and kept on an always-on node of yours otherwise",
        ),
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
        "agents",
        DirClass::Excluded(
            "each agent run's own settings, its key included — written fresh for every run",
        ),
    ),
    (
        "mail-triage",
        DirClass::Excluded("an empty working folder for sorting mail with Claude Code"),
    ),
    (
        RESTORE_MARKER,
        DirClass::Excluded("a note to the next start that a restore was just placed"),
    ),
    (
        "claude-accounts",
        DirClass::Excluded(
            "extra Claude Code sign-ins for the agent pool — logins (a credentials file on Linux) never export; sign in again",
        ),
    ),
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
        "setup-code",
        DirClass::Excluded("a one-time secret for the first login; gone once it exists"),
    ),
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
pub async fn run_export(state: AppState, with_archives: bool) {
    let started = std::time::Instant::now();
    let result = build(&state, started, with_archives).await;
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

async fn build(
    state: &AppState,
    started: std::time::Instant,
    with_archives: bool,
) -> Result<(std::path::PathBuf, u64)> {
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
    let (file_list, warnings) = collect_files(&state.home, with_archives);
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
/// `with_archives`: also the mail archives this node holds (0.5.90, the
/// owner's choice at export time: a Takeout runs to tens of GB). Their
/// indexes rebuild, and a half-arrived `.part` is never a backup.
fn collect_files(home: &hearthloom_core::Home, with_archives: bool) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let root = home.root();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (files, warnings);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if with_archives && name == "archives" {
            let mut found = Vec::new();
            walk(&entry.path(), root, &mut found);
            files.extend(found.into_iter().filter(|f| f.ends_with(".mbox")));
            continue;
        }
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

/// Left by `apply_incoming_restore` for `after_restore`, which runs once
/// the store is open (a crash between the two only repeats the check).
pub const RESTORE_MARKER: &str = "restore-applied";

/// Restore-side boot hook: a fresh (or restoring) node finding
/// `hearthloom.db.incoming` swaps it into place BEFORE the store opens.
/// Crash-safe: the swap is rename-based; the wizard writes .incoming
/// then exits the daemon, and launchd/Fly bring it back through here.
pub fn apply_incoming_restore(home: &hearthloom_core::Home) -> Result<()> {
    let incoming = home.root().join("hearthloom.db.incoming");
    if !incoming.exists() {
        return Ok(());
    }
    std::fs::write(home.root().join(RESTORE_MARKER), b"")
        .context("noting the restore for after the store opens")?;
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

/// After a restore, once the store is open and before any ember starts.
/// A hosted node runs only embers it installed from the registry itself
/// (plugins.rs `install_from_dir`), and a restore installs nothing: the
/// restored embers' entries are taken off the list — their key revoked,
/// their data and stored values kept — and the owner is told to install
/// them again from the gallery. Their packages weren't restored
/// (`restorable`). Elsewhere a restore brings embers back as they were.
pub fn after_restore(state: &AppState) {
    after_restore_on(state, !crate::registry_client::unreviewed_install_allowed());
}

fn after_restore_on(state: &AppState, hosted: bool) {
    let marker = state.home.root().join(RESTORE_MARKER);
    if !marker.exists() {
        return;
    }
    if hosted {
        let actor = hearthloom_core::Actor::new("system:restore");
        let mut names = Vec::new();
        for row in state.store.list_plugins().unwrap_or_default() {
            let _ = state.store.revoke_api_key(&actor, &row.api_key_id);
            if state.store.unlist_plugin(&actor, &row.name).is_ok() {
                names.push(row.name);
            }
        }
        // a package that reached disk anyway (a zip staged by an older
        // version) goes too: only the registry puts one here
        if let Ok(dirs) = std::fs::read_dir(crate::plugins::plugins_root(state)) {
            for dir in dirs.flatten() {
                let _ = std::fs::remove_dir_all(dir.path().join("pkg"));
            }
        }
        if !names.is_empty() {
            tracing::info!(
                "restore: {} ember(s) taken off until installed from the registry",
                names.len()
            );
            let _ = state.store.notify(
                &actor,
                "system:restore",
                "warn",
                "Install your embers again from the gallery",
                &format!(
                    "Your record is back. This hosted Hearthloom runs only embers it installs                      from the registry itself, so {} didn't come back with it — install each                      again from the gallery and its data is still here.",
                    names.join(", ")
                ),
                Some(("#/embers", Some("open the gallery"))),
                Some("restore-embers"),
            );
        }
    }
    let _ = std::fs::remove_file(marker);
}

/// Where an export entry lands on restore, or None to skip it. The
/// database is staged beside the live one, `files/…` lands under the home
/// root, and the rest (manifest, tables/) is informational. zip-slip guard:
/// plain relative segments only. `..` escapes the home root, and so does
/// an absolute name ("files//etc/x"): joining one replaces the root.
/// Then `restorable`: only what an export writes. The settings in
/// `config/` come back beside the node's own, as `<name>.restored`, for
/// the owner to look at: never read, so a zip can't change where the node
/// listens or which AI it talks to.
fn restore_dest(root: &Path, name: &str, keep_packages: bool) -> Option<std::path::PathBuf> {
    if name == "hearthloom.db" {
        return Some(root.join("hearthloom.db.incoming"));
    }
    let rel = Path::new(name.strip_prefix("files/")?);
    let parts = rel
        .components()
        .map(|c| match c {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<&str>>>()?;
    if !restorable(&parts, keep_packages) {
        return None;
    }
    let mut dest = root.join(rel);
    if parts[0].eq_ignore_ascii_case("config") {
        let set_aside = format!("{}.restored", parts[parts.len() - 1]);
        dest.set_file_name(set_aside);
    }
    Some(dest)
}

/// A restore writes only what an export writes (0.5.100). An export never
/// holds the node's secrets and settings (`.env`, `.env.sealed`, the seal
/// keys — every name starting with `.`), the database files, or anything
/// else `DIR_CLASSES` leaves out, archives excepted (an export asked for
/// them); nor an ember's key or sealed secrets. A zip that carries them
/// isn't an export, and writing them would let it change how the node
/// runs: a plaintext `.env` is read into the environment at the next
/// start. `keep_packages` false (a hosted node) leaves embers' packages
/// out too: only the registry installs code there (`after_restore`).
/// Names compare ignoring case: a Mac's disk does (`Mail-Triage/` IS
/// `mail-triage/` there).
fn restorable(parts: &[&str], keep_packages: bool) -> bool {
    let lower: Vec<String> = parts.iter().map(|p| p.to_ascii_lowercase()).collect();
    let Some(top) = lower.first().map(String::as_str) else {
        return false;
    };
    if top.starts_with('.') || top.starts_with("hearthloom.db") {
        return false;
    }
    if DIR_CLASSES.iter().any(|(name, class)| {
        name.eq_ignore_ascii_case(top)
            && !name.eq_ignore_ascii_case("archives")
            && matches!(class, DirClass::Excluded(_))
    }) {
        return false;
    }
    if top == "plugins" {
        match lower.get(2).map(String::as_str) {
            Some("key") | Some("secrets") => return false,
            Some("pkg") if !keep_packages => return false,
            _ => {}
        }
    }
    true
}

/// Fresh-node restore from an uploaded export zip: only allowed while
/// the record is EMPTY (the wizard's job — no exotic merge-into-existing
/// cases). Extracts files/, stages the db as .incoming, then exits so
/// the supervisor restarts us through apply_incoming_restore.
pub fn stage_restore(state: &AppState, zip_path: &Path) -> Result<Value> {
    if state.store.sync_head().unwrap_or(0) > 0 {
        return Err(anyhow!(
            "this record already has memories — restore only targets a fresh node"
        ));
    }
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let root = state.home.root().to_path_buf();
    let keep_packages = crate::registry_client::unreviewed_install_allowed();
    let mut extracted = 0usize;
    let mut has_db = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name()?.to_string();
        let Some(dest) = restore_dest(&root, &name, keep_packages) else {
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
        let dest = |name: &str| restore_dest(root, name, true);
        assert_eq!(
            dest("hearthloom.db"),
            Some(root.join("hearthloom.db.incoming"))
        );
        assert_eq!(dest("files/notes/a.md"), Some(root.join("notes/a.md")));
        assert_eq!(dest("manifest.json"), None);
        for evil in [
            "files/../../etc/x",
            "files//etc/passwd",
            "files/./x",
            "files/a/../../b",
        ] {
            assert_eq!(dest(evil), None, "{evil:?} must be skipped");
        }
    }

    /// A zip can hold anything; a restore writes only what an export does.
    #[test]
    fn test_restore_writes_only_what_an_export_writes() {
        let root = Path::new("/srv/hl");
        for kept in [
            "files/raw/2026/01/01/x.txt",
            "files/notes/a.md",
            "files/eval/suite.yaml",
            "files/archives/gmail.mbox",
            "files/plugins/demo/data/state.json",
            "files/plugins/demo/pkg/hearthember.json",
            "files/somenewfeature/data.bin",
        ] {
            assert!(
                restore_dest(root, kept, true).is_some(),
                "{kept} should restore"
            );
        }
        for refused in [
            "files/.env",
            "files/.env.sealed",
            "files/.ember-secrets.key",
            "files/setup-code",
            "files/hearthloom.db",
            "files/hearthloom.db.incoming",
            "files/inbox/drop.json",
            "files/models/model.onnx",
            "files/claude-accounts/a/credentials.json",
            "files/mail-triage/.claude/settings.json",
            "files/agents/t.mcp.json",
            "files/restore-applied",
            "files/plugins/demo/key",
            "files/plugins/demo/secrets/x.sealed",
            // a Mac's disk ignores case: these ARE the names above there
            "files/Mail-Triage/.claude/settings.json",
            "files/INBOX/drop.json",
            "files/Hearthloom.db",
            "files/Plugins/demo/KEY",
            "files/plugins/demo/Secrets/x.sealed",
        ] {
            assert_eq!(
                restore_dest(root, refused, true),
                None,
                "{refused} must not restore"
            );
        }
        // settings come back beside the node's own, never in place
        assert_eq!(
            restore_dest(root, "files/config/production.yaml", true),
            Some(root.join("config/production.yaml.restored"))
        );
        assert_eq!(
            restore_dest(root, "files/plugins/demo/PKG/bin/demo", false),
            None
        );
        // a hosted node: no ember code from a zip, its data still comes back
        assert_eq!(
            restore_dest(root, "files/plugins/demo/pkg/bin/demo", false),
            None
        );
        assert!(restore_dest(root, "files/plugins/demo/data/state.json", false).is_some());
    }

    /// After a restore a hosted node holds no ember it didn't install
    /// from the registry: off the list, key revoked, package gone, data
    /// and stored values kept, the owner told. Elsewhere nothing changes.
    #[test]
    fn test_a_hosted_node_takes_restored_embers_off_until_reinstalled() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path());
        home.init().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(
                hearthloom_core::store::Store::open(&home.db_path()).unwrap(),
            ),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        let actor = hearthloom_core::Actor::new("human:test");
        let install = |name: &str| {
            let (key, _) = state
                .store
                .create_api_key(
                    &actor,
                    &format!("ember:{name}"),
                    &["read".to_string()],
                    None,
                )
                .unwrap();
            state
                .store
                .install_plugin(
                    &actor,
                    name,
                    "1",
                    &json!({"name": name}),
                    &[],
                    &key.id,
                    "local-only",
                )
                .unwrap();
            state.store.plugin_kv_set(name, "k", "v").unwrap();
            let dir = crate::plugins::plugin_dir(&state, name);
            std::fs::create_dir_all(dir.join("pkg/bin")).unwrap();
            std::fs::create_dir_all(dir.join("data")).unwrap();
            std::fs::write(dir.join("pkg/bin/x"), "#!/bin/sh").unwrap();
            std::fs::write(dir.join("data/state.json"), "{}").unwrap();
            key.id
        };
        let key = install("demo");
        let marker = state.home.root().join(RESTORE_MARKER);

        // no restore just happened: nothing to do, even hosted
        after_restore_on(&state, true);
        assert_eq!(state.store.list_plugins().unwrap().len(), 1);

        // a node of the owner's own: the restore stands as it was
        std::fs::write(&marker, b"").unwrap();
        after_restore_on(&state, false);
        assert_eq!(state.store.list_plugins().unwrap().len(), 1);
        assert!(!marker.exists());

        // hosted
        std::fs::write(&marker, b"").unwrap();
        after_restore_on(&state, true);
        assert!(state.store.list_plugins().unwrap().is_empty());
        let dir = crate::plugins::plugin_dir(&state, "demo");
        assert!(!dir.join("pkg").exists(), "the restored package is gone");
        assert!(dir.join("data/state.json").exists(), "its data is kept");
        assert_eq!(
            state.store.plugin_kv_get("demo", "k").unwrap().as_deref(),
            Some("v")
        );
        let keys = state.store.list_api_keys().unwrap();
        assert!(
            keys.iter().all(|k| k.id != key || k.revoked_at.is_some()),
            "its key is revoked"
        );
        let notes = state.store.notifications(false, 10).unwrap();
        assert!(notes
            .iter()
            .any(|n| n.title.contains("Install your embers again")));
        assert!(!marker.exists());
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
        let (files, warnings) = collect_files(&home, false);
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
