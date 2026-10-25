//! The Integrations surface: lifecycle for optional external systems.
//!
//! HearthFS here means *managing the mount* (a LaunchAgent owns the
//! `hearthfs mount` process so it survives reboots; disable = bootout +
//! unmount). The virtual-source read path itself lives in
//! `virtual_source.rs` and degrades cleanly when the mount is absent —
//! nothing in the substrate ever depends on it (design law #1).
//!
//! Also home to the web-upload path into the inbox drop folder: the
//! browser POSTs a file, it lands in `~/.hearthloom/inbox/` exactly as a
//! Finder drop would, and the existing watcher ingests it. Uploads are
//! written to a dotfile first (the watcher skips dotfiles) then renamed,
//! so a half-received body is never ingested.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use hearthloom_core::store::Actor;
use serde::Serialize;
use serde_json::json;

use crate::AppState;

/// The launchd label owning the mount process. Hearthloom's agent, even
/// though the binary it runs is hearthfs — hearthloom manages the mount.
const AGENT_LABEL: &str = "com.hearthloom.hearthfs";
/// Where hearthloom expects the HearthFS mount (matches the registered
/// virtual source; fixed — the *source dir* is what the user configures).
const MOUNTPOINT: &str = "~/.hearthloom/mounts/hearthfs";
const DEFAULT_PORT: u16 = 11171;
/// The inbox watcher refuses .mbox over this — full Takeouts go through
/// the filtered CLI pipeline instead (see inbox.rs MBOX_AUTO_MAX_BYTES).
const MBOX_MAX_BYTES: usize = 250 * 1024 * 1024;

fn home_path(rel: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/{rel}")
}

fn expand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        home_path(rest)
    } else {
        p.to_string()
    }
}

fn plist_path() -> PathBuf {
    PathBuf::from(home_path(&format!("Library/LaunchAgents/{AGENT_LABEL}.plist")))
}

fn uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "501".into())
}

/// Find a hearthfs binary: explicit config first, then the usual homes.
fn find_binary(configured: Option<&str>) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(b) = configured {
        candidates.push(expand(b));
    }
    if let Ok(b) = std::env::var("HEARTHFS_BIN") {
        candidates.push(expand(&b));
    }
    candidates.push(home_path(".local/bin/hearthfs"));
    candidates.push(home_path(".cargo/bin/hearthfs"));
    candidates.into_iter().find(|c| PathBuf::from(c).is_file())
}

#[derive(Debug, Serialize)]
pub struct HearthfsStatus {
    pub registered: bool,
    pub enabled: bool,
    pub online: bool,
    pub agent_installed: bool,
    pub mount: String,
    pub source_dir: Option<String>,
    pub port: u16,
    pub binary: Option<String>,
    /// The exact command to run by hand when we can't manage the mount.
    pub manual_command: Option<String>,
}

pub fn hearthfs_status(state: &AppState) -> Result<HearthfsStatus> {
    let def = state.store.virtual_source("hearthfs")?;
    let (registered, enabled, source_dir, port, binary_cfg) = match &def {
        Some(d) => (
            true,
            d.config.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true),
            d.config
                .get("source_dir")
                .and_then(|s| s.as_str())
                .map(String::from),
            d.config
                .get("port")
                .and_then(|p| p.as_u64())
                .map(|p| p as u16)
                .unwrap_or(DEFAULT_PORT),
            d.config
                .get("binary")
                .and_then(|b| b.as_str())
                .map(String::from),
        ),
        None => (false, false, None, DEFAULT_PORT, None),
    };
    let binary = find_binary(binary_cfg.as_deref());
    let mountpoint = expand(MOUNTPOINT);
    // NEVER touch the mount on the calling thread: a wedged userspace
    // filesystem puts the caller into UNINTERRUPTIBLE kernel wait — one
    // Integrations page-load per tokio worker froze the whole daemon on
    // 2026-07-17. Probe on a throwaway thread with a hard deadline; a
    // timeout means "not answering", and the leaked blocked thread is
    // contained instead of starving the runtime.
    let online = {
        let probe_path = mountpoint.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let alive = std::fs::read_dir(&probe_path)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false);
            let _ = tx.send(alive);
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap_or(false)
    };
    let manual_command = match (&binary, &source_dir) {
        (Some(b), Some(s)) => Some(format!("{b} mount {s} {mountpoint} --port {port}")),
        _ => None,
    };
    Ok(HearthfsStatus {
        registered,
        enabled,
        online,
        agent_installed: plist_path().is_file(),
        mount: MOUNTPOINT.into(),
        source_dir,
        port,
        binary,
        manual_command,
    })
}

