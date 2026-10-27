//! Plugin lifecycle + the sidecar contract (docs/plugins.md, built
//! 2026-07-20). A plugin is a SEPARATE PROCESS talking to the loopback
//! API with a scoped PAT — never in-process code. This module owns:
//!
//! - the package format: a directory (or zip of one) with
//!   `hearthember.json` + optional `bin/<platform>` + optional `ui/`
//! - manifest validation (additive wire discipline: protocol integer,
//!   unknown fields ignored, capability strings)
//! - install/uninstall/enable (mint the PAT, copy the package under
//!   ~/.hearthloom/plugins/<name>/pkg, write/remove the launchd unit,
//!   registry row) — NO central index, NO signature verification here:
//!   distribution is deferred to the provisioner; every install is local and
//!   labeled "unreviewed" in the UI
//! - THE DEFAULT-DENY ROUTE TABLE for plugin-bound keys: an ordinary
//!   key passes the blanket auth on unscoped routes (the trusted-
//!   integration posture), but a key bound to a plugin reaches ONLY
//!   what its granted scopes map to below — otherwise the scope grant
//!   would be theater.
//!
//! Layout under ~/.hearthloom/plugins/<name>/:
//!   pkg/   — the installed package (manifest, bin, ui) — replaced on upgrade
//!   data/  — the plugin's file storage (daemon-written via /v1/plugins/self)
//!   key    — the sidecar's PAT secret, 0600 (the one place it exists)

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::AppState;
use hearthloom_core::store::Actor;

/// Scopes a plugin may REQUEST. Deliberately small; grows with real
/// plugins, never speculatively. admin/peer/friend are infrastructure
/// scopes — never grantable to a plugin.
pub const PLUGIN_ALLOWED_SCOPES: &[&str] = &["read", "ingest", "notify"];

pub const PROTOCOL: i64 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub protocol: i64,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Relative launch path per platform, e.g. {"darwin": "bin/sampler"}.
    /// Absent = no sidecar process (a pure-UI or externally-run plugin).
    #[serde(default)]
    pub launch: std::collections::HashMap<String, String>,
    /// "mirror" replicates KV + files to the peer; default "local-only".
    #[serde(default)]
    pub sync: Option<String>,
    /// Reads THIS machine (screen, apps, local software) — never offers
    /// itself on cloud nodes.
    #[serde(default, alias = "localOnly")]
    pub local_only: bool,
    /// Where the sidecar belongs: "local" (reads this machine — never a
    /// cloud node), "cloud" (belongs on the always-on node), "any"
    /// (default). localOnly=true is the legacy alias for "local".
    #[serde(default)]
    pub placement: Option<String>,
    /// "single" (default): a mirror-sync'd sidecar runs in ONE place
    /// across a pair (the runner lease). "per-node": duplicates are
    /// MEANINGFUL — run wherever installed (presence samplers: each
    /// machine's presence is its own fact).
    #[serde(default)]
    pub runner: Option<String>,
    /// Host dependency, e.g. {"host": ">=0.5.25"} — the minimum
    /// hearthloom this ember needs (npm `engines` pattern). Install is
    /// refused when the running host is older.
    #[serde(default)]
    pub requires: Option<Value>,
    /// Declarative settings: [{key, label, type: string|number|boolean|
    /// list, default?, help?}]. The dashboard renders the form on the
    /// ember's Integrations card and writes the values into the
    /// ember's own `config` KV — embers just read KV, no settings UI
    /// to build.
    #[serde(default)]
    pub config: Option<Value>,
    /// UI contributions — see ui_contributions().
    #[serde(default)]
    pub ui: Option<Value>,
    /// Public files: [{path, file, type?}] — a file from the ember's
    /// own data store served WITHOUT auth at a fixed address under
    /// /.well-known/ (a third-party app's domain-verification key, the
    /// Tesla Fleet API case). Shown on the grant screen; one ember per
    /// path. See serve_well_known().
    #[serde(default)]
    pub publish: Vec<PublishEntry>,
    // "signature" is reserved (additive) for a registry's countersignature.
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct PublishEntry {
    /// The public address, e.g. "/.well-known/appspecific/x.pem".
    pub path: String,
    /// Relative path inside the ember's data store (/v1/plugins/self/files).
    pub file: String,
    /// Content-Type; default text/plain.
    #[serde(default, rename = "type")]
    pub content_type: Option<String>,
}

/// Public files are tiny by nature (keys, verification tokens).
pub const PUBLISH_MAX_BYTES: u64 = 64 * 1024;

/// A clean public address under /.well-known/: [A-Za-z0-9._-] segments,
/// no dot-segments, bounded length.
pub fn valid_publish_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/.well-known/") else {
        return false;
    };
    path.len() <= 200
        && !rest.is_empty()
        && rest.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

