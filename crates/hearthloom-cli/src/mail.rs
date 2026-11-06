//! `hearthloom mail` (0.5.92): what waits on you in your mail, your mail
//! rules, and how new mail is sorted — the Desk's "Waiting on you" from a
//! terminal (daemon mail_triage.rs, store/mail.rs).
use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::client::Client;

pub enum Op {
    Threads(String),
    Set(String, String),
    Rules,
    RuleAdd {
        kind: String,
        pattern: String,
        action: Option<String>,
        note: Option<String>,
    },
    RuleRemove(String),
    Triage {
        enabled: Option<bool>,
        model: Option<String>,
        now: bool,
    },
}

fn print_threads(threads: &[Value]) {
    if threads.is_empty() {
        println!("nothing waiting on you");
    }
    for t in threads {
        println!(
            "{}  {}  — {}  [{}]{}",
            t["id"].as_str().unwrap_or("?"),
            t["subject"].as_str().unwrap_or("(no subject)"),
            t["last_from"].as_str().unwrap_or("?"),
            t["state"].as_str().unwrap_or("?"),
            t["due_by"]
                .as_str()
                .map(|d| format!(" by {d}"))
                .unwrap_or_default()
        );
        if let Some(why) = t["why"].as_str() {
            println!("    {why}");
        }
    }
}

fn print_triage(s: &Value) {
    println!(
        "sorting new mail: {} · model {} · {}{}",
        if s["enabled"] == json!(true) {
            "on"
        } else {
            "off"
        },
        s["model"].as_str().unwrap_or("?"),
        match s["provider"].as_str() {
            Some("claude-code") => "Claude Code on this machine (your subscription)",
            Some("api") => "this node's AI key",
            _ => "no model here (your rules only)",
        },
        if s["here"] == json!(true) {
            ""
        } else {
            " · Gmail isn't watched on this node, so it sorts nothing here"
        }
    );
    if let Some(err) = s["last"]["error"].as_str() {
        println!("  last attempt failed: {err}");
    }
    let counts: Vec<String> = s["counts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| format!("{} {}", c["n"], c["state"].as_str().unwrap_or("?")))
        .collect();
    if !counts.is_empty() {
        println!("  threads: {}", counts.join(" · "));
    }
}

pub async fn run(client: &Client, op: Op, as_json: bool) -> Result<()> {
    match op {
        Op::Threads(which) => {
            let res = client
                .get(
                    "/v1/mail/threads",
                    &[("state", which), ("limit", "100".into())],
                )
                .await?;
            if as_json {
                println!("{res}");
            } else {
                print_threads(res["threads"].as_array().map(Vec::as_slice).unwrap_or(&[]));
            }
        }
        Op::Set(id, state) => {
            let res = client
                .post(&format!("/v1/mail/threads/{id}"), &json!({"state": state}))
                .await?;
            if as_json {
                println!("{res}");
            } else {
                println!(
                    "✓ {} is now {}",
                    res["thread"]["subject"].as_str().unwrap_or(&id),
                    res["thread"]["state"].as_str().unwrap_or(&state)
                );
            }
        }
        Op::Rules => {
            let res = client.get("/v1/mail/rules", &[]).await?;
            if as_json {
                println!("{res}");
                return Ok(());
            }
            let rules = res["rules"].as_array().cloned().unwrap_or_default();
            if rules.is_empty() {
                println!("no mail rules yet — e.g. hearthloom mail rules add domain mylawfirm.com --action needs_reply");
            }
            for r in rules {
                println!(
                    "{}  {} {:?} → {}{}",
                    r["id"].as_str().unwrap_or("?"),
                    r["kind"].as_str().unwrap_or("?"),
                    r["pattern"].as_str().unwrap_or("?"),
                    r["action"].as_str().unwrap_or("?"),
                    if r["enabled"] == json!(false) {
                        " (off)"
                    } else {
                        ""
                    }
                );
            }
        }
        Op::RuleAdd {
            kind,
            pattern,
            action,
            note,
        } => {
            if kind != "plain" && action.is_none() {
                bail!("say what the rule does: --action needs_reply|scheduling|opportunity|fyi|ignore|triage");
            }
            let res = client
                .post(
                    "/v1/mail/rules",
                    &json!({"kind": kind, "pattern": pattern, "action": action, "note": note}),
                )
                .await?;
            if as_json {
                println!("{res}");
            } else {
                println!(
                    "✓ rule {} added — it applies to new mail from now on",
                    res["rule"]["id"].as_str().unwrap_or("?")
                );
            }
        }
        Op::RuleRemove(id) => {
            let res = client.delete(&format!("/v1/mail/rules/{id}"), &[]).await?;
            if as_json {
                println!("{res}");
            } else {
                println!("✓ rule {id} removed");
            }
        }
        Op::Triage {
            enabled,
            model,
            now,
        } => {
            let res = if enabled.is_some() || model.is_some() || now {
                client
                    .post(
                        "/v1/mail/triage",
                        &json!({"enabled": enabled, "model": model, "run": now}),
                    )
                    .await?
            } else {
                client.get("/v1/mail/triage", &[]).await?
            };
            if as_json {
                println!("{res}");
            } else {
                print_triage(&res);
                if now {
                    println!("  sorting what's waiting now");
                }
            }
        }
    }
    Ok(())
}