/// Save config (and optionally flip `enabled`) on the registered source.
fn save_config(
    state: &AppState,
    actor: &Actor,
    source_dir: &str,
    port: u16,
    binary: Option<&str>,
    enabled: bool,
) -> Result<()> {
    state.store.register_virtual_source(
        actor,
        "hearthfs",
        "hearthfs",
        &json!({
            "mount": MOUNTPOINT,
            "magic": true,
            "trust": "imported",
            "enabled": enabled,
            "source_dir": source_dir,
            "port": port,
            "binary": binary,
        }),
        &["search".into(), "fetch".into(), "promote".into()],
    )
}

/// Enable: persist config, write the LaunchAgent, bootstrap it. The agent
/// (not this daemon) owns the mount process, so it survives daemon
/// restarts and reboots alike.
pub fn hearthfs_enable(
    state: &AppState,
    actor: &Actor,
    source_dir: &str,
    port: Option<u16>,
    binary: Option<&str>,
) -> Result<HearthfsStatus> {
    let source = expand(source_dir);
    if !PathBuf::from(&source).is_dir() {
        bail!("source directory {source:?} does not exist — pick the folder HearthFS should index");
    }
    let port = port.unwrap_or(DEFAULT_PORT);
    let binary = find_binary(binary)
        .ok_or_else(|| anyhow!(
            "no hearthfs binary found (looked in HEARTHFS_BIN, ~/.local/bin and \
             ~/.cargo/bin) — install HearthFS first, or set HEARTHFS_BIN"
        ))?;
    save_config(state, actor, &source, port, Some(&binary), true)?;

    let mountpoint = expand(MOUNTPOINT);
    std::fs::create_dir_all(&mountpoint).ok();
    let log = home_path(".hearthloom/hearthfs-mount.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>mount</string>
    <string>{source}</string>
    <string>{mountpoint}</string>
    <string>--port</string>
    <string>{port}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    );
    let path = plist_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&path, plist).with_context(|| format!("writing {}", path.display()))?;

    let uid = uid();
    // A previous incarnation (agent or manual mount) may hold the
    // mountpoint — clear both before bootstrapping.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{AGENT_LABEL}")])
        .output();
    let _ = Command::new(&binary).args(["unmount", &mountpoint]).output();
    let boot = Command::new("launchctl")
        .args(["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()])
        .output()
        .context("running launchctl bootstrap")?;
    if !boot.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&boot.stderr).trim()
        );
    }
    // Mount + first index take a moment; report what we can see now.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    hearthfs_status(state)
}

/// Disable: bootout the agent, remove the plist, best-effort unmount, and
/// mark the source disabled so the RLM prompt stops advertising it.
pub fn hearthfs_disable(state: &AppState, actor: &Actor) -> Result<HearthfsStatus> {
    let status = hearthfs_status(state)?;
    let uid = uid();
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("gui/{uid}/{AGENT_LABEL}")])
        .output();
    let _ = std::fs::remove_file(plist_path());
    if let Some(binary) = &status.binary {
        let _ = Command::new(binary)
            .args(["unmount", &expand(MOUNTPOINT)])
            .output();
    }
    if status.registered {
        save_config(
            state,
            actor,
            status.source_dir.as_deref().unwrap_or(""),
            status.port,
            status.binary.as_deref(),
            false,
        )?;
    }
    hearthfs_status(state)
}

/// ————— the Anthropic key (the LLM dependency), managed from the UI —————
/// The key lives in ~/.hearthloom/.env (0600 — on the cloud node that
/// path is the persistent volume, so the same UI works there). Saving
/// validates against the API (a models list — no tokens spent), then
/// schedules a daemon restart; launchd/Fly bring it back in seconds
/// with the provider loaded.
/// The env var holding a provider's API key.
pub fn provider_key_env(provider: &str) -> &'static str {
    match provider {
        "openai" => "OPENAI_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "grok" => "GROK_API_KEY",
        // Optional — a loopback server usually has none.
        "custom" => "CUSTOM_LLM_API_KEY",
        _ => "ANTHROPIC_API_KEY",
    }
}

