//! The hybrid-sync engine: a consolidation-style background loop that
//! runs ONLY when a peer is paired. Both directions are initiated from
//! here (the counterpart never dials us — NAT reality):
//!
//!   push:  export local-new episodes → POST {peer}/v1/ingest
//!   pull:  GET {peer}/v1/sync/episodes → apply locally
//!
//! Idempotent both ways (episode ids are deterministic), so overlap,
//! retries, and echo-back all converge to no-ops. Private never moves:
//! the export feed on BOTH nodes hardcodes the exclusion. Watermarks
//! live in consolidation_state; the peer's bearer key lives in
//! ~/.hearthloom/.env (0600), never the DB. An offline peer is a clean
//! skip — sync is best-effort by doctrine, never a dependency.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::AppState;
use hearthloom_core::ingest::{EpisodeDraft, IngestBatch};
use hearthloom_core::Home;

const SYNC_INTERVAL_SECS: u64 = 300;
const BATCH: usize = 100;
/// The unfair-mutex rule: breathe between batches so a long backfill
/// never starves the dashboard.
const BATCH_SLEEP_MS: u64 = 60;

const KEY_PUSH_CURSOR: &str = "peer_push_cursor";
const KEY_PULL_CURSOR: &str = "peer_pull_cursor";
const KEY_LAST: &str = "peer_last_outcome";
const KEY_SCOPE_PUSH: &str = "peer_scope_push_ts";
const KEY_SCOPE_PULL: &str = "peer_scope_pull_ts";
const KEY_PAUSED: &str = "peer_sync_paused";
/// Stamp of the last cycle whose operational mirror POST succeeded.
/// The mirror is replace-all-per-cycle, so "has it ever landed" is the
/// honest all-or-nothing progress signal for that category.
const KEY_MIRROR_TS: &str = "peer_mirror_ts";
/// What the counterpart last told us about ITSELF (stored on the
/// receiving node; renders as the "inbound" card on its Peering page).
const KEY_INBOUND: &str = "peer_inbound";

fn env_path(home: &Home) -> std::path::PathBuf {
    home.root().join(".env")
}

/// Persist the peer's bearer key: .env line + current process env (so
/// the loop works without a daemon restart).
pub fn store_peer_key(home: &Home, key: &str) -> Result<()> {
    let path = env_path(home);
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with("HEARTHLOOM_PEER_KEY="))
        .map(String::from)
        .collect();
    lines.push(format!("HEARTHLOOM_PEER_KEY={key}"));
    std::fs::write(&path, lines.join("\n") + "\n").context("writing .env")?;
    let _ = std::process::Command::new("chmod")
        .args(["600", &path.to_string_lossy()])
        .output();
    std::env::set_var("HEARTHLOOM_PEER_KEY", key);
    Ok(())
}

pub fn remove_peer_key(home: &Home) -> Result<()> {
    let path = env_path(home);
    let lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with("HEARTHLOOM_PEER_KEY="))
        .map(String::from)
        .collect();
    std::fs::write(&path, lines.join("\n") + "\n")?;
    std::env::remove_var("HEARTHLOOM_PEER_KEY");
    Ok(())
}

pub(crate) fn peer_key() -> Option<String> {
    std::env::var("HEARTHLOOM_PEER_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// Phase 5: build the LLM key envelope to fold into the operational mirror.
/// `None` when this node has no provider key to share (the common keyless
/// case) or the peer's public key can't be obtained. The api key is SEALED
/// to the peer's public key — opaque on the wire, openable only by the peer.
/// The clear provider + tier-model ids ride alongside (non-secret).
async fn build_llm_envelope(
    state: &AppState,
    client: &reqwest::Client,
    url: &str,
    key: &str,
) -> Option<Value> {
    let provider = state
        .store
        .consolidation_state("llm.provider")
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "anthropic".into());
    // A `custom` endpoint is node-local (usually a loopback server the
    // peer can't reach) — sharing it would switch the peer onto an
    // address that doesn't exist there and silence its thinking passes.
    if provider == "custom" {
        return None;
    }
    // Only share when we actually hold this provider's key.
    let api_key = std::env::var(crate::integrations::provider_key_env(&provider)).ok()?;
    if api_key.trim().is_empty() {
        return None;
    }
    // The peer's public key: fetch it fresh (it mints one on demand), else
    // fall back to the last one we cached. Cache successful fetches.
    let fetched = client
        .get(format!("{url}/v1/peer/pubkey"))
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success());
    let peer_public = match fetched {
        Some(res) => {
            let pk = res
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v.get("public").and_then(|p| p.as_str()).map(String::from));
            if let Some(pk) = &pk {
                let _ = state.store.set_consolidation_state("peer.public", pk);
            }
            pk
        }
        None => None,
    }
    .or_else(|| {
        state
            .store
            .consolidation_state("peer.public")
            .ok()
            .flatten()
    })?;

    let sealed = crate::peerkey::seal_to(&peer_public, api_key.trim().as_bytes()).ok()?;
    let mut env = serde_json::Map::new();
    env.insert("provider".into(), json!(provider));
    env.insert("sealed_key".into(), json!(sealed));
    for tier in ["low", "medium", "high"] {
        if let Some(m) = state
            .store
            .consolidation_state(&format!("llm.model.{tier}"))
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
        {
            env.insert(format!("model_{tier}"), json!(m));
        }
    }
    Some(Value::Object(env))
}

/// The combined replication bar: every category weighted by its actual
/// item share — floor(100 · Σdone / Σtotal). done is clamped into
/// [0, total] per category, and a record with nothing to sync is 100
/// (the existing "fully caught up ✓" idiom). Floor, not round: 100
/// must mean every item moved.
fn total_pct(categories: &[(i64, i64)]) -> i64 {
    let (mut done, mut total) = (0i64, 0i64);
    for &(d, t) in categories {
        let t = t.max(0);
        done += d.clamp(0, t);
        total += t;
    }
    if total == 0 {
        return 100;
    }
    100 * done / total
}

