//! The push bus (15b): one node-to-node event channel, two trust regimes.
//! Design locked in the design log 2026-07-14 — implement against that
//! entry, don't re-design.
//!
//! Notify-then-fetch: an event is a HINT (envelope of ids/kinds, never
//! content). The receiver reacts by PULLING through the existing
//! authorized read paths, so WILL-ENFORCE stays exactly where it lives
//! today (`Store::scope_guard` at every read). Transport is SSE + POST
//! over the existing HTTP surface, reachability-asymmetric: the NAT-ed
//! laptop holds ONE outbound subscription to its always-on peer
//! (`GET /v1/bus/stream`) and POSTs its own events outbound
//! (`POST /v1/bus/publish`) — neither side needs to dial in.
//!
//! The bus is an accelerator, never the source of truth: at-most-once
//! delivery, the 5-min poll + operational mirror stay the reconciling
//! backstop. A small replay ring serves Last-Event-ID catch-up; a gap
//! beyond the ring emits `resync` and the subscriber runs a full cycle.
//! Peering feature-detects (404/405 on /v1/bus/* → poll-only, the
//! Phase-5 key-sync manner).

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::AppState;

/// Coalescing window for Desk-write bursts: at most one event per topic
/// per window (leading-edge — the first write publishes soon, the burst
/// rides along in the same hint).
const DEBOUNCE_MS: u64 = 2500;
/// Quiet gap before an event-triggered sync cycle actually runs.
const EARLY_SYNC_DELAY_MS: u64 = 1500;
/// Replay ring bounds (~256 events / 24h per the locked design).
const RING_CAP: usize = 256;
const RING_TTL_HOURS: i64 = 24;
/// After a peer answers 404/405 on /v1/bus/*, stay poll-only this long
/// before probing again (an old peer during a dual-deploy window).
const UNSUPPORTED_BACKOFF_SECS: u64 = 900;
/// Reconnect pause after a dropped/failed subscription.
const RECONNECT_SECS: u64 = 20;
/// Per-chunk read patience: the server heartbeats every 30s, so a
/// silent minute-and-a-half means the connection is dead.
const READ_TIMEOUT_SECS: u64 = 90;

const KEY_BUS_CURSOR: &str = "peer_bus_cursor";

/// The wire envelope — hints, never data. Additive-only: never remove,
/// rename, or repurpose a field; unknown fields are ignored (wire-compat
/// discipline, CLAUDE.md "Peering vs federation").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusEvent {
    /// Sortable per-hop stream position (millis-hex + sequence). Each
    /// node re-ids on receipt: Last-Event-ID is a position in THIS
    /// node's ring, not a global identity.
    pub id: String,
    pub ts: String,
    pub origin: String,
    pub topic: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default)]
    pub refs: Vec<String>,
}

/// What the replay ring can offer a resuming subscriber.
pub enum Replay {
    /// Events after the cursor (possibly empty — fully caught up).
    Events(Vec<BusEvent>),
    /// The cursor predates the ring (or the ring was lost to a restart):
    /// the subscriber must run a full reconcile cycle.
    Resync,
}

pub struct Bus {
    ring: Mutex<VecDeque<BusEvent>>,
    seq: AtomicU64,
    tx: broadcast::Sender<BusEvent>,
    /// Topics inside their debounce window.
    pending: Mutex<HashSet<String>>,
    /// An early sync is already scheduled (dedupe, not a queue).
    sync_requested: AtomicBool,
    /// Outbound feature-detect: the peer answered 404/405 recently.
    unsupported_until: Mutex<Option<Instant>>,
    /// Status surface (the Peering card / sync status JSON).
    subscriber_connected: AtomicBool,
    last_event_ts: Mutex<Option<String>>,
    last_publish_ts: Mutex<Option<String>>,
}

impl Default for Bus {
    fn default() -> Self {
        Bus {
            ring: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(0),
            tx: broadcast::channel(64).0,
            pending: Mutex::new(HashSet::new()),
            sync_requested: AtomicBool::new(false),
            unsupported_until: Mutex::new(None),
            subscriber_connected: AtomicBool::new(false),
            last_event_ts: Mutex::new(None),
            last_publish_ts: Mutex::new(None),
        }
    }
}

