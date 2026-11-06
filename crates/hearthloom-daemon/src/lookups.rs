//! Looking things up on another of the owner's nodes (0.5.88).
//!
//! Each node holds its own mail archives (the Takeout on one machine, the
//! live Gmail archive on another), and Ask runs where there is an AI key.
//! Peering has a fixed shape: spokes (a laptop, a home server) dial the hub
//! (the hosted node); nothing dials a spoke. So a lookup on another node
//! goes THROUGH the hub, the way the push bus does:
//!
//! 1. the asking node posts it to the hub (`POST /v1/sync/lookup`) — or,
//!    on the hub, queues it directly — and waits up to `WAIT`;
//! 2. the hub tells its spokes over the push bus (`lookup.requested`, a
//!    hint with no content);
//! 3. the node holding the archive fetches what's for it
//!    (`GET /v1/sync/lookup/pending`), runs it under ITS privacy rules
//!    with the asker's vault state, and posts the answer
//!    (`POST /v1/sync/lookup/{id}/answer`);
//! 4. the hub hands the answer to the waiting asker.
//!
//! Requests live in the hub's memory only, for seconds. Only peer keys —
//! the owner's own nodes, full trust — reach these routes. A node that's
//! asleep simply doesn't answer: the asker says so and carries on.
//!
//! Each node also publishes what archives it holds in the node list
//! (`archives_here`), so Ask anywhere knows what exists and where.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::AppState;

/// How long an asker waits for the node holding the archive.
pub const WAIT: Duration = Duration::from_secs(25);
/// An unclaimed lookup's hint is repeated this often.
const HINT_AGAIN: Duration = Duration::from_secs(2);
/// A request nobody claimed is dropped after this.
const STALE: Duration = Duration::from_secs(60);
/// What another node may ask: Ask's two archive reads, and `vchunk` — a
/// piece of an archive file, for a keeper bringing it in (archives_place).
pub const TOOLS: &[&str] = &["vsearch", "vfetch", "vchunk"];
pub const TOPIC: &str = "lookup.requested";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lookup {
    pub id: String,
    /// the node that holds the archive
    pub node: String,
    pub tool: String,
    pub args: Value,
    /// the asking viewer has the vault open (peers are the owner's own
    /// nodes: believed)
    pub vault_open: bool,
}

struct Entry {
    lookup: Lookup,
    at: Instant,
    claimed: bool,
    answer: Option<std::result::Result<String, String>>,
}

static QUEUE: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

fn with_queue<T>(f: impl FnOnce(&mut HashMap<String, Entry>) -> T) -> T {
    let mut guard = QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let queue = guard.get_or_insert_with(HashMap::new);
    queue.retain(|_, e| e.at.elapsed() < STALE);
    f(queue)
}

/* ————— what this node holds, for the node list ————— */

/// This node's mail archives, as the node list carries them.
pub fn archives_here(state: &AppState) -> Value {
    let mut out = vec![];
    // the live mailboxes, while Gmail is connected here (0.5.89): one
    // `gmail` source over all of them (`account` = the first, for older
    // nodes; `accounts` = every one, 0.5.98)
    let accounts = crate::gmail_search::accounts();
    if let Some(first) = accounts.first() {
        out.push(json!({"name": crate::gmail_search::SOURCE,
                        "adapter": crate::gmail_search::ADAPTER, "account": first,
                        "accounts": accounts}));
    }
    for def in state.store.list_virtual_sources().unwrap_or_default() {
        let Some(mbox) = crate::virtual_source::mbox_path_if_online(&def) else {
            continue;
        };
        let summary = crate::mbox_index::summary(&mbox);
        let text = if crate::mbox_text::is_off(&mbox) {
            json!("off")
        } else {
            match crate::mbox_text::progress(&mbox) {
                Some((done, total)) if total > 0 && done * 100 >= total * 99 => json!("on"),
                Some((done, total)) if total > 0 => json!(format!("{}%", done * 100 / total)),
                _ => json!("starting"),
            }
        };
        let (bytes, settled) =
            crate::archives_place::file_facts(&mbox, &def.name).unwrap_or((0, false));
        out.push(json!({
            "name": def.name,
            "adapter": def.adapter,
            "bytes": bytes,
            "settled": settled,
            "messages": summary.as_ref().map(|s| s.messages),
            "first": summary.as_ref().and_then(|s| s.first.clone()),
            "last": summary.as_ref().and_then(|s| s.last.clone()),
            "text": text,
        }));
    }
    Value::Array(out)
}

