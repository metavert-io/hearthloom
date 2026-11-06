//! The Gmail live watch (0.5.81): new mail within seconds, not once per
//! six-hour consolidation cycle. One watch per mailbox since 0.5.98
//! (mailboxes.rs).
//!
//! While a mailbox is connected on this node, its own background loop
//! keeps an IMAP connection in IDLE on All Mail (RFC 2177 — the server
//! pushes over a connection we opened, so it needs no public address and
//! works on a laptop, a home server or a hosted node alike). Any change, or
//! the catch-up interval passing, runs the SAME incremental pull the
//! consolidation pass uses (`gmail_pull::pull_once`): that mailbox's live
//! archive, the Core-strata episodes and its UID cursor behave exactly as
//! before, only sooner. The catch-up is the reconciliation path: a dropped
//! connection or a missed notice costs at most one interval.
//!
//! Settings are per node and per mailbox (the credential is per node):
//! `gmail.watch` on|off (default on) and `gmail.watch.every_minutes`
//! (default 5, 1–60) — suffixed `.<id>` for an added mailbox. A loop pauses
//! while the node is fenced by its provisioner, backs off on errors, and
//! stops retrying a refused sign-in until that mailbox is saved again
//! (Google locks accounts that keep failing). The node list shows which
//! node watches which mailbox (`roles.mailboxes`; `roles.mail` = any), so
//! two nodes pulling one mailbox is visible.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hearthloom_adapters::gmail_imap::{is_auth_failure, ImapFeed};
use serde_json::{json, Value};

use crate::mailboxes::{self, Mailbox};
use crate::AppState;

const STARTUP_DELAY: Duration = Duration::from_secs(20);
/// How often the supervisor looks for mailboxes connected or removed.
const SUPERVISE: Duration = Duration::from_secs(15);
/// How long the gates are re-checked while a watch can't run.
const GATE_RECHECK: Duration = Duration::from_secs(60);
/// IDLE is re-issued at least this often, so a settings change or a
/// disconnect is noticed within a minute (no new login: same session).
const IDLE_SLICE: Duration = Duration::from_secs(60);
/// Mail often arrives in bursts; let one settle before pulling.
const SETTLE: Duration = Duration::from_secs(3);
const BACKOFF_MIN: Duration = Duration::from_secs(60);
const BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

pub const KEY_WATCH: &str = "gmail.watch";
pub const KEY_EVERY: &str = "gmail.watch.every_minutes";
pub const DEFAULT_EVERY_MINUTES: u64 = 5;

/// One mailbox's watch: a flag that wakes it to re-read its gates, and
/// what it's doing.
#[derive(Default)]
struct Slot {
    recheck: Arc<AtomicBool>,
    status: Status,
}

static SLOTS: Mutex<BTreeMap<String, Slot>> = Mutex::new(BTreeMap::new());
/// Wakes the supervisor: a mailbox was connected or removed.
static SUPERVISOR: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Default)]
struct Status {
    state: String,
    since: String,
    last_check: Option<String>,
    last_new_mail: Option<String>,
    last_error: Option<String>,
    fetched: u64,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn slots() -> std::sync::MutexGuard<'static, BTreeMap<String, Slot>> {
    SLOTS.lock().unwrap_or_else(|p| p.into_inner())
}

fn recheck_flag(id: &str) -> Arc<AtomicBool> {
    slots().entry(id.to_string()).or_default().recheck.clone()
}

fn set_state(id: &str, label: &str) {
    let mut slots = slots();
    let status = &mut slots.entry(id.to_string()).or_default().status;
    if status.state != label {
        status.state = label.to_string();
        status.since = now();
    }
}

fn with_status(id: &str, f: impl FnOnce(&mut Status)) {
    f(&mut slots().entry(id.to_string()).or_default().status);
}

/// Wake one mailbox's watch; `credential_changed` (its password was saved
/// again, or it was removed) clears its refusal and its search session.
pub fn recheck_mailbox(id: &str, credential_changed: bool) {
    if credential_changed {
        mailboxes::clear_refused(id);
        // the search session signed in with the old password
        crate::gmail_search::reset(Some(id));
    }
    recheck_flag(id).store(true, Ordering::SeqCst);
    SUPERVISOR.store(true, Ordering::SeqCst);
}

pub fn enabled_for(state: &AppState, mb: &Mailbox) -> bool {
    state
        .store
        .consolidation_state(&mb.key(KEY_WATCH))
        .ok()
        .flatten()
        .is_none_or(|v| v != "off")
}

