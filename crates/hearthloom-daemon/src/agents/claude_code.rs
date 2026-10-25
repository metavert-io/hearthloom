//! The Claude Code bridge (governance depth: full). Runs `claude -p` on
//! this Mac in the folder the owner chose, with exactly two tool servers:
//! the Hearthloom record (through the agent's own scoped, read-only key)
//! and the governance endpoint, which Claude Code consults for EVERY
//! permission it would otherwise prompt for (`--permission-prompt-tool`).
//!
//! Isolation from the owner's own Claude Code setup matters: their user
//! settings may pre-allow commands (`Bash(fly deploy:*)`…) that would
//! then skip governance. So: `--setting-sources project` (the agent's
//! folder only), `--strict-mcp-config` (no other MCP servers),
//! `--permission-mode manual`, no session persistence.

use anyhow::{anyhow, Context, Result};
use hearthloom_core::store::{AgentRow, AgentTaskRow};
use serde_json::{json, Value};

use crate::AppState;

/// Where `claude` usually lives; launchd's PATH is minimal.
pub fn find_claude(agent: &AgentRow) -> Result<std::path::PathBuf> {
    if let Some(path) = agent.config["claude_path"].as_str().filter(|p| !p.trim().is_empty()) {
        let path = std::path::PathBuf::from(path);
        return if path.is_file() { Ok(path) } else { Err(anyhow!("claude not found at {}", path.display())) };
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<std::path::PathBuf> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|d| std::path::Path::new(d).join("claude"))
        .collect();
    for extra in [".local/node-arm64/bin/claude", ".claude/local/claude", ".local/bin/claude"] {
        candidates.push(std::path::Path::new(&home).join(extra));
    }
    candidates.push("/usr/local/bin/claude".into());
    candidates.push("/opt/homebrew/bin/claude".into());
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("Claude Code isn't installed on this Mac (no `claude` command found)"))
}

fn port() -> String {
    std::env::var("HEARTHLOOM_PORT").unwrap_or_else(|_| "43278".into())
}

