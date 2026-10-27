//! The operator + capture layer (2026-07-21): auth, status, doctor,
//! keys, vault, export, sync, tasks/notes quick capture, ember dev
//! tools, notify, logs. Everything talks HTTP like the rest of the CLI;
//! the one local secret is the CLI's own PAT, kept in the macOS login
//! keychain (service `hearthloom-cli`, account = the node's base URL) —
//! a file fallback (~/.hearthloom/cli-key.<host>, 0600) covers linux.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::client::Client;

const ACTOR: &str = "human:cli";
const KEYCHAIN_SERVICE: &str = "hearthloom-cli";

fn home() -> hearthloom_core::Home {
    hearthloom_core::Home::resolve().expect("resolving ~/.hearthloom")
}

/* ————— key storage ————— */

fn key_file_path(base_url: &str) -> std::path::PathBuf {
    let slug: String = base_url
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    home().root().join(format!("cli-key.{slug}"))
}

pub fn stored_key(base_url: &str) -> Option<String> {
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                base_url,
                "-w",
            ])
            .output()
            .ok()?;
        if out.status.success() {
            let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !key.is_empty() {
                return Some(key);
            }
        }
        None
    } else {
        std::fs::read_to_string(key_file_path(base_url))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

fn store_key(base_url: &str, key: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        // -U updates in place on re-login
        let out = std::process::Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-U",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                base_url,
                "-w",
                key,
            ])
            .output()
            .context("running security add-generic-password")?;
        if !out.status.success() {
            bail!(
                "keychain store failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    } else {
        let path = key_file_path(base_url);
        std::fs::write(&path, key)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

fn forget_key(base_url: &str) {
    if cfg!(target_os = "macos") {
        let _ = std::process::Command::new("/usr/bin/security")
            .args([
                "delete-generic-password",
                "-s",
                KEYCHAIN_SERVICE,
                "-a",
                base_url,
            ])
            .output();
    } else {
        let _ = std::fs::remove_file(key_file_path(base_url));
    }
}

/// Prompt without echo (POSIX stty; good enough for macOS + linux).
fn prompt_secret(label: &str) -> Result<String> {
    use std::io::Write;
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    let _ = std::process::Command::new("stty").arg("-echo").status();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let _ = std::process::Command::new("stty").arg("echo").status();
    eprintln!();
    Ok(line.trim().to_string())
}

fn prompt(label: &str) -> Result<String> {
    use std::io::Write;
    eprint!("{label}: ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

/* ————— auth ————— */

/// Sign in → mint a CLI-owned admin PAT → keychain. The session exists
/// only for the mint; the PAT (revocable in ☰ → API Keys as
/// `cli:<hostname>`) is what every later command sends.
pub async fn auth_login(base_url: &str, email: Option<String>) -> Result<()> {
    let email = match email {
        Some(e) => e,
        None => prompt("email")?,
    };
    let password = prompt_secret("password")?;
    let http = reqwest::Client::builder().build()?;
    let res = http
        .post(format!("{base_url}/v1/auth/login"))
        .json(&json!({"email": email, "password": password}))
        .send()
        .await
        .with_context(|| format!("couldn't reach {base_url}"))?;
    if !res.status().is_success() {
        let msg: Value = res.json().await.unwrap_or_default();
        bail!(
            "{}",
            msg.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("sign-in refused")
        );
    }
    let cookie = res
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("hl_session="))
        .map(|v| v.split(';').next().unwrap_or_default().to_string())
        .ok_or_else(|| anyhow!("login succeeded but no session cookie came back"))?;
    let host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "shell".into());
    let minted: Value = http
        .post(format!("{base_url}/v1/keys"))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "actor": ACTOR,
            "name": format!("cli:{host}"),
            "scopes": ["admin"],
        }))
        .send()
        .await?
        .json()
        .await?;
    let secret = minted
        .get("secret")
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("key mint failed: {minted}"))?;
    store_key(base_url, secret)?;
    println!(
        "✓ signed in — a key named cli:{host} now lives in your {} (revoke it any time in ☰ → API Keys)",
        if cfg!(target_os = "macos") { "login keychain" } else { "~/.hearthloom/cli-key file" }
    );
    Ok(())
}