/// Archives other nodes hold: (node id, node name, last seen, archive).
pub fn archives_elsewhere(state: &AppState) -> Vec<(String, String, String, Value)> {
    let here = crate::agents::node_id(state);
    let mut out = vec![];
    for node in state.store.nodes().unwrap_or_default() {
        let id = node["node_id"].as_str().unwrap_or_default().to_string();
        if id == here {
            continue;
        }
        let name = node["name"].as_str().unwrap_or_default().to_string();
        let seen = node["last_seen"].as_str().unwrap_or_default().to_string();
        let roles: Value = match &node["roles"] {
            Value::String(raw) => serde_json::from_str(raw).unwrap_or(Value::Null),
            other => other.clone(),
        };
        for archive in roles["archives"].as_array().into_iter().flatten() {
            out.push((id.clone(), name.clone(), seen.clone(), archive.clone()));
        }
    }
    out
}

/// Seen in the last ten minutes (the heartbeat is five).
pub fn recently_seen(last_seen: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(last_seen)
        .map(|t| chrono::Utc::now().signed_duration_since(t) < chrono::Duration::minutes(10))
        .unwrap_or(false)
}

/// The node a name (or id) means.
pub fn resolve_node(state: &AppState, name: &str) -> Option<(String, String, String)> {
    state.store.nodes().ok()?.into_iter().find_map(|n| {
        let id = n["node_id"].as_str()?.to_string();
        let nm = n["name"].as_str()?.to_string();
        (id == name || nm.eq_ignore_ascii_case(name.trim()))
            .then(|| (id, nm, n["last_seen"].as_str().unwrap_or("").to_string()))
    })
}

/* ————— running one, on the node that holds the archive ————— */