/// An ember name: 1-40 chars of [a-z0-9-], not starting with '-'. The
/// manifest rule, and the check before a name goes into a registry URL.
pub fn valid_ember_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        if self.protocol != PROTOCOL {
            bail!(
                "manifest protocol {} unsupported (this node speaks {PROTOCOL})",
                self.protocol
            );
        }
        if !valid_ember_name(&self.name) {
            bail!("plugin name must be 1-40 chars of [a-z0-9-] (got {:?})", self.name);
        }
        if self.name == "self" || self.name == "ui" {
            bail!("{:?} is a reserved name", self.name);
        }
        if self.version.trim().is_empty() {
            bail!("manifest needs a version");
        }
        for scope in &self.scopes {
            if !PLUGIN_ALLOWED_SCOPES.contains(&scope.as_str()) {
                bail!(
                    "plugins may request only: {} (asked for {scope:?})",
                    PLUGIN_ALLOWED_SCOPES.join(", ")
                );
            }
        }
        if let Some(req) = self.host_requirement() {
            if req.trim().is_empty() {
                bail!("requires.host must be a version range like \">=0.5.25\"");
            }
        }
        if let Some(cfg) = &self.config {
            let items = cfg.as_array().ok_or_else(|| anyhow!("config must be an array"))?;
            for item in items {
                let key = item.get("key").and_then(|k| k.as_str()).unwrap_or("");
                if key.is_empty()
                    || key.starts_with("__")
                    || !key.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    bail!("config keys are [a-z0-9_] (got {key:?})");
                }
                let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("string");
                if !matches!(ty, "string" | "number" | "boolean" | "list") {
                    bail!("config type must be string|number|boolean|list");
                }
            }
        }
        if let Some(r) = &self.runner {
            if !matches!(r.as_str(), "single" | "per-node") {
                bail!("runner must be \"single\" or \"per-node\"");
            }
        }
        if let Some(p) = &self.placement {
            if !matches!(p.as_str(), "local" | "cloud" | "any") {
                bail!("placement must be \"local\", \"cloud\", or \"any\"");
            }
        }
        if let Some(sync) = &self.sync {
            if !matches!(sync.as_str(), "mirror" | "local-only") {
                bail!("sync must be \"mirror\" or \"local-only\"");
            }
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.publish {
            if !valid_publish_path(&p.path) {
                bail!("publish paths must be clean addresses under /.well-known/ (got {:?})", p.path);
            }
            if safe_rel_path(&p.file).is_err() {
                bail!("publish file {:?} must be a clean path in the ember's data store", p.file);
            }
            if !seen.insert(p.path.as_str()) {
                bail!("{:?} is published twice", p.path);
            }
        }
        Ok(())
    }
}

impl Manifest {
    /// placement > localOnly alias > "any".
    pub fn effective_placement(&self) -> &str {
        match self.placement.as_deref() {
            Some(p) => p,
            None if self.local_only => "local",
            None => "any",
        }
    }

    /// May a sidecar launch on THIS node? (cloud nodes run no local
    /// sidecars in v1 regardless — that check stays at the call site)
    pub fn placement_allows_here(&self) -> bool {
        let cloud = crate::monitor::node_kind() == "cloud";
        match self.effective_placement() {
            "local" => !cloud,
            "cloud" => cloud,
            _ => true,
        }
    }

    /// The `requires.host` version range, if declared.
    pub fn host_requirement(&self) -> Option<String> {
        self.requires
            .as_ref()?
            .get("host")?
            .as_str()
            .map(|s| s.to_string())
    }
}

/// The rust target triple for this build's platform (for the host
/// self-update download key).
pub fn host_target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        _ => "unknown",
    }
}

fn parse_ver(s: &str) -> (u64, u64, u64) {
    let mut it = s
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .map(|x| x.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Does `current` satisfy the requirement `req`? A leading operator
/// (>=, >, <=, <, =) is honored; bare means ">=".
pub fn version_satisfies(current: &str, req: &str) -> bool {
    let req = req.trim();
    let (op, ver) = if let Some(r) = req.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = req.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = req.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = req.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = req.strip_prefix('=') {
        ("=", r)
    } else {
        (">=", req)
    };
    let (c, v) = (parse_ver(current), parse_ver(ver));
    match op {
        ">=" => c >= v,
        ">" => c > v,
        "<=" => c <= v,
        "<" => c < v,
        _ => c == v,
    }
}

pub fn plugins_root(state: &AppState) -> PathBuf {
    state.home.root().join("plugins")
}

pub fn plugin_dir(state: &AppState, name: &str) -> PathBuf {
    plugins_root(state).join(name)
}

/// The manifest filename. Plugins are branded EMBERS (Jon 2026-07-20);
/// `hearthember.json` is the canonical name, `hearthplugin.json` the
/// accepted fallback from the day-one packages. Wire/disk identifiers
/// (routes, tables, ~/.hearthloom/plugins/) stay `plugins` — frozen
/// internals under the ember brand, the Memories/#browse precedent.
pub const MANIFEST_NAMES: &[&str] = &["hearthember.json", "hearthplugin.json"];

pub fn manifest_path(dir: &Path) -> Option<PathBuf> {
    MANIFEST_NAMES
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.exists())
}

/// Parse + validate a package directory's manifest.
pub fn load_manifest(dir: &Path) -> Result<Manifest> {
    let path = manifest_path(dir)
        .ok_or_else(|| anyhow!("no hearthember.json in {}", dir.display()))?;
    let raw = std::fs::read_to_string(&path)?;
    let manifest: Manifest =
        serde_json::from_str(&raw).with_context(|| format!("{} didn't parse", path.display()))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Recursive copy for the package install (small trees; sync, called
/// from spawn_blocking).
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &to)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &to)?;
        } // symlinks are skipped — a package carries plain files
    }
    Ok(())
}

fn launchd_label(name: &str) -> String {
    format!("com.hearthloom.plugin.{name}")
}

fn launchd_plist_path(name: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{}.plist", launchd_label(name)))
}

/* ————— running an ember's sidecar: launchd on a Mac, the daemon's own
   supervisor (sidecar.rs) everywhere else ————— */

/// The `launch` entry for THIS machine: the exact target triple first
/// (`x86_64-unknown-linux-gnu` — a package can carry one binary per
/// architecture), then the OS key (`darwin` / `linux`).
pub fn launch_entry(manifest: &Manifest) -> Option<&String> {
    let os_key = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    manifest
        .launch
        .get(host_target())
        .or_else(|| manifest.launch.get(os_key))
}

/// launchd owns sidecars on a Mac; the daemon supervises them
/// everywhere else. `HEARTHLOOM_SIDECAR_BACKEND=supervisor` forces the
/// supervisor on a Mac (testing the hosted path locally).
fn use_launchd() -> bool {
    cfg!(target_os = "macos")
        && std::env::var("HEARTHLOOM_SIDECAR_BACKEND").map(|v| v != "supervisor").unwrap_or(true)
}