pub async fn auth_status(client: &Client) -> Result<()> {
    let has_key =
        stored_key(client.base_url()).is_some() || std::env::var("HEARTHLOOM_API_KEY").is_ok();
    match client.get("/v1/stats", &[]).await {
        Ok(_) => println!(
            "✓ {} answers and accepts this shell's credentials ({})",
            client.base_url(),
            if has_key {
                "stored key"
            } else {
                "open posture — no key needed"
            },
        ),
        Err(e) => println!("✗ {}: {e}", client.base_url()),
    }
    Ok(())
}

pub async fn auth_logout(client: &Client) -> Result<()> {
    // best effort: revoke server-side, then forget locally either way
    let _ = client
        .post("/v1/keys/self-revoke", &json!({"actor": ACTOR}))
        .await;
    forget_key(client.base_url());
    println!("✓ key forgotten locally (and revoked on the node if it was reachable)");
    Ok(())
}

/* ————— status ————— */

pub async fn status(client: &Client, as_json: bool) -> Result<()> {
    let health = client.get("/health", &[]).await?;
    let stats = client.get("/v1/stats", &[]).await.ok();
    let vault = client.get("/v1/vault", &[]).await.ok();
    let imports = client.get("/v1/integrations/imports", &[]).await.ok();
    let peer = client.get("/v1/peer", &[]).await.ok();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "health": health, "stats": stats, "vault": vault,
                "imports": imports, "peer": peer,
            }))?
        );
        return Ok(());
    }
    let g = |v: &Option<Value>, path: &[&str]| -> Value {
        let mut cur = match v {
            Some(v) => v.clone(),
            None => return Value::Null,
        };
        for p in path {
            cur = cur.get(p).cloned().unwrap_or(Value::Null);
        }
        cur
    };
    println!(
        "hearthloom {} at {}",
        health
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        client.base_url()
    );
    if let Some(s) = &stats {
        println!(
            "  record   {} episodes · {} entities · {} edges",
            g(&stats, &["episodes"]),
            s.get("entities").unwrap_or(&Value::Null),
            g(&stats, &["edges"])
        );
        let tend = [
            "review_pending",
            "quarantine_pending",
            "propositions_pending",
            "opportunities_pending",
        ]
        .iter()
        .map(|k| s.get(*k).and_then(|v| v.as_i64()).unwrap_or(0))
        .sum::<i64>();
        if tend > 0 {
            println!("  tend     {tend} waiting for you");
        }
    } else {
        println!("  record   (stats need auth — run `hearthloom auth login`)");
    }
    let locked = g(&vault, &["locked"]);
    if !locked.is_null() {
        println!(
            "  vault    {}",
            if locked == json!(true) {
                "locked"
            } else {
                "UNLOCKED"
            }
        );
    }
    if let Some(imp) = &imports {
        let delegated = imp
            .get("llm_delegated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let here = imp
            .get("llm_here")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let blocked = imp
            .get("llm_blocked")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let pending: i64 = imp
            .get("interpreting")
            .and_then(|v| v.as_object())
            .map(|m| {
                m.values()
                    .filter_map(|e| e.get("pending").and_then(|p| p.as_i64()))
                    .sum()
            })
            .unwrap_or(0);
        let llm = if blocked {
            "PAUSED (provider needs attention)".to_string()
        } else if delegated {
            "delegated to the always-on peer".to_string()
        } else if here {
            "active on this node".to_string()
        } else {
            "no provider".to_string()
        };
        println!(
            "  thinking {llm}{}",
            if pending > 0 {
                format!(" · {pending} to interpret")
            } else {
                String::new()
            }
        );
    }
    if let Some(p) = &peer {
        if !p.get("url").map(Value::is_null).unwrap_or(true) {
            println!(
                "  peer     {} · last sync {}",
                g(&peer, &["url"]).as_str().unwrap_or("?"),
                g(&peer, &["last_outcome", "ts"])
                    .as_str()
                    .unwrap_or("never"),
            );
        }
    }
    Ok(())
}

/* ————— doctor ————— */