impl Bus {
    /// Lexicographically monotonic stream id: fixed-width millis hex +
    /// a process-lifetime sequence (ULID-shaped ordering without the
    /// dependency; the id is opaque on the wire).
    fn next_id(&self) -> String {
        let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        format!("{millis:012x}-{seq:08x}")
    }

    /// Append to the ring (pruning cap + TTL) and fan out to live
    /// subscribers. Returns the event as enveloped.
    fn insert(&self, origin: &str, topic: &str, scope: Option<String>, refs: Vec<String>) -> BusEvent {
        let event = BusEvent {
            id: self.next_id(),
            ts: hearthloom_core::time::now_utc(),
            origin: origin.to_string(),
            topic: topic.to_string(),
            scope,
            refs,
        };
        {
            let mut ring = self.ring.lock().unwrap();
            let cutoff = (chrono::Utc::now() - chrono::Duration::hours(RING_TTL_HOURS))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            while ring.len() >= RING_CAP
                || ring.front().map(|e| e.ts.as_str() < cutoff.as_str()).unwrap_or(false)
            {
                if ring.pop_front().is_none() {
                    break;
                }
            }
            ring.push_back(event.clone());
        }
        // No subscribers is fine — the ring still serves late resumes.
        let _ = self.tx.send(event.clone());
        *self.last_publish_ts.lock().unwrap() = Some(event.ts.clone());
        event
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }

    /// Last-Event-ID catch-up. `None` cursor = a fresh subscriber:
    /// start from now (its regular poll already reconciles the past).
    pub fn replay_since(&self, cursor: Option<&str>) -> Replay {
        let Some(cursor) = cursor.filter(|c| !c.is_empty()) else {
            return Replay::Events(Vec::new());
        };
        let ring = self.ring.lock().unwrap();
        match ring.front() {
            // Ring lost (restart) or never fed while the subscriber holds
            // a cursor — can't prove there's no gap, so say resync.
            None => Replay::Resync,
            Some(first) if cursor < first.id.as_str() && cursor != first.id => {
                // A cursor exactly one before the ring head is fine ONLY
                // if it equals some evicted id — indistinguishable from a
                // real gap, so the honest answer stays resync.
                Replay::Resync
            }
            Some(_) => Replay::Events(
                ring.iter().filter(|e| e.id.as_str() > cursor).cloned().collect(),
            ),
        }
    }

    /// Newest ring id, for stamping `resync` events so a subscriber's
    /// cursor jumps past the gap it just reconciled.
    pub fn head_id(&self) -> Option<String> {
        self.ring.lock().unwrap().back().map(|e| e.id.clone())
    }

    fn outbound_supported(&self) -> bool {
        !matches!(*self.unsupported_until.lock().unwrap(), Some(until) if until > Instant::now())
    }

    fn mark_unsupported(&self) {
        *self.unsupported_until.lock().unwrap() =
            Some(Instant::now() + std::time::Duration::from_secs(UNSUPPORTED_BACKOFF_SECS));
    }

    pub fn status_json(&self) -> Value {
        json!({
            "subscriber_connected": self.subscriber_connected.load(Ordering::Relaxed),
            "last_event": self.last_event_ts.lock().unwrap().clone(),
            "last_publish": self.last_publish_ts.lock().unwrap().clone(),
            "peer_supported": self.outbound_supported(),
        })
    }
}

/// MAY — the per-subscriber willingness filter, consulted live at
/// fan-out (a grant change takes effect immediately, no version bump).
/// Peer scope = the same owner: every topic. Friend keys receive only
/// events their grants cover — and every CURRENT topic is same-owner
/// peering traffic, so friends receive nothing until federation topics
/// (`task.shared`, …) arrive with federation itself.
pub fn offered(claims: &hearthloom_core::store::ApiKeyClaims, _event: &BusEvent) -> bool {
    claims.scopes.iter().any(|s| s == "peer" || s == "admin")
}