fn sidecar_env(state: &AppState, name: &str) -> Vec<(String, String)> {
    let port = std::env::var("HEARTHLOOM_PORT").unwrap_or_else(|_| "43278".into());
    vec![
        ("HEARTHLOOM_URL".into(), format!("http://127.0.0.1:{port}")),
        (
            "HEARTHLOOM_PLUGIN_KEY_FILE".into(),
            plugin_dir(state, name).join("key").display().to_string(),
        ),
        ("HEARTHLOOM_PLUGIN_NAME".into(), name.to_string()),
    ]
}

/// Start an ember's sidecar on this machine (no-op when its package
/// has no binary for this platform).
pub fn sidecar_start(state: &AppState, manifest: &Manifest) -> Result<()> {
    let Some(rel) = launch_entry(manifest) else {
        return Ok(());
    };
    let name = &manifest.name;
    let bin = plugin_dir(state, name).join("pkg").join(rel);
    if !bin.exists() {
        bail!("manifest launch entry points at {rel:?}, which isn't in the package");
    }
    // The binary must be executable — zips don't always preserve mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
    }
    if use_launchd() {
        return launchd_install(state, name, &bin);
    }
    crate::sidecar::start(crate::sidecar::Spec {
        name: name.clone(),
        bin,
        env: sidecar_env(state, name),
        log: plugin_dir(state, name).join("plugin.log"),
        cwd: plugin_dir(state, name),
    })
}

pub fn sidecar_stop(name: &str) {
    if use_launchd() {
        launchd_remove(name);
    } else {
        crate::sidecar::stop(name);
    }
}

/// Is the sidecar running right now (launchd-loaded or supervised)?
pub fn sidecar_running(name: &str) -> bool {
    if use_launchd() {
        launchd_loaded(name)
    } else {
        crate::sidecar::running(name)
    }
}

/// Boot: supervised sidecars don't survive a daemon restart the way
/// launchd units do, so start every enabled one that belongs here (the
/// runner lease still decides for mirror-sync'd embers).
pub fn start_enabled_sidecars(state: &AppState) {
    if use_launchd() {
        return;
    }
    let Ok(rows) = state.store.list_plugins() else { return };
    for row in rows.into_iter().filter(|r| r.enabled) {
        let Ok(manifest) = serde_json::from_value::<Manifest>(row.manifest) else {
            continue;
        };
        if manifest.placement_allows_here() && lease_allows_launch(state, &manifest) {
            if let Err(e) = sidecar_start(state, &manifest) {
                tracing::warn!("ember {}: couldn't start its sidecar: {e:#}", manifest.name);
            }
        }
    }
}

/// Write + load the launchd unit for a plugin's sidecar binary.
fn launchd_install(state: &AppState, name: &str, bin: &Path) -> Result<()> {
    let plist = launchd_plist_path(name);
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let port = std::env::var("HEARTHLOOM_PORT").unwrap_or_else(|_| "43278".into());
    let log = plugin_dir(state, name).join("plugin.log");
    let key_file = plugin_dir(state, name).join("key");
    std::fs::write(
        &plist,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>ProgramArguments</key>
    <array><string>{bin}</string></array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>HEARTHLOOM_URL</key><string>http://127.0.0.1:{port}</string>
        <key>HEARTHLOOM_PLUGIN_KEY_FILE</key><string>{key_file}</string>
        <key>HEARTHLOOM_PLUGIN_NAME</key><string>{name}</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>{log}</string>
    <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
            label = launchd_label(name),
            bin = bin.display(),
            key_file = key_file.display(),
            log = log.display(),
        ),
    )?;
    let _ = std::process::Command::new("launchctl")
        .args(["unload", plist.to_str().unwrap_or_default()])
        .output();
    let out = std::process::Command::new("launchctl")
        .args(["load", plist.to_str().unwrap_or_default()])
        .output()
        .context("running launchctl load")?;
    if !out.status.success() {
        bail!(
            "launchctl load failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn launchd_remove(name: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let plist = launchd_plist_path(name);
    if plist.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", plist.to_str().unwrap_or_default()])
            .output();
        let _ = std::fs::remove_file(&plist);
    }
}