pub async fn doctor(client: &Client) -> Result<()> {
    let mut findings = 0usize;
    let mut warn = |msg: String| {
        findings += 1;
        println!("  ✗ {msg}");
    };
    println!("hearthloom doctor — encoding every wedge we've ever debugged:");

    // 1. how many daemons? (the orphan-daemon trap)
    let ps = std::process::Command::new("pgrep")
        .args(["-fl", "hearthloom daemon"])
        .output()?;
    let lines: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .map(String::from)
        .collect();
    match lines.len() {
        0 => println!("  · no daemon running (one auto-spawns on demand)"),
        1 => println!("  ✓ exactly one daemon: {}", lines[0]),
        n => warn(format!(
            "{n} daemons are fighting for the port — kill the target/ ones:\n      {}",
            lines.join("\n      ")
        )),
    }
    if lines.iter().any(|l| l.contains("/target/")) {
        warn("a daemon is running from a BUILD dir (target/…) — auto-spawned; the launchd one should win".into());
    }

    // 2. launchd points at the right binary (the deploy-binary trap)
    if cfg!(target_os = "macos") {
        let uid = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
        let out = std::process::Command::new("launchctl")
            .args(["print", &format!("gui/{uid}/com.hearthloom.daemon")])
            .output()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() {
            warn(
                "launchd agent com.hearthloom.daemon isn't loaded (scripts/install.sh installs it)"
                    .into(),
            );
        } else if let Some(line) = text
            .lines()
            .find(|l| l.trim_start().starts_with("program ="))
        {
            if line.contains(".local/bin/hearthloom") {
                println!("  ✓ launchd runs the installed CLI binary");
            } else {
                warn(format!(
                    "launchd runs {} — deploys to ~/.local/bin won't take",
                    line.trim()
                ));
            }
        }
    }

    // 3. the daemon answers, and versions agree
    match client.get("/health", &[]).await {
        Ok(h) => {
            let daemon_v = h.get("version").and_then(|v| v.as_str()).unwrap_or("?");
            let cli_v = env!("CARGO_PKG_VERSION");
            if daemon_v == cli_v {
                println!(
                    "  ✓ daemon answers at {} (v{daemon_v}, matches this CLI)",
                    client.base_url()
                );
            } else {
                warn(format!("daemon is v{daemon_v} but this CLI is v{cli_v} — kickstart the daemon after deploys"));
            }
        }
        Err(e) => warn(format!("no answer from {}: {e}", client.base_url())),
    }

    // 4. auth reaches past the door
    match client.get("/v1/stats", &[]).await {
        Ok(_) => println!("  ✓ credentials work (or the node is open-posture)"),
        Err(e) if e.to_string().contains("401") => warn(
            "the node requires sign-in and this shell has no key — run `hearthloom auth login`"
                .into(),
        ),
        Err(_) => {}
    }

    let home = home();
    // 5. embedding model (hybrid search)
    if home.models_dir().join("all-MiniLM-L6-v2.onnx").exists() {
        println!("  ✓ embedding model present (hybrid search on)");
    } else {
        println!("  · no embedding model — search is FTS-only (drop all-MiniLM-L6-v2.onnx + tokenizer.json in {})", home.models_dir().display());
    }

    // 6. WAL bloat (checkpoint starvation)
    if let Ok(meta) = std::fs::metadata(home.root().join("hearthloom.db-wal")) {
        let mb = meta.len() / 1_048_576;
        if mb > 512 {
            warn(format!("db WAL is {mb}MB — checkpoints are being starved; is something holding a read open?"));
        } else {
            println!("  ✓ db WAL healthy ({mb}MB)");
        }
    }

    // 7. PATH
    let on_path = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|p| p.ends_with("/.local/bin") || p.ends_with("/.local/bin/"));
    if on_path {
        println!("  ✓ ~/.local/bin is on PATH");
    } else {
        warn("~/.local/bin is not on PATH — `hearthloom` won't be found from a fresh shell (rerun scripts/install.sh)".into());
    }

    if findings == 0 {
        println!("all clear.");
    } else {
        println!(
            "{findings} finding{}.",
            if findings == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/* ————— keys / vault / export / sync ————— */

pub async fn key_mint(client: &Client, name: &str, scopes: &[String]) -> Result<()> {
    let res = client
        .post(
            "/v1/keys",
            &json!({"actor": ACTOR, "name": name, "scopes": scopes}),
        )
        .await?;
    println!(
        "{}",
        res.get("secret").and_then(|s| s.as_str()).unwrap_or("?")
    );
    eprintln!("(shown once — store it now)");
    Ok(())
}

pub async fn key_list(client: &Client, as_json: bool) -> Result<()> {
    let res = client.get("/v1/keys", &[]).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&res)?);
        return Ok(());
    }
    for k in res
        .get("keys")
        .and_then(|v| v.as_array())
        .unwrap_or(&vec![])
    {
        println!(
            "{}  {}  [{}]{}  last used {}",
            k.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
            k.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            k.get("scopes")
                .and_then(|v| v.as_array())
                .map(|a| a
                    .iter()
                    .filter_map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(","))
                .unwrap_or_default(),
            if k.get("revoked_at").map(|v| !v.is_null()).unwrap_or(false) {
                "  REVOKED"
            } else {
                ""
            },
            k.get("last_used_at")
                .and_then(|v| v.as_str())
                .unwrap_or("never"),
        );
    }
    Ok(())
}

