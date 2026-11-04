//! `hearthloom agents accounts` (0.5.85): the Claude accounts this
//! machine's Claude Code agents run on — the same pool as the Claude Code
//! agent's setup on the Agents page (daemon agents/claude_pool.rs).
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::client::Client;

async fn pool(client: &Client) -> Result<Value> {
    let view = client.get("/v1/agents", &[]).await?;
    if view["locked"] == json!(true) {
        bail!("agent settings need the vault open: hearthloom vault unlock");
    }
    view.get("claude_pool")
        .filter(|p| !p.is_null())
        .cloned()
        .ok_or_else(|| {
            anyhow!("no Claude Code agent on this node yet — add one on the Agents page first")
        })
}

async fn change(client: &Client, pool: Value) -> Result<Value> {
    client
        .post("/v1/agents", &json!({"action": "pool", "pool": pool}))
        .await
}

fn when(secs: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|d| d.format("%a %H:%M").to_string())
        .unwrap_or_default()
}

fn print(pool: &Value) {
    let accounts = pool["accounts"].as_array().cloned().unwrap_or_default();
    let many = accounts.len() > 1;
    for (i, a) in accounts.iter().enumerate() {
        let name = if a["usual"] == json!(true) {
            "your usual sign-in".to_string()
        } else {
            a["label"].as_str().unwrap_or("?").to_string()
        };
        let s = &a["sign_in"];
        let who = if s["signed_in"] == json!(true) {
            format!(
                "{}{}",
                s["email"].as_str().unwrap_or("signed in"),
                s["plan"]
                    .as_str()
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_default()
            )
        } else {
            match a["login_command"].as_str() {
                Some(cmd) => format!("not signed in — run: {cmd}"),
                None => s["error"].as_str().unwrap_or("not signed in").to_string(),
            }
        };
        let pct = |w: &Value| {
            w["utilization"]
                .as_f64()
                .map(|u| format!("{:.0}%", u * 100.0))
                .unwrap_or_else(|| "—".into())
        };
        let w = &a["windows"];
        let used = if w["five_hour"].is_null() && w["seven_day"].is_null() {
            "usage not read yet".to_string()
        } else {
            format!(
                "used: 5 hours {} · week {}{}",
                pct(&w["five_hour"]),
                pct(&w["seven_day"]),
                w["seven_day"]["resets_at"]
                    .as_i64()
                    .map(|t| format!(" (resets {})", when(t)))
                    .unwrap_or_default()
            )
        };
        let state = if let Some(t) = a["exhausted_until"].as_i64() {
            format!(" · out until {}", when(t))
        } else if let Some(t) = a["held_back_until"].as_i64() {
            format!(" · resting until {}", when(t))
        } else {
            String::new()
        };
        let place = if many {
            format!("{}. ", i + 1)
        } else {
            String::new()
        };
        let off = if a["enabled"] == json!(false) {
            " [off]"
        } else {
            ""
        };
        println!("{place}{name}{off} — {who}");
        println!(
            "   {used}{state} · agents may use up to {}% of 5 hours, {}% of the week",
            a["cap_5h"], a["cap_7d"]
        );
    }
    let onoff = |k: &str| {
        if pool[k] == json!(true) {
            "on"
        } else {
            "off"
        }
    };
    if many {
        println!("moving on to the next account: {}", onoff("switching"));
    }
    if pool["has_api_key"] == json!(true) {
        println!("API-key fallback: {}", onoff("api_fallback"));
    }
}

fn show(pool: &Value, as_json: bool, done: Option<&str>) {
    if as_json {
        println!("{pool}");
    } else {
        if let Some(done) = done {
            println!("✓ {done}");
        }
        print(pool);
    }
}

