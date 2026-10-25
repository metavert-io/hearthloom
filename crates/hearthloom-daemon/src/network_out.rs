//! Outbound network-brain calls: the RLM's nsearch/nfetch handles reach
//! a connected friend's brain under THEIR grant. Read-only by
//! construction (their surface only serves granted excerpts/bodies).
//!
//! Dispatch is preference-aware: every successful response may carry a
//! `preferred_url` (the friend's always-on node). When it differs from
//! what we have stored, we upgrade the connection automatically — so a
//! friend who pairs a cloud node later gets targeted there without
//! anyone reconfiguring (Jon's dispatch rule, 2026-07-11).

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::AppState;

fn blocking_get(url: &str, key: &str) -> Result<Value> {
    // run_tool is sync inside a tokio runtime — hop off the reactor.
    tokio::task::block_in_place(|| {
        let res = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?
            .get(url)
            .bearer_auth(key)
            .send()?;
        if !res.status().is_success() {
            return Err(anyhow!(
                "their brain answered {} — the grant may have been revoked or narrowed",
                res.status()
            ));
        }
        Ok(res.json()?)
    })
}

fn friend_conn(state: &AppState, friend: &str) -> Result<(String, String)> {
    let friends = state.store.friends()?;
    let (_, url, _) = friends
        .iter()
        .find(|(n, _, _)| n == friend)
        .ok_or_else(|| anyhow!("no connected brain named {friend:?}"))?;
    let key = crate::network_keys::friend_key(friend)
        .ok_or_else(|| anyhow!("the key for {friend:?} is missing — reconnect on the Network page"))?;
    Ok((url.clone(), key))
}

/// Auto-upgrade: adopt the friend's advertised always-on address.
fn maybe_upgrade(state: &AppState, friend: &str, current_url: &str, response: &Value) {
    if let Some(preferred) = response.get("preferred_url").and_then(|p| p.as_str()) {
        let preferred = preferred.trim_end_matches('/');
        if !preferred.is_empty() && preferred != current_url {
            let entity = state
                .store
                .friends()
                .ok()
                .and_then(|f| f.into_iter().find(|(n, _, _)| n == friend))
                .and_then(|(_, _, e)| e);
            if state
                .store
                .set_friend(
                    &hearthloom_core::store::Actor::new("agent:network"),
                    friend,
                    preferred,
                    entity.as_deref(),
                )
                .is_ok()
            {
                tracing::info!("network: {friend} upgraded to always-on address {preferred}");
            }
        }
    }
}

pub fn search(state: &AppState, friend: &str, query: &str) -> Result<String> {
    let (url, key) = friend_conn(state, friend)?;
    let response = blocking_get(
        &format!(
            "{url}/v1/network/search?q={}&limit=10",
            urlencoding_encode(query)
        ),
        &key,
    )?;
    maybe_upgrade(state, friend, &url, &response);
    let hits = response
        .get("hits")
        .and_then(|h| h.as_array())
        .cloned()
        .unwrap_or_default();
    if hits.is_empty() {
        return Ok(format!("no hits from {friend}'s brain (within what they share)"));
    }
    let lines: Vec<String> = hits
        .iter()
        .map(|h| {
            format!(
                "{} | {} [{}] {} — {}",
                h.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                h.get("day").and_then(|v| v.as_str()).unwrap_or(""),
                h.get("kind").and_then(|v| v.as_str()).unwrap_or(""),
                h.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                h.get("excerpt").and_then(|v| v.as_str()).unwrap_or(""),
            )
        })
        .collect();
    Ok(format!("from {friend}'s brain:\n{}", lines.join("\n")))
}

pub fn fetch(state: &AppState, friend: &str, id: &str) -> Result<String> {
    let (url, key) = friend_conn(state, friend)?;
    let response = blocking_get(
        &format!("{url}/v1/network/fetch?id={}", urlencoding_encode(id)),
        &key,
    )?;
    maybe_upgrade(state, friend, &url, &response);
    Ok(format!(
        "from {friend}'s brain ({id}):\n{}",
        response.get("body").and_then(|b| b.as_str()).unwrap_or("")
    ))
}