pub fn every_minutes_for(state: &AppState, mb: &Mailbox) -> u64 {
    parse_every(
        state
            .store
            .consolidation_state(&mb.key(KEY_EVERY))
            .ok()
            .flatten()
            .as_deref(),
    )
}

fn parse_every(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_EVERY_MINUTES, |m| m.clamp(1, 60))
}

/// Why this mailbox's watch can't run right now, if it can't.
fn blocked(state: &AppState, mb: &Mailbox) -> Option<&'static str> {
    if state.killswitch.load(Ordering::Relaxed) {
        Some("paused: this node is fenced by its provider")
    } else if !enabled_for(state, mb) {
        Some("off")
    } else if mailboxes::refused(&mb.id) {
        Some("stopped: Gmail refused the sign-in")
    } else {
        None
    }
}

/// Is this node watching this mailbox right now?
pub fn watching_mailbox(state: &AppState, mb: &Mailbox) -> bool {
    blocked(state, mb).is_none()
}

/// Is this node watching any mailbox right now? (`roles.mail`)
pub fn watching(state: &AppState) -> bool {
    mailboxes::all()
        .iter()
        .any(|mb| watching_mailbox(state, mb))
}

/// One mailbox's watch, as the Integrations card, the CLI and
/// `/v1/integrations/gmail` show it.
pub fn status_json_for(state: &AppState, mb: &Mailbox) -> Value {
    let status = slots()
        .get(&mb.id)
        .map(|s| s.status.clone())
        .unwrap_or_default();
    // the loop's last label can be a gate it has just left behind
    let gated = ["not connected", "off"].contains(&status.state.as_str())
        || status.state.starts_with("paused")
        || status.state.starts_with("stopped");
    json!({
        "enabled": enabled_for(state, mb),
        "every_minutes": every_minutes_for(state, mb),
        "state": blocked(state, mb).map(String::from).unwrap_or_else(|| {
            if status.state.is_empty() || gated { "starting".into() } else { status.state.clone() }
        }),
        "since": status.since,
        "last_check": status.last_check,
        "last_new_mail": status.last_new_mail,
        "last_error": status.last_error,
        "fetched_since_start": status.fetched,
    })
}

/// The watch of the mailbox a caller means without naming one (the only
/// one, or the first) — what `/v1/integrations/gmail` reported before
/// 0.5.98.
pub fn status_json(state: &AppState) -> Value {
    match mailboxes::default_one() {
        Some(mb) => status_json_for(state, &mb),
        None => json!({
            "enabled": false,
            "every_minutes": DEFAULT_EVERY_MINUTES,
            "state": "not connected",
            "since": "",
            "last_check": null,
            "last_new_mail": null,
            "last_error": null,
            "fetched_since_start": 0,
        }),
    }
}

