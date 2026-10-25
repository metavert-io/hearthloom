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
    // "signature" is reserved (additive) for a registry's countersignature.
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        if self.protocol != PROTOCOL {
            bail!(
                "manifest protocol {} unsupported (this node speaks {PROTOCOL})",
                self.protocol
            );
        }
        let ok_name = !self.name.is_empty()
            && self.name.len() <= 40
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !self.name.starts_with('-');
        if !ok_name {
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

/// Write + load the launchd unit for a plugin with a darwin launch
/// entry. No-op elsewhere (cloud nodes run no local sidecars in v1).
fn launchd_install(state: &AppState, manifest: &Manifest) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let Some(rel) = manifest.launch.get("darwin") else {
        return Ok(());
    };
    let name = &manifest.name;
    let bin = plugin_dir(state, name).join("pkg").join(rel);
    if !bin.exists() {
        bail!("manifest launch.darwin points at {rel:?}, which isn't in the package");
    }
    // The binary must be executable — zips don't always preserve mode.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755));
    }
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
pub fn sidecar_running(name: &str) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::process::Command::new("launchctl")
        .args(["list", &launchd_label(name)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
    if let Some(req) = manifest.host_requirement() {
        let current = env!("CARGO_PKG_VERSION");
        if !version_satisfies(current, &req) {
            bail!(
                "{} needs hearthloom {req} (this node is {current}) — upgrade hearthloom first",
                manifest.name
            );
        }
    }
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
    let dir = plugin_dir(state, &name);
    let pkg = dir.join("pkg");
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
        launchd_install(state, &manifest)?;
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
    launchd_remove(name);
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
    state
        .store
        .set_plugin_enabled(&Actor::new("human:dashboard"), name, enabled)?;
    if enabled {
        let manifest: Manifest = serde_json::from_value(row.manifest)?;
        if manifest.placement_allows_here() && lease_allows_launch(state, &manifest) {
            launchd_install(state, &manifest)?;
        }
    } else {
        launchd_remove(name);
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
        if let Some(n) = ui.get("nav").and_then(&entry) {
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
        let can_run_here = manifest.placement_allows_here()
            && cfg!(target_os = "macos")
            && manifest.launch.contains_key("darwin");
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
                launchd_remove(&row.name);
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
            let _ = launchd_install(state, &manifest);
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
    fn test_safe_rel_path() {
        assert!(safe_rel_path("state/today.json").is_ok());
        assert!(safe_rel_path("../escape").is_err());
        assert!(safe_rel_path("a/../../b").is_err());
        assert!(safe_rel_path(".hidden").is_err());
        assert!(safe_rel_path("").is_err());
        assert_eq!(safe_rel_path("/leading/slash").unwrap(), "leading/slash");
    }
}