/// GET a friend-surface path and return the raw JSON (404/405 → a clear
/// "their node doesn't offer this yet" error — wire discipline; callers
/// that aggregate skip per-friend failures, never fail the whole view).
fn get_json(state: &AppState, friend: &str, path: &str) -> Result<Value> {
    let (url, key) = friend_conn(state, friend)?;
    let response = blocking_get(&format!("{url}{path}"), &key)?;
    maybe_upgrade(state, friend, &url, &response);
    Ok(response)
}

/// POST a friend-surface path (the collaborative-toggle leg).
fn post_json(state: &AppState, friend: &str, path: &str, body: &Value) -> Result<Value> {
    let (url, key) = friend_conn(state, friend)?;
    tokio::task::block_in_place(|| {
        let res = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?
            .post(format!("{url}{path}"))
            .bearer_auth(&key)
            .json(body)
            .send()?;
        if !res.status().is_success() {
            let msg: Value = res.json().unwrap_or_default();
            return Err(anyhow!(
                "their brain refused: {}",
                msg.get("error").and_then(|e| e.as_str()).unwrap_or("unknown")
            ));
        }
        Ok(res.json()?)
    })
}

pub fn tasks(state: &AppState, friend: &str, status: &str) -> Result<Value> {
    get_json(state, friend, &format!("/v1/network/tasks?status={}", urlencoding_encode(status)))
}

pub fn relationships(state: &AppState, friend: &str) -> Result<Value> {
    get_json(state, friend, "/v1/network/relationships")
}

pub fn notes(state: &AppState, friend: &str) -> Result<Value> {
    get_json(state, friend, "/v1/network/notes")
}

pub fn note(state: &AppState, friend: &str, id: &str) -> Result<Value> {
    get_json(state, friend, &format!("/v1/network/note/{}", urlencoding_encode(id)))
}

pub fn shared(state: &AppState, friend: &str) -> Result<Value> {
    get_json(state, friend, "/v1/network/shared")
}

pub fn offer(state: &AppState, friend: &str) -> Result<Value> {
    get_json(state, friend, "/v1/network/offer")
}

pub fn profile(state: &AppState, friend: &str) -> Result<Value> {
    get_json(state, friend, "/v1/network/profile")
}

pub fn toggle(state: &AppState, friend: &str, task_id: &str, body: &Value) -> Result<Value> {
    post_json(
        state,
        friend,
        &format!("/v1/network/tasks/{}/toggle", urlencoding_encode(task_id)),
        body,
    )
}

