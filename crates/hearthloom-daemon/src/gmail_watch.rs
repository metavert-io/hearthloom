//! The Gmail live watch (0.5.81): new mail within seconds, not once per
//! six-hour consolidation cycle.
//!
//! While Gmail is connected on this node, one background loop keeps an
//! IMAP connection in IDLE on All Mail (RFC 2177 — the server pushes over
//! a connection we opened, so it needs no public address and works on a
//! laptop, a home server or a hosted node alike). Any change, or the
//! catch-up interval passing, runs the SAME incremental pull the
//! consolidation pass uses (`gmail_pull::pull_once`): the live archive,
//! the Core-strata episodes and the UID cursor behave exactly as before,
//! only sooner. The catch-up is the reconciliation path: a dropped
//! connection or a missed notice costs at most one interval.
//!
//! Settings are per node (the credential is per node): `gmail.watch`
//! on|off (default on) and `gmail.watch.every_minutes` (default 5,
//! 1–60). The loop pauses while the node is fenced by its provisioner,
//! backs off on errors, and stops retrying a refused sign-in until the
//! connection is saved again (Google locks accounts that keep failing).
//! The node list shows which node watches (`roles.mail`), so two nodes
//! pulling one mailbox is visible.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hearthloom_adapters::gmail_imap::{is_auth_failure, ImapFeed};
use serde_json::{json, Value};

use crate::AppState;

const STARTUP_DELAY: Duration = Duration::from_secs(20);
/// How long the gates are re-checked while the watch can't run.
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

/// Set by a settings change or a (dis)connect: the IDLE wait returns and
/// the loop re-reads its gates at once.
static RECHECK: AtomicBool = AtomicBool::new(false);
/// A refused sign-in stops the loop until the credential is saved again.
static REFUSED: AtomicBool = AtomicBool::new(false);
static STATUS: Mutex<Option<Status>> = Mutex::new(None);

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

fn set_state(label: &str) {
    let mut guard = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    let status = guard.get_or_insert_with(Status::default);
    if status.state != label {
        status.state = label.to_string();
        status.since = now();
    }
}

fn with_status(f: impl FnOnce(&mut Status)) {
    let mut guard = STATUS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(Status::default));
}

/// Wake the loop to re-read its settings (call after changing them, or
/// after saving or removing the Gmail connection).
pub fn recheck(credential_changed: bool) {
    if credential_changed {
        REFUSED.store(false, Ordering::SeqCst);
    }
    RECHECK.store(true, Ordering::SeqCst);
}

pub fn enabled(state: &AppState) -> bool {
    state
        .store
        .consolidation_state(KEY_WATCH)
        .ok()
        .flatten()
        .is_none_or(|v| v != "off")
}

pub fn every_minutes(state: &AppState) -> u64 {
    parse_every(
        state
            .store
            .consolidation_state(KEY_EVERY)
            .ok()
            .flatten()
            .as_deref(),
    )
}

fn parse_every(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_EVERY_MINUTES, |m| m.clamp(1, 60))
}

/// Why the watch can't run right now, if it can't.
fn blocked(state: &AppState) -> Option<&'static str> {
    if state.killswitch.load(Ordering::Relaxed) {
        Some("paused: this node is fenced by its provider")
    } else if crate::gmail_pull::credentials().is_none() {
        Some("not connected")
    } else if !enabled(state) {
        Some("off")
    } else if REFUSED.load(Ordering::SeqCst) {
        Some("stopped: Gmail refused the sign-in")
    } else {
        None
    }
}

/// Is this node watching a mailbox right now? (`roles.mail` in the node list)
pub fn watching(state: &AppState) -> bool {
    blocked(state).is_none()
}