/// Debounced publish for local writes: the first write in a window
/// schedules one event; the burst rides along. Fire-and-forget — the
/// bus is an accelerator, a lost hint costs latency, not correctness.
pub fn touch(state: &AppState, topic: &str) {
    let newly = state.bus.pending.lock().unwrap().insert(topic.to_string());
    if !newly {
        return;
    }
    let state = state.clone();
    let topic = topic.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS)).await;
        state.bus.pending.lock().unwrap().remove(&topic);
        publish_local(&state, &topic, None, Vec::new()).await;
    });
}

/// Envelope a locally-originated event: ring + live fan-out, an
/// outbound POST to the paired peer (feature-detected), and — on the
/// dialing node — an early sync cycle so the change actually moves now
/// instead of at the tick.
pub async fn publish_local(state: &AppState, topic: &str, scope: Option<String>, refs: Vec<String>) {
    let event = state.bus.insert(&crate::sync::hostname(), topic, scope, refs);
    if let (Ok(Some((_, url))), Some(key)) = (state.store.peer(), crate::sync::peer_key()) {
        if state.bus.outbound_supported() {
            let bus = state.bus.clone();
            let event = event.clone();
            tokio::spawn(async move {
                let res = reqwest::Client::new()
                    .post(format!("{url}/v1/bus/publish"))
                    .bearer_auth(&key)
                    .json(&event)
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await;
                match res {
                    Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND
                        || r.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED =>
                    {
                        // Old peer: degrade to poll-only, probe again later.
                        bus.mark_unsupported();
                    }
                    Ok(_) | Err(_) => {}
                }
            });
        }
        request_early_sync(state);
    }
}

/// A counterpart pushed an event at us (`POST /v1/bus/publish`):
/// re-envelope into OUR ring (ids are per-hop stream positions), fan
/// out, and react — notify-then-fetch, so the reaction is a pull
/// through the existing sync cycle (a no-op on a node that doesn't
/// dial, i.e. the always-on peer).
pub fn receive(state: &AppState, event: &Value) -> Value {
    let topic = event.get("topic").and_then(|t| t.as_str()).unwrap_or("unknown");
    let origin = event.get("origin").and_then(|o| o.as_str()).unwrap_or("peer");
    let scope = event.get("scope").and_then(|s| s.as_str()).map(String::from);
    let refs: Vec<String> =
        serde_json::from_value(event.get("refs").cloned().unwrap_or(json!([]))).unwrap_or_default();
    let stored = state.bus.insert(origin, topic, scope, refs);
    request_early_sync(state);
    json!({"ok": true, "id": stored.id})
}

/// Schedule one near-term sync cycle (deduped): the notify-then-fetch
/// "fetch". Only meaningful on the node that dials (peer() is set);
/// single-flighted so an event burst and the 5-min tick never stack.
pub fn request_early_sync(state: &AppState) {
    if !matches!(state.store.peer(), Ok(Some(_))) {
        return;
    }
    if state.bus.sync_requested.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(EARLY_SYNC_DELAY_MS)).await;
        state.bus.sync_requested.store(false, Ordering::SeqCst);
        let Some(_guard) = state.try_start_automation("peer-sync-early") else {
            return; // a cycle is already moving; the poll reconciles the rest
        };
        match crate::sync::sync_once(&state).await {
            Ok(outcome) => tracing::info!("bus-triggered peer sync: {outcome}"),
            Err(err) => tracing::debug!("bus-triggered peer sync skipped: {err:#}"),
        }
    });
}

/// One SSE message as dispatched by the incremental parser.
#[derive(Debug, PartialEq)]
pub struct SseMessage {
    pub id: Option<String>,
    pub event: String,
    pub data: String,
}

/// Incremental server-sent-events parser for the outbound subscription
/// (reqwest gives us raw chunks; message boundaries land anywhere).
#[derive(Default)]
pub struct SseParser {
    buf: String,
    id: Option<String>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn feed(&mut self, chunk: &str) -> Vec<SseMessage> {
        self.buf.push_str(chunk);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line: String = self.buf.drain(..=pos).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                // dispatch — but a blank line with no fields is a heartbeat
                if self.id.is_some() || self.event.is_some() || !self.data.is_empty() {
                    out.push(SseMessage {
                        id: self.id.take(),
                        event: self.event.take().unwrap_or_else(|| "message".into()),
                        data: self.data.drain(..).collect::<Vec<_>>().join("\n"),
                    });
                }
                continue;
            }
            if line.starts_with(':') {
                continue; // comment / keep-alive
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "id" => self.id = Some(value.to_string()),
                "event" => self.event = Some(value.to_string()),
                "data" => self.data.push(value.to_string()),
                _ => {} // unknown fields are ignored (additive-only)
            }
        }
        out
    }
}

