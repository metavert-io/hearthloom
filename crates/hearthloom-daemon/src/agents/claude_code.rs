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
    if let Some(path) = agent.config["claude_path"]
        .as_str()
        .filter(|p| !p.trim().is_empty())
    {
        let path = super::expand_home(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(anyhow!("claude not found at {}", path.display()))
        };
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<std::path::PathBuf> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(|d| std::path::Path::new(d).join("claude"))
        .collect();
    for extra in [
        ".local/node-arm64/bin/claude",
        ".claude/local/claude",
        ".local/bin/claude",
    ] {
        candidates.push(std::path::Path::new(&home).join(extra));
    }
    candidates.push("/usr/local/bin/claude".into());
    candidates.push("/opt/homebrew/bin/claude".into());
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        anyhow!("Claude Code isn't installed on this Mac (no `claude` command found)")
    })
}

fn port() -> String {
    std::env::var("HEARTHLOOM_PORT").unwrap_or_else(|_| "43278".into())
}

/// The MCP config handed to Claude Code: written 0600 for the run, then
/// removed. It holds two bearer tokens (the agent's key and the task's
/// governance token), so it never outlives the task.
fn write_mcp_config(
    state: &AppState,
    agent: &AgentRow,
    task: &AgentTaskRow,
    token: &str,
) -> Result<std::path::PathBuf> {
    let base = format!("http://127.0.0.1:{}", port());
    let mut servers = serde_json::Map::new();
    servers.insert(
        "hearthloom_gov".into(),
        json!({
            "type": "http",
            "url": format!("{base}/mcp/governance"),
            "headers": {"Authorization": format!("Bearer {token}")},
        }),
    );
    if let Some(key) = super::secret(&agent.id, "KEY") {
        servers.insert(
            "hearthloom".into(),
            json!({
                "type": "http",
                "url": format!("{base}/mcp"),
                "headers": {"Authorization": format!("Bearer {key}")},
            }),
        );
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
        "-p".into(),
        prompt.into(),
        // stream-json: Claude Code's events as they happen — the usage
        // readings (rate_limit_event), the tools it used, then the result
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--no-session-persistence".into(),
        "--setting-sources".into(),
        "project".into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        mcp_config.display().to_string(),
        "--permission-mode".into(),
        "manual".into(),
        "--permission-prompt-tool".into(),
        "mcp__hearthloom_gov__approve".into(),
    ];
    if let Some(model) = agent.config["model"]
        .as_str()
        .filter(|m| !m.trim().is_empty())
    {
        args.push("--model".into());
        args.push(model.into());
    }
    args
}

/// A run stopped because its Claude account reached a usage limit (or,
/// with `stop_at_cap`, the share of it the owner allows).
#[derive(Debug)]
pub struct LimitHit {
    pub account: super::claude_pool::Account,
    /// When the account can take work again.
    pub until: i64,
    /// It had used a tool that changes something before it stopped, so
    /// starting again from the top could do that twice.
    pub did_something: bool,
}

impl std::fmt::Display for LimitHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the Claude account reached its usage limit")
    }
}

impl std::error::Error for LimitHit {}

/// Tools that only look. A run that used nothing else can start again.
const LOOK_ONLY: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "LS",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
    "mcp__hearthloom__search",
    "mcp__hearthloom__recall",
    "mcp__hearthloom__traverse",
];

pub fn did_something(tools: &[String]) -> bool {
    tools.iter().any(|t| !LOOK_ONLY.contains(&t.as_str()))
}

/// Claude Code's words when an account is out ("You've hit your session
/// limit · resets 3:45pm", older "Claude AI usage limit reached|<time>").
pub fn is_limit_message(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    (t.contains("hit your") && t.contains("limit")) || t.contains("usage limit reached")
}

/// The reset time the older message carries after a `|`.
pub fn limit_reset(text: &str) -> Option<i64> {
    let (_, tail) = text.rsplit_once('|')?;
    tail.trim()
        .parse::<i64>()
        .ok()
        .filter(|t| *t > 1_000_000_000)
}