/// Is the sidecar loaded in launchd right now?
fn launchd_loaded(name: &str) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("launchctl")
        .args(["list", &launchd_label(name)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Which ENABLED ember publishes this public address, and its entry.
/// Paths are exclusive (install refuses a second claimant), so the
/// first match is the only one.
pub fn publisher_of(state: &AppState, path: &str) -> Option<(String, PublishEntry)> {
    for row in state.store.list_plugins().ok()? {
        if !row.enabled {
            continue;
        }
        let Ok(manifest) = serde_json::from_value::<Manifest>(row.manifest) else {
            continue;
        };
        if let Some(entry) = manifest.publish.into_iter().find(|p| p.path == path) {
            return Some((row.name, entry));
        }
    }
    None
}

/// Refuse an install whose public addresses another installed ember
/// already holds (enabled or not — a disabled claimant gets its path
/// back when re-enabled).
fn check_publish_free(state: &AppState, manifest: &Manifest) -> Result<()> {
    for row in state.store.list_plugins()? {
        if row.name == manifest.name {
            continue;
        }
        let Ok(other) = serde_json::from_value::<Manifest>(row.manifest) else {
            continue;
        };
        for p in &manifest.publish {
            if other.publish.iter().any(|o| o.path == p.path) {
                bail!("{} already publishes {} — only one ember may hold a public address", row.name, p.path);
            }
        }
    }
    Ok(())
}

/// Install (or upgrade) from a package directory. Blocking: call from
/// spawn_blocking. Returns (registry row, plaintext key or None on
/// re-key-less upgrade).
pub fn install_from_dir(
    state: &AppState,
    src: &Path,
    granted_scopes: Option<Vec<String>>,
) -> Result<(hearthloom_core::store::PluginRow, Option<String>)> {
    let manifest = load_manifest(src)?;
    // A hosted node installs only what the registry verified in this
    // process (stage → install). The zip-upload and local-path routes
    // used to skip the cloud-eligible + countersignature checks that
    // registry staging enforces.
    if !crate::registry_client::unreviewed_install_allowed()
        && !crate::registry_client::is_verified_stage(src)
    {
        bail!(
            "hosted instances install only reviewed embers from the registry — {} wasn't staged through it",
            manifest.name
        );
    }
    if let Some(req) = manifest.host_requirement() {
        let current = env!("CARGO_PKG_VERSION");
        if !version_satisfies(current, &req) {
            bail!(
                "{} needs hearthloom {req} (this node is {current}) — upgrade hearthloom first",
                manifest.name
            );
        }
    }
    check_publish_free(state, &manifest)?;
    let granted = granted_scopes.unwrap_or_else(|| manifest.scopes.clone());
    for scope in &granted {
        if !PLUGIN_ALLOWED_SCOPES.contains(&scope.as_str()) {
            bail!("plugins may be granted only: {}", PLUGIN_ALLOWED_SCOPES.join(", "));
        }
    }
    let name = manifest.name.clone();
    let actor = Actor::new("human:dashboard");

    // Upgrade keeps the existing key (the sidecar's key file stays
    // valid); fresh install mints one.
    let existing = state.store.plugin(&name)?;
    let existing_row_present = existing.is_some();
    let (api_key_id, secret) = match existing {
        Some(row) => (row.api_key_id, None),
        None => {
            let (row, secret) = state.store.create_api_key(
                &actor,
                &format!("plugin:{name}"),
                &granted,
                None,
            )?;
            (row.id, Some(secret))
        }
    };

    // Package lands under plugins/<name>/pkg (replaced whole on upgrade).
    // An upgrade stops the running program first — never swap a binary
    // under a live process, and the start below then runs the new one
    // (the supervisor's start is a no-op for an ember it already runs).
    let dir = plugin_dir(state, &name);
    let pkg = dir.join("pkg");
    if existing_row_present {
        sidecar_stop(&name);
    }
    if pkg.exists() {
        std::fs::remove_dir_all(&pkg)?;
    }
    copy_tree(src, &pkg)?;
    std::fs::create_dir_all(dir.join("data"))?;
    if let Some(secret) = &secret {
        let key_path = dir.join("key");
        std::fs::write(&key_path, secret)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    let manifest_json: Value =
        serde_json::from_str(&std::fs::read_to_string(
            manifest_path(src).ok_or_else(|| anyhow!("manifest vanished mid-install"))?,
        )?)?;
    let sync = manifest.sync.clone().unwrap_or_else(|| "local-only".into());
    let row = state.store.install_plugin(
        &actor,
        &name,
        &manifest.version,
        &manifest_json,
        &granted,
        &api_key_id,
        &sync,
    )?;

    // Placement gates: a local-placement ember can't even INSTALL on a
    // cloud node; otherwise the sidecar launches only where it belongs
    // (and the runner lease may still hold it back — see runner_tick).
    let cloud = crate::monitor::node_kind() == "cloud";
    if cloud && manifest.effective_placement() == "local" {
        // cleanup: the registry row was written; pull it back out
        let _ = state.store.uninstall_plugin(&actor, &name);
        bail!("{name} reads the user's own machine (placement: local) — it can't run on a hosted instance");
    }
    if manifest.placement_allows_here() && lease_allows_launch(state, &manifest) {
        sidecar_start(state, &manifest)?;
    }
    Ok((row, secret))
}

/// Uninstall: stop + remove the unit, revoke the key (and any live UI
/// tokens), drop the registry row. The data dir is REPORTED, never
/// deleted — it's the user's.
pub fn uninstall(state: &AppState, name: &str) -> Result<PathBuf> {
    let row = state
        .store
        .plugin(name)?
        .ok_or_else(|| anyhow!("no installed plugin {name:?}"))?;
    sidecar_stop(name);
    let actor = Actor::new("human:dashboard");
    let _ = state.store.revoke_api_key(&actor, &row.api_key_id);
    revoke_ui_tokens(state, name);
    state.store.uninstall_plugin(&actor, name)?;
    Ok(plugin_dir(state, name).join("data"))
}

pub fn set_enabled(state: &AppState, name: &str, enabled: bool) -> Result<()> {
    let row = state
        .store
        .plugin(name)?
        .ok_or_else(|| anyhow!("no installed plugin {name:?}"))?;
    if !enabled {
        // Stop FIRST: the program gets SIGTERM while its key still
        // works, so its last flush lands (a disabled ember's key is
        // refused — plugin_for_key binds enabled rows only).
        sidecar_stop(name);
    }
    state
        .store
        .set_plugin_enabled(&Actor::new("human:dashboard"), name, enabled)?;
    if enabled {
        let manifest: Manifest = serde_json::from_value(row.manifest)?;
        if manifest.placement_allows_here() && lease_allows_launch(state, &manifest) {
            sidecar_start(state, &manifest)?;
        }
    } else {
        revoke_ui_tokens(state, name);
    }
    Ok(())
}

/// Revoke any live ephemeral UI tokens for a plugin (minted as
/// `plugin-ui:<name>` — see the ui-token handler).
fn revoke_ui_tokens(state: &AppState, name: &str) {
    let actor = Actor::new("human:dashboard");
    if let Ok(keys) = state.store.list_api_keys() {
        for key in keys {
            if key.name == format!("plugin-ui:{name}") && key.revoked_at.is_none() {
                let _ = state.store.revoke_api_key(&actor, &key.id);
            }
        }
    }
}

/// Which plugin does this verified key belong to, if any? Sidecar keys
/// bind through the registry row; ephemeral UI tokens bind by name.
pub fn plugin_for_claims(
    state: &AppState,
    claims: &hearthloom_core::store::ApiKeyClaims,
) -> Option<String> {
    if let Ok(Some(name)) = state.store.plugin_for_key(&claims.id) {
        return Some(name);
    }
    let name = claims.name.strip_prefix("plugin-ui:")?;
    match state.store.plugin(name) {
        Ok(Some(row)) if row.enabled => Some(name.to_string()),
        _ => None,
    }
}

/// THE DEFAULT-DENY TABLE. A plugin-bound key reaches exactly this —
/// its scopes are the ceiling, the vault stays WILL-ENFORCE beneath.
/// Everything not listed answers 403. Grows with the scope list, in
/// code, reviewably.
pub fn plugin_route_allowed(path: &str, method: &str, scopes: &[String]) -> bool {
    // Every registered plugin key, regardless of scopes:
    if path.starts_with("/v1/plugins/self/") {
        return true;
    }
    if path == "/v1/bus/stream" && method == "GET" {
        return true; // event HINTS only; every fetch re-checks scope
    }
    let has = |s: &str| scopes.iter().any(|x| x == s);
    if has("ingest")
        && method == "POST"
        && (path == "/v1/ingest"
            // a vehicle's drives into the movement layer (the handler
            // refuses the person's own movement from an ember)
            || path == "/v1/ingest/movement"
            || path == "/v1/sources/virtual"
            || (path.starts_with("/v1/sources/virtual/") && path.ends_with("/promote")))
    {
        return true;
    }
    if has("notify") && method == "POST" && path == "/v1/notifications" {
        return true;
    }
    if has("read") && method == "GET" {
        // The read surface, minus admin/infrastructure. Deny-prefix
        // list, not allow-list: read is a broad scope by design and the
        // vault guard filters private beneath it either way.
        const DENIED: &[&str] = &[
            "/v1/auth", "/v1/keys", "/v1/export", "/v1/users", "/v1/vault",
            "/v1/plugins", "/v1/sync", "/v1/peer", "/v1/provisioner",
            "/v1/privacy", "/v1/integrations", "/v1/monitor", "/v1/agents",
            // who drives your car, and the questions about it, are yours
            "/v1/vehicles", "/v1/trips",
        ];
        return path.starts_with("/v1/") && !DENIED.iter().any(|d| path.starts_with(d));
    }
    false
}

/// The chrome contributions of enabled plugins, shaped for app.js:
/// nav entries, Visualize items, hamburger items, Desk widgets. Pages
/// resolve to /plugins/<name>/ui/<page> inside the sandbox host.
pub fn ui_contributions(state: &AppState) -> Result<Value> {
    let mut nav = Vec::new();
    let mut visualize = Vec::new();
    let mut hamburger = Vec::new();
    let mut desk = Vec::new();
    let mut card = Vec::new();
    let cloud = crate::monitor::node_kind() == "cloud";
    for row in state.store.list_plugins()? {
        if !row.enabled {
            continue;
        }
        let manifest: Manifest = match serde_json::from_value(row.manifest.clone()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if manifest.effective_placement() == "local" && cloud {
            continue;
        }
        let Some(ui) = &manifest.ui else { continue };
        let entry = |v: &Value| -> Option<Value> {
            let label = v.get("label")?.as_str()?;
            let page = v.get("page")?.as_str()?;
            if page.contains("..") || page.starts_with('/') {
                return None;
            }
            Some(json!({
                "plugin": row.name,
                "label": label,
                "page": page,
                "height": v.get("height").and_then(|h| h.as_i64()),
                // the host mounts the core Dial above the sandbox and
                // streams {from,to} in — embers never import dial.js
                "dial": v.get("dial").and_then(|d| d.as_bool()).unwrap_or(false),
            }))
        };
        if let Some(n) = ui.get("nav").and_then(entry) {
            nav.push(n); // one top-nav entry per plugin, by shape
        }
        for (list, out) in [("visualize", &mut visualize), ("hamburger", &mut hamburger), ("desk", &mut desk), ("card", &mut card)] {
            if let Some(items) = ui.get(list).and_then(|v| v.as_array()) {
                out.extend(items.iter().filter_map(entry));
            }
        }
    }
    Ok(json!({
        "nav": nav, "visualize": visualize, "hamburger": hamburger, "desk": desk,
        // links rendered ON the ember's own Integrations card — the
        // right home for a view that shouldn't crowd the chrome
        "card": card,
    }))
}

/// Extract an uploaded package zip into plugins/.staging/<uuid>/ and
/// return the directory holding the manifest (the zip may nest one
/// top-level folder). Blocking: call from spawn_blocking.
pub fn stage_zip(state: &AppState, bytes: &[u8]) -> Result<PathBuf> {
    let staging = plugins_root(state)
        .join(".staging")
        .join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&staging)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let Some(rel) = file.enclosed_name() else { continue }; // traversal-safe
        let out = staging.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut w = std::fs::File::create(&out)?;
            std::io::copy(&mut file, &mut w)?;
        }
    }
    if manifest_path(&staging).is_some() {
        return Ok(staging);
    }
    // one nested top-level dir is the common zip shape
    for entry in std::fs::read_dir(&staging)? {
        let path = entry?.path();
        if path.is_dir() && manifest_path(&path).is_some() {
            return Ok(path);
        }
    }
    bail!("the zip has no hearthember.json at its root")
}

/// Path-traversal guard for plugin file storage: a clean relative path
/// under data/files/. Rejects absolute paths, .., empty, and dotfiles
/// at the root (the `key` file lives beside data/).
pub fn safe_rel_path(rel: &str) -> Result<&str> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty()
        || rel.len() > 512
        || rel.split('/').any(|seg| seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('.'))
    {
        bail!("invalid storage path {rel:?}");
    }
    Ok(rel)
}