pub async fn key_revoke(client: &Client, id: &str) -> Result<()> {
    client
        .post(&format!("/v1/keys/{id}/revoke"), &json!({"actor": ACTOR}))
        .await?;
    println!("✓ revoked {id}");
    Ok(())
}

pub async fn vault(client: &Client, action: Option<&str>, minutes: Option<u32>) -> Result<()> {
    match action {
        None | Some("status") => {
            let v = client.get("/v1/vault", &[]).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Some(a @ ("unlock" | "lock")) => {
            let mut body = json!({"actor": ACTOR, "action": a});
            if let Some(m) = minutes {
                body["minutes"] = json!(m);
            }
            let v = client.post("/v1/vault", &body).await?;
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Some(other) => bail!("vault takes status|unlock|lock, got {other:?}"),
    }
    Ok(())
}

pub async fn export(client: &Client, out: Option<std::path::PathBuf>) -> Result<()> {
    client
        .post("/v1/export/start", &json!({"actor": ACTOR}))
        .await
        .map_err(|e| {
            if e.to_string().contains("vault") {
                anyhow!(
                    "{e}\n(an export contains everything — unlock first: hearthloom vault unlock)"
                )
            } else {
                e
            }
        })?;
    eprintln!("export started — waiting…");
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let s = client.get("/v1/export/status", &[]).await?;
        let phase = s.get("phase").and_then(|v| v.as_str()).unwrap_or("");
        if phase == "done" || s.get("ready").and_then(|v| v.as_bool()).unwrap_or(false) {
            break;
        }
        if phase == "failed" || s.get("error").map(|e| !e.is_null()).unwrap_or(false) {
            bail!("export failed: {s}");
        }
        eprint!(".");
    }
    eprintln!();
    let out = out.unwrap_or_else(|| {
        std::path::PathBuf::from(format!(
            "hearthloom-export-{}.zip",
            chrono::Utc::now().format("%Y%m%d-%H%M%S")
        ))
    });
    client.download("/v1/export/download", &out).await?;
    println!("✓ {}", out.display());
    Ok(())
}

pub async fn sync(client: &Client, now: bool, full: bool) -> Result<()> {
    if now {
        let res = client
            .post("/v1/peer/sync-now", &json!({"actor": ACTOR, "full": full}))
            .await?;
        println!("{}", serde_json::to_string_pretty(&res)?);
    } else {
        let res = client.get("/v1/peer", &[]).await?;
        println!("{}", serde_json::to_string_pretty(&res)?);
    }
    Ok(())
}

/* ————— host upgrade ————— */

pub async fn upgrade(client: &Client, check: bool) -> Result<()> {
    let info = client.get("/v1/version", &[]).await?;
    let current = info.get("current").and_then(|v| v.as_str()).unwrap_or("?");
    let latest = info.get("latest").and_then(|v| v.as_str());
    let update = info
        .get("update_available")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    match latest {
        Some(l) if update => println!("hearthloom {current} → {l} available"),
        Some(l) => {
            println!("✓ hearthloom {current} is the latest ({l})");
            return Ok(());
        }
        None => {
            println!("hearthloom {current} — the release channel isn't reachable right now");
            return Ok(());
        }
    }
    if let Some(n) = info.get("notes_url").and_then(|v| v.as_str()) {
        println!("  what's new: {n}");
    }
    if check {
        return Ok(());
    }
    if !info
        .get("can_self_update")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        bail!(
            "this node can't self-update (built from source, an unsupported platform, or a hosted \n               instance). Source builds: pull and rebuild. Hosted instances: your hosting provider updates the image."
        );
    }
    println!("downloading and verifying the official build…");
    let res = client.post("/v1/upgrade", &json!({})).await?;
    println!(
        "✓ upgrading to {} — the daemon is restarting; give it a few seconds",
        res.get("to").and_then(|v| v.as_str()).unwrap_or("?")
    );
    Ok(())
}