pub enum Op {
    List,
    Add(String),
    Login(String),
    Set {
        account: String,
        cap_5h: Option<u8>,
        cap_week: Option<u8>,
        enabled: Option<bool>,
        models: Option<Vec<String>>,
        rename: Option<String>,
    },
    Move(String, usize),
    Remove(String, bool),
    Check(String, bool),
    Setting(&'static str, bool),
}

pub fn on_off(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        _ => bail!("say on or off"),
    }
}

pub async fn run(client: &Client, op: Op, as_json: bool) -> Result<()> {
    match op {
        Op::List => show(&pool(client).await?, as_json, None),
        Op::Add(label) => {
            let res = change(client, json!({"op": "add", "label": label})).await?;
            if as_json {
                println!("{res}");
            } else {
                println!("✓ added {label}. Sign it in on this machine:");
                println!("  {}", res["login_command"].as_str().unwrap_or(""));
            }
        }
        Op::Login(label) => {
            if client.is_remote() {
                bail!("signing in happens on the machine the agents run on: run this there, without --url");
            }
            let pool = pool(client).await?;
            let account = pool["accounts"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| {
                    a["id"].as_str() == Some(label.as_str())
                        || a["label"]
                            .as_str()
                            .is_some_and(|l| l.eq_ignore_ascii_case(&label))
                        || (a["usual"] == json!(true) && label.eq_ignore_ascii_case("usual"))
                })
                .cloned()
                .ok_or_else(|| anyhow!("no Claude account {label:?} in the pool"))?;
            let mut cmd = std::process::Command::new("claude");
            cmd.args(["auth", "login", "--claudeai"]);
            if let Some(dir) = account["dir"].as_str() {
                cmd.env("CLAUDE_CONFIG_DIR", dir);
            }
            eprintln!(
                "Claude Code will sign this account in (it keeps the sign-in; Hearthloom never sees it)…"
            );
            let status = cmd
                .status()
                .map_err(|e| anyhow!("couldn't start claude ({e}) — is Claude Code installed?"))?;
            if !status.success() {
                bail!("the sign-in didn't finish");
            }
            let id = account["id"].as_str().unwrap_or_default();
            let res = change(client, json!({"op": "check", "id": id})).await?;
            show(&res["pool"], as_json, Some("signed in"));
        }
        Op::Set {
            account,
            cap_5h,
            cap_week,
            enabled,
            models,
            rename,
        } => {
            let mut body = json!({"op": "set", "id": account});
            if let Some(v) = cap_5h {
                body["cap_5h"] = json!(v);
            }
            if let Some(v) = cap_week {
                body["cap_7d"] = json!(v);
            }
            if let Some(v) = enabled {
                body["enabled"] = json!(v);
            }
            if let Some(v) = models {
                body["models"] = json!(v);
            }
            if let Some(v) = rename {
                body["label"] = json!(v);
            }
            let res = change(client, body).await?;
            show(&res["pool"], as_json, Some("saved"));
        }
        Op::Move(account, to) => {
            let res = change(client, json!({"op": "move", "id": account, "to": to})).await?;
            show(&res["pool"], as_json, Some("moved"));
        }
        Op::Remove(account, yes) => {
            if !yes {
                use std::io::{IsTerminal, Write};
                if !std::io::stdin().is_terminal() {
                    bail!("removing needs --yes when no one is at a terminal to confirm");
                }
                eprint!("remove {account} from the pool? Claude Code signs it out on this machine. [y/N] ");
                std::io::stderr().flush().ok();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    bail!("not removed");
                }
            }
            let res = change(client, json!({"op": "remove", "id": account})).await?;
            show(&res["pool"], as_json, Some("removed"));
        }
        Op::Check(account, usage) => {
            let res = change(
                client,
                json!({"op": "check", "id": account, "usage": usage}),
            )
            .await?;
            show(&res["pool"], as_json, None);
        }
        Op::Setting(key, value) => {
            let res = change(client, json!({"op": "settings", key: value})).await?;
            show(&res["pool"], as_json, Some("saved"));
        }
    }
    Ok(())
}