/// Is an OpenAI-compatible endpoint answering at `base_url`? A model-list
/// GET (no tokens); the key is sent only when there is one.
pub async fn validate_endpoint(base_url: &str, key: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let mut req = client.get(format!("{base_url}/models"));
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    let res = req
        .send()
        .await
        .map_err(|e| anyhow!("couldn't reach {base_url} — is the server running? ({e})"))?;
    match res.status().as_u16() {
        401 | 403 => bail!("{base_url} rejected the key — check it and try again"),
        s if (200..300).contains(&s) => Ok(()),
        s => bail!("{base_url}/models answered {s} — is this an OpenAI-compatible /v1 address?"),
    }
}

pub fn provider_configured(provider: &str) -> bool {
    std::env::var(provider_key_env(provider)).map(|k| !k.trim().is_empty()).unwrap_or(false)
}

/// Back-compat shim (the old dashboard route).
pub fn anthropic_configured() -> bool {
    provider_configured("anthropic")
}

/// Cheap API-backed key check per provider (a model-list GET — no tokens).
pub async fn validate_key(provider: &str, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("no API key given");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let req = match provider {
        "anthropic" => client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        "openai" => client.get("https://api.openai.com/v1/models").bearer_auth(key),
        "grok" => client.get("https://api.x.ai/v1/models").bearer_auth(key),
        "gemini" => client
            .get("https://generativelanguage.googleapis.com/v1beta/models")
            .header("x-goog-api-key", key),
        other => bail!("unknown provider {other}"),
    };
    let res = req.send().await.map_err(|e| anyhow!("couldn't reach {provider} to validate: {e}"))?;
    match res.status().as_u16() {
        401 | 403 => bail!("{provider} rejected that key — check it and try again"),
        s if (200..300).contains(&s) => Ok(()),
        s => bail!("{provider} answered {s} while validating the key"),
    }
}

/// Persist a provider's API key (seal-aware; survives the sealed regime).
pub async fn set_provider_key(home: &hearthloom_core::Home, provider: &str, key: &str) -> Result<()> {
    validate_key(provider, key).await?;
    hearthloom_core::secrets::set_env_vars(home.root(), &[(provider_key_env(provider), key.trim())])?;
    Ok(())
}

/// Accept a browser-uploaded file into the inbox drop folder. Returns the
/// final filename (the watcher's non-clobber rule may add a prefix).
pub fn inbox_accept(home: &hearthloom_core::Home, filename: &str, bytes: &[u8]) -> Result<String> {
    let name = filename.trim();
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains("..")
        || name.contains('\\')
    {
        bail!("invalid filename {name:?}");
    }
    let lower = name.to_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    let known_name = lower == "chat.db" || lower == "photos.sqlite";
    let known_ext = matches!(ext, "mbox" | "ics" | "json" | "zip" | "tgz" | "gz");
    if !known_name && !known_ext {
        bail!(
            "unsupported file type .{ext} — the inbox takes mbox, ics, json, \
             zip/tgz, chat.db, Photos.sqlite"
        );
    }
    if ext == "mbox" && bytes.len() > MBOX_MAX_BYTES {
        bail!(
            "mbox over 250MB — run the filtered pipeline instead: \
             hearthloom ingest mbox <file> --include-labels …"
        );
    }
    let inbox = home.inbox_dir();
    std::fs::create_dir_all(&inbox)?;
    // Dotfile first (the watcher skips dotfiles), then rename into place.
    let part = inbox.join(format!(".upload-{name}.part"));
    std::fs::write(&part, bytes).with_context(|| format!("writing {}", part.display()))?;
    let mut dest = inbox.join(name);
    let mut n = 1;
    while dest.exists() {
        dest = inbox.join(format!("{n}-{name}"));
        n += 1;
    }
    std::fs::rename(&part, &dest)?;
    Ok(dest
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string()))
}