/* ————— ember secrets: sealed at rest, never exported, never mirrored —————
   plugins/<name>/secrets/<key>.sealed, sealed by core secrets::seal_node
   (master key, else this node's key file). The SIDECAR reads values
   back; the ember's UI page can set/list/delete but never read (a page
   is a less-trusted surface than the binary the user installed). */

pub const SECRET_MAX_BYTES: usize = 64 * 1024;

fn secrets_dir(state: &AppState, name: &str) -> PathBuf {
    plugin_dir(state, name).join("secrets")
}

/// Secret names: 1-64 of [a-z0-9_.-], no leading dot. `__`-prefixed
/// names are daemon-written (the OAuth callback); embers read them.
pub fn valid_secret_name(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && !key.starts_with('.')
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '.' | '-'))
}

pub fn secret_put(state: &AppState, name: &str, key: &str, value: &[u8]) -> Result<()> {
    if !valid_secret_name(key) {
        bail!("secret names are 1-64 chars of [a-z0-9_.-]");
    }
    if value.len() > SECRET_MAX_BYTES {
        bail!("secrets cap at 64KB");
    }
    let sealed = hearthloom_core::secrets::seal_node(state.home.root(), value)?;
    let dir = secrets_dir(state, name);
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // write-then-rename: a crash never leaves half a sealed file
    let tmp = dir.join(format!(".{key}.tmp"));
    std::fs::write(&tmp, &sealed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, dir.join(format!("{key}.sealed")))?;
    Ok(())
}