/// Per-category outbound replication progress in honest countable
/// units: episodes by rowid watermark vs the export head (peer-echo
/// rows excluded from BOTH sides), scope relabels by stamp watermark,
/// and the operational mirror (notes+bodies, tasks, templates, goals,
/// self-model, privacy rules, standing queries, relationships) as an
/// all-or-nothing snapshot weighted by its item count. Rides both the
/// status surface and the heartbeat, so the counterpart's inbound card
/// can render the same truth.
fn progress(state: &AppState) -> Result<Value> {
    let push: i64 = state
        .store
        .consolidation_state(KEY_PUSH_CURSOR)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let head = state.store.sync_export_head()?;
    let scope_since = state
        .store
        .consolidation_state(KEY_SCOPE_PUSH)?
        .unwrap_or_default();
    let (scopes_done, scopes_total) = state.store.scope_change_counts(&scope_since)?;
    let op_total = state.store.operational_count(&state.home)?;
    // replace-all snapshot: complete iff a mirror POST ever succeeded
    // (every non-paused successful cycle refreshes the stamp).
    let mirrored = state
        .store
        .consolidation_state(KEY_MIRROR_TS)?
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let op_done = if mirrored { op_total } else { 0 };
    let episodes_done = push.clamp(0, head);
    Ok(json!({
        "categories": {
            "episodes": {"done": episodes_done, "total": head},
            "scopes": {"done": scopes_done, "total": scopes_total},
            "operational": {"done": op_done, "total": op_total},
        },
        "total_pct": total_pct(&[
            (episodes_done, head),
            (scopes_done, scopes_total),
            (op_done, op_total),
        ]),
    }))
}

/// Status for the Integrations card / CLI: pairing, watermarks, heads,
/// and the last cycle's outcome.
pub fn status(state: &AppState) -> Result<Value> {
    let peer = state.store.peer()?;
    let cursor = |key: &str| -> i64 {
        state
            .store
            .consolidation_state(key)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    let last: Option<Value> = state
        .store
        .consolidation_state(KEY_LAST)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str(&v).ok());
    Ok(json!({
        "paused": is_paused(state),
        "version": env!("CARGO_PKG_VERSION"),
        "peer_version": state.store.consolidation_state("peer_version").ok().flatten(),
        "llm_configured": state.llm.is_some(),
        "peer_llm_configured": state
            .store
            .consolidation_state("peer_llm_configured")
            .ok()
            .flatten()
            .map(|v| v == "1"),
        "inbound": state
            .store
            .consolidation_state(KEY_INBOUND)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<Value>(&v).ok()),
        "paired": peer.as_ref().map(|(name, url)| json!({"name": name, "url": url})),
        "key_present": peer_key().is_some(),
        "push_cursor": cursor(KEY_PUSH_CURSOR),
        "pull_cursor": cursor(KEY_PULL_CURSOR),
        "bus": state.bus.status_json(),
        "local_head": state.store.sync_head()?,
        "progress": progress(state)?,
        "last": last,
    }))
}

/// One full cycle: push local-new, pull remote-new. The outcome —
/// success OR failure — is persisted for the status surface.
pub async fn sync_once(state: &AppState) -> Result<Value> {
    sync_once_opts(state, false).await
}

/// full=true re-walks the episode feed from rowid 0 — an idempotent
/// re-offer of everything (used after sync-policy changes, e.g. private
/// episodes becoming replicable).
pub async fn sync_once_opts(state: &AppState, full: bool) -> Result<Value> {
    if full {
        state.store.set_consolidation_state(KEY_PUSH_CURSOR, "0")?;
        state.store.set_consolidation_state(KEY_PULL_CURSOR, "0")?;
    }
    match sync_inner(state).await {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            let _ = record_outcome(
                state,
                json!({"ok": false, "error": format!("{err:#}"),
                       "ts": hearthloom_core::time::now_utc()}),
            );
            Err(err)
        }
    }
}

/// Store a counterpart's heartbeat (its hostname, progress, pause state)
/// so THIS node's Peering page can show who syncs to it and how far
/// along they are.
pub fn record_inbound(state: &AppState, from_key: &str, body: &Value) -> Result<()> {
    let entry = json!({
        "key_name": from_key,
        "hostname": body.get("hostname").and_then(|h| h.as_str()).unwrap_or("unknown"),
        "head": body.get("head").and_then(|h| h.as_i64()).unwrap_or(0),
        "push_cursor": body.get("push_cursor").and_then(|c| c.as_i64()).unwrap_or(0),
        "paused": body.get("paused").and_then(|p| p.as_bool()).unwrap_or(false),
        "llm_configured": body.get("llm_configured").and_then(|p| p.as_bool()),
        // the sender's own per-category replication progress (absent
        // from an older peer's heartbeat — the UI falls back to the
        // episodes-only numbers rather than inventing any).
        "progress": body.get("progress").cloned().unwrap_or(Value::Null),
        "last_contact": hearthloom_core::time::now_utc(),
    });
    state
        .store
        .set_consolidation_state(KEY_INBOUND, &entry.to_string())
}

pub(crate) fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this machine".into())
}

/// Tell the counterpart who we are and how far our push has gotten —
/// metadata only, sent even while replication is paused (the remote's
/// page should say "paused", not "vanished").
async fn send_heartbeat(state: &AppState, url: &str, key: &str, paused: bool) {
    let cursor: i64 = state
        .store
        .consolidation_state(KEY_PUSH_CURSOR)
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // head for the inbound card = what this node will actually export
    // (peer-echo rows excluded), so the counterpart's bar can reach 100.
    let head = state.store.sync_export_head().unwrap_or(0);
    if let Ok(res) = reqwest::Client::new()
        .post(format!("{url}/v1/peer/heartbeat"))
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(10))
        .json(&json!({"hostname": hostname(), "head": head,
                      "push_cursor": cursor, "paused": paused,
                      "llm_configured": state.llm.is_some(),
                      "version": env!("CARGO_PKG_VERSION"),
                      "progress": progress(state).ok()}))
        .send()
        .await
    {
        if let Ok(body) = res.json::<Value>().await {
            if let Some(v) = body.get("version").and_then(|v| v.as_str()) {
                let _ = state.store.set_consolidation_state("peer_version", v);
            }
            if let Some(peer_llm) = body.get("llm_configured").and_then(|b| b.as_bool()) {
                let _ = state.store.set_consolidation_state(
                    "peer_llm_configured",
                    if peer_llm { "1" } else { "0" },
                );
            }
        }
    }
    // The response tells us about THEM — peer key state feeds the
    // consolidator-role rule and the Peering page's dependency alert.
}