/// Seed (or refresh) the relationship record for a connected brain —
/// idempotent, so it runs on connect AND retroactively for connections
/// made before v46. The friend's own profile provides the base facts;
/// the stable handle `brain:<name>` is the identity claim that makes
/// re-runs converge on the same Person even before any email is known.
/// Later imports that carry their email enrich the SAME entity through
/// the normal identity machinery.
pub fn ensure_friend_card(state: &AppState, friend: &str) -> Result<Value> {
    use hearthloom_core::store::ClaimOutcome;
    let actor = hearthloom_core::store::Actor::new("agent:network");
    let friends = state.store.friends()?;
    let (_, url, existing_entity) = friends
        .iter()
        .find(|(n, _, _)| n == friend)
        .ok_or_else(|| anyhow!("no connected brain named {friend:?}"))?
        .clone();
    // their card, if their node offers profile-v1 (older peers 404 → minimal)
    let card = profile(state, friend).unwrap_or(Value::Null);
    let display_name = card
        .get("name")
        .and_then(|n| n.as_str())
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(friend)
        .to_string();
    // the connection IS an identity: bind the stable brain handle
    let outcome = state.store.claim_identity(
        &actor,
        &display_name,
        "handle",
        &format!("brain:{friend}"),
        Some("personal"),
    )?;
    let entity_id = match outcome {
        ClaimOutcome::Attached { entity_id }
        | ClaimOutcome::AlreadyYours { entity_id }
        | ClaimOutcome::MergedFrom { entity_id, .. } => entity_id,
    };
    // their emails bind too — this links the connection to a person the
    // record may already know from mail/messages
    for email in card
        .get("emails")
        .and_then(|e| e.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .take(4)
    {
        let _ = state
            .store
            .claim_identity(&actor, &display_name, "email", email, Some("personal"));
    }
    // the relationship row: connecting was a deliberate act → declared;
    // add_relationship refuses a duplicate kind per entity → idempotent
    let _ = state.store.add_relationship(
        &actor,
        &entity_id,
        "friend",
        "declared",
        &[],
        Some(&format!("connected brain: {friend}")),
        None,
    );
    if existing_entity.as_deref() != Some(entity_id.as_str()) {
        state.store.set_friend(&actor, friend, &url, Some(&entity_id))?;
    }
    Ok(serde_json::json!({
        "friend": friend, "entity": entity_id, "name": display_name,
        "profile": card,
    }))
}

/// Diff a friend's shared bundle against the last-seen snapshot and
/// mint notifications for what changed: new shares, checklist progress,
/// completions. Runs from the Desk's shared-rail load AND the
/// friend_activity consolidation pass; idempotent — the snapshot in
/// consolidation_state (`friend_seen:<name>`) advances only after the
/// notifications land, and dedupe keys keep re-runs quiet.
pub fn scan_friend_activity(state: &AppState, friend: &str, bundle: &Value) -> usize {
    let seen_key = format!("friend_seen:{friend}");
    let prev: Value = state
        .store
        .consolidation_state(&seen_key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let first_look = prev.is_null();
    let mut snapshot = serde_json::Map::new();
    let mut notified = 0usize;
    for t in bundle.get("tasks").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some(id) = t.get("id").and_then(|v| v.as_str()) else { continue };
        let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("a task");
        let status = t.get("status").and_then(|v| v.as_str()).unwrap_or("open");
        let done_items = t
            .get("payload")
            .and_then(|p| p.get("items"))
            .and_then(|i| i.as_array())
            .map(|items| items.iter().filter(|i| i["done"] == serde_json::json!(true)).count())
            .unwrap_or(0);
        let sig = format!("{status}:{done_items}");
        snapshot.insert(id.to_string(), serde_json::json!(sig));
        if first_look {
            continue; // baseline quietly — don't replay history as news
        }
        match prev.get(id).and_then(|v| v.as_str()) {
            None => {
                crate::provisioner::notify_system(
                    state, "info",
                    &format!("{friend} shared a task with you"),
                    &format!("\"{desc}\" is now on your shared rail"),
                    Some(&format!("fshare:{friend}:{id}")),
                );
                notified += 1;
            }
            Some(old) if old != sig => {
                let msg = if status == "done" && !old.starts_with("done") {
                    format!("\"{desc}\" was completed")
                } else {
                    format!("\"{desc}\" — {done_items} item(s) now done")
                };
                crate::provisioner::notify_system(
                    state, "info",
                    &format!("{friend}'s shared task changed"),
                    &msg,
                    Some(&format!("fact:{friend}:{id}:{sig}")),
                );
                notified += 1;
            }
            _ => {}
        }
    }
    for n in bundle.get("notes").and_then(|v| v.as_array()).into_iter().flatten() {
        let Some(id) = n.get("id").and_then(|v| v.as_str()) else { continue };
        let key = format!("note:{id}");
        snapshot.insert(key.clone(), serde_json::json!("shared"));
        if !first_look && prev.get(&key).is_none() {
            crate::provisioner::notify_system(
                state, "info",
                &format!("{friend} shared a note with you"),
                &format!("\"{}\"", n.get("title").and_then(|v| v.as_str()).unwrap_or("a note")),
                Some(&format!("fshare:{friend}:{id}")),
            );
            notified += 1;
        }
    }
    let _ = state
        .store
        .set_consolidation_state(&seen_key, &Value::Object(snapshot).to_string());
    notified
}

/// The friend_activity pass: poll each connection's shared bundle and
/// notify on changes — so news arrives even when the Desk isn't open.
/// Ask a friend's node which of ITS groups this record is in, and keep
/// the answer (replacing the last one). An unreachable or older node —
/// any failure — leaves what we knew untouched.
pub fn refresh_peer_groups(state: &AppState, friend: &str) {
    let Ok(answer) = get_json(state, friend, "/v1/network/groups") else {
        return;
    };
    let Some(groups) = answer.get("groups").and_then(|g| g.as_array()) else {
        return;
    };
    if let Err(err) = state.store.set_peer_groups(friend, groups) {
        tracing::warn!("could not record {friend}'s groups: {err:#}");
    }
}

pub fn friend_activity_scan(state: &AppState) -> usize {
    let Ok(friends) = state.store.friends() else { return 0 };
    let mut total = 0usize;
    for (name, _, _) in friends {
        if let Ok(bundle) = shared(state, &name) {
            total += scan_friend_activity(state, &name, &bundle);
        }
        refresh_peer_groups(state, &name);
    }
    total
}

pub(crate) fn urlencoding_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The RLM prompt fragment — present when ≥1 friend is connected
/// (outbound handles) OR ≥1 grant exists (the owner introspecting what
/// THEY share — "what am I sharing with Sam?" must work on the
/// granting node even when it has no outbound connections).
pub fn rlm_prompt_section(state: &AppState) -> Option<String> {
    let friends = state.store.friends().ok()?;
    if friends.is_empty() {
        let has_grants = state
            .store
            .list_grants()
            .map(|g| !g.is_empty())
            .unwrap_or(false);
        if !has_grants {
            return None;
        }
        return Some(
            "\n\nSharing (this record grants slices of itself to connected friends): \
             extra READ handle `sharing {}` — the full sharing posture (default \
             template, per-friend resolved permissions, explicitly shared items). \
             Use it for any \"what am I sharing / who can see X?\" question."
                .into(),
        );
    }
    let names: Vec<String> = friends.iter().map(|(n, _, _)| n.clone()).collect();
    Some(format!(
        "\n\nNetworked brains (friends who granted this record access to slices of \
         theirs — read-only, consent-governed; you see ONLY what each grant covers):\n\
         - connected: {}\n\
         Extra READ handles:\n\
         - nsearch {{friend, query}}   search what that friend shares\n\
         - nfetch {{friend, id}}       one shared memory's full text (grant permitting)\n\
         - ntasks {{friend, status?}}  their visible tasks (open|backlog|done; grant/shares permitting)\n\
         - nrels {{friend}}            relationships they share (declared + anything shared to us)\n\
         - nnotes {{friend}}           notes they share (metadata)\n\
         - nnote {{friend, id}}        one shared note's full text\n\
         - noffer {{friend}}           what their brain currently OFFERS this record\n\
         - sharing {{}}                what THIS record shares (default template + per-friend + items) — LOCAL, use for \"what am I sharing?\"\n\
         RULES: network results are ANOTHER PERSON's record — never put their ids in \
         \"citations\" (those are for this record only); attribute in prose instead \
         (\"according to Ada's brain, …\", with the day). If a fetch is refused, the \
         grant doesn't cover it — say so plainly, never work around it. Use the \
         network only when the question involves that person or the user asks. \
         ENFORCED: a friend handle runs only when the user's own question names \
         that friend, and only BEFORE this ask has read imported content (mail, \
         exports, mounts) or another friend's brain — so call friend handles \
         FIRST. A blocked call says why; relay it, don't retry.",
        names.join(", ")
    ))
}
