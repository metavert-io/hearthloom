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
    Embers {
        ember: Option<String>,
        view: Option<String>,
        chars: Option<i64>,
        reset: bool,
    },
    Draft {
        id: String,
        say: Option<String>,
        all: bool,
        wait: bool,
    },
    Drafts {
        model: Option<String>,
    },
}

fn print_draft(d: &Value) {
    match d["state"].as_str() {
        Some("saved") => {
            println!(
                "✓ a reply is in your Drafts ({}) — to {}",
                d["folder"].as_str().unwrap_or("Drafts"),
                d["recipients"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!("  {}\n", d["subject"].as_str().unwrap_or(""));
            for line in d["body"].as_str().unwrap_or("").lines() {
                println!("  {line}");
            }
            let fill: Vec<&str> = d["placeholders"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str())
                .collect();
            if !fill.is_empty() {
                println!("\n  fill in: {}", fill.join(", "));
            }
            println!("  Nothing was sent: review and send it from Gmail.");
        }
        Some("failed") => println!(
            "the draft didn't work: {}",
            d["error"].as_str().unwrap_or("unknown")
        ),
        Some(s) => println!("{} {s}", d["id"].as_str().unwrap_or("?")),
        None => println!("?"),
    }
}

fn view_words(view: &Value, chars: &Value) -> String {
    match view.as_str() {
        Some("headers") => "sender, subject and date".into(),
        Some("snippet") => format!(
            "sender, subject, date and the first {} characters",
            chars.as_i64().unwrap_or(300)
        ),
        Some("full") => "the whole readable text".into(),
        _ => "?".into(),
    }
}

fn print_embers(res: &Value) {
    println!(
        "embers see: {} (unless set per ember)",
        view_words(&res["default"]["view"], &res["default"]["chars"])
    );
    let embers = res["embers"].as_array().cloned().unwrap_or_default();
    if embers.is_empty() {
        println!("no ember watches your mail");
    }
    for e in embers {
        let name = e["name"].as_str().unwrap_or("?");
        let mut notes = vec![];
        if e["own_view"] == json!(true) {
            notes.push(format!("sees {}", view_words(&e["view"], &e["chars"])));
        }
        if e["granted"] == json!(false) {
            notes.push("installed here without mail.watch: gets nothing".into());
        } else if e["installed_here"] == json!(false) {
            notes.push("runs on another node".into());
        }
        println!(
            "{name}{}",
            if notes.is_empty() {
                String::new()
            } else {
                format!("  ({})", notes.join("; "))
            }
        );
        for w in e["watches"].as_array().into_iter().flatten() {
            let f = &w["filter"];
            let parts: Vec<String> = ["from", "domains", "subject", "words", "labels", "classes"]
                .iter()
                .filter_map(|k| {
                    let vals: Vec<&str> = f[*k]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str())
                        .collect();
                    (!vals.is_empty()).then(|| format!("{k}: {}", vals.join(" | ")))
                })
                .collect();
            println!(
                "  · {} — {}{}",
                w["name"].as_str().unwrap_or("?"),
                parts.join("; "),
                if w["enabled"] == json!(false) {
                    " (off)"
                } else {
                    ""
                }
            );
        }
    }
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
        Op::Draft { id, say, all, wait } => {
            let res = client
                .post(
                    &format!("/v1/mail/threads/{id}/draft"),
                    &json!({"say": say, "reply_all": all}),
                )
                .await?;
            let draft_id = res["draft"]["id"].as_str().unwrap_or("").to_string();
            if !wait {
                if as_json {
                    println!("{res}");
                } else {
                    println!(
                        "asked for a draft ({draft_id}); it's written on {}",
                        res["drafter"]["writer"]
                            .as_str()
                            .unwrap_or("the machine that watches Gmail")
                    );
                }
                return Ok(());
            }
            if !as_json {
                eprintln!(
                    "drafting on {}…",
                    res["drafter"]["writer"]
                        .as_str()
                        .unwrap_or("the machine that watches Gmail")
                );
            }
            let started = std::time::Instant::now();
            let draft = loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let got = client
                    .get(&format!("/v1/mail/drafts/{draft_id}"), &[])
                    .await?;
                let state = got["draft"]["state"].as_str().unwrap_or("");
                if state == "saved" || state == "failed" {
                    break got["draft"].clone();
                }
                if started.elapsed() > std::time::Duration::from_secs(300) {
                    bail!(
                        "still {state} after five minutes — `hearthloom mail drafts` shows it when it's done"
                    );
                }
            };
            if as_json {
                println!("{}", json!({"draft": draft}));
            } else {
                print_draft(&draft);
            }
        }
        Op::Drafts { model } => {
            let res = match &model {
                Some(m) => {
                    client
                        .post("/v1/mail/drafts/settings", &json!({"model": m}))
                        .await?;
                    client
                        .get("/v1/mail/drafts", &[("limit", "20".into())])
                        .await?
                }
                None => {
                    client
                        .get("/v1/mail/drafts", &[("limit", "20".into())])
                        .await?
                }
            };
            if as_json {
                println!("{res}");
                return Ok(());
            }
            let d = &res["drafter"];
            println!(
                "drafts are written on {}{} · model {}",
                d["writer"]
                    .as_str()
                    .unwrap_or("no machine (Gmail isn't watched anywhere)"),
                if d["writer_is_me"] == json!(true) {
                    " (this machine)"
                } else {
                    ""
                },
                d["model"].as_str().unwrap_or("?")
            );
            if model.is_some() && d["writer_is_me"] != json!(true) {
                println!(
                    "  the model setting belongs to the machine that writes them: use --url for it"
                );
            }
            let drafts = res["drafts"].as_array().cloned().unwrap_or_default();
            if drafts.is_empty() {
                println!("no drafts yet — hearthloom mail draft <thread id>");
            }
            for x in drafts {
                println!(
                    "{}  {}  {}  {}",
                    x["created_at"].as_str().unwrap_or("?"),
                    x["state"].as_str().unwrap_or("?"),
                    x["subject"]
                        .as_str()
                        .or(x["thread_id"].as_str())
                        .unwrap_or("?"),
                    x["requested_by"]
                        .as_str()
                        .and_then(|r| r.strip_prefix("plugin:"))
                        .map(|e| format!("(by {e})"))
                        .unwrap_or_default()
                );
                if let Some(e) = x["error"].as_str() {
                    println!("    {e}");
                }
            }
        }
        Op::Embers {
            ember,
            view,
            chars,
            reset,
        } => {
            let res = if view.is_some() || chars.is_some() || reset {
                if !reset && view.is_none() {
                    bail!("say what it sees: --view headers|snippet|full");
                }
                client
                    .post(
                        "/v1/mail/embers",
                        &json!({"ember": ember, "view": view, "chars": chars, "reset": reset}),
                    )
                    .await?
            } else {
                client.get("/v1/mail/embers", &[]).await?
            };
            if as_json {
                println!("{res}");
            } else {
                print_embers(&res);
            }
        }
    }
    Ok(())
}
