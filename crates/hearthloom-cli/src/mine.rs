//! `hearthloom mine`: the sources you say are yours ("this source is
//! mine", core store/owned.rs). A repository you mark counts its commits
//! by one of your own addresses as your words (not merges, co-written
//! commits or commits made while an agent ran); a publication you mark
//! counts every post. Ask may then message friends, and agents act under
//! your normal rules, after reading them. Adding needs the vault unlocked.
use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::client::Client;

pub enum Op {
    List,
    Add { source: String, address: String },
    Remove(Vec<String>),
}

fn day(ts: &Value) -> &str {
    ts.as_str().map(|t| t.get(..10).unwrap_or(t)).unwrap_or("?")
}

/// How many stored memories an entry names.
fn stored_note(n: u64) -> String {
    match n {
        0 => "nothing stored under it yet".into(),
        1 => "1 memory stored under it".into(),
        n => format!("{n} memories stored under it"),
    }
}

pub async fn run(client: &Client, op: Op, as_json: bool) -> Result<()> {
    match op {
        Op::List => {
            let res = client.get("/v1/owned-sources", &[]).await?;
            if as_json {
                println!("{res}");
                return Ok(());
            }
            let entries = res["entries"].as_array().cloned().unwrap_or_default();
            if entries.is_empty() {
                println!("nothing is marked as yours — e.g. hearthloom mine add git https://github.com/you/repo");
            }
            for e in &entries {
                let id = e["id"].as_str().unwrap_or("?");
                println!(
                    "{id}  {} {}  (set {} by {}; {})",
                    e["source"].as_str().unwrap_or("?"),
                    e["key"].as_str().unwrap_or("?"),
                    day(&e["created_at"]),
                    e["set_by"].as_str().unwrap_or("?"),
                    stored_note(res["stored"][id].as_u64().unwrap_or(0)),
                );
            }
            let identities: Vec<&str> = res["identities"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str())
                .collect();
            println!();
            if identities.is_empty() {
                println!(
                    "your addresses: none known — a repository's commits count as yours only when \
                     their author is one of your addresses (a connected mailbox, its aliases, or \
                     the addresses on your profile card, which friends you share your profile \
                     with can see)"
                );
            } else {
                println!(
                    "your addresses (commits by these count): {}",
                    identities.join(", ")
                );
            }
            println!(
                "not counted even then: merges, commits co-written with anyone else (a \
                 Co-authored-by line), commits with one of your agents at work anywhere between \
                 when they were first written and their last rewrite, and commits (or posts) an \
                 earlier version of Hearthloom imported. Git \
                 doesn't check author addresses — mark only repositories whose history you control."
            );
        }
        Op::Add { source, address } => {
            let res = client
                .post(
                    "/v1/owned-sources",
                    &json!({"source": source, "address": address}),
                )
                .await?;
            if as_json {
                println!("{res}");
                return Ok(());
            }
            let e = &res["entry"];
            if res["added"] == json!(true) {
                println!(
                    "✓ {} {} is yours ({}) — Ask may message friends and agents may act under \
                     your normal rules after reading it",
                    e["source"].as_str().unwrap_or("?"),
                    e["key"].as_str().unwrap_or("?"),
                    e["id"].as_str().unwrap_or("?"),
                );
            } else {
                println!(
                    "{} {} was already yours (since {})",
                    e["source"].as_str().unwrap_or("?"),
                    e["key"].as_str().unwrap_or("?"),
                    day(&e["created_at"]),
                );
            }
            if res["stored"].as_u64() == Some(0) {
                println!(
                    "  nothing stored is filed under {} yet, so this changes nothing for now \
                     (a publication with its own domain files its posts under that domain)",
                    e["key"].as_str().unwrap_or("?"),
                );
            }
        }
        Op::Remove(target) => {
            let id = match target.as_slice() {
                [id] => id.clone(),
                [source, address] => {
                    let found = client
                        .get(
                            "/v1/owned-sources/check",
                            &[("source", source.clone()), ("address", address.clone())],
                        )
                        .await?;
                    match found["entry"]["id"].as_str() {
                        Some(id) => id.to_string(),
                        None => bail!(
                            "{source} {} isn't on your list",
                            found["key"].as_str().unwrap_or(address)
                        ),
                    }
                }
                _ => bail!("say which: hearthloom mine remove <id>, or <source> <address>"),
            };
            let res = client
                .delete(&format!("/v1/owned-sources/{id}"), &[])
                .await?;
            if as_json {
                println!("{res}");
            } else {
                println!(
                    "✓ {} {} is outside text again",
                    res["removed"]["source"].as_str().unwrap_or("?"),
                    res["removed"]["key"].as_str().unwrap_or(&id),
                );
            }
        }
    }
    Ok(())
}