/// The MCP config handed to Claude Code: written 0600 for the run, then
/// removed. It holds two bearer tokens (the agent's key and the task's
/// governance token), so it never outlives the task.
fn write_mcp_config(state: &AppState, agent: &AgentRow, task: &AgentTaskRow, token: &str) -> Result<std::path::PathBuf> {
    let base = format!("http://127.0.0.1:{}", port());
    let mut servers = serde_json::Map::new();
    servers.insert("hearthloom_gov".into(), json!({
        "type": "http",
        "url": format!("{base}/mcp/governance"),
        "headers": {"Authorization": format!("Bearer {token}")},
    }));
    if let Some(key) = super::secret(&agent.id, "KEY") {
        servers.insert("hearthloom".into(), json!({
            "type": "http",
            "url": format!("{base}/mcp"),
            "headers": {"Authorization": format!("Bearer {key}")},
        }));
    }
    let dir = state.home.root().join("agents");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.mcp.json", task.id));
    std::fs::write(&path, json!({"mcpServers": servers}).to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// The command line (separate so tests can check it without running it).
pub fn command_args(agent: &AgentRow, prompt: &str, mcp_config: &std::path::Path) -> Vec<String> {
    let mut args = vec![
        "-p".into(), prompt.into(),
        "--output-format".into(), "json".into(),
        "--no-session-persistence".into(),
        "--setting-sources".into(), "project".into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(), mcp_config.display().to_string(),
        "--permission-mode".into(), "manual".into(),
        "--permission-prompt-tool".into(), "mcp__hearthloom_gov__approve".into(),
    ];
    if let Some(model) = agent.config["model"].as_str().filter(|m| !m.trim().is_empty()) {
        args.push("--model".into());
        args.push(model.into());
    }
    args
}

pub async fn run(state: &AppState, agent: &AgentRow, task: &AgentTaskRow, token: &str) -> Result<String> {
    let workdir = agent.config["workdir"]
        .as_str()
        .filter(|w| !w.trim().is_empty())
        .ok_or_else(|| anyhow!("choose a working folder for {} on the Agents page", agent.name))?;
    if !std::path::Path::new(workdir).is_dir() {
        return Err(anyhow!("the working folder {workdir} doesn't exist"));
    }
    let claude = find_claude(agent)?;
    let config_path = write_mcp_config(state, agent, task, token)?;
    let prompt = format!("{}{}", super::preamble(agent), task.brief);
    let timeout = std::time::Duration::from_secs(
        agent.config["timeout_secs"].as_u64().unwrap_or(1800).clamp(60, 86_400),
    );
    let mut child = tokio::process::Command::new(&claude)
        .args(command_args(agent, &prompt, &config_path))
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", claude.display()))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let reader = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let _ = stdout.read_to_end(&mut out).await;
        let _ = stderr.read_to_end(&mut err).await;
        (out, err)
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() > timeout {
            let _ = child.kill().await;
            break None;
        }
        // stopped from the UI → stop the process
        if !matches!(state.store.agent_task(&task.id), Ok(Some(t)) if t.status == "running") {
            let _ = child.kill().await;
            let _ = std::fs::remove_file(&config_path);
            return Err(anyhow!("stopped"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    };
    let _ = std::fs::remove_file(&config_path);
    let (out, err) = reader.await.unwrap_or_default();
    let Some(status) = status else {
        return Err(anyhow!("it ran past its time limit ({}s) and was stopped", timeout.as_secs()));
    };
    parse_result(&out, &err, status.success())
}

/// `--output-format json` prints one result object.
pub fn parse_result(out: &[u8], err: &[u8], exited_ok: bool) -> Result<String> {
    let text = String::from_utf8_lossy(out);
    let parsed: Option<Value> = text.lines().rev().find_map(|l| serde_json::from_str(l.trim()).ok())
        .or_else(|| serde_json::from_str(text.trim()).ok());
    match parsed {
        Some(v) if v["is_error"] == json!(true) || v["subtype"].as_str().is_some_and(|s| s != "success") => {
            Err(anyhow!("{}", v["result"].as_str().unwrap_or("Claude Code reported an error")))
        }
        Some(v) => Ok(v["result"].as_str().unwrap_or("(no summary)").to_string()),
        None if exited_ok => Ok(text.trim().chars().take(20_000).collect()),
        None => Err(anyhow!(
            "Claude Code failed: {}",
            String::from_utf8_lossy(err).trim().chars().take(600).collect::<String>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_is_isolated_from_the_owners_settings() {
        let agent = AgentRow {
            id: "ag_1".into(), name: "Code".into(), harness: "claude_code".into(),
            config: json!({"model": "claude-sonnet-5-5"}), secret_name: None, key_id: None,
            enabled: true, created_at: String::new(), updated_at: String::new(),
        };
        let args = command_args(&agent, "do it", std::path::Path::new("/tmp/x.json"));
        let joined = args.join(" ");
        for must in [
            "--setting-sources project", "--strict-mcp-config", "--permission-mode manual",
            "--permission-prompt-tool mcp__hearthloom_gov__approve", "--no-session-persistence",
            "--model claude-sonnet-5-5",
        ] {
            assert!(joined.contains(must), "{must} missing from {joined}");
        }
        assert!(!joined.contains("skip-permissions") && !joined.contains("bypass"));
    }

    #[test]
    fn test_parse_result() {
        let ok = br#"{"type":"result","subtype":"success","is_error":false,"result":"Wrote the summary."}"#;
        assert_eq!(parse_result(ok, b"", true).unwrap(), "Wrote the summary.");
        let bad = br#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"ran out"}"#;
        assert!(parse_result(bad, b"", false).is_err());
        assert!(parse_result(b"", b"auth required", false).unwrap_err().to_string().contains("auth required"));
    }
}