pub fn secret_get(state: &AppState, name: &str, key: &str) -> Result<Option<Vec<u8>>> {
    if !valid_secret_name(key) {
        return Ok(None);
    }
    let path = secrets_dir(state, name).join(format!("{key}.sealed"));
    let Ok(sealed) = std::fs::read(&path) else {
        return Ok(None);
    };
    Ok(Some(hearthloom_core::secrets::unseal_node(state.home.root(), &sealed)?))
}

pub fn secret_delete(state: &AppState, name: &str, key: &str) -> bool {
    valid_secret_name(key)
        && std::fs::remove_file(secrets_dir(state, name).join(format!("{key}.sealed"))).is_ok()
}

/// Names + when each was set — never values.
pub fn secret_list(state: &AppState, name: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(secrets_dir(state, name)) else {
        return out;
    };
    for entry in entries.flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(key) = file.strip_suffix(".sealed") else { continue };
        let set_at = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| chrono::DateTime::<chrono::Utc>::from(t).format("%Y-%m-%dT%H:%M:%SZ").to_string());
        out.push(json!({"key": key, "set_at": set_at}));
    }
    out.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    out
}

/* ————— the OAuth callback relay —————
   A third-party sign-in needs a public HTTPS redirect address; an
   ember has none of its own (it's a process behind the daemon). So the
   daemon answers `/plugins/<name>/oauth/callback` for it:
   1. the ember registers a PENDING sign-in {state, authorize_url} via
      /v1/plugins/self/oauth/pending (its page or its sidecar);
   2. the dashboard opens exactly that authorize_url (an ember page is
      a sandboxed iframe without popups — it asks the shell, and the
      shell opens only the URL the daemon holds);
   3. the provider redirects back with ?code&state; the daemon checks
      the state (single use, expiring), seals the code as the ember's
      `__oauth_code` secret, and touches the bus;
   4. the sidecar takes the code ONCE (/v1/plugins/self/oauth/take)
      and exchanges it with its client secret.
   Pending sign-ins live in memory on the node the redirect address
   points at — start sign-in from that node's dashboard. */

pub struct PendingOAuth {
    pub state: String,
    pub authorize_url: String,
    pub expires: std::time::Instant,
}

fn oauth_pending() -> &'static std::sync::Mutex<std::collections::HashMap<String, PendingOAuth>> {
    static MAP: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, PendingOAuth>>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Register (or replace) an ember's pending sign-in.
pub fn oauth_register(name: &str, state: &str, authorize_url: &str, ttl_secs: u64) -> Result<()> {
    if state.len() < 16 || state.len() > 256 || !state.chars().all(|c| c.is_ascii_graphic()) {
        bail!("state must be 16-256 printable characters (use a random value)");
    }
    let url = reqwest::Url::parse(authorize_url).map_err(|_| anyhow!("authorize_url isn't a URL"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        bail!("authorize_url must be an https address");
    }
    if !url.query_pairs().any(|(k, v)| k == "state" && v == state) {
        bail!("authorize_url must carry the same state parameter");
    }
    let ttl = ttl_secs.clamp(60, 3600);
    oauth_pending().lock().unwrap().insert(
        name.to_string(),
        PendingOAuth {
            state: state.to_string(),
            authorize_url: authorize_url.to_string(),
            expires: std::time::Instant::now() + std::time::Duration::from_secs(ttl),
        },
    );
    Ok(())
}

/// The authorize URL the dashboard may open for this ember, if a
/// sign-in is pending and fresh.
pub fn oauth_authorize_url(name: &str) -> Option<String> {
    let map = oauth_pending().lock().unwrap();
    map.get(name)
        .filter(|p| p.expires > std::time::Instant::now())
        .map(|p| p.authorize_url.clone())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Consume the pending sign-in when `state` matches (single use).
pub fn oauth_consume(name: &str, state: &str) -> bool {
    let mut map = oauth_pending().lock().unwrap();
    let ok = map
        .get(name)
        .is_some_and(|p| p.expires > std::time::Instant::now() && ct_eq(p.state.as_bytes(), state.as_bytes()));
    if ok {
        map.remove(name);
    }
    ok
}

/* ————— the single-runner lease (peered setups) —————
   A mirror-sync'd ember with a sidecar must run in ONE place across a
   pair — two samplers double-ingest and fight over state. The lease
   is a reserved `__runner` key in the ember's own MIRRORED KV (it
   replicates over the existing /v1/sync/plugins leg): the running
   node heartbeats {node, ts}; a peer seeing a fresh foreign lease
   holds its sidecar back, and takes over when the lease goes stale
   (>10 min — the LLM-delegation take-the-role-back shape). Plugin
   self-storage rejects `__`-prefixed keys, so an ember can't forge
   its own lease. */

pub const RUNNER_KEY: &str = "__runner";
const LEASE_STALE_SECS: i64 = 600;

fn node_id() -> String {
    let host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "node".into());
    format!("{}:{host}", crate::monitor::node_kind())
}

fn lease_of(state: &AppState, name: &str) -> Option<(String, String)> {
    let raw = state.store.plugin_kv_get(name, RUNNER_KEY).ok()??;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some((
        v.get("node")?.as_str()?.to_string(),
        v.get("ts")?.as_str()?.to_string(),
    ))
}

fn lease_fresh(ts: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds() < LEASE_STALE_SECS)
        .unwrap_or(false)
}