/// Run a lookup here, as the asker's vault state allows.
pub fn run_here(state: &AppState, tool: &str, args: &Value, vault_open: bool) -> Result<String> {
    if tool == "vchunk" {
        // raw bytes for the owner's own keeper node (peers already carry
        // everything, private included)
        return crate::archives_place::chunk(state, args);
    }
    let source = args["source"]
        .as_str()
        .ok_or_else(|| anyhow!("{tool} needs args.source"))?;
    let def = crate::virtual_source::resolve(state, source)?;
    let locked = !vault_open;
    match tool {
        "vsearch" => {
            let query = args["query"]
                .as_str()
                .ok_or_else(|| anyhow!("vsearch needs args.query"))?;
            let limit = args["limit"].as_u64().unwrap_or(10).min(25) as usize;
            let hits = crate::virtual_source::search_with(state, &def, query, limit, locked)?;
            Ok(if hits.is_empty() {
                "no hits".into()
            } else {
                hits.iter()
                    .map(|h| format!("{} | {}", h.path, h.snippet))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        "vfetch" => {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow!("vfetch needs args.path"))?;
            let budget = args["budget"].as_u64().map(|b| b as u32);
            crate::virtual_source::fetch_with(state, &def, path, budget, locked)
        }
        other => bail!("{other} can't run on another node"),
    }
}

/* ————— the hub's queue ————— */

/// Queue a lookup for a spoke, tell the spokes, and wait for the answer.
pub async fn queue_and_wait(state: &AppState, lookup: Lookup) -> Result<String> {
    let id = lookup.id.clone();
    with_queue(|q| {
        q.insert(
            id.clone(),
            Entry {
                lookup,
                at: Instant::now(),
                claimed: false,
                answer: None,
            },
        )
    });
    crate::bus::publish_local(state, TOPIC, None, Vec::new()).await;
    let deadline = Instant::now() + WAIT;
    let mut hinted = Instant::now();
    loop {
        // the bus delivers at most once (a spoke reconnecting misses it):
        // say it again while nobody has picked the lookup up
        if hinted.elapsed() >= HINT_AGAIN && with_queue(|q| q.get(&id).is_some_and(|e| !e.claimed))
        {
            crate::bus::publish_local(state, TOPIC, None, Vec::new()).await;
            hinted = Instant::now();
        }
        let answer = with_queue(|q| q.get(&id).and_then(|e| e.answer.clone()));
        if let Some(answer) = answer {
            with_queue(|q| q.remove(&id));
            return answer.map_err(|e| anyhow!(e));
        }
        if Instant::now() >= deadline {
            let claimed = with_queue(|q| q.remove(&id).is_some_and(|e| e.claimed));
            bail!(if claimed {
                "the node holding it started but didn't finish in time"
            } else {
                "the node holding it didn't answer (it may be asleep or offline)"
            });
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// What's waiting for this node (claimed as it's handed out).
pub fn pending_for(node: &str) -> Vec<Lookup> {
    with_queue(|q| {
        q.values_mut()
            .filter(|e| e.lookup.node == node && !e.claimed)
            .map(|e| {
                e.claimed = true;
                e.lookup.clone()
            })
            .collect()
    })
}

pub fn answer(id: &str, outcome: std::result::Result<String, String>) -> bool {
    with_queue(|q| match q.get_mut(id) {
        Some(e) => {
            e.answer = Some(outcome);
            true
        }
        None => false,
    })
}

/* ————— the asking side ————— */

/// Look something up in an archive another node holds. On the hub: queue
/// it. On a spoke: ask the hub, which runs it itself if it holds the
/// archive, or relays it.
pub async fn ask_remote(
    state: &AppState,
    node_id: &str,
    tool: &str,
    args: &Value,
    vault_open: bool,
) -> Result<String> {
    if !TOOLS.contains(&tool) {
        bail!("{tool} can't run on another node");
    }
    let lookup = Lookup {
        id: format!("lk_{}", uuid::Uuid::new_v4().simple()),
        node: node_id.to_string(),
        tool: tool.to_string(),
        args: args.clone(),
        vault_open,
    };
    match (state.store.peer()?, crate::sync::peer_key()) {
        (Some((_, url)), Some(key)) => {
            let res = reqwest::Client::new()
                .post(format!("{url}/v1/sync/lookup"))
                .bearer_auth(key)
                .timeout(WAIT + Duration::from_secs(10))
                .json(&lookup)
                .send()
                .await
                .map_err(|e| anyhow!("couldn't reach the hub: {e}"))?;
            let status = res.status();
            let body: Value = res.json().await.unwrap_or_default();
            if status == reqwest::StatusCode::NOT_FOUND {
                bail!("the hub runs an older Hearthloom that can't relay lookups yet");
            }
            match (body["result"].as_str(), body["error"].as_str()) {
                (Some(result), _) => Ok(result.to_string()),
                (_, Some(error)) => Err(anyhow!("{error}")),
                _ => Err(anyhow!("the hub answered {status}")),
            }
        }
        // no hub above us: we are the hub
        _ => queue_and_wait(state, lookup).await,
    }
}

/// A spoke heard `lookup.requested`: fetch what's for it, run, answer.
pub async fn serve_pending(state: AppState) {
    let (Ok(Some((_, url))), Some(key)) = (state.store.peer(), crate::sync::peer_key()) else {
        return;
    };
    let me = crate::agents::node_id(&state);
    let client = reqwest::Client::new();
    let pending: Vec<Lookup> = match client
        .get(format!("{url}/v1/sync/lookup/pending"))
        .query(&[("node", me.as_str())])
        .bearer_auth(&key)
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(res) => res
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| serde_json::from_value(v["lookups"].clone()).ok())
            .unwrap_or_default(),
        Err(_) => return,
    };
    for lookup in pending {
        let st = state.clone();
        let (tool, args, open) = (lookup.tool.clone(), lookup.args.clone(), lookup.vault_open);
        let outcome = tokio::time::timeout(
            WAIT,
            tokio::task::spawn_blocking(move || run_here(&st, &tool, &args, open)),
        )
        .await;
        let body = match outcome {
            Ok(Ok(Ok(result))) => json!({"result": result}),
            Ok(Ok(Err(err))) => json!({"error": format!("{err:#}")}),
            _ => json!({"error": "it took too long on the node holding it"}),
        };
        let _ = client
            .post(format!("{url}/v1/sync/lookup/{}/answer", lookup.id))
            .bearer_auth(&key)
            .timeout(Duration::from_secs(15))
            .json(&body)
            .send()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_the_hub_queue_hands_out_and_returns_answers() {
        let lookup = |node: &str| Lookup {
            id: format!("lk_{}", uuid::Uuid::new_v4().simple()),
            node: node.into(),
            tool: "vsearch".into(),
            args: json!({"source": "gmail-live", "query": "portal"}),
            vault_open: false,
        };
        let mine = lookup("nd_mini");
        let other = lookup("nd_laptop");
        let id = mine.id.clone();
        with_queue(|q| {
            for l in [mine.clone(), other.clone()] {
                q.insert(
                    l.id.clone(),
                    Entry {
                        lookup: l,
                        at: Instant::now(),
                        claimed: false,
                        answer: None,
                    },
                );
            }
        });
        let got = pending_for("nd_mini");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, id);
        assert!(pending_for("nd_mini").is_empty(), "handed out once");
        assert!(answer(&id, Ok("m1@x | …".into())));
        assert!(!answer("lk_nobody", Ok(String::new())));
        let answered = with_queue(|q| q.get(&id).and_then(|e| e.answer.clone()));
        assert_eq!(answered, Some(Ok("m1@x | …".to_string())));
    }

    /// Another node's archives reach Ask's instructions through the node
    /// list; this node's own are left to the local list.
    #[test]
    fn test_ask_learns_what_other_nodes_hold() {
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
        let archive = json!({"name": "gmail-live", "adapter": "mbox", "messages": 19544,
                             "first": "2026-08", "last": "2026-11", "text": "on"});
        state
            .store
            .publish_node(
                "nd_mini",
                "Mac mini",
                "local",
                &json!({"archives": [archive]}),
                "0.5.88",
            )
            .unwrap();
        crate::agents::publish_self(&state);
        let elsewhere = archives_elsewhere(&state);
        assert_eq!(elsewhere.len(), 1);
        assert_eq!(elsewhere[0].1, "Mac mini");
        assert_eq!(elsewhere[0].3["name"], "gmail-live");
        let prompt = crate::virtual_source::rlm_prompt_section(&state).unwrap();
        assert!(prompt.contains(r#"gmail-live [mbox] on node "Mac mini" — about 19,500 messages, 2026-08 → 2026-11; its message text and links are searchable"#), "{prompt}");
        assert!(prompt.contains("Finding things in mail"));
        assert_eq!(
            resolve_node(&state, "mac mini").map(|n| n.0),
            Some("nd_mini".to_string())
        );
    }

    #[test]
    fn test_only_recent_nodes_count_as_reachable() {
        let now = chrono::Utc::now();
        assert!(recently_seen(&now.to_rfc3339()));
        assert!(!recently_seen(
            &(now - chrono::Duration::hours(2)).to_rfc3339()
        ));
        assert!(!recently_seen("not a time"));
    }
}