/// Should this node skip idle-time LLM passes? Only a LOCAL node with a
/// fresh (<24h) successful sync to a peer that reports a configured LLM.
pub fn llm_delegated_to_peer(state: &AppState) -> bool {
    if crate::monitor::node_kind() != "local" {
        return false;
    }
    let peer_llm = state
        .store
        .consolidation_state("peer_llm_configured")
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false);
    if !peer_llm {
        return false;
    }
    let fresh = (chrono::Utc::now() - chrono::Duration::hours(24))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    state
        .store
        .consolidation_state(KEY_LAST)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .map(|last| {
            last.get("ok").and_then(|o| o.as_bool()).unwrap_or(false)
                && last.get("ts").and_then(|t| t.as_str()).unwrap_or("") > fresh.as_str()
        })
        .unwrap_or(false)
}

pub fn is_paused(state: &AppState) -> bool {
    state
        .store
        .consolidation_state(KEY_PAUSED)
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Set the pause flag locally and inform the peer (best-effort) so both
/// ends show the same state.
pub async fn set_paused(state: &AppState, paused: bool) -> Result<Value> {
    state
        .store
        .set_consolidation_state(KEY_PAUSED, if paused { "1" } else { "0" })?;
    let mut peer_informed = false;
    if let (Ok(Some((_, url))), Some(key)) = (state.store.peer(), peer_key()) {
        peer_informed = reqwest::Client::new()
            .post(format!("{url}/v1/sync/paused"))
            .bearer_auth(&key)
            .json(&json!({"paused": paused}))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
    }
    Ok(json!({"paused": paused, "peer_informed": peer_informed}))
}

async fn sync_inner(state: &AppState) -> Result<Value> {
    if is_paused(state) {
        if let (Ok(Some((_, url))), Some(key)) = (state.store.peer(), peer_key()) {
            send_heartbeat(state, &url, &key, true).await;
        }
        return record_outcome(
            state,
            json!({"ok": true, "paused": true,
                   "ts": hearthloom_core::time::now_utc()}),
        );
    }
    // this node is alive, with these roles (the synced node list)
    crate::agents::publish_self(state);
    let Some((name, url)) = state.store.peer()? else {
        return Err(anyhow!("no peer is paired"));
    };
    let key = peer_key().ok_or_else(|| {
        anyhow!("peer key missing — re-pair (HEARTHLOOM_PEER_KEY not in the environment)")
    })?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let started = std::time::Instant::now();
    // Heartbeat BEFORE the work too: a long first push used to leave
    // the counterpart displaying the PREVIOUS cycle's (stale) progress
    // for hours (the 100%-vs-53% confusion, 2026-07-18).
    send_heartbeat(state, &url, &key, false).await;

    // — push: local episodes the peer hasn't seen —
    let mut pushed = 0usize;
    let mut batches_since_heartbeat = 0usize;
    loop {
        let cursor: i64 = state
            .store
            .consolidation_state(KEY_PUSH_CURSOR)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let page = state.store.sync_export(&state.home, cursor, BATCH)?;
        if page.drained {
            break;
        }
        let count = page.episodes.len();
        let batch = IngestBatch {
            actor: format!("peer:{name}"),
            session_id: None,
            episodes: page.episodes,
        };
        let res = client
            .post(format!("{url}/v1/ingest"))
            .bearer_auth(&key)
            .json(&batch)
            .send()
            .await
            .context("pushing to peer")?;
        if !res.status().is_success() {
            return record_outcome(
                state,
                json!({"ok": false, "error": format!("peer ingest answered {}", res.status())}),
            );
        }
        state
            .store
            .set_consolidation_state(KEY_PUSH_CURSOR, &page.next_cursor.to_string())?;
        pushed += count;
        batches_since_heartbeat += 1;
        if batches_since_heartbeat >= 25 {
            batches_since_heartbeat = 0;
            // live progress for the counterpart's inbound card mid-push
            send_heartbeat(state, &url, &key, false).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(BATCH_SLEEP_MS)).await;
    }

    // — pull: remote episodes we haven't seen —
    let mut pulled_new = 0usize;
    let mut pulled_existing = 0usize;
    loop {
        let cursor: i64 = state
            .store
            .consolidation_state(KEY_PULL_CURSOR)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let res = client
            .get(format!(
                "{url}/v1/sync/episodes?after={cursor}&limit={BATCH}"
            ))
            .bearer_auth(&key)
            .send()
            .await
            .context("pulling from peer")?;
        if !res.status().is_success() {
            return record_outcome(
                state,
                json!({"ok": false, "error": format!("peer export answered {}", res.status())}),
            );
        }
        let page: Value = res.json().await?;
        let drained = page
            .get("drained")
            .and_then(|d| d.as_bool())
            .unwrap_or(true);
        if drained {
            break;
        }
        let episodes: Vec<EpisodeDraft> =
            serde_json::from_value(page.get("episodes").cloned().unwrap_or(json!([])))?;
        let next = page
            .get("next_cursor")
            .and_then(|c| c.as_i64())
            .ok_or_else(|| anyhow!("peer export had no cursor"))?;
        let report = state.store.apply_batch(
            &state.home,
            &IngestBatch {
                actor: format!("peer:{name}"),
                session_id: None,
                episodes,
            },
        )?;
        pulled_new += report.episodes_new;
        pulled_existing += report.episodes_existing;
        state
            .store
            .set_consolidation_state(KEY_PULL_CURSOR, &next.to_string())?;
        tokio::time::sleep(std::time::Duration::from_millis(BATCH_SLEEP_MS)).await;
    }

    // Scope changes replicate with last-writer-wins (a privacy relabel
    // on one node takes effect on both).
    let mut scopes_pushed = 0usize;
    {
        let since = state
            .store
            .consolidation_state(KEY_SCOPE_PUSH)?
            .unwrap_or_default();
        let changes = state.store.scope_changes_since(&since)?;
        if !changes.is_empty() {
            // v57: the cursor is this node's relay stamp, not the change's own
            let latest = state.store.scope_changes_cursor(&since)?;
            let res = client
                .post(format!("{url}/v1/sync/scopes"))
                .bearer_auth(&key)
                .json(&json!({"changes": changes}))
                .send()
                .await
                .context("pushing scope changes")?;
            if res.status().is_success() {
                scopes_pushed = changes.len();
                state
                    .store
                    .set_consolidation_state(KEY_SCOPE_PUSH, &latest)?;
            }
        }
        // pull theirs
        let since_pull = state
            .store
            .consolidation_state(KEY_SCOPE_PULL)?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/scopes?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(body) = res.json::<Value>().await {
                    let changes: Vec<(String, String, String)> =
                        serde_json::from_value(body.get("changes").cloned().unwrap_or(json!([])))
                            .unwrap_or_default();
                    if let Some((_, _, latest)) = changes.last() {
                        // v57 peers send their relay cursor; older ones don't
                        let latest = body
                            .get("next")
                            .and_then(|n| n.as_str())
                            .map(String::from)
                            .unwrap_or_else(|| latest.clone());
                        let _ = state.store.apply_scope_changes(
                            &hearthloom_core::store::Actor::new(format!("peer:{name}")),
                            &changes,
                        );
                        state
                            .store
                            .set_consolidation_state(KEY_SCOPE_PULL, &latest)?;
                    }
                }
            }
        }
    }

    // Full operational mirror (one-way, this node → the always-on
    // peer): notes with bodies, tasks, templates, goals, self-model,
    // privacy rules, relationships — what makes the remote's dashboard
    // and Ask whole. Verbatim replace-all; the authoritative node owns
    // operational state (v1).
    let mut mirrored = 0usize;
    let mut mirror_ok = false;
    match state.store.export_operational(&state.home) {
        Ok(mut operational) => {
            // Phase 5: fold in the LLM key envelope (sealed to the peer's
            // public key) + clear tier config, so a peer keyed by hand shares
            // the credential without it ever crossing in the clear.
            if let Some(envelope) = build_llm_envelope(state, &client, &url, &key).await {
                if let Some(obj) = operational.as_object_mut() {
                    obj.insert("llm".into(), envelope);
                }
            }
            // v46: the profile card follows the record between peers
            // (LWW by its own stamp — edited on either node, wins by
            // recency on both).
            if let (Ok(Some(card)), Ok(Some(stamp))) = (
                state.store.consolidation_state("network.profile"),
                state
                    .store
                    .consolidation_state("network.profile.updated_at"),
            ) {
                if let Some(obj) = operational.as_object_mut() {
                    obj.insert(
                        "profile".into(),
                        json!({"value": card, "updated_at": stamp}),
                    );
                }
            }
            // The monthly spend budget rides along the same way; an empty
            // value is "no budget", and still wins if it is the newer word.
            if let Ok(Some(stamp)) = state.store.consolidation_state(crate::budget::KEY_STAMP) {
                let value = state
                    .store
                    .consolidation_state(crate::budget::KEY)
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                if let Some(obj) = operational.as_object_mut() {
                    obj.insert(
                        "llm_budget".into(),
                        json!({"value": value, "updated_at": stamp}),
                    );
                }
            }
            // The visit rule too — and it comes back on the pull below,
            // since both nodes must cluster visits by the same rule.
            if let (Some(rule), Some(obj)) = (
                crate::visits::rule_envelope(&state.store),
                operational.as_object_mut(),
            ) {
                obj.insert("visit_rule".into(), rule);
            }
            match client
                .post(format!("{url}/v1/sync/operational"))
                .bearer_auth(&key)
                .json(&operational)
                .send()
                .await
            {
                Ok(res) if res.status().is_success() => {
                    mirror_ok = true;
                    state.store.set_consolidation_state(
                        KEY_MIRROR_TS,
                        &hearthloom_core::time::now_utc(),
                    )?;
                    mirrored = res
                        .json::<Value>()
                        .await
                        .ok()
                        .and_then(|v| v.get("applied").and_then(|a| a.as_u64()))
                        .unwrap_or(0) as usize;
                }
                // Best-effort by doctrine, but never SILENT: the progress
                // surface counts this category as incomplete — say why.
                Ok(res) => tracing::warn!(
                    "operational mirror refused by peer: {} {}",
                    res.status(),
                    res.text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect::<String>()
                ),
                Err(err) => tracing::warn!("operational mirror push failed: {err:#}"),
            }
        }
        Err(err) => tracing::warn!("operational export failed: {err:#}"),
    }

    // Pull the peer's operational state back (multi-node M5): with a hub,
    // what another node mirrored there reaches this one. Merged LWW like a
    // push (apply_operational never deletes; the resurrection gate holds).
    // An older peer answers 404/405 — skipped.
    if mirror_ok {
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/operational"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(theirs) = res.json::<Value>().await {
                    if let Some(rule) = theirs.get("visit_rule") {
                        if let Err(err) = crate::visits::adopt_rule(&state.store, rule) {
                            tracing::warn!("visit rule from the peer not applied: {err:#}");
                        }
                    }
                    let (store, home) = (state.store.clone(), state.home.clone());
                    let applied = tokio::task::spawn_blocking(move || {
                        store.apply_operational(&home, &theirs)
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|r| r)
                    .unwrap_or_else(|err| {
                        tracing::warn!("operational mirror pull failed: {err:#}");
                        0
                    });
                    if applied > 0 {
                        crate::bus::touch(state, "operational.changed");
                    }
                }
            }
        }
    }

    // Operational-journal exchange (v43 Phase 3): push our own journal
    // entries after a seq cursor, pull the peer's. Apply is idempotent
    // by entry_id; delete tombstones propagate LWW-gated — the ONE sync
    // leg that removes rows across nodes (purge on A finally removes
    // the row on B instead of leaving a union orphan). Cursors are the
    // sender's local op_journal.seq. 404 = older peer: skip (wire
    // discipline).
    {
        let push_seq: i64 = state
            .store
            .consolidation_state("peer_journal_push_seq")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if let Ok(batch) = state.store.journal_export(push_seq, 500) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_i64())
                .unwrap_or(push_seq);
            let empty = batch
                .get("drained")
                .and_then(|d| d.as_bool())
                .unwrap_or(true);
            if !empty {
                if let Ok(res) = client
                    .post(format!("{url}/v1/sync/journal"))
                    .bearer_auth(&key)
                    .json(&batch)
                    .send()
                    .await
                {
                    if res.status().is_success() && next > push_seq {
                        state
                            .store
                            .set_consolidation_state("peer_journal_push_seq", &next.to_string())?;
                    }
                }
            }
        }
        let pull_seq: i64 = state
            .store
            .consolidation_state("peer_journal_pull_seq")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/journal?after={pull_seq}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_i64())
                        .unwrap_or(pull_seq);
                    let _ = state.store.apply_journal(
                        &hearthloom_core::store::Actor::new(format!("peer:{name}")),
                        &batch,
                    );
                    if next > pull_seq {
                        state
                            .store
                            .set_consolidation_state("peer_journal_pull_seq", &next.to_string())?;
                    }
                }
            }
        }
    }

    // Photo-point exchange (v47): the Mac scan lives on the laptop and
    // the phone posts to the always-on node — the photos#DAY union needs
    // both sides to hold both sets. Idempotent by key; 404 = older peer.
    {
        let push_seq: i64 = state
            .store
            .consolidation_state("peer_photo_push_seq")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if let Ok(batch) = state.store.photo_points_export(push_seq, 500) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_i64())
                .unwrap_or(push_seq);
            let empty = batch
                .get("drained")
                .and_then(|d| d.as_bool())
                .unwrap_or(true);
            if !empty {
                if let Ok(res) = client
                    .post(format!("{url}/v1/sync/photos"))
                    .bearer_auth(&key)
                    .json(&batch)
                    .send()
                    .await
                {
                    if res.status().is_success() && next > push_seq {
                        state
                            .store
                            .set_consolidation_state("peer_photo_push_seq", &next.to_string())?;
                    }
                }
            }
        }
        let pull_seq: i64 = state
            .store
            .consolidation_state("peer_photo_pull_seq")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/photos?after={pull_seq}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_i64())
                        .unwrap_or(pull_seq);
                    let _ = state.store.photo_points_apply(&batch);
                    if next > pull_seq {
                        state
                            .store
                            .set_consolidation_state("peer_photo_pull_seq", &next.to_string())?;
                    }
                }
            }
        }
    }

    // Agents exchange (v55): the governance policy, agent tasks and every
    // approval decision, both ways, LWW by updated_at — so a task proposed
    // or approved on one node runs on the node that hosts its agent.
    // Agent CONFIGURATIONS stay node-local (addresses and secrets belong
    // to one machine). 404 = an older peer; skipped.
    {
        let since = state
            .store
            .consolidation_state("peer_agents_push_ts")?
            .unwrap_or_default();
        if let Ok(batch) = state.store.agents_sync_export(&since) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_str())
                .unwrap_or(&since)
                .to_string();
            let has_rows = ["governance", "tasks", "approvals", "directory", "nodes"]
                .iter()
                .any(|k| {
                    batch
                        .get(*k)
                        .and_then(|v| v.as_array())
                        .is_some_and(|a| !a.is_empty())
                });
            if has_rows {
                if let Ok(res) = client
                    .post(format!("{url}/v1/sync/agents"))
                    .bearer_auth(&key)
                    .json(&batch)
                    .send()
                    .await
                {
                    if res.status().is_success() && next > since {
                        state
                            .store
                            .set_consolidation_state("peer_agents_push_ts", &next)?;
                    }
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_agents_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/agents?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    if state.store.agents_sync_apply(&batch).unwrap_or(0) > 0 {
                        crate::agents::kick();
                    }
                    if !next.is_empty() && next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_agents_pull_ts", &next)?;
                    }
                }
            }
        }
    }

    // Retirements exchange (v59, D.12): the retirement EVENTS (what was
    // retired, which summaries to rewrite, the undo) and what each summary
    // read, both ways. Each node then applies or undoes the event itself —
    // entity and edge ids differ between nodes, so the undo record never
    // travels. 404 = an older peer; skipped.
    {
        let since = state
            .store
            .consolidation_state("peer_retire_push_ts")?
            .unwrap_or_default();
        if let Ok(batch) = state.store.retirements_sync_export(&since) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_str())
                .unwrap_or(&since)
                .to_string();
            let has_rows = ["retirements", "summary_inputs"].iter().any(|k| {
                batch
                    .get(*k)
                    .and_then(|v| v.as_array())
                    .is_some_and(|a| !a.is_empty())
            });
            if has_rows {
                if let Ok(res) = client
                    .post(format!("{url}/v1/sync/retirements"))
                    .bearer_auth(&key)
                    .json(&batch)
                    .send()
                    .await
                {
                    if res.status().is_success() && next > since {
                        state
                            .store
                            .set_consolidation_state("peer_retire_push_ts", &next)?;
                    }
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_retire_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/retirements?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !state
                        .store
                        .retirements_sync_apply(&batch)
                        .unwrap_or_default()
                        .is_empty()
                    {
                        crate::retire::catch_up(state).await;
                    }
                    if !next.is_empty() && next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_retire_pull_ts", &next)?;
                    }
                }
            }
        }
    }

    // Grants/shares exchange (v47): ONE sharing posture across a user's
    // own nodes — a grant edited or revoked on the laptop reaches the
    // tenant (where friends actually connect) and vice versa. LWW by
    // updated_at; friend KEYS never travel. 404 = older peer.
    {
        let since = state
            .store
            .consolidation_state("peer_sharing_push_ts")?
            .unwrap_or_default();
        if let Ok(batch) = state.store.sharing_sync_export(&since) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_str())
                .unwrap_or(&since)
                .to_string();
            let has_rows = batch
                .get("grants")
                .and_then(|g| g.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
                || batch
                    .get("shares")
                    .and_then(|s| s.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
            // v54: group rows ride every page whole (outside the cursor),
            // so push when THEY changed too — judged by a signature of
            // their stamps, remembered once the peer has accepted it.
            let groups_sig = batch
                .get("groups")
                .and_then(|g| g.as_array())
                .map(|rows| {
                    rows.iter()
                        .map(|g| {
                            format!(
                                "{}@{}",
                                g["id"].as_str().unwrap_or(""),
                                g["updated_at"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let groups_changed = groups_sig
                != state
                    .store
                    .consolidation_state("peer_sharing_groups_sig")?
                    .unwrap_or_default();
            if has_rows || groups_changed {
                if let Ok(res) = client
                    .post(format!("{url}/v1/sync/sharing"))
                    .bearer_auth(&key)
                    .json(&batch)
                    .send()
                    .await
                {
                    if res.status().is_success() {
                        if next > since {
                            state
                                .store
                                .set_consolidation_state("peer_sharing_push_ts", &next)?;
                        }
                        state
                            .store
                            .set_consolidation_state("peer_sharing_groups_sig", &groups_sig)?;
                    }
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_sharing_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/sharing?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = state.store.sharing_sync_apply(&batch);
                    if !next.is_empty() && next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_sharing_pull_ts", &next)?;
                    }
                }
            }
        }
    }

    // Same-owner declared state (grants + friend-key hashes) replicates
    // so friend keys minted on either node open BOTH — friends always
    // target the always-on address.
    let declared = state.store.export_declared()?;
    let _ = client
        .post(format!("{url}/v1/sync/declared"))
        .bearer_auth(&key)
        .json(&declared)
        .send()
        .await;
    // Telemetry exchange (append-only, replay-safe): push ours since
    // watermark, pull theirs — either dashboard shows both nodes.
    {
        let since = state
            .store
            .consolidation_state("peer_telemetry_push_ts")?
            .unwrap_or_default();
        if let Ok(batch) = state.store.telemetry_since(&since, 2000) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_str())
                .unwrap_or(&since)
                .to_string();
            if let Ok(res) = client
                .post(format!("{url}/v1/sync/telemetry"))
                .bearer_auth(&key)
                .json(&batch)
                .send()
                .await
            {
                if res.status().is_success() && next > since {
                    state
                        .store
                        .set_consolidation_state("peer_telemetry_push_ts", &next)?;
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_telemetry_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/telemetry?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = state.store.apply_telemetry(&batch);
                    if next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_telemetry_pull_ts", &next)?;
                    }
                }
            }
        }
    }
    // LLM-usage ledger exchange (v50, telemetry pattern): push ours
    // since watermark, pull theirs — the Monitoring "LLM usage" view on
    // either node shows BOTH nodes' spend, which reconciliation against
    // the provider's console actually needs. Append-only + id-keyed
    // INSERT OR IGNORE, so replays are no-ops and feature-detect
    // degrade (404 on an old peer) skips the leg cleanly.
    {
        let since = state
            .store
            .consolidation_state("peer_llm_usage_push_ts")?
            .unwrap_or_default();
        if let Ok(batch) = state.store.llm_usage_since(&since, 2000) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_str())
                .unwrap_or(&since)
                .to_string();
            if let Ok(res) = client
                .post(format!("{url}/v1/sync/llm_usage"))
                .bearer_auth(&key)
                .json(&batch)
                .send()
                .await
            {
                if res.status().is_success() && next > since {
                    state
                        .store
                        .set_consolidation_state("peer_llm_usage_push_ts", &next)?;
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_llm_usage_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/llm_usage?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = state.store.apply_llm_usage(&batch);
                    if next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_llm_usage_pull_ts", &next)?;
                    }
                }
            }
        }
    }
    // v51 device-feed legs (telemetry pattern ×2 + contact LWW): the
    // phone posts observations/cards/states to whichever node it
    // targets; visit clustering, contact apply, and the audit surfaces
    // on the OTHER node need them too. Feature-detect degrade: 404 on
    // an older peer skips the leg, never fails the exchange.
    {
        let legs: [(&str, &str, &str); 5] = [
            ("locations", "peer_locations", "location observations"),
            ("contacts", "peer_contacts", "contact cards"),
            ("device_state", "peer_device_state", "device states"),
            // v60 movement layer: travel spans (telemetry) + per-car
            // settings and Tend answers (LWW) — both nodes reconcile trips
            ("movements", "peer_movements", "movements"),
            ("vehicles", "peer_vehicles", "vehicle settings"),
        ];
        for (path, wm, _label) in legs {
            let since = state
                .store
                .consolidation_state(&format!("{wm}_push_ts"))?
                .unwrap_or_default();
            let batch = match path {
                "locations" => state.store.observations_since(&since, 2000),
                "contacts" => state.store.contacts_since(&since, 1000),
                "movements" => state.store.movements_since(&since, 2000),
                "vehicles" => state.store.vehicles_since(&since, 500),
                _ => state.store.device_states_since(&since, 2000),
            };
            if let Ok(batch) = batch {
                let next = batch
                    .get("next")
                    .and_then(|n| n.as_str())
                    .unwrap_or(&since)
                    .to_string();
                if let Ok(res) = client
                    .post(format!("{url}/v1/sync/{path}"))
                    .bearer_auth(&key)
                    .json(&batch)
                    .send()
                    .await
                {
                    if res.status().is_success() && next > since {
                        state
                            .store
                            .set_consolidation_state(&format!("{wm}_push_ts"), &next)?;
                    }
                }
            }
            let since_pull = state
                .store
                .consolidation_state(&format!("{wm}_pull_ts"))?
                .unwrap_or_default();
            if let Ok(res) = client
                .get(format!("{url}/v1/sync/{path}?since={since_pull}"))
                .bearer_auth(&key)
                .send()
                .await
            {
                if res.status().is_success() {
                    if let Ok(batch) = res.json::<Value>().await {
                        let next = batch
                            .get("next")
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let _ = match path {
                            "locations" => state.store.observations_apply(&batch),
                            "contacts" => state.store.contacts_apply(&batch),
                            "movements" => state.store.movements_apply(&batch),
                            "vehicles" => state.store.vehicles_apply(&batch),
                            _ => state.store.device_states_apply(&batch),
                        };
                        if next > since_pull {
                            state
                                .store
                                .set_consolidation_state(&format!("{wm}_pull_ts"), &next)?;
                        }
                    }
                }
            }
        }
    }
    // Notification exchange (v38, telemetry pattern): push ours since
    // watermark, pull theirs — a provisioner notice on the cloud node
    // rings the laptop's bell (bus hint accelerates, this reconciles).
    // Read/dismiss stamps merge LWW inside apply_notifications.
    {
        let since = state
            .store
            .consolidation_state("peer_notif_push_ts")?
            .unwrap_or_default();
        if let Ok(batch) = state.store.notifications_since(&since, 500) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_str())
                .unwrap_or(&since)
                .to_string();
            if let Ok(res) = client
                .post(format!("{url}/v1/sync/notifications"))
                .bearer_auth(&key)
                .json(&batch)
                .send()
                .await
            {
                // 404 = an older peer without v38: skip, never fail (wire discipline)
                if res.status().is_success() && next > since {
                    state
                        .store
                        .set_consolidation_state("peer_notif_push_ts", &next)?;
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_notif_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/notifications?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = state.store.apply_notifications(&batch);
                    if next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_notif_pull_ts", &next)?;
                    }
                }
            }
        }
    }
    // Extraction-verdict exchange (v39, design 2026-07-17): push ours,
    // pull theirs — the interpretation lands on BOTH nodes, and applying
    // marks episodes extracted so only one node ever spends per episode.
    // 404 = older peer: skip (wire discipline).
    {
        let since = state
            .store
            .consolidation_state("peer_verdict_push_ts")?
            .unwrap_or_default();
        if let Ok(batch) = state.store.verdicts_since(&since, 500) {
            let next = batch
                .get("next")
                .and_then(|n| n.as_str())
                .unwrap_or(&since)
                .to_string();
            if let Ok(res) = client
                .post(format!("{url}/v1/sync/extractions"))
                .bearer_auth(&key)
                .json(&batch)
                .send()
                .await
            {
                if res.status().is_success() && next > since {
                    state
                        .store
                        .set_consolidation_state("peer_verdict_push_ts", &next)?;
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_verdict_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/extractions?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = state.store.apply_verdicts(
                        &hearthloom_core::store::Actor::new(format!("peer:{name}")),
                        &batch,
                    );
                    if next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_verdict_pull_ts", &next)?;
                    }
                }
            }
        }
    }
    // Plugin-data exchange (v42): mirror-sync'd plugins' KV pushes+pulls
    // (LWW by updated_at; the receiver refuses plugins it doesn't also
    // mirror), and their data FILES converge by fingerprint —
    // newer-mtime-wins, fetched one file at a time. 404 = a peer that
    // predates plugins: skip, never fail (wire discipline).
    {
        let since = state
            .store
            .consolidation_state("peer_plugin_kv_push_ts")?
            .unwrap_or_default();
        if let Ok(kv) = state.store.plugin_kv_since(&since, 500) {
            if !kv.is_empty() {
                let next = state
                    .store
                    .plugin_kv_cursor(&since, 500)
                    .unwrap_or_else(|_| since.clone());
                let batch = json!({
                    "kv": kv.iter()
                        .map(|(p, k, v, ts)| json!([p, k, v, ts]))
                        .collect::<Vec<_>>(),
                });
                if let Ok(res) = client
                    .post(format!("{url}/v1/sync/plugins"))
                    .bearer_auth(&key)
                    .json(&batch)
                    .send()
                    .await
                {
                    if res.status().is_success() && next > since {
                        state
                            .store
                            .set_consolidation_state("peer_plugin_kv_push_ts", &next)?;
                    }
                }
            }
        }
        let since_pull = state
            .store
            .consolidation_state("peer_plugin_kv_pull_ts")?
            .unwrap_or_default();
        if let Ok(res) = client
            .get(format!("{url}/v1/sync/plugins?since={since_pull}"))
            .bearer_auth(&key)
            .send()
            .await
        {
            if res.status().is_success() {
                if let Ok(batch) = res.json::<Value>().await {
                    let next = batch
                        .get("next")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let rows: Vec<(String, String, String, String)> = batch
                        .get("kv")
                        .and_then(|v| v.as_array())
                        .map(|list| {
                            list.iter()
                                .filter_map(|row| {
                                    let a = row.as_array()?;
                                    Some((
                                        a.first()?.as_str()?.to_string(),
                                        a.get(1)?.as_str()?.to_string(),
                                        a.get(2)?.as_str()?.to_string(),
                                        a.get(3)?.as_str()?.to_string(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let _ = state.store.apply_plugin_kv(&rows);
                    if next > since_pull {
                        state
                            .store
                            .set_consolidation_state("peer_plugin_kv_pull_ts", &next)?;
                    }
                    // files: fetch any entry newer than what's on disk
                    if let Some(files) = batch.get("files").and_then(|v| v.as_array()) {
                        for f in files.iter().take(200) {
                            let (Some(plugin), Some(rel), Some(mtime)) = (
                                f.get("plugin").and_then(|v| v.as_str()),
                                f.get("path").and_then(|v| v.as_str()),
                                f.get("mtime").and_then(|v| v.as_u64()),
                            ) else {
                                continue;
                            };
                            // only for plugins THIS node mirrors too
                            let mirrored_here = matches!(state.store.plugin(plugin),
                                Ok(Some(row)) if row.sync == "mirror");
                            if !mirrored_here {
                                continue;
                            }
                            let Ok(rel) = crate::plugins::safe_rel_path(rel) else {
                                continue;
                            };
                            let local = crate::plugins::plugin_dir(state, plugin)
                                .join("data")
                                .join(rel);
                            let local_mtime = std::fs::metadata(&local)
                                .and_then(|m| m.modified())
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            if mtime <= local_mtime {
                                continue;
                            }
                            if let Ok(res) = client
                                .get(format!(
                                    "{url}/v1/sync/plugins/file?plugin={plugin}&path={}",
                                    crate::network_out::urlencoding_encode(rel)
                                ))
                                .bearer_auth(&key)
                                .send()
                                .await
                            {
                                if res.status().is_success() {
                                    if let Ok(bytes) = res.bytes().await {
                                        let local = local.clone();
                                        let _ = tokio::task::spawn_blocking(
                                            move || -> anyhow::Result<()> {
                                                if let Some(parent) = local.parent() {
                                                    std::fs::create_dir_all(parent)?;
                                                }
                                                std::fs::write(&local, &bytes)?;
                                                // stamp the peer's mtime so LWW converges
                                                let f = std::fs::File::options()
                                                    .write(true)
                                                    .open(&local)?;
                                                f.set_modified(
                                                    std::time::UNIX_EPOCH
                                                        + std::time::Duration::from_secs(mtime),
                                                )?;
                                                Ok(())
                                            },
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // First full successful cycle = a moment worth announcing.
    if state
        .store
        .consolidation_state("peer_first_sync_notified")?
        .is_none()
    {
        state
            .store
            .set_consolidation_state("peer_first_sync_notified", "1")?;
        crate::provisioner::notify_system(
            state,
            "info",
            &format!("First sync with {name} completed"),
            "your two nodes are converging — new memories now flow both ways",
            Some("first-sync"),
        );
    }
    let _ = state.store.set_consolidation_state("peer_fail_streak", "0");
    send_heartbeat(state, &url, &key, false).await;
    record_outcome(
        state,
        json!({
            "ok": true,
            "ts": hearthloom_core::time::now_utc(),
            "pushed": pushed,
            "scopes_pushed": scopes_pushed,
            "mirrored": mirrored,
            "mirror_ok": mirror_ok,
            "pulled_new": pulled_new,
            "pulled_existing": pulled_existing,
            "took_ms": started.elapsed().as_millis() as u64,
        }),
    )
}

fn record_outcome(state: &AppState, outcome: Value) -> Result<Value> {
    state
        .store
        .set_consolidation_state(KEY_LAST, &outcome.to_string())?;
    Ok(outcome)
}

/// The background loop — spawned at startup, dormant until paired.
pub async fn run_loop(state: AppState) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(SYNC_INTERVAL_SECS)).await;
        let paired = matches!(state.store.peer(), Ok(Some(_)));
        if !paired {
            continue;
        }
        // Same single-flight key as the bus-triggered early sync (15b):
        // a hint-driven cycle already in flight makes this tick a skip.
        let Some(_guard) = state.try_start_automation("peer-sync-early") else {
            continue;
        };
        match sync_once(&state).await {
            Ok(outcome) => tracing::info!("peer sync: {outcome}"),
            // Best-effort by doctrine: an offline peer is a skip (the
            // failure is already recorded for the status surface).
            Err(err) => {
                tracing::warn!("peer sync skipped: {err:#}");
                crate::monitor::log(
                    &state,
                    "warn",
                    "sync_failed",
                    &format!("peer sync skipped: {err:#}"),
                    None,
                );
                // FAILSAFE: a failed cycle (peer restarting, transient
                // 502) retries once after 60s instead of waiting out the
                // full tick — an interrupted big push resumes fast with
                // zero human prodding. The cursor makes it free.
                let retry_state = state.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    let Some(_guard) = retry_state.try_start_automation("peer-sync-early") else {
                        return;
                    };
                    match sync_once(&retry_state).await {
                        Ok(outcome) => tracing::info!("failsafe retry peer sync: {outcome}"),
                        Err(e) => tracing::debug!("failsafe retry still failing: {e:#}"),
                    }
                });
                // Sustained failure is a notification (once per streak,
                // at the 5th consecutive miss ≈ 25 minutes dark).
                let streak: i64 = state
                    .store
                    .consolidation_state("peer_fail_streak")
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
                    + 1;
                let _ = state
                    .store
                    .set_consolidation_state("peer_fail_streak", &streak.to_string());
                if streak == 5 {
                    crate::provisioner::notify_system(
                        &state,
                        "warn",
                        "Peer sync has been failing",
                        "five consecutive cycles couldn't reach your other node — \
                         check it on the Peering page",
                        Some("sync-failing"),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::total_pct;

    #[test]
    fn test_total_pct_weights_by_item_share() {
        // episodes are only part of the story: 4120/5000 memories with
        // scopes and the operational snapshot fully mirrored is NOT the
        // 82% the episodes-only bar showed — it's Σdone/Σtotal.
        let cats = [(4120, 5000), (12, 12), (640, 640)];
        assert_eq!(
            total_pct(&cats),
            100 * (4120 + 12 + 640) / (5000 + 12 + 640)
        ); // 84
        assert_eq!(total_pct(&cats), 84);
        // a tiny finished category barely moves a huge unfinished one
        assert_eq!(total_pct(&[(0, 10_000), (5, 5)]), 0);
    }

    #[test]
    fn test_total_pct_empty_record_is_caught_up() {
        // fresh node, nothing to sync — never divide by zero; the UI
        // idiom for 100 is "fully caught up ✓".
        assert_eq!(total_pct(&[]), 100);
        assert_eq!(total_pct(&[(0, 0), (0, 0), (0, 0)]), 100);
    }

    #[test]
    fn test_total_pct_zero_total_categories_carry_no_weight() {
        // a node with no scope relabels and no operational items:
        // the episodes category alone decides the number.
        assert_eq!(total_pct(&[(50, 100), (0, 0), (0, 0)]), 50);
    }

    #[test]
    fn test_total_pct_partial_categories_floor_not_round() {
        // 100 must MEAN complete: one missing item can't round up.
        assert_eq!(total_pct(&[(999, 1000)]), 99);
        assert_eq!(total_pct(&[(1000, 1000)]), 100);
        // and floor never shows 100 early even at scale
        assert_eq!(total_pct(&[(99_999, 100_000), (10, 10)]), 99);
    }

    #[test]
    fn test_total_pct_clamps_dishonest_inputs() {
        // done can never exceed total (a cursor past a shrunken head),
        // and negatives never underflow the sum.
        assert_eq!(total_pct(&[(500, 100)]), 100);
        assert_eq!(total_pct(&[(-5, 100), (100, 100)]), 50);
        assert_eq!(total_pct(&[(10, -10), (7, 10)]), 70);
    }
}