enum Stop {
    ApiKey(String),
    Cap,
}

#[derive(Default)]
struct Watch {
    stop: Option<Stop>,
    tools: Vec<String>,
    rejected: bool,
}

/// One event of Claude Code's stream, as it arrives.
fn observe(
    state: &AppState,
    seat: &super::claude_pool::Seat,
    stop_at_cap: bool,
    watch: &std::sync::Mutex<Watch>,
    event: &Value,
) {
    let mut w = watch.lock().unwrap_or_else(|p| p.into_inner());
    match event["type"].as_str() {
        // the first event, before any model call: a subscription run that
        // would bill an API key is stopped before it spends anything
        Some("system") if event["subtype"] == "init" => {
            let source = event["apiKeySource"].as_str().unwrap_or("none");
            if seat.account.is_some() && source != "none" && w.stop.is_none() {
                w.stop = Some(Stop::ApiKey(source.to_string()));
            }
        }
        Some("rate_limit_event") => {
            if let Some(account) = &seat.account {
                let now = chrono::Utc::now().timestamp();
                let info = &event["rate_limit_info"];
                let reading =
                    super::claude_pool::update_reading(state, &account.id, |r| r.absorb(info, now));
                if info["status"] == "rejected" {
                    w.rejected = true;
                }
                if stop_at_cap && w.stop.is_none() && reading.over_cap(account, now) {
                    w.stop = Some(Stop::Cap);
                }
            }
        }
        Some("assistant") => {
            for item in event["message"]["content"].as_array().into_iter().flatten() {
                if item["type"] == "tool_use" {
                    if let Some(name) = item["name"].as_str() {
                        w.tools.push(name.to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

/// The run's tokens into the llm_usage ledger, one row per model: a
/// subscription run at zero cost (`subscription:<model>`), the API-key
/// fallback at the model's price.
fn record_usage(
    state: &AppState,
    task: &AgentTaskRow,
    seat: &super::claude_pool::Seat,
    result: &Value,
) {
    let ms = result["duration_ms"].as_u64().unwrap_or(0);
    let status = if result["is_error"] == json!(true) {
        "error"
    } else {
        "ok"
    };
    let n = |v: &Value, k: &str| v[k].as_i64().unwrap_or(0);
    for (model, u) in result["modelUsage"].as_object().into_iter().flatten() {
        let usage = hearthloom_core::provider::Usage {
            input_tokens: n(u, "inputTokens"),
            output_tokens: n(u, "outputTokens"),
            cache_creation_tokens: n(u, "cacheCreationInputTokens"),
            cache_read_tokens: n(u, "cacheReadInputTokens"),
        };
        let model = match seat.account {
            Some(_) => format!(
                "{}{model}",
                hearthloom_core::provider::SUBSCRIPTION_MODEL_PREFIX
            ),
            None => model.clone(),
        };
        let _ = state.store.record_llm_usage(
            crate::monitor::node_kind(),
            "agent",
            Some(&task.id),
            "claude-code",
            &model,
            "agent",
            &usage,
            ms,
            status,
        );
    }
}

pub async fn run(
    state: &AppState,
    agent: &AgentRow,
    task: &AgentTaskRow,
    token: &str,
    seat: &super::claude_pool::Seat,
) -> Result<String> {
    let workdir = super::workdir(&agent.config).ok_or_else(|| {
        anyhow!(
            "choose a working folder for {} on the Agents page",
            agent.name
        )
    })?;
    if !workdir.is_dir() {
        return Err(anyhow!(
            "the working folder {} doesn't exist",
            workdir.display()
        ));
    }
    let claude = find_claude(agent)?;
    let config_path = write_mcp_config(state, agent, task, token)?;
    let prompt = format!("{}{}", super::preamble(agent), task.brief);
    let timeout = std::time::Duration::from_secs(
        agent.config["timeout_secs"]
            .as_u64()
            .unwrap_or(1800)
            .clamp(60, 86_400),
    );
    let stop_at_cap = super::claude_pool::load(state).stop_at_cap;
    // never the daemon's secrets (an API key there would also be billed
    // instead of the owner's subscription): see child_env.rs. The seat
    // adds the account's Claude Code home, or the key for the fallback.
    let mut child = tokio::process::Command::new(&claude)
        .env_clear()
        .envs(crate::child_env::inherited())
        .envs(seat.env())
        .args(command_args(agent, &prompt, &config_path))
        .current_dir(workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", claude.display()))?;
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let watch = std::sync::Arc::new(std::sync::Mutex::new(Watch::default()));
    let reader = {
        let (state, seat, watch) = (state.clone(), seat.clone(), watch.clone());
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            // the stream can be long (tool results): keep its tail, which
            // ends with the result
            let mut tail = std::collections::VecDeque::new();
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(event) = serde_json::from_str::<Value>(&line) {
                    observe(&state, &seat, stop_at_cap, &watch, &event);
                }
                tail.push_back(line);
                if tail.len() > 40 {
                    tail.pop_front();
                }
            }
            tail.into_iter().collect::<Vec<_>>().join("\n").into_bytes()
        })
    };
    let errs = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut err = Vec::new();
        let _ = stderr.read_to_end(&mut err).await;
        err
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
        let stop = watch.lock().unwrap_or_else(|p| p.into_inner()).stop.take();
        match stop {
            Some(Stop::ApiKey(source)) => {
                let _ = child.kill().await;
                let _ = std::fs::remove_file(&config_path);
                return Err(anyhow!(
                    "Claude Code was about to bill an API key ({source}) instead of {}; \
                     it was stopped before it spent anything",
                    seat.label()
                ));
            }
            Some(Stop::Cap) => {
                let _ = child.kill().await;
                let _ = std::fs::remove_file(&config_path);
                let account = seat.account.clone().expect("a cap stop has an account");
                let now = chrono::Utc::now().timestamp();
                let until = super::claude_pool::reading(state, &account.id)
                    .windows
                    .values()
                    .map(|w| w.resets_at)
                    .max()
                    .unwrap_or(now + 3600);
                let tools = watch
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .tools
                    .clone();
                return Err(LimitHit {
                    account,
                    until,
                    did_something: did_something(&tools),
                }
                .into());
            }
            None => {}
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    };
    let _ = std::fs::remove_file(&config_path);
    let out = reader.await.unwrap_or_default();
    let err = errs.await.unwrap_or_default();
    let Some(status) = status else {
        return Err(anyhow!(
            "it ran past its time limit ({}s) and was stopped",
            timeout.as_secs()
        ));
    };
    let result: Value = String::from_utf8_lossy(&out)
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(|v| v["type"] == "result")
        .unwrap_or(Value::Null);
    record_usage(state, task, seat, &result);
    let parsed = parse_result(&out, &err, status.success());
    if let (Err(e), Some(account)) = (&parsed, &seat.account) {
        let (rejected, tools) = {
            let w = watch.lock().unwrap_or_else(|p| p.into_inner());
            (w.rejected, w.tools.clone())
        };
        let text = format!("{e:#}");
        if rejected || is_limit_message(&text) {
            let until = super::claude_pool::exhausted(state, account, limit_reset(&text));
            return Err(LimitHit {
                account: account.clone(),
                until,
                did_something: did_something(&tools),
            }
            .into());
        }
    }
    parsed
}

/// The result event (the stream's last line; `json` output prints only it).
pub fn parse_result(out: &[u8], err: &[u8], exited_ok: bool) -> Result<String> {
    let text = String::from_utf8_lossy(out);
    // only the result event counts: in a stream, the last line can be any
    // event (a run that died mid-way ends on a tool call or a reading)
    let is_result = |v: &Value| v["type"] == "result";
    let parsed: Option<Value> = text
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(is_result)
        .or_else(|| {
            serde_json::from_str::<Value>(text.trim())
                .ok()
                .filter(is_result)
        });
    match parsed {
        Some(v)
            if v["is_error"] == json!(true)
                || v["subtype"].as_str().is_some_and(|s| s != "success") =>
        {
            Err(anyhow!(
                "{}",
                v["result"]
                    .as_str()
                    .unwrap_or("Claude Code reported an error")
            ))
        }
        Some(v) => Ok(v["result"].as_str().unwrap_or("(no summary)").to_string()),
        // a stream with no result event: nothing to report as its answer
        None if exited_ok && text.trim_start().starts_with('{') => Ok("(no summary)".into()),
        None if exited_ok => Ok(text.trim().chars().take(20_000).collect()),
        None => Err(anyhow!(
            "Claude Code failed: {}",
            String::from_utf8_lossy(err)
                .trim()
                .chars()
                .take(600)
                .collect::<String>()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_is_isolated_from_the_owners_settings() {
        let agent = AgentRow {
            id: "ag_1".into(),
            name: "Code".into(),
            harness: "claude_code".into(),
            config: json!({"model": "claude-sonnet-5-5"}),
            secret_name: None,
            key_id: None,
            enabled: true,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let args = command_args(&agent, "do it", std::path::Path::new("/tmp/x.json"));
        let joined = args.join(" ");
        for must in [
            "--setting-sources project",
            "--strict-mcp-config",
            "--permission-mode manual",
            "--permission-prompt-tool mcp__hearthloom_gov__approve",
            "--no-session-persistence",
            "--model claude-sonnet-5-5",
        ] {
            assert!(joined.contains(must), "{must} missing from {joined}");
        }
        assert!(!joined.contains("skip-permissions") && !joined.contains("bypass"));
    }

    /// The bridge end to end, against a stand-in `claude` that prints a
    /// scripted stream: the account it ran on, the readings it reported,
    /// a limit hit before and after it changed something, an API key it
    /// would have billed, and the ledger row.
    #[tokio::test]
    async fn test_a_run_on_a_pooled_account() {
        use super::super::claude_pool::{self, Account, Seat};
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
        // a secret in the daemon's environment must not reach the run
        std::env::set_var("HEARTHLOOM_TEST_SECRET_POOL", "leaked");
        let actor = hearthloom_core::Actor::new("human:test");
        let fake = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let init = |source: &str| {
            format!(r#"echo '{{"type":"system","subtype":"init","apiKeySource":"{source}"}}'"#)
        };
        let tool = |name: &str| {
            format!(
                r#"echo '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"{name}"}}]}}}}'"#
            )
        };
        let rate = |status: &str, u: f64| {
            format!(
                r#"echo '{{"type":"rate_limit_event","rate_limit_info":{{"status":"{status}","resetsAt":4102444800,"unifiedWindows":{{"seven_day":{{"utilization":{u},"resetsAt":4102444800}}}}}}}}'"#
            )
        };
        let limit = concat!(
            "cat <<'EOF'\n",
            r#"{"type":"result","subtype":"success","is_error":true,"result":"You've hit your session limit · resets 3:45pm"}"#,
            "\nEOF\nexit 1"
        );
        let workdir = tmp.path().join("work");
        std::fs::create_dir_all(&workdir).unwrap();
        let account = Account {
            id: "ca_test".into(),
            label: "work".into(),
            cap_5h: 100,
            cap_7d: 100,
            enabled: true,
            models: vec![],
        };
        let seat = Seat {
            config_dir: Some(tmp.path().join("acct")),
            account: Some(account.clone()),
            api_key: None,
        };
        let run_with = |script: std::path::PathBuf| {
            let state = state.clone();
            let actor = actor.clone();
            let seat = seat.clone();
            let workdir = workdir.clone();
            async move {
                let agent = state
                    .store
                    .save_agent(
                        &actor,
                        None,
                        &hearthloom_core::store::AgentIn {
                            name: "Desk hand".into(),
                            harness: "claude_code".into(),
                            config: json!({"workdir": workdir, "claude_path": script}),
                            enabled: Some(true),
                        },
                    )
                    .unwrap();
                let task = state
                    .store
                    .create_agent_task(
                        &actor,
                        None,
                        Some(&agent.id),
                        "do it",
                        "do it",
                        &[],
                        None,
                        false,
                    )
                    .unwrap();
                state
                    .store
                    .set_agent_task_status(&actor, &task.id, "running", None, None, None)
                    .unwrap();
                run(&state, &agent, &task, "tok", &seat).await
            }
        };

        // 1. a good run: on the account's own Claude Code home, no secret,
        //    its reading kept, its tokens in the ledger at no cost
        let result_line = concat!(
            "cat <<EOF\n",
            r#"{"type":"result","subtype":"success","is_error":false,"#,
            r#""result":"dir=${CLAUDE_CONFIG_DIR:-none} secret=${HEARTHLOOM_TEST_SECRET_POOL:-unset}","#,
            r#""duration_ms":5,"modelUsage":{"claude-sonnet-5-5":{"inputTokens":10,"outputTokens":3}}}"#,
            "\nEOF"
        );
        let ok = fake(
            "ok.sh",
            &[
                init("none"),
                rate("allowed", 0.42),
                tool("Read"),
                result_line.to_string(),
            ]
            .join("\n"),
        );
        let text = run_with(ok).await.unwrap();
        assert!(
            text.contains(&format!("dir={}", tmp.path().join("acct").display())),
            "{text}"
        );
        assert!(
            text.ends_with("secret=unset"),
            "the daemon's secrets stay out: {text}"
        );
        let reading = claude_pool::reading(&state, "ca_test");
        assert_eq!(reading.windows["seven_day"].utilization, 0.42);
        let usage = state.store.llm_usage_since("2000-01-01", 50).unwrap();
        assert!(
            usage.to_string().contains("subscription:claude-sonnet-5-5"),
            "{usage}"
        );

        // 2. out of usage after only looking: can start again
        let looked = fake(
            "looked.sh",
            &[
                init("none"),
                tool("Grep"),
                rate("rejected", 1.0),
                limit.to_string(),
            ]
            .join("\n"),
        );
        let err = run_with(looked).await.unwrap_err();
        let hit = err.downcast_ref::<LimitHit>().expect("a limit hit");
        assert!(!hit.did_something);
        assert!(claude_pool::reading(&state, "ca_test")
            .exhausted_until
            .is_some());

        // 3. out of usage after changing something: must not start again
        let changed = fake(
            "changed.sh",
            &[init("none"), tool("Bash"), limit.to_string()].join("\n"),
        );
        let err = run_with(changed).await.unwrap_err();
        assert!(
            err.downcast_ref::<LimitHit>()
                .expect("a limit hit")
                .did_something
        );

        // 4. about to bill an API key on a subscription seat: stopped first
        let keyed = fake(
            "keyed.sh",
            &[init("ANTHROPIC_API_KEY"), "sleep 20".to_string()].join("\n"),
        );
        let err = run_with(keyed).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("about to bill an API key"),
            "{err:#}"
        );
        std::env::remove_var("HEARTHLOOM_TEST_SECRET_POOL");
    }

    #[test]
    fn test_parse_result() {
        // a stream that never reached its result is not a success
        let cut = b"{\"type\":\"system\",\"subtype\":\"init\"}\n{\"type\":\"rate_limit_event\"}";
        assert!(parse_result(cut, b"killed", false).is_err());
        let ok = br#"{"type":"result","subtype":"success","is_error":false,"result":"Wrote the summary."}"#;
        assert_eq!(parse_result(ok, b"", true).unwrap(), "Wrote the summary.");
        let bad =
            br#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"ran out"}"#;
        assert!(parse_result(bad, b"", false).is_err());
        assert!(parse_result(b"", b"auth required", false)
            .unwrap_err()
            .to_string()
            .contains("auth required"));
    }
}