/// The outbound subscriber loop — spawned at startup, dormant until a
/// peer is paired (the dialing node holds ONE long-lived subscription).
pub async fn run_subscriber(state: AppState) {
    // Let the daemon settle before the first dial.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    loop {
        let (url, key) = match (state.store.peer(), crate::sync::peer_key()) {
            (Ok(Some((_, url))), Some(key)) => (url, key),
            _ => {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                continue;
            }
        };
        if !state.bus.outbound_supported() {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            continue;
        }
        match subscribe_once(&state, &url, &key).await {
            SubscribeOutcome::Unsupported => {
                state.bus.mark_unsupported();
                tracing::info!("peer has no push bus (404) — staying poll-only for now");
            }
            SubscribeOutcome::Dropped => {}
        }
        state.bus.subscriber_connected.store(false, Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_secs(RECONNECT_SECS)).await;
    }
}

enum SubscribeOutcome {
    Unsupported,
    Dropped,
}

/// One subscription attempt: connect, then react to events until the
/// stream dies. Every reaction is the same pull — request_early_sync —
/// so at-most-once delivery is enough (the poll reconciles the rest).
async fn subscribe_once(state: &AppState, url: &str, key: &str) -> SubscribeOutcome {
    let cursor = state
        .store
        .consolidation_state(KEY_BUS_CURSOR)
        .ok()
        .flatten()
        .unwrap_or_default();
    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return SubscribeOutcome::Dropped,
    };
    let mut req = client.get(format!("{url}/v1/bus/stream")).bearer_auth(key);
    if !cursor.is_empty() {
        req = req.header("Last-Event-ID", &cursor);
    }
    let mut res = match req.send().await {
        Ok(r) => r,
        Err(err) => {
            tracing::debug!("bus subscribe failed: {err:#}");
            return SubscribeOutcome::Dropped;
        }
    };
    if res.status() == reqwest::StatusCode::NOT_FOUND
        || res.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
    {
        return SubscribeOutcome::Unsupported;
    }
    if !res.status().is_success() {
        tracing::warn!("bus subscribe refused: {}", res.status());
        return SubscribeOutcome::Dropped;
    }
    state.bus.subscriber_connected.store(true, Ordering::Relaxed);
    tracing::info!("push bus subscribed to peer (cursor {:?})", cursor);
    let mut parser = SseParser::default();
    loop {
        let chunk = tokio::time::timeout(
            std::time::Duration::from_secs(READ_TIMEOUT_SECS),
            res.chunk(),
        )
        .await;
        let bytes = match chunk {
            Ok(Ok(Some(bytes))) => bytes,
            // clean end, error, or a silent read timeout: reconnect
            _ => return SubscribeOutcome::Dropped,
        };
        for msg in parser.feed(&String::from_utf8_lossy(&bytes)) {
            if let Some(id) = &msg.id {
                let _ = state.store.set_consolidation_state(KEY_BUS_CURSOR, id);
            }
            *state.bus.last_event_ts.lock().unwrap() =
                Some(hearthloom_core::time::now_utc());
            // resync and topic hints trigger the SAME pull: one full
            // cycle reconciles everything the hint (or the gap) implies.
            request_early_sync(state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_events(bus: &Bus, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| bus.insert("test", &format!("topic.{i}"), None, vec![]).id)
            .collect()
    }

    #[test]
    fn test_ids_are_lexicographically_monotonic() {
        let bus = Bus::default();
        let ids = feed_events(&bus, 50);
        for pair in ids.windows(2) {
            assert!(pair[0] < pair[1], "{} !< {}", pair[0], pair[1]);
        }
    }

    #[test]
    fn test_replay_fresh_subscriber_starts_from_now() {
        let bus = Bus::default();
        feed_events(&bus, 3);
        // no cursor = no replay AND no resync (the poll owns the past)
        match bus.replay_since(None) {
            Replay::Events(events) => assert!(events.is_empty()),
            Replay::Resync => panic!("fresh subscriber must not resync"),
        }
        match bus.replay_since(Some("")) {
            Replay::Events(events) => assert!(events.is_empty()),
            Replay::Resync => panic!("empty cursor is a fresh subscriber"),
        }
    }

    #[test]
    fn test_replay_resumes_after_cursor() {
        let bus = Bus::default();
        let ids = feed_events(&bus, 5);
        match bus.replay_since(Some(&ids[1])) {
            Replay::Events(events) => {
                assert_eq!(events.len(), 3);
                assert_eq!(events[0].id, ids[2]);
            }
            Replay::Resync => panic!("cursor inside the ring must replay"),
        }
        // fully caught up: empty replay, still not a resync
        match bus.replay_since(Some(&ids[4])) {
            Replay::Events(events) => assert!(events.is_empty()),
            Replay::Resync => panic!("head cursor is caught up"),
        }
    }

    #[test]
    fn test_replay_gap_beyond_ring_is_resync() {
        let bus = Bus::default();
        let ids = feed_events(&bus, RING_CAP + 10);
        // the first ids were evicted by the cap
        assert!(matches!(bus.replay_since(Some(&ids[0])), Replay::Resync));
        // an empty ring with a cursor (post-restart) is also a resync
        let fresh = Bus::default();
        assert!(matches!(fresh.replay_since(Some("anything")), Replay::Resync));
    }

    #[test]
    fn test_ring_capped() {
        let bus = Bus::default();
        feed_events(&bus, RING_CAP * 2);
        assert!(bus.ring.lock().unwrap().len() <= RING_CAP);
    }

    #[test]
    fn test_envelope_ignores_unknown_fields() {
        // wire-compat: additive-only, unknown fields never break a peer
        let event: BusEvent = serde_json::from_value(json!({
            "id": "x", "ts": "2026-07-15T00:00:00Z", "origin": "n1",
            "topic": "operational.changed",
            "some_future_field": {"nested": true}
        }))
        .unwrap();
        assert_eq!(event.topic, "operational.changed");
        assert!(event.scope.is_none());
        assert!(event.refs.is_empty());
    }

    #[test]
    fn test_sse_parser_across_chunk_boundaries() {
        let mut parser = SseParser::default();
        // one event split mid-line across three chunks + a keep-alive
        assert!(parser.feed("id: 0001\nev").is_empty());
        assert!(parser.feed("ent: bus\ndata: {\"topic\":\"operational.").is_empty());
        let msgs = parser.feed("changed\"}\n\n: hb\n\n");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id.as_deref(), Some("0001"));
        assert_eq!(msgs[0].event, "bus");
        assert_eq!(msgs[0].data, "{\"topic\":\"operational.changed\"}");
        // heartbeat-only blank lines dispatch nothing
        assert!(parser.feed("\n\n").is_empty());
    }

    #[test]
    fn test_sse_parser_resync_event() {
        let mut parser = SseParser::default();
        let msgs = parser.feed("event: resync\nid: 00ff-0001\ndata: {}\n\n");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].event, "resync");
        assert_eq!(msgs[0].id.as_deref(), Some("00ff-0001"));
    }

    #[test]
    fn test_willingness_peer_gets_everything_friend_nothing_yet() {
        let event = BusEvent {
            id: "1".into(),
            ts: "2026-07-15T00:00:00Z".into(),
            origin: "n1".into(),
            topic: "operational.changed".into(),
            scope: None,
            refs: vec![],
        };
        let claims = |scopes: &[&str]| hearthloom_core::store::ApiKeyClaims {
            id: "k".into(),
            name: "k".into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            grants: None,
        };
        assert!(offered(&claims(&["peer"]), &event));
        assert!(offered(&claims(&["admin"]), &event));
        // every current topic is same-owner peering traffic: a friend
        // key's grants cover none of it until federation topics land
        assert!(!offered(&claims(&["friend"]), &event));
        assert!(!offered(&claims(&["read"]), &event));
    }
}