/* ————— capture ————— */

pub async fn task_add(client: &Client, description: &str) -> Result<()> {
    let res = client
        .post(
            "/v1/tasks",
            &json!({"actor": ACTOR, "description": description}),
        )
        .await?;
    println!(
        "✓ on the desk: {}",
        res.get("task")
            .and_then(|t| t.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or(description)
    );
    Ok(())
}

pub async fn note_new(client: &Client, title: Option<String>) -> Result<()> {
    let title = match title {
        Some(t) => t,
        None => prompt("title")?,
    };
    // $EDITOR flow: seed a temp file, edit, read back
    let tmp = std::env::temp_dir().join(format!("hearthloom-note-{}.md", std::process::id()));
    std::fs::write(&tmp, "")?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .with_context(|| format!("launching {editor}"))?;
    if !status.success() {
        bail!("{editor} exited nonzero — note not saved");
    }
    let body = std::fs::read_to_string(&tmp).unwrap_or_default();
    let _ = std::fs::remove_file(&tmp);
    if body.trim().is_empty() {
        bail!("empty note — nothing saved");
    }
    client
        .post(
            "/v1/notes",
            &json!({"actor": ACTOR, "title": title, "body": body}),
        )
        .await?;
    println!("✓ note saved: {title}");
    Ok(())
}

pub async fn note_list(client: &Client) -> Result<()> {
    let res = client.get("/v1/notes", &[("limit", "15".into())]).await?;
    for n in res
        .get("notes")
        .and_then(|v| v.as_array())
        .unwrap_or(&vec![])
    {
        println!(
            "{}  {}",
            n.get("modified_at")
                .or(n.get("updated_at"))
                .and_then(|v| v.as_str())
                .map(|s| &s[..10.min(s.len())])
                .unwrap_or("          "),
            n.get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("(untitled)"),
        );
    }
    Ok(())
}

/* ————— embers (dev tools — NOT an installer) ————— */

pub fn ember_validate(dir: &std::path::Path) -> Result<()> {
    let manifest = hearthloom_daemon::plugins::load_manifest(dir)?;
    println!("✓ valid ember: {} v{}", manifest.name, manifest.version);
    println!("  scopes: {}", manifest.scopes.join(", "));
    if let Some(rel) = manifest.launch.get("darwin") {
        if dir.join(rel).exists() {
            println!("  ✓ launch.darwin → {rel}");
        } else {
            bail!("launch.darwin points at {rel:?}, which isn't in the package");
        }
    }
    if let Some(ui) = &manifest.ui {
        for (section, entries) in [
            (
                "nav",
                ui.get("nav").map(|v| vec![v.clone()]).unwrap_or_default(),
            ),
            (
                "visualize",
                ui.get("visualize")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                "hamburger",
                ui.get("hamburger")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            ),
            (
                "desk",
                ui.get("desk")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            ),
        ] {
            for e in entries {
                if let Some(page) = e.get("page").and_then(|p| p.as_str()) {
                    if dir.join("ui").join(page).exists() {
                        println!("  ✓ ui.{section} → {page}");
                    } else {
                        bail!("ui.{section} points at {page:?}, which isn't in ui/");
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn ember_pack(dir: &std::path::Path, out: Option<std::path::PathBuf>) -> Result<()> {
    ember_validate(dir)?;
    let manifest = hearthloom_daemon::plugins::load_manifest(dir)?;
    let out = out.unwrap_or_else(|| {
        std::path::PathBuf::from(format!("{}-{}.zip", manifest.name, manifest.version))
    });
    let out_abs = std::fs::canonicalize(out.parent().unwrap_or(std::path::Path::new(".")))?
        .join(out.file_name().unwrap_or_default());
    let _ = std::fs::remove_file(&out_abs);
    let status = std::process::Command::new("zip")
        .args(["-r", "-q", out_abs.to_str().unwrap_or_default(), "."])
        .current_dir(dir)
        .status()
        .context("running zip")?;
    if !status.success() {
        bail!("zip failed");
    }
    println!("✓ {}", out_abs.display());
    Ok(())
}

pub async fn ember_list(client: &Client) -> Result<()> {
    let res = client.get("/v1/plugins", &[]).await?;
    let list = res
        .get("plugins")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if list.is_empty() {
        println!("no embers installed (☰ → Integrations → embers)");
    }
    for p in list {
        println!(
            "{} v{}  {}{}",
            p.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
            p.get("version").and_then(|v| v.as_str()).unwrap_or("?"),
            if p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false) {
                if p.get("running").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "running"
                } else {
                    "enabled"
                }
            } else {
                "disabled"
            },
            p.get("sync")
                .and_then(|v| v.as_str())
                .map(|s| if s == "mirror" { " · mirrored" } else { "" })
                .unwrap_or(""),
        );
    }
    Ok(())
}

pub fn ember_logs(name: &str, follow: bool) -> Result<()> {
    let path = home().root().join("plugins").join(name).join("plugin.log");
    if !path.exists() {
        bail!("no log at {} — is the ember installed?", path.display());
    }
    tail_file(&path, follow)
}

/* ————— notify / logs ————— */

pub async fn notify(client: &Client, title: &str, body: Option<String>) -> Result<()> {
    // origin derives from the KEY server-side; the body field is required
    client
        .post(
            "/v1/notifications",
            &json!({
                "title": title,
                "body": body.unwrap_or_default(),
            }),
        )
        .await?;
    println!("✓ the bell will ring");
    Ok(())
}

pub fn logs(follow: bool) -> Result<()> {
    let path = home().root().join("daemon.log");
    tail_file(&path, follow)
}

fn tail_file(path: &std::path::Path, follow: bool) -> Result<()> {
    let strip = |line: &str| -> String {
        // drop ANSI color noise for shell readability
        let mut out = String::with_capacity(line.len());
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    let content = std::fs::read_to_string(path).unwrap_or_default();
    for line in content
        .lines()
        .rev()
        .take(40)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        println!("{}", strip(line));
    }
    if follow {
        let mut pos = content.len() as u64;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let meta = std::fs::metadata(path)?;
            if meta.len() > pos {
                use std::io::{Read, Seek};
                let mut f = std::fs::File::open(path)?;
                f.seek(std::io::SeekFrom::Start(pos))?;
                let mut buf = String::new();
                f.read_to_string(&mut buf)?;
                pos = meta.len();
                for line in buf.lines() {
                    println!("{}", strip(line));
                }
            } else if meta.len() < pos {
                pos = 0; // rotated
            }
        }
    }
    Ok(())
}

/* ————— operator secrets (.env.sealed) ————— */

/// `hearthloom secret set KEY` — value from stdin, persisted through
/// core secrets::set_env_vars (re-seals; a naive .env write would
/// clobber the sealed file). Restart the daemon to apply.
pub fn secret_set(home: &hearthloom_core::Home, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("secret names are [A-Za-z0-9_]");
    }
    use std::io::Read;
    let mut value = String::new();
    std::io::stdin().read_to_string(&mut value)?;
    let value = value.trim();
    if value.is_empty() {
        bail!(
            "empty value on stdin — pipe it: printf '%s' \"$VALUE\" | hearthloom secret set {key}"
        );
    }
    hearthloom_core::secrets::set_env_vars(home.root(), &[(key, value)])?;
    let sealed = home.root().join(".env.sealed").exists();
    println!(
        "{key} saved to {}. Restart the daemon to apply:",
        if sealed {
            ".env.sealed (encrypted)"
        } else {
            ".env (plaintext - no master key)"
        }
    );
    println!("  launchctl kickstart -k gui/$(id -u)/com.hearthloom.daemon");
    Ok(())
}

/// `hearthloom secret list` — names only, never values.
pub fn secret_list(home: &hearthloom_core::Home) -> Result<()> {
    let Some(content) = hearthloom_core::secrets::load_sealed_env(home.root()) else {
        println!("no secrets configured (no .env / .env.sealed)");
        return Ok(());
    };
    let mut names: Vec<&str> = content
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            l.split('=').next().map(str::trim)
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    for n in names {
        println!("{n}");
    }
    Ok(())
}