/// Does an ember need the lease at all? Only mirror-sync'd sidecars —
/// everything else runs wherever it's installed.
fn needs_lease(manifest: &Manifest, sync: &str) -> bool {
    sync == "mirror"
        && !manifest.launch.is_empty()
        && manifest.runner.as_deref() != Some("per-node")
}

/// Checked at install/enable: don't launch over a fresh foreign lease.
fn lease_allows_launch(state: &AppState, manifest: &Manifest) -> bool {
    let row = match state.store.plugin(&manifest.name) {
        Ok(Some(r)) => r,
        _ => return true, // not registered yet (install path) — tick sorts it out
    };
    if !needs_lease(manifest, &row.sync) {
        return true;
    }
    !matches!(lease_of(state, &manifest.name),
        Some((node, ts)) if node != node_id() && lease_fresh(&ts))
}

/// One lease pass over every enabled ember — called every ~60s from
/// the monitor loop (already on the blocking pool). Claims/refreshes
/// where this node runs the sidecar; stands down when a peer holds a
/// fresh lease; takes over when it goes stale.
pub fn runner_tick(state: &AppState) {
    let rows = match state.store.list_plugins() {
        Ok(r) => r,
        Err(_) => return,
    };
    for row in rows.into_iter().filter(|r| r.enabled) {
        let manifest: Manifest = match serde_json::from_value(row.manifest.clone()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !needs_lease(&manifest, &row.sync) {
            continue;
        }
        let can_run_here = manifest.placement_allows_here() && launch_entry(&manifest).is_some();
        let me = node_id();
        let lease = lease_of(state, &row.name);
        let foreign_fresh = matches!(&lease, Some((node, ts)) if *node != me && lease_fresh(ts));
        let running = sidecar_running(&row.name);
        if foreign_fresh {
            if running {
                tracing::info!(
                    "ember {}: standing down — running on {}",
                    row.name,
                    lease.as_ref().map(|(n, _)| n.as_str()).unwrap_or("peer")
                );
                sidecar_stop(&row.name);
            }
            continue;
        }
        if !can_run_here {
            continue; // nobody home to run it; leave the lease for a node that can
        }
        // claim/refresh, then make sure it's actually up
        let _ = state.store.plugin_kv_set(
            &row.name,
            RUNNER_KEY,
            &json!({"node": me, "ts": hearthloom_core::time::now_utc()}).to_string(),
        );
        if !running {
            let _ = sidecar_start(state, &manifest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_satisfies() {
        assert!(version_satisfies("0.5.25", ">=0.5.25"));
        assert!(version_satisfies("0.5.26", ">=0.5.25"));
        assert!(!version_satisfies("0.5.24", ">=0.5.25"));
        assert!(version_satisfies("0.10.0", ">=0.9.0"));
        assert!(version_satisfies("1.0.0", ">0.9.9"));
        assert!(version_satisfies("0.5.25", "0.5.0")); // bare = >=
        assert!(!version_satisfies("0.5.25", "=0.5.24"));
    }

    #[test]
    fn test_manifest_validation() {
        let ok: Manifest = serde_json::from_value(json!({
            "protocol": 1, "name": "presence-sampler", "version": "0.1.0",
            "scopes": ["ingest"], "unknown_future_field": {"x": 1}
        }))
        .unwrap();
        ok.validate().unwrap();

        let bad_scope: Manifest = serde_json::from_value(json!({
            "protocol": 1, "name": "x", "version": "1", "scopes": ["admin"]
        }))
        .unwrap();
        assert!(bad_scope.validate().is_err());

        let bad_proto: Manifest = serde_json::from_value(json!({
            "protocol": 2, "name": "x", "version": "1"
        }))
        .unwrap();
        assert!(bad_proto.validate().is_err());

        for name in ["", "Self", "has space", "self", "-lead", &"a".repeat(41)] {
            let m: Manifest = serde_json::from_value(json!({
                "protocol": 1, "name": name, "version": "1"
            }))
            .unwrap();
            assert!(m.validate().is_err(), "{name:?} should be refused");
        }
    }

    #[test]
    fn test_route_table_default_deny() {
        let scopes = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // no scopes: only the self surface + bus
        assert!(plugin_route_allowed("/v1/plugins/self/kv/a", "PUT", &[]));
        assert!(plugin_route_allowed("/v1/bus/stream", "GET", &[]));
        assert!(!plugin_route_allowed("/v1/search", "GET", &[]));
        // ingest: exactly the write trio
        let ing = scopes(&["ingest"]);
        assert!(plugin_route_allowed("/v1/ingest", "POST", &ing));
        assert!(plugin_route_allowed("/v1/sources/virtual", "POST", &ing));
        assert!(plugin_route_allowed("/v1/sources/virtual/x/promote", "POST", &ing));
        assert!(plugin_route_allowed("/v1/ingest/movement", "POST", &ing));
        assert!(!plugin_route_allowed("/v1/ingest/location", "POST", &ing), "the person's location stays core's");
        assert!(!plugin_route_allowed("/v1/vehicles", "GET", &scopes(&["read"])));
        assert!(!plugin_route_allowed("/v1/trips/questions", "GET", &scopes(&["read"])));
        assert!(!plugin_route_allowed("/v1/search", "GET", &ing));
        assert!(!plugin_route_allowed("/v1/notifications", "POST", &ing));
        // read: broad GET minus the denied prefixes
        let rd = scopes(&["read"]);
        assert!(plugin_route_allowed("/v1/search", "GET", &rd));
        assert!(plugin_route_allowed("/v1/stats", "GET", &rd));
        assert!(!plugin_route_allowed("/v1/export/start", "GET", &rd));
        assert!(!plugin_route_allowed("/v1/vault", "GET", &rd));
        assert!(!plugin_route_allowed("/v1/keys", "GET", &rd));
        assert!(!plugin_route_allowed("/v1/plugins", "GET", &rd));
        assert!(!plugin_route_allowed("/v1/search", "POST", &rd));
        assert!(!plugin_route_allowed("/v1/ingest", "POST", &rd));
    }

    #[test]
    fn test_publish_paths() {
        assert!(valid_publish_path("/.well-known/appspecific/com.tesla.3p.public-key.pem"));
        assert!(valid_publish_path("/.well-known/security.txt"));
        for bad in [
            "/.well-known/", "/.well-known/../x", "/.well-known/a//b", "/assets/x",
            "/.well-known/a b", "/.well-known/./x", "/.well-known/%2e%2e/x",
        ] {
            assert!(!valid_publish_path(bad), "{bad:?} should be refused");
        }
        let m = |publish: Value| -> Manifest {
            serde_json::from_value(json!({"protocol": 1, "name": "x", "version": "1", "publish": publish})).unwrap()
        };
        m(json!([{"path": "/.well-known/appspecific/k.pem", "file": "pub/k.pem"}])).validate().unwrap();
        assert!(m(json!([{"path": "/robots.txt", "file": "r"}])).validate().is_err(), "outside .well-known");
        assert!(m(json!([{"path": "/.well-known/k", "file": "../key"}])).validate().is_err(), "file escapes data");
        assert!(m(json!([
            {"path": "/.well-known/k", "file": "a"}, {"path": "/.well-known/k", "file": "b"}
        ])).validate().is_err(), "a path twice");
    }

    #[test]
    fn test_launch_entry_prefers_exact_target() {
        let os_key = if cfg!(target_os = "macos") { "darwin" } else { std::env::consts::OS };
        let m: Manifest = serde_json::from_value(json!({
            "protocol": 1, "name": "x", "version": "1",
            "launch": {os_key: "bin/generic", host_target(): "bin/exact"}
        })).unwrap();
        assert_eq!(launch_entry(&m).map(String::as_str), Some("bin/exact"));
        let m: Manifest = serde_json::from_value(json!({
            "protocol": 1, "name": "x", "version": "1", "launch": {os_key: "bin/generic"}
        })).unwrap();
        assert_eq!(launch_entry(&m).map(String::as_str), Some("bin/generic"));
        let m: Manifest = serde_json::from_value(json!({
            "protocol": 1, "name": "x", "version": "1", "launch": {"plan9": "bin/p"}
        })).unwrap();
        assert!(launch_entry(&m).is_none(), "no binary for this platform = no sidecar");
    }

    #[test]
    fn test_oauth_state_is_single_use_and_checked() {
        let name = format!("oauth-{}", uuid::Uuid::new_v4().simple());
        let st = "s3cr3t-state-0123456789";
        let url = format!("https://auth.example.com/authorize?client_id=c&state={st}");
        assert!(oauth_register(&name, st, "http://auth.example.com/a?state=s3cr3t-state-0123456789", 900).is_err(), "https only");
        assert!(oauth_register(&name, st, "https://auth.example.com/a?state=other", 900).is_err(), "state must match");
        assert!(oauth_register(&name, "short", "https://a.example.com/?state=short", 900).is_err(), "weak state");
        oauth_register(&name, st, &url, 900).unwrap();
        assert_eq!(oauth_authorize_url(&name).as_deref(), Some(url.as_str()));
        assert!(!oauth_consume(&name, "s3cr3t-state-0123456780"), "wrong state");
        assert!(oauth_consume(&name, st));
        assert!(!oauth_consume(&name, st), "single use");
        assert!(oauth_authorize_url(&name).is_none());
    }

    #[test]
    fn test_secrets_roundtrip_sealed_at_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path());
        home.init().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(hearthloom_core::store::Store::open(&home.db_path()).unwrap()),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        secret_put(&state, "demo", "client_secret", b"ta-secret.0123456789").unwrap();
        let raw = std::fs::read(plugin_dir(&state, "demo").join("secrets/client_secret.sealed")).unwrap();
        assert!(!raw.windows(9).any(|w| w == b"ta-secret"), "no plaintext on disk");
        assert_eq!(secret_get(&state, "demo", "client_secret").unwrap().unwrap(), b"ta-secret.0123456789");
        assert!(secret_get(&state, "demo", "missing").unwrap().is_none());
        assert!(secret_get(&state, "other", "client_secret").unwrap().is_none(), "per-ember namespaces");
        assert!(secret_put(&state, "demo", "../escape", b"x").is_err());
        assert!(secret_put(&state, "demo", ".hidden", b"x").is_err());
        assert!(secret_put(&state, "demo", "big", &vec![b'x'; SECRET_MAX_BYTES + 1]).is_err());
        let listed = secret_list(&state, "demo");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["key"], "client_secret");
        assert!(listed[0].get("value").is_none(), "listing never carries values");
        assert!(secret_delete(&state, "demo", "client_secret"));
        assert!(secret_get(&state, "demo", "client_secret").unwrap().is_none());
    }

    #[test]
    fn test_safe_rel_path() {
        assert!(safe_rel_path("state/today.json").is_ok());
        assert!(safe_rel_path("../escape").is_err());
        assert!(safe_rel_path("a/../../b").is_err());
        assert!(safe_rel_path(".hidden").is_err());
        assert!(safe_rel_path("").is_err());
        assert_eq!(safe_rel_path("/leading/slash").unwrap(), "leading/slash");
    }
}