/// Starts a watch for every mailbox connected here, and a new one when a
/// mailbox is connected; each ends when its mailbox is removed.
pub async fn run_loop(state: AppState) {
    tokio::time::sleep(STARTUP_DELAY).await;
    let mut running: BTreeMap<String, tokio::task::JoinHandle<()>> = BTreeMap::new();
    loop {
        SUPERVISOR.store(false, Ordering::SeqCst);
        for mb in mailboxes::all() {
            if running.get(&mb.id).is_none_or(|h| h.is_finished()) {
                running.insert(
                    mb.id.clone(),
                    tokio::spawn(watch_one(state.clone(), mb.id.clone())),
                );
            }
        }
        running.retain(|_, h| !h.is_finished());
        let until = Instant::now() + SUPERVISE;
        while Instant::now() < until && !SUPERVISOR.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn logout(session: &mut Option<ImapFeed>) {
    if let Some(feed) = session.take() {
        let _ = tokio::task::spawn_blocking(move || feed.logout()).await;
    }
}

/// One mailbox's watch, until the mailbox is removed.
async fn watch_one(state: AppState, id: String) {
    let recheck = recheck_flag(&id);
    let mut session: Option<ImapFeed> = None;
    let mut backoff = BACKOFF_MIN;
    loop {
        recheck.store(false, Ordering::SeqCst);
        // read again each round: a new password, or removed
        let Some(mb) = mailboxes::find(&id) else {
            logout(&mut session).await;
            slots().remove(&id);
            return;
        };
        if let Some(why) = blocked(&state, &mb) {
            if mailboxes::refused(&mb.id) {
                mailboxes::tell_refused(&state, &mb);
            }
            set_state(&id, why);
            logout(&mut session).await;
            // a connect or a settings change wakes this at once
            let until = Instant::now() + GATE_RECHECK;
            while Instant::now() < until && !recheck.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            continue;
        }

        // every mailbox, searchable live from any node (0.5.89)
        crate::gmail_search::ensure_registered(&state);
        // Catch up: the one pull path, serialized with the 6-hour pass.
        set_state(&id, "catching up");
        let (st, m) = (state.clone(), mb.clone());
        let pulled =
            tokio::task::spawn_blocking(move || crate::gmail_pull::pull_once(&st, &m)).await;
        let more = match pulled {
            Ok(Ok((fetched, episodes_new, more))) => {
                backoff = BACKOFF_MIN;
                with_status(&id, |s| {
                    s.last_check = Some(now());
                    s.last_error = None;
                    s.fetched += fetched as u64;
                    if fetched > 0 {
                        s.last_new_mail = Some(now());
                    }
                });
                if fetched > 0 {
                    tracing::info!(
                        "gmail watch: {} — {fetched} new message(s), {episodes_new} new episode(s)",
                        mb.address
                    );
                }
                more
            }
            Ok(Err(err)) => {
                failed(&state, &mb, &err, &mut backoff, &mut session).await;
                continue;
            }
            Err(join) => {
                failed(
                    &state,
                    &mb,
                    &anyhow::anyhow!("{join}"),
                    &mut backoff,
                    &mut session,
                )
                .await;
                continue;
            }
        };
        if more {
            continue; // a large backlog: keep pulling before waiting
        }

        // Wait for Gmail to say something changed, or the catch-up interval.
        set_state(&id, "watching");
        let interval = Duration::from_secs(every_minutes_for(&state, &mb) * 60);
        let feed = session.take();
        let (flag, m) = (recheck.clone(), mb.clone());
        let waited = tokio::task::spawn_blocking(move || -> anyhow::Result<(ImapFeed, bool)> {
            let mut feed = match feed {
                Some(feed) => feed,
                None => ImapFeed::connect(&m.address, m.password())?,
            };
            let deadline = Instant::now() + interval;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || flag.load(Ordering::SeqCst) {
                    return Ok((feed, false));
                }
                if feed.wait_for_change(left.min(IDLE_SLICE))? {
                    return Ok((feed, true));
                }
            }
        })
        .await;
        match waited {
            Ok(Ok((feed, changed))) => {
                session = Some(feed);
                if changed {
                    tokio::time::sleep(SETTLE).await;
                }
            }
            Ok(Err(err)) => failed(&state, &mb, &err, &mut backoff, &mut session).await,
            Err(join) => {
                failed(
                    &state,
                    &mb,
                    &anyhow::anyhow!("{join}"),
                    &mut backoff,
                    &mut session,
                )
                .await
            }
        }
    }
}

/// An error: drop the session, say what happened, and wait before trying
/// again — doubling up to half an hour. A refused sign-in stops this
/// mailbox (everywhere: mailboxes.rs) until it's saved again, and tells
/// the owner once.
async fn failed(
    state: &AppState,
    mb: &Mailbox,
    err: &anyhow::Error,
    backoff: &mut Duration,
    session: &mut Option<ImapFeed>,
) {
    *session = None;
    let message = format!("{err:#}");
    with_status(&mb.id, |s| s.last_error = Some(message.clone()));
    if is_auth_failure(err) || mailboxes::refused(&mb.id) {
        mailboxes::refuse(Some(state), mb, err);
        set_state(&mb.id, "stopped: Gmail refused the sign-in");
        return;
    }
    set_state(&mb.id, "retrying after an error");
    tracing::warn!(
        "gmail watch: {}: {message} — retrying in {}s",
        mb.address,
        backoff.as_secs()
    );
    tokio::time::sleep(*backoff).await;
    *backoff = (*backoff * 2).min(BACKOFF_MAX);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_catch_up_interval_is_bounded() {
        assert_eq!(parse_every(None), DEFAULT_EVERY_MINUTES);
        assert_eq!(parse_every(Some("2")), 2);
        assert_eq!(parse_every(Some("0")), 1);
        assert_eq!(parse_every(Some("600")), 60);
        assert_eq!(parse_every(Some("soon")), DEFAULT_EVERY_MINUTES);
    }

    #[test]
    fn test_a_refused_sign_in_is_told_apart_from_a_network_error() {
        assert!(is_auth_failure(&anyhow::anyhow!(
            "gmail login failed: No Response: [AUTHENTICATIONFAILED] Invalid credentials (Failure)"
        )));
        assert!(!is_auth_failure(&anyhow::anyhow!(
            "connection reset by peer"
        )));
    }
}