/// What the Integrations card, the CLI and `/v1/integrations/gmail` show.
pub fn status_json(state: &AppState) -> Value {
    let status = STATUS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .unwrap_or_default();
    // the loop's last label can be a gate it has just left behind
    let gated = ["not connected", "off"].contains(&status.state.as_str())
        || status.state.starts_with("paused")
        || status.state.starts_with("stopped");
    json!({
        "enabled": enabled(state),
        "every_minutes": every_minutes(state),
        "state": blocked(state).map(String::from).unwrap_or_else(|| {
            if status.state.is_empty() || gated { "starting".into() } else { status.state.clone() }
        }),
        "since": status.since,
        "last_check": status.last_check,
        "last_new_mail": status.last_new_mail,
        "last_error": status.last_error,
        "fetched_since_start": status.fetched,
    })
}

pub async fn run_loop(state: AppState) {
    tokio::time::sleep(STARTUP_DELAY).await;
    let mut session: Option<ImapFeed> = None;
    let mut backoff = BACKOFF_MIN;
    loop {
        RECHECK.store(false, Ordering::SeqCst);
        if let Some(why) = blocked(&state) {
            set_state(why);
            if let Some(feed) = session.take() {
                let _ = tokio::task::spawn_blocking(move || feed.logout()).await;
            }
            // a connect or a settings change wakes this at once
            let until = Instant::now() + GATE_RECHECK;
            while Instant::now() < until && !RECHECK.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            continue;
        }

        // Catch up: the one pull path, serialized with the 6-hour pass.
        set_state("catching up");
        let st = state.clone();
        let pulled = tokio::task::spawn_blocking(move || crate::gmail_pull::pull_once(&st)).await;
        let more = match pulled {
            Ok(Ok((fetched, episodes_new, more))) => {
                backoff = BACKOFF_MIN;
                with_status(|s| {
                    s.last_check = Some(now());
                    s.last_error = None;
                    s.fetched += fetched as u64;
                    if fetched > 0 {
                        s.last_new_mail = Some(now());
                    }
                });
                if fetched > 0 {
                    tracing::info!(
                        "gmail watch: {fetched} new message(s), {episodes_new} new episode(s)"
                    );
                }
                more
            }
            Ok(Err(err)) => {
                failed(&state, &err, &mut backoff, &mut session).await;
                continue;
            }
            Err(join) => {
                failed(
                    &state,
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
        set_state("watching");
        let interval = Duration::from_secs(every_minutes(&state) * 60);
        let feed = session.take();
        let waited = tokio::task::spawn_blocking(move || -> anyhow::Result<(ImapFeed, bool)> {
            let mut feed = match feed {
                Some(feed) => feed,
                None => {
                    let (user, password) = crate::gmail_pull::credentials()
                        .ok_or_else(|| anyhow::anyhow!("Gmail is no longer connected"))?;
                    ImapFeed::connect(&user, &password)?
                }
            };
            let deadline = Instant::now() + interval;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || RECHECK.load(Ordering::SeqCst) {
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
            Ok(Err(err)) => failed(&state, &err, &mut backoff, &mut session).await,
            Err(join) => {
                failed(
                    &state,
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
/// again — doubling up to half an hour. A refused sign-in stops the loop
/// until the connection is saved again, and tells the owner once.
async fn failed(
    state: &AppState,
    err: &anyhow::Error,
    backoff: &mut Duration,
    session: &mut Option<ImapFeed>,
) {
    *session = None;
    let message = format!("{err:#}");
    with_status(|s| s.last_error = Some(message.clone()));
    if is_auth_failure(err) {
        REFUSED.store(true, Ordering::SeqCst);
        set_state("stopped: Gmail refused the sign-in");
        tracing::warn!(
            "gmail watch: sign-in refused — stopped until Gmail is reconnected: {message}"
        );
        crate::provisioner::notify_system(
            state,
            "warn",
            "Gmail stopped accepting Hearthloom's sign-in",
            "New mail isn't arriving. Reconnect Gmail on the Integrations page (the app password \
             may have been revoked or changed).",
            Some("gmail-watch:refused"),
        );
        return;
    }
    set_state("retrying after an error");
    tracing::warn!(
        "gmail watch: {message} — retrying in {}s",
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
