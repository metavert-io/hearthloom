//! Google Calendar, read-only free/busy (0.5.95).
//!
//! The owner's own Google Cloud "Desktop app" client signs this node in
//! (OAuth 2.0 with PKCE, `access_type=offline`): `start` hands back the
//! address to open in any browser, Google sends that browser to a
//! loopback address that needn't answer, and the owner pastes the address
//! it ended on (`finish`) — so a headless node signs in from a phone or a
//! laptop. Only two scopes are asked: `calendar.freebusy` (when the owner
//! is busy — never what the events are) and `calendar.calendarlist.readonly`
//! (which calendars there are), plus the account's email to show which one
//! is connected. The refresh token is sealed with the node's other secrets
//! (`set_env_vars`); access tokens live in memory.
//!
//! `free` turns busy blocks into free windows inside the owner's working
//! hours (settings on this node: days, hours, time zone, calendars, how
//! much notice). Reply drafts that settle a time offer only these.
//!
//! Which client signs in (0.5.97): the owner's own (`calendar setup`),
//! else Hearthloom's shared Desktop client, which the configured registry
//! publishes at `/.well-known/hearthloom-oauth` (Google treats an installed
//! app's secret as not confidential) — so no Cloud console chore. A sign-in
//! records the client that made it (`ENV_SIGNIN_CLIENT`); refreshes use
//! that client. Three ways to finish:
//! - **loopback**: on a local node the daemon listens on 127.0.0.1 (an
//!   ephemeral port, while the sign-in is pending) and finishes by itself
//!   when the browser is on this machine;
//! - **paste**: a browser elsewhere lands on a loopback address that
//!   doesn't load, and the owner pastes it (`finish`);
//! - **relay**: a hosted node signs in with Hearthlaunch's shared Web
//!   client, Google returns to Hearthlaunch, which checks the state this
//!   node signed and forwards code + state to this node's own
//!   `/oauth/google/callback`. Hearthlaunch never sees a token: only this
//!   node holds the PKCE verifier.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, Datelike, NaiveTime, TimeZone, Utc};
use serde_json::{json, Value};
use sha2::Digest;

use crate::AppState;

pub const ENV_CLIENT_ID: &str = "GOOGLE_OAUTH_CLIENT_ID";
pub const ENV_CLIENT_SECRET: &str = "GOOGLE_OAUTH_CLIENT_SECRET";
pub const ENV_REFRESH: &str = "GOOGLE_CALENDAR_REFRESH_TOKEN";
pub const ENV_ACCOUNT: &str = "GOOGLE_CALENDAR_ACCOUNT";
/// The client ID the saved sign-in came from (absent before 0.5.97: the
/// owner's own client).
pub const ENV_SIGNIN_CLIENT: &str = "GOOGLE_CALENDAR_CLIENT";
/// The shared clients, kept sealed so a restart without the network still
/// refreshes: the Desktop one from the registry, the Web one a hosted
/// node fetches from its provisioner.
const ENV_SHARED_ID: &str = "GOOGLE_SHARED_CLIENT_ID";
const ENV_SHARED_SECRET: &str = "GOOGLE_SHARED_CLIENT_SECRET";
const ENV_WEB_ID: &str = "GOOGLE_WEB_CLIENT_ID";
const ENV_WEB_SECRET: &str = "GOOGLE_WEB_CLIENT_SECRET";
const SCOPES: &str = "openid email https://www.googleapis.com/auth/calendar.freebusy https://www.googleapis.com/auth/calendar.calendarlist.readonly";
/// Where Google sends the browser when nothing on this machine can listen:
/// the owner copies the address it ended on.
pub const REDIRECT_URI: &str = "http://127.0.0.1:43279/hearthloom-calendar";
const LOOPBACK_PATH: &str = "/hearthloom-calendar";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/calendar/v3";
/// How long a sign-in may take, start to finish.
const PENDING_FOR: Duration = Duration::from_secs(15 * 60);

pub const KEY_DAYS: &str = "calendar.days";
pub const KEY_HOURS: &str = "calendar.hours";
pub const KEY_TZ: &str = "calendar.tz";
pub const KEY_CALENDARS: &str = "calendar.calendars";
pub const KEY_NOTICE: &str = "calendar.notice_hours";
pub const KEY_LINK: &str = "calendar.link";
/// The ember scope (0.5.97): free windows only, never events — held on the
/// plugin row like `mail.watch`.
pub const EMBER_SCOPE: &str = "calendar.availability";
const DEFAULT_DAYS: &str = "mon,tue,wed,thu,fri";
const DEFAULT_HOURS: &str = "09:00-18:00";
const DEFAULT_NOTICE: i64 = 12;

struct Pending {
    state: String,
    verifier: String,
    started: Instant,
    redirect_uri: String,
    client_id: String,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// Why the last loopback or relayed sign-in failed (the CLI and the card
/// read it from `status`); cleared when a sign-in starts.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
/// Bumped by every `start`, so an older sign-in's loopback listener stops.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SHARED_FETCHED: Mutex<Option<Instant>> = Mutex::new(None);

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// The owner's own client (`calendar setup`).
fn client() -> Option<(String, String)> {
    Some((env(ENV_CLIENT_ID)?, env(ENV_CLIENT_SECRET)?))
}

fn shared_saved() -> Option<(String, String)> {
    Some((env(ENV_SHARED_ID)?, env(ENV_SHARED_SECRET)?))
}

fn web_saved() -> Option<(String, String)> {
    Some((env(ENV_WEB_ID)?, env(ENV_WEB_SECRET)?))
}

pub fn connected() -> bool {
    env(ENV_REFRESH).is_some()
}

fn pending_now() -> bool {
    PENDING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|p| p.started.elapsed() < PENDING_FOR)
}

fn set_last_error(msg: Option<String>) {
    *LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = msg;
}

/// A client as Google issues it: an ID ending in .apps.googleusercontent.com
/// and a secret of letters, digits, `-` and `_`.
fn plausible(id: &str, secret: &str) -> bool {
    id.ends_with(".apps.googleusercontent.com")
        && !id.contains(char::is_whitespace)
        && secret.len() >= 10
        && secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Hearthloom's shared Desktop client, as the configured registry publishes
/// it (`/.well-known/hearthloom-oauth`), fetched at most daily unless
/// `force`d (a refresh Google refused with the saved secret). No registry,
/// no shared client: a self-hosted node that configured none uses its own.
async fn shared(state: &AppState, force: bool) -> Option<(String, String)> {
    let fresh = SHARED_FETCHED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .is_some_and(|t| t.elapsed() < Duration::from_secs(24 * 3600));
    if fresh && !force {
        return shared_saved();
    }
    let Some(base) = crate::registry_client::configured_registry() else {
        return shared_saved();
    };
    let fetched = async {
        let res = reqwest::Client::new()
            .get(format!("{base}/.well-known/hearthloom-oauth"))
            .timeout(Duration::from_secs(15))
            .send()
            .await?;
        if !res.status().is_success() {
            return Ok::<_, anyhow::Error>(None);
        }
        let body: Value = res.json().await?;
        let d = &body["google"]["desktop"];
        Ok(
            match (d["client_id"].as_str(), d["client_secret"].as_str()) {
                (Some(i), Some(s)) if plausible(i, s) => Some((i.to_string(), s.to_string())),
                _ => None,
            },
        )
    }
    .await;
    *SHARED_FETCHED.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    match fetched {
        Ok(Some(pair)) => {
            if shared_saved().as_ref() != Some(&pair) {
                let _ = hearthloom_core::secrets::set_env_vars(
                    state.home.root(),
                    &[(ENV_SHARED_ID, &pair.0), (ENV_SHARED_SECRET, &pair.1)],
                );
            }
            Some(pair)
        }
        Ok(None) => shared_saved(),
        Err(e) => {
            tracing::warn!("calendar: the shared Google client: {e:#}");
            shared_saved()
        }
    }
}

/// The shared Web client a hosted node signs in with through Hearthlaunch's
/// relay, fetched with a token this node signs (its secret never leaves
/// Hearthlaunch for anyone else).
async fn web_client(state: &AppState, force: bool) -> Option<(String, String)> {
    if !force {
        if let Some(saved) = web_saved() {
            return Some(saved);
        }
    }
    let base = crate::provisioner::provisioner_url()?;
    let label = crate::provisioner::public_label(state)?;
    let token =
        crate::provisioner::sign_for_provisioner("google-web-client", json!({"node": label}), 120)?;
    let res = reqwest::Client::new()
        .post(format!("{base}/oauth/google/web-client"))
        .timeout(Duration::from_secs(15))
        .json(&json!({ "token": token }))
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        tracing::warn!("calendar: the hosted Google client: {}", res.status());
        return web_saved();
    }
    let body: Value = res.json().await.ok()?;
    let (i, s) = (body["client_id"].as_str()?, body["client_secret"].as_str()?);
    if !plausible(i, s) {
        return web_saved();
    }
    let pair = (i.to_string(), s.to_string());
    if web_saved().as_ref() != Some(&pair) {
        let _ = hearthloom_core::secrets::set_env_vars(
            state.home.root(),
            &[(ENV_WEB_ID, &pair.0), (ENV_WEB_SECRET, &pair.1)],
        );
    }
    Some(pair)
}

/// The secret of the client that made the saved sign-in (looked up again
/// when `force`d: a rotated shared secret).
async fn secret_for(state: &AppState, id: &str, force: bool) -> Option<String> {
    if !force {
        for (i, s) in [client(), shared_saved(), web_saved()]
            .into_iter()
            .flatten()
        {
            if i == id {
                return Some(s);
            }
        }
    }
    for (i, s) in [shared(state, true).await, web_client(state, true).await]
        .into_iter()
        .flatten()
    {
        if i == id {
            return Some(s);
        }
    }
    client().filter(|(i, _)| i == id).map(|(_, s)| s)
}

/// Which client a new sign-in here would use: "own", "shared" or "hosted".
fn source() -> Option<&'static str> {
    if client().is_some() {
        Some("own")
    } else if crate::provisioner::provisioner_url().is_some() {
        Some("hosted")
    } else if crate::registry_client::configured_registry().is_some() || shared_saved().is_some() {
        Some("shared")
    } else {
        None
    }
}

fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Save the owner's Desktop-app client (sealed, live at once) — only once
/// Google has accepted the pair. Either field may hold the JSON Google
/// offers as a download; a bare secret keeps the client ID saved here.
/// A different client forgets the sign-in, which belonged to the old one.
pub async fn set_client(state: &AppState, id: &str, secret: &str) -> Result<()> {
    let saved = env(ENV_CLIENT_ID);
    let (id, secret) = client_from_input(id, secret, saved.as_deref())?;
    check_with_google(&id, &secret).await?;
    // a sign-in made with the client being replaced goes with it; one made
    // with the shared client stays (before 0.5.97 every sign-in was the own client's)
    let signed_with = env(ENV_SIGNIN_CLIENT).or_else(|| saved.clone());
    if saved.as_deref().is_some_and(|s| s != id) && signed_with == saved {
        disconnect(state)?;
    }
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[(ENV_CLIENT_ID, &id), (ENV_CLIENT_SECRET, &secret)],
    )
}

/// The client ID and secret in what the owner pasted: the downloaded JSON
/// (`{"installed": {…}}`, or just the inside), or the secret alone with
/// the ID typed beside it or saved here.
fn client_from_input(id: &str, secret: &str, saved_id: Option<&str>) -> Result<(String, String)> {
    let (id, secret) = (id.trim(), secret.trim());
    let (id, secret) = if let Some(text) = [secret, id].into_iter().find(|s| s.starts_with('{')) {
        let v: Value = serde_json::from_str(text)
            .context("that looks like the JSON file but doesn't read whole — paste all of it")?;
        if v.get("web").is_some() {
            bail!("that JSON is for a Web application client — Hearthloom needs a Desktop app client (Clients → Create client → Desktop app)");
        }
        let c = v.get("installed").unwrap_or(&v);
        let (Some(jid), Some(jsecret)) = (c["client_id"].as_str(), c["client_secret"].as_str())
        else {
            bail!("that JSON has no client ID and secret — download it again from Clients → your Desktop app client");
        };
        let (jid, jsecret) = (jid.trim(), jsecret.trim());
        let (other, matches) = if text == secret {
            (id, id == jid)
        } else {
            (secret, secret == jsecret)
        };
        if !other.is_empty() && !matches {
            bail!("the JSON is for a different client than what you typed beside it — paste the JSON alone");
        }
        (jid.to_string(), jsecret.to_string())
    } else {
        let id = if id.is_empty() {
            saved_id.unwrap_or("")
        } else {
            id
        };
        if id.is_empty() {
            bail!("the client ID is needed too — or paste the whole JSON Google gave you");
        }
        (id.to_string(), secret.to_string())
    };
    if !id.ends_with(".apps.googleusercontent.com") || id.contains(char::is_whitespace) {
        bail!("that isn't a Google OAuth client ID (it ends in .apps.googleusercontent.com)");
    }
    if secret == id || secret.ends_with(".apps.googleusercontent.com") {
        bail!("that's the client ID again — paste the client secret (it starts with GOCSPX-) or the whole JSON");
    }
    if secret.len() < 10
        || !secret
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("that doesn't look like a client secret (it starts with GOCSPX-) — paste it again, or the whole JSON");
    }
    Ok((id, secret))
}

/// Ask Google whether the ID and secret belong together before anything is
/// saved: a token request with a made-up refresh token. Google checks the
/// client first, so a wrong pair is `invalid_client` and a right one gets
/// as far as refusing the made-up token.
async fn check_with_google(id: &str, secret: &str) -> Result<()> {
    let res = reqwest::Client::new()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(20))
        .form(&[
            ("client_id", id),
            ("client_secret", secret),
            ("grant_type", "refresh_token"),
            ("refresh_token", "1//hearthloom-setup-check"),
        ])
        .send()
        .await
        .context("couldn't reach Google to check the client — nothing was saved")?;
    google_verdict(&res.json().await.unwrap_or_default())
}

fn google_verdict(body: &Value) -> Result<()> {
    let error = body["error"].as_str().unwrap_or("");
    let said = body["error_description"].as_str().unwrap_or(error);
    match error {
        // the pair passed; only the made-up token was refused
        "invalid_grant" | "unauthorized_client" => Ok(()),
        "invalid_client" if said.contains("not found") => bail!(
            "Google doesn't know that client ID (\"{said}\") — check it, or wait a few minutes if you've just made the client"
        ),
        "invalid_client" => bail!(
            "Google says that isn't the client's secret (\"{said}\") — copy it again from Clients → your Desktop app client, or add a new secret there"
        ),
        _ => bail!(
            "Google gave an unexpected answer (\"{}\") — nothing was saved",
            if said.is_empty() { "no error" } else { said }
        ),
    }
}

/// Forget the client and the sign-in here, so setup starts over.
pub fn reset(state: &AppState) -> Result<()> {
    *PENDING.lock().unwrap_or_else(|p| p.into_inner()) = None;
    disconnect(state)?;
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[(ENV_CLIENT_ID, ""), (ENV_CLIENT_SECRET, "")],
    )
}

/// A sign-in under way: the address to open, the account Google is asked
/// to suggest (the Gmail this node reads, the likeliest calendar — the
/// owner can still choose another), and how it finishes: "relay" (back by
/// itself through Hearthlaunch), "loopback" (by itself when the browser is
/// on this machine, else pasted) or "paste".
pub struct Started {
    pub url: String,
    pub hint: Option<String>,
    pub mode: &'static str,
}

pub async fn start(state: &AppState, host: Option<&str>) -> Result<Started> {
    if let Some(h) = host {
        crate::provisioner::remember_public_host(state, h);
    }
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    set_last_error(None);
    let verifier = random();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    // the mailbox this node reads (the only one, or the first)
    let hint = crate::mailboxes::default_one()
        .map(|mb| mb.address)
        .filter(|u| u.contains('@'));
    // a hosted node without a client of its own signs in through the relay
    let relay = match (
        client(),
        crate::provisioner::provisioner_url(),
        crate::provisioner::public_label(state),
    ) {
        (None, Some(base), Some(label)) => match web_client(state, false).await {
            Some((id, _)) => crate::provisioner::sign_for_provisioner(
                "google-relay",
                json!({ "node": label }),
                PENDING_FOR.as_secs() as i64,
            )
            .map(|st| (id, format!("{base}/oauth/google/callback"), st)),
            None => None,
        },
        _ => None,
    };
    let (client_id, redirect_uri, st, mode) = match relay {
        Some((id, redirect, st)) => (id, redirect, st, "relay"),
        None => {
            let (id, _) = match client() {
                Some(own) => own,
                None => shared(state, false).await.ok_or_else(|| {
                    anyhow!("set up the Google client first (hearthloom calendar setup)")
                })?,
            };
            let (redirect, mode) = if crate::monitor::node_kind() == "local" {
                match listen(state.clone(), generation).await {
                    Ok(port) => (
                        format!("http://127.0.0.1:{port}{LOOPBACK_PATH}"),
                        "loopback",
                    ),
                    Err(e) => {
                        tracing::warn!("calendar: no loopback listener ({e:#}); paste instead");
                        (REDIRECT_URI.to_string(), "paste")
                    }
                }
            } else {
                (REDIRECT_URI.to_string(), "paste")
            };
            (id, redirect, random(), mode)
        }
    };
    let url = auth_url(&client_id, &redirect_uri, &st, &challenge, hint.as_deref())?;
    *PENDING.lock().unwrap_or_else(|p| p.into_inner()) = Some(Pending {
        state: st,
        verifier,
        started: Instant::now(),
        redirect_uri,
        client_id,
    });
    Ok(Started { url, hint, mode })
}

fn auth_url(
    id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    hint: Option<&str>,
) -> Result<String> {
    let mut params = vec![
        ("client_id", id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", SCOPES),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ];
    if let Some(h) = hint {
        params.push(("login_hint", h));
    }
    Ok(reqwest::Url::parse_with_params(AUTH_URL, &params)?.to_string())
}

/// Listen on an ephemeral loopback port while this sign-in is pending: a
/// browser on this machine lands here and the sign-in finishes by itself.
/// Loopback only, this one sign-in, at most PENDING_FOR; a newer `start`
/// stops it.
async fn listen(state: AppState, generation: u64) -> Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + PENDING_FOR;
        while GENERATION.load(std::sync::atomic::Ordering::SeqCst) == generation
            && tokio::time::Instant::now() < deadline
        {
            let Ok(Ok((mut conn, _))) =
                tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
            else {
                continue;
            };
            if answer(&state, &mut conn).await {
                break;
            }
        }
    });
    Ok(port)
}

/// One browser request on the loopback listener; true once the sign-in is
/// settled (finished, refused or declined).
async fn answer(state: &AppState, conn: &mut tokio::net::TcpStream) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 8192];
    let mut n = 0;
    while n < buf.len() {
        match tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf[n..])).await {
            Ok(Ok(k)) if k > 0 => {
                n += k;
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            _ => break,
        }
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let (status, body, settled) = if target.split('?').next() != Some(LOOPBACK_PATH) {
        ("404 Not Found", String::new(), false)
    } else {
        match parse_landing(&format!("http://127.0.0.1{target}")) {
            Ok((code, st)) => match complete(state, &code, &st).await {
                Ok(account) => (
                    "200 OK",
                    landing_page(true, &connected_message(&account), None),
                    true,
                ),
                Err(e) => {
                    let msg = format!("{e:#}");
                    // a stray request (another state) leaves the sign-in
                    // waiting, and this listener up for the real one
                    let settled = !pending_now();
                    if settled {
                        set_last_error(Some(msg.clone()));
                    }
                    let status = if settled { "200 OK" } else { "400 Bad Request" };
                    (status, landing_page(false, &msg, None), settled)
                }
            },
            Err(e) => {
                let msg = format!("{e:#}");
                // Google said no (declined, say): the sign-in is over
                let over = target.contains("error=");
                if over {
                    PENDING.lock().unwrap_or_else(|p| p.into_inner()).take();
                    set_last_error(Some(msg.clone()));
                }
                ("400 Bad Request", landing_page(false, &msg, None), over)
            }
        }
    };
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.write_all(reply.as_bytes()).await;
    let _ = conn.shutdown().await;
    settled
}

fn connected_message(account: &str) -> String {
    if account.is_empty() {
        "Google Calendar is connected. You can close this tab.".into()
    } else {
        format!("Google Calendar is connected ({account}). You can close this tab.")
    }
}

/// The page a browser lands on when a sign-in finishes here: plain, no
/// scripts; `back` links to the dashboard.
pub fn landing_page(ok: bool, msg: &str, back: Option<&str>) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
         <title>Hearthloom</title><body style=\"font:16px system-ui,sans-serif;margin:3em auto;\
         max-width:32em;padding:0 1em\"><h1 style=\"font-size:1.3em\">{}</h1><p>{}</p>{}",
        if ok { "Connected" } else { "Not connected" },
        esc(msg),
        back.map(|b| format!("<p><a href=\"{}\">Back to Hearthloom</a></p>", esc(b)))
            .unwrap_or_default()
    )
}

/// `code` and `state` from the address the browser ended on.
pub fn parse_landing(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    let url = reqwest::Url::parse(pasted)
        .or_else(|_| reqwest::Url::parse(&format!("http://x/?{}", pasted.trim_start_matches('?'))))
        .context("paste the whole address the browser ended on")?;
    let get = |k: &str| {
        url.query_pairs()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.to_string())
    };
    if let Some(err) = get("error") {
        bail!("Google said: {err}");
    }
    match (get("code"), get("state")) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => {
            bail!("that address has no sign-in code — paste the whole address the browser ended on")
        }
    }
}

/// The JSON payload of a JWT (the id token Google just handed us over
/// TLS — read for the email only, never trusted for anything else).
fn jwt_email(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v["email"].as_str().map(String::from)
}

/// Finish connecting with the address the owner pasted. Returns the
/// account's email.
pub async fn finish(state: &AppState, pasted: &str) -> Result<String> {
    let (code, got_state) = parse_landing(pasted)?;
    complete(state, &code, &got_state).await
}

/// A hosted node's sign-in coming back through Hearthlaunch's relay
/// (`GET /oauth/google/callback`): the state this node signed and the
/// verifier only it holds are the credentials — no session needed.
pub async fn relay_callback(state: &AppState, query: &str) -> (bool, String) {
    let was_pending = pending_now();
    let result = match parse_landing(&format!("http://x/?{query}")) {
        Ok((code, st)) => complete(state, &code, &st).await,
        Err(e) => {
            // Google said no (declined, say): the sign-in is over
            if query.contains("error=") {
                PENDING.lock().unwrap_or_else(|p| p.into_inner()).take();
            }
            Err(e)
        }
    };
    match result {
        Ok(account) => (true, connected_message(&account)),
        Err(e) => {
            let msg = format!("{e:#}");
            // only a request that used up the sign-in is its outcome: a
            // stray one (anyone can reach this address) leaves no trace
            if was_pending && !pending_now() {
                set_last_error(Some(msg.clone()));
            }
            (false, msg)
        }
    }
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Exchange Google's code with this sign-in's verifier and client, check
/// the account has a calendar, and save the sign-in. The pending sign-in
/// is used up only by its own state, so a stray request can't cancel it.
async fn complete(state: &AppState, code: &str, got_state: &str) -> Result<String> {
    let pending = {
        let mut guard = PENDING.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            None => bail!("no sign-in is waiting here — start again"),
            Some(p) if p.started.elapsed() > PENDING_FOR => {
                *guard = None;
                bail!("that sign-in took too long — start again")
            }
            Some(p) if !same(&p.state, got_state) => {
                bail!("that address belongs to a different sign-in — start again")
            }
            Some(_) => guard.take().expect("checked above"),
        }
    };
    let secret = secret_for(state, &pending.client_id, false)
        .await
        .ok_or_else(|| anyhow!("the Google client isn't set up"))?;
    let res = reqwest::Client::new()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(30))
        .form(&[
            ("code", code),
            ("client_id", pending.client_id.as_str()),
            ("client_secret", secret.as_str()),
            ("redirect_uri", pending.redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", pending.verifier.as_str()),
        ])
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        let said = body["error_description"]
            .as_str()
            .or(body["error"].as_str())
            .unwrap_or("unknown");
        if body["error"] == json!("invalid_client") {
            // the sign-in itself worked; it's this node's client Google won't take
            bail!("Google turned down this node's client (\"{said}\") — run hearthloom calendar setup again");
        }
        bail!("Google refused the sign-in code: {said}");
    }
    let refresh = body["refresh_token"]
        .as_str()
        .ok_or_else(|| anyhow!("Google sent no refresh token — start again"))?;
    let granted = body["scope"].as_str().unwrap_or("");
    if !granted.contains("calendar.freebusy") {
        bail!("the calendar permission wasn't granted — start again and tick it");
    }
    let account = body["id_token"]
        .as_str()
        .and_then(jwt_email)
        .unwrap_or_default();
    // An account without Google Calendar (a Workspace admin account, say)
    // signs in fine and then fails every read: turn it away now.
    if let Some(token) = body["access_token"].as_str() {
        let res = reqwest::Client::new()
            .get(format!("{API}/users/me/calendarList?maxResults=1"))
            .bearer_auth(token)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        if !res.status().is_success() {
            let body: Value = res.json().await.unwrap_or_default();
            if not_a_calendar_user(&body) {
                bail!("{} has no Google Calendar — connect again and choose the account whose calendar you use", if account.is_empty() { "that account" } else { &account });
            }
        }
    }
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[
            (ENV_REFRESH, refresh),
            (ENV_ACCOUNT, &account),
            (ENV_SIGNIN_CLIENT, &pending.client_id),
        ],
    )?;
    if let (Some(token), Some(secs)) = (body["access_token"].as_str(), body["expires_in"].as_u64())
    {
        *TOKEN.lock().unwrap_or_else(|p| p.into_inner()) = Some((
            token.to_string(),
            Instant::now() + Duration::from_secs(secs.saturating_sub(60)),
        ));
    }
    set_last_error(None);
    crate::agents::publish_self(state);
    Ok(account)
}

pub fn disconnect(state: &AppState) -> Result<()> {
    *TOKEN.lock().unwrap_or_else(|p| p.into_inner()) = None;
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[
            (ENV_REFRESH, ""),
            (ENV_ACCOUNT, ""),
            (ENV_SIGNIN_CLIENT, ""),
        ],
    )?;
    crate::agents::publish_self(state);
    Ok(())
}

async fn access_token(state: &AppState) -> Result<String> {
    if let Some((t, until)) = TOKEN.lock().unwrap_or_else(|p| p.into_inner()).clone() {
        if Instant::now() < until {
            return Ok(t);
        }
    }
    let refresh = env(ENV_REFRESH).ok_or_else(|| anyhow!("Google Calendar isn't connected"))?;
    // before 0.5.97 every sign-in was the owner's own client's
    let id = env(ENV_SIGNIN_CLIENT)
        .or_else(|| env(ENV_CLIENT_ID))
        .ok_or_else(|| {
            anyhow!("this node doesn't know which Google client signed in — connect again")
        })?;
    let mut forced = false;
    loop {
        let secret = secret_for(state, &id, forced).await.ok_or_else(|| {
            anyhow!("the Google client that signed in is gone — connect again (hearthloom calendar connect)")
        })?;
        let res = reqwest::Client::new()
            .post(TOKEN_URL)
            .timeout(Duration::from_secs(30))
            .form(&[
                ("client_id", id.as_str()),
                ("client_secret", secret.as_str()),
                ("refresh_token", refresh.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            // a shared secret rotated since it was saved: look it up again, once
            if body["error"] == json!("invalid_client") && !forced {
                forced = true;
                continue;
            }
            bail!(
                "Google refused the saved sign-in ({}) — connect again",
                body["error"].as_str().unwrap_or("unknown")
            );
        }
        let token = body["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("Google sent no access token"))?
            .to_string();
        let secs = body["expires_in"].as_u64().unwrap_or(3600);
        *TOKEN.lock().unwrap_or_else(|p| p.into_inner()) = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(secs.saturating_sub(60)),
        ));
        return Ok(token);
    }
}

async fn api_get(state: &AppState, path: &str) -> Result<Value> {
    let res = reqwest::Client::new()
        .get(format!("{API}{path}"))
        .bearer_auth(access_token(state).await?)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(api_error(&body, "request failed"));
    }
    Ok(body)
}

/// Google's "The user must be signed up for Google Calendar": the account
/// signed in has no calendar at all.
fn not_a_calendar_user(body: &Value) -> bool {
    let e = &body["error"];
    e["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|x| x["reason"] == json!("notACalendarUser"))
        || e["message"]
            .as_str()
            .is_some_and(|m| m.contains("signed up for Google Calendar"))
}

fn api_error(body: &Value, fallback: &str) -> anyhow::Error {
    if not_a_calendar_user(body) {
        return anyhow!(
            "{} has no Google Calendar — run hearthloom calendar connect and choose the account whose calendar you use",
            env(ENV_ACCOUNT).unwrap_or_else(|| "the connected account".into())
        );
    }
    anyhow!(
        "Google Calendar: {}",
        body["error"]["message"].as_str().unwrap_or(fallback)
    )
}

/// The account's calendars: id, name, primary, shown in Google Calendar,
/// time zone.
pub async fn calendars(state: &AppState) -> Result<Vec<Value>> {
    let body = api_get(
        state,
        "/users/me/calendarList?minAccessRole=freeBusyReader&maxResults=250",
    )
    .await?;
    Ok(body["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            json!({
                "id": c["id"], "name": c["summaryOverride"].as_str().or(c["summary"].as_str()),
                "primary": c["primary"] == json!(true),
                "selected": c["selected"] == json!(true),
                "time_zone": c["timeZone"],
            })
        })
        .collect())
}

/// Busy blocks across these calendars between two instants.
pub async fn busy(
    state: &AppState,
    ids: &[String],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
    let res = reqwest::Client::new()
        .post(format!("{API}/freeBusy"))
        .bearer_auth(access_token(state).await?)
        .timeout(Duration::from_secs(30))
        .json(&json!({
            "timeMin": from.to_rfc3339(), "timeMax": to.to_rfc3339(),
            "items": ids.iter().take(50).map(|id| json!({"id": id})).collect::<Vec<_>>(),
        }))
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        return Err(api_error(&body, "free/busy failed"));
    }
    let mut out = Vec::new();
    for (_, cal) in body["calendars"].as_object().into_iter().flatten() {
        for b in cal["busy"].as_array().into_iter().flatten() {
            let parse = |k: &str| {
                b[k].as_str()
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map(|t| t.with_timezone(&Utc))
            };
            if let (Some(s), Some(e)) = (parse("start"), parse("end")) {
                out.push((s, e));
            }
        }
    }
    out.sort();
    Ok(out)
}

/* ————— working hours and free windows ————— */

#[derive(Debug, Clone, PartialEq)]
pub struct Hours {
    pub days: Vec<chrono::Weekday>,
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub tz: chrono_tz::Tz,
    pub notice_hours: i64,
}

fn weekday(s: &str) -> Option<chrono::Weekday> {
    use chrono::Weekday::*;
    Some(match s.trim().to_lowercase().get(..3)? {
        "mon" => Mon,
        "tue" => Tue,
        "wed" => Wed,
        "thu" => Thu,
        "fri" => Fri,
        "sat" => Sat,
        "sun" => Sun,
        _ => return None,
    })
}

pub fn parse_days(s: &str) -> Result<Vec<chrono::Weekday>> {
    let days: Option<Vec<_>> = s.split(',').map(weekday).collect();
    match days {
        Some(d) if !d.is_empty() => Ok(d),
        _ => bail!("days are like mon,tue,wed,thu,fri"),
    }
}

pub fn parse_hours(s: &str) -> Result<(NaiveTime, NaiveTime)> {
    let (a, b) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("hours are like 09:00-18:00"))?;
    let t = |x: &str| NaiveTime::parse_from_str(x.trim(), "%H:%M");
    let (a, b) = (t(a)?, t(b)?);
    if a >= b {
        bail!("the day has to end after it starts");
    }
    Ok((a, b))
}

fn setting(state: &AppState, key: &str) -> Option<String> {
    state
        .store
        .consolidation_state(key)
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty())
}

/// The owner's working hours on this node (the defaults until set).
pub fn hours(state: &AppState, fallback_tz: Option<&str>) -> Hours {
    let days = setting(state, KEY_DAYS)
        .and_then(|d| parse_days(&d).ok())
        .unwrap_or_else(|| parse_days(DEFAULT_DAYS).unwrap_or_default());
    let (start, end) = setting(state, KEY_HOURS)
        .and_then(|h| parse_hours(&h).ok())
        .unwrap_or_else(|| parse_hours(DEFAULT_HOURS).expect("default hours"));
    let tz = setting(state, KEY_TZ)
        .or_else(|| fallback_tz.map(String::from))
        .and_then(|t| t.parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::America::New_York);
    let notice_hours = setting(state, KEY_NOTICE)
        .and_then(|n| n.parse().ok())
        .unwrap_or(DEFAULT_NOTICE);
    Hours {
        days,
        start,
        end,
        tz,
        notice_hours,
    }
}

/// Free windows of at least `minutes` inside working hours, from `now`
/// for `days` calendar days, given the busy blocks.
pub fn free_windows(
    h: &Hours,
    busy: &[(DateTime<Utc>, DateTime<Utc>)],
    now: DateTime<Utc>,
    days: i64,
    minutes: i64,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let earliest = now + chrono::Duration::hours(h.notice_hours);
    let today = now.with_timezone(&h.tz).date_naive();
    let mut out = Vec::new();
    for offset in 0..days {
        let day = today + chrono::Duration::days(offset);
        if !h.days.contains(&day.weekday()) {
            continue;
        }
        let at = |t: NaiveTime| {
            h.tz.from_local_datetime(&day.and_time(t))
                .earliest()
                .map(|x| x.with_timezone(&Utc))
        };
        let (Some(mut start), Some(end)) = (at(h.start), at(h.end)) else {
            continue;
        };
        if start < earliest {
            // round the notice up to the next half hour
            let e = earliest.timestamp();
            let rounded = e + (1800 - e.rem_euclid(1800)) % 1800;
            start = DateTime::from_timestamp(rounded, 0).unwrap_or(earliest);
        }
        let mut cursor = start;
        for (bs, be) in busy {
            if *be <= cursor || *bs >= end {
                continue;
            }
            if *bs > cursor && (*bs - cursor).num_minutes() >= minutes {
                out.push((cursor, (*bs).min(end)));
            }
            cursor = cursor.max(*be);
            if cursor >= end {
                break;
            }
        }
        if cursor < end && (end - cursor).num_minutes() >= minutes {
            out.push((cursor, end));
        }
    }
    out
}

/// The calendars that count as busy: the owner's choice, else the primary
/// and every calendar shown in Google Calendar.
async fn busy_calendars(state: &AppState) -> Result<(Vec<String>, Option<String>)> {
    let cals = calendars(state).await?;
    let tz = cals
        .iter()
        .find(|c| c["primary"] == json!(true))
        .and_then(|c| c["time_zone"].as_str().map(String::from));
    let ids = match setting(state, KEY_CALENDARS) {
        Some(list) => list.split(',').map(|s| s.trim().to_string()).collect(),
        None => cals
            .iter()
            .filter(|c| c["primary"] == json!(true) || c["selected"] == json!(true))
            .filter_map(|c| c["id"].as_str().map(String::from))
            .collect(),
    };
    Ok((ids, tz))
}

/// Free windows over the next `days` days.
pub async fn free(
    state: &AppState,
    days: i64,
    minutes: i64,
) -> Result<(Hours, Vec<(DateTime<Utc>, DateTime<Utc>)>)> {
    let (ids, tz) = busy_calendars(state).await?;
    let h = hours(state, tz.as_deref());
    let now = Utc::now();
    let blocks = busy(state, &ids, now, now + chrono::Duration::days(days + 1)).await?;
    Ok((h.clone(), free_windows(&h, &blocks, now, days, minutes)))
}

/// Windows as the owner reads them: "Tue Nov 10: 9:00–11:30, 2:00–5:00 PM".
pub fn describe(h: &Hours, windows: &[(DateTime<Utc>, DateTime<Utc>)]) -> Vec<String> {
    let mut lines: Vec<(chrono::NaiveDate, Vec<String>)> = Vec::new();
    for (s, e) in windows {
        let (s, e) = (s.with_timezone(&h.tz), e.with_timezone(&h.tz));
        let span = format!("{}–{}", s.format("%-I:%M %p"), e.format("%-I:%M %p"));
        match lines.last_mut() {
            Some((d, spans)) if *d == s.date_naive() => spans.push(span),
            _ => lines.push((s.date_naive(), vec![span])),
        }
    }
    lines
        .into_iter()
        .map(|(d, spans)| format!("{}: {}", d.format("%a %b %-d"), spans.join(", ")))
        .collect()
}

/// Working hours in a given zone, for tests elsewhere in the crate.
#[cfg(test)]
pub fn hours_for_test(tz: &str) -> Hours {
    Hours {
        days: parse_days(DEFAULT_DAYS).unwrap(),
        start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        end: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
        tz: tz.parse().unwrap(),
        notice_hours: 0,
    }
}

/// The same windows at a fixed UTC offset (another person's zone, from
/// their message's Date header), in `describe`'s form.
pub fn describe_offset(
    windows: &[(DateTime<Utc>, DateTime<Utc>)],
    offset_minutes: i32,
) -> Vec<String> {
    let Some(tz) = chrono::FixedOffset::east_opt(offset_minutes * 60) else {
        return Vec::new();
    };
    let mut lines: Vec<(chrono::NaiveDate, Vec<String>)> = Vec::new();
    for (s, e) in windows {
        let (s, e) = (s.with_timezone(&tz), e.with_timezone(&tz));
        let span = format!("{}–{}", s.format("%-I:%M %p"), e.format("%-I:%M %p"));
        match lines.last_mut() {
            Some((d, spans)) if *d == s.date_naive() => spans.push(span),
            _ => lines.push((s.date_naive(), vec![span])),
        }
    }
    lines
        .into_iter()
        .map(|(d, spans)| format!("{}: {}", d.format("%a %b %-d"), spans.join(", ")))
        .collect()
}

/// "UTC−8", "UTC+5:30".
pub fn utc_label(offset_minutes: i32) -> String {
    let sign = if offset_minutes < 0 { '−' } else { '+' };
    let (h, m) = (offset_minutes.abs() / 60, offset_minutes.abs() % 60);
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h}:{m:02}")
    }
}

pub fn status(state: &AppState) -> Value {
    let h = hours(state, None);
    let source = source();
    json!({
        "client": source.is_some(),
        // "own" (calendar setup), "shared" (Hearthloom's, from the registry)
        // or "hosted" (signs in through Hearthlaunch's relay)
        "client_source": source,
        // not a secret: Google puts it in every sign-in address
        "client_id": client().map(|c| c.0).or_else(|| match source {
            Some("hosted") => web_saved().map(|c| c.0),
            _ => shared_saved().map(|c| c.0),
        }),
        "signed_in_with": connected()
            .then(|| env(ENV_SIGNIN_CLIENT).or_else(|| env(ENV_CLIENT_ID)))
            .flatten(),
        "connected": connected(),
        "pending": pending_now(),
        "sign_in_error": LAST_ERROR.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        "account": env(ENV_ACCOUNT),
        "days": h.days.iter().map(|d| d.to_string().to_lowercase()).collect::<Vec<_>>(),
        "hours": format!("{}-{}", h.start.format("%H:%M"), h.end.format("%H:%M")),
        "time_zone": setting(state, KEY_TZ),
        "calendars": setting(state, KEY_CALENDARS),
        "notice_hours": h.notice_hours,
        "link": setting(state, KEY_LINK),
        "redirect_uri": REDIRECT_URI,
    })
}

/// Change the working hours and the rest (validated; empty = default).
pub fn set(state: &AppState, body: &Value) -> Result<()> {
    let put = |k: &str, v: &str| state.store.set_consolidation_state(k, v);
    if let Some(d) = body["days"].as_str() {
        if !d.is_empty() {
            parse_days(d)?;
        }
        put(KEY_DAYS, d)?;
    }
    if let Some(hrs) = body["hours"].as_str() {
        if !hrs.is_empty() {
            parse_hours(hrs)?;
        }
        put(KEY_HOURS, hrs)?;
    }
    if let Some(tz) = body["time_zone"].as_str() {
        if !tz.is_empty() && tz.parse::<chrono_tz::Tz>().is_err() {
            bail!("a time zone is like America/New_York");
        }
        put(KEY_TZ, tz)?;
    }
    if let Some(c) = body["calendars"].as_str() {
        put(KEY_CALENDARS, c)?;
    }
    if let Some(n) = body["notice_hours"].as_i64() {
        if !(0..=168).contains(&n) {
            bail!("notice is 0 to 168 hours");
        }
        put(KEY_NOTICE, &n.to_string())?;
    }
    if let Some(link) = body["link"].as_str() {
        let link = link.trim();
        if !link.is_empty() && !scheduling_link_ok(link) {
            bail!("a scheduling link is an https address, like https://calendly.com/you");
        }
        put(KEY_LINK, link)?;
    }
    Ok(())
}

/// The owner's scheduling page (Google appointment schedule, Calendly,
/// Cal.com…): an https address, offered in drafts beside free times.
fn scheduling_link_ok(link: &str) -> bool {
    link.len() <= 300
        && !link.contains(char::is_whitespace)
        && reqwest::Url::parse(link)
            .is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some_and(|h| h.contains('.')))
}

/// The owner's scheduling link, when one is set.
pub fn scheduling_link(state: &AppState) -> Option<String> {
    setting(state, KEY_LINK).filter(|l| scheduling_link_ok(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h() -> Hours {
        Hours {
            days: parse_days("mon,tue,wed,thu,fri").unwrap(),
            start: NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            end: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
            tz: chrono_tz::America::New_York,
            notice_hours: 0,
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn test_free_windows_skip_busy_blocks_weekends_and_short_gaps() {
        // Friday Nov 6 2026, 8:00 in New York (13:00Z)
        let now = at("2026-11-06T13:00:00Z");
        let busy = vec![
            (at("2026-11-06T15:00:00Z"), at("2026-11-06T16:00:00Z")), // 10–11
            (at("2026-11-06T16:10:00Z"), at("2026-11-06T17:00:00Z")), // 11:10–12, leaves a 10-minute gap
        ];
        let w = free_windows(&h(), &busy, now, 4, 30);
        let lines = describe(&h(), &w);
        assert_eq!(
            lines,
            vec![
                "Fri Nov 6: 9:00 AM–10:00 AM, 12:00 PM–5:00 PM".to_string(),
                "Mon Nov 9: 9:00 AM–5:00 PM".to_string(),
            ],
            "{lines:?}"
        );
        // notice pushes the first window, rounded to the half hour
        let mut later = h();
        later.notice_hours = 3; // 11:00 → past the busy block, starts at 12:00
        let w = free_windows(&later, &busy, now, 1, 30);
        assert_eq!(w[0].0, at("2026-11-06T17:00:00Z"));
    }

    #[test]
    fn test_settings_and_the_landing_address_parse() {
        assert!(parse_days("mon,funday").is_err());
        assert!(parse_hours("18:00-09:00").is_err());
        assert_eq!(
            parse_hours("08:30-17:00").unwrap().0,
            NaiveTime::from_hms_opt(8, 30, 0).unwrap()
        );
        let (code, state) = parse_landing(
            "http://127.0.0.1:43279/hearthloom-calendar?state=abc&code=4/0Ab-xyz&scope=email",
        )
        .unwrap();
        assert_eq!((code.as_str(), state.as_str()), ("4/0Ab-xyz", "abc"));
        assert!(
            parse_landing("http://127.0.0.1:43279/hearthloom-calendar?error=access_denied")
                .unwrap_err()
                .to_string()
                .contains("access_denied")
        );
        assert!(parse_landing("hello").is_err());
        // the email inside an id token
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"email":"sam@x.example"}"#);
        assert_eq!(
            jwt_email(&format!("h.{payload}.s")).as_deref(),
            Some("sam@x.example")
        );
    }

    #[test]
    fn test_the_client_comes_from_the_json_or_the_bare_secret() {
        let id = "123-abc.apps.googleusercontent.com";
        let secret = "GOCSPX-fake-secret-for-tests";
        let want = Ok((id.to_string(), secret.to_string()));
        let ok = |i: &str, s: &str, saved: Option<&str>| {
            client_from_input(i, s, saved).map_err(|e| e.to_string())
        };
        // the file Google offers, one line, pasted where the secret goes
        let file = format!(
            r#"{{"installed":{{"client_id":"{id}","project_id":"p","auth_uri":"https://accounts.google.com/o/oauth2/auth","token_uri":"https://oauth2.googleapis.com/token","client_secret":"{secret}","redirect_uris":["http://localhost"]}}}}"#
        );
        assert_eq!(ok("", &file, None), want);
        assert_eq!(ok(id, &file, None), want, "the same ID typed beside it");
        assert_eq!(ok(&file, "", None), want, "pasted where the ID goes");
        // pretty-printed, and just the inside
        let pretty =
            serde_json::to_string_pretty(&serde_json::from_str::<Value>(&file).unwrap()).unwrap();
        assert_eq!(ok("", &pretty, None), want);
        let inside = format!(r#"{{"client_id":"{id}","client_secret":"{secret}"}}"#);
        assert_eq!(ok("", &inside, None), want);
        // the bare secret: with the ID, or keeping the saved one
        assert_eq!(ok(id, secret, None), want);
        assert_eq!(ok("", &format!("  {secret}\n"), Some(id)), want);
        let refused = |i: &str, s: &str, saved: Option<&str>, says: &str| {
            let e = ok(i, s, saved).unwrap_err();
            assert!(e.contains(says), "{e}");
        };
        refused("", secret, None, "client ID is needed");
        refused(id, id, None, "client ID again");
        refused(
            id,
            "GOCSPX-abc def",
            None,
            "doesn't look like a client secret",
        );
        refused(
            id,
            "GOCSPX-abcdefghij\u{200b}klm",
            None,
            "doesn't look like",
        );
        refused(
            "",
            "{\"installed\": {\"client_id\"",
            None,
            "doesn't read whole",
        );
        refused(
            "",
            &file.replace("installed", "web"),
            None,
            "Web application client",
        );
        refused(
            "999-other.apps.googleusercontent.com",
            &file,
            None,
            "different client",
        );
        refused("", r#"{"installed":{}}"#, None, "no client ID and secret");
    }

    #[test]
    fn test_the_sign_in_suggests_the_gmail_account_and_names_a_calendarless_one() {
        let with = auth_url(
            "id.apps.googleusercontent.com",
            REDIRECT_URI,
            "st",
            "ch",
            Some("sam@x.example"),
        )
        .unwrap();
        assert!(with.contains("login_hint=sam%40x.example"), "{with}");
        assert!(with.contains("prompt=consent") && with.contains("access_type=offline"));
        let without = auth_url(
            "id.apps.googleusercontent.com",
            REDIRECT_URI,
            "st",
            "ch",
            None,
        )
        .unwrap();
        assert!(!without.contains("login_hint"), "{without}");
        // what Google says for an account with no Google Calendar (2026-11-06)
        let body = json!({"error": {"code": 403, "message": "The user must be signed up for Google Calendar.",
            "errors": [{"reason": "notACalendarUser"}]}});
        assert!(not_a_calendar_user(&body));
        assert!(not_a_calendar_user(
            &json!({"error": {"message": "The user must be signed up for Google Calendar."}})
        ));
        assert!(api_error(&body, "x")
            .to_string()
            .contains("has no Google Calendar"));
        let other = json!({"error": {"message": "Rate limit exceeded"}});
        assert!(!not_a_calendar_user(&other));
        assert_eq!(
            api_error(&other, "x").to_string(),
            "Google Calendar: Rate limit exceeded"
        );
    }

    #[test]
    fn test_times_in_their_zone_the_link_and_the_landing_page() {
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let w = vec![(at("2026-11-09T13:00:00Z"), at("2026-11-09T15:00:00Z"))];
        assert_eq!(
            describe_offset(&w, -8 * 60),
            vec!["Mon Nov 9: 5:00 AM–7:00 AM"]
        );
        assert_eq!(describe_offset(&w, 330), vec!["Mon Nov 9: 6:30 PM–8:30 PM"]);
        assert_eq!(utc_label(-480), "UTC−8");
        assert_eq!(utc_label(330), "UTC+5:30");
        assert!(scheduling_link_ok("https://calendly.com/sam/30min"));
        assert!(!scheduling_link_ok("http://calendly.com/sam"));
        assert!(!scheduling_link_ok("https://cal com/sam"));
        assert!(!scheduling_link_ok("javascript:alert(1)"));
        let page = landing_page(false, "<b>no</b> & \"x\"", Some("/#/integrations"));
        assert!(
            page.contains("&lt;b&gt;no&lt;/b&gt; &amp; &quot;x&quot;"),
            "{page}"
        );
        assert!(page.contains("href=\"/#/integrations\"") && !page.contains("<script"));
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }

    /// A stray request (a wrong state) is refused without using up the
    /// pending sign-in; only its own state takes it.
    #[tokio::test]
    async fn test_only_the_sign_ins_own_state_can_finish_it() {
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
        *PENDING.lock().unwrap() = Some(Pending {
            state: "the-real-state".into(),
            verifier: "v".into(),
            started: Instant::now(),
            redirect_uri: REDIRECT_URI.into(),
            client_id: "nobody.apps.googleusercontent.com".into(),
        });
        let e = complete(&state, "code", "a-forged-state")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("different sign-in"), "{e}");
        assert!(pending_now(), "a stray request must not cancel the sign-in");
        // its own state takes it (the exchange then fails: no such client here)
        let e = complete(&state, "code", "the-real-state")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("isn't set up"), "{e}");
        assert!(!pending_now());
    }

    #[test]
    fn test_googles_answer_decides_whether_the_pair_is_saved() {
        // what Google said to a real client ID with a made-up secret (2026-11-06)
        let wrong = json!({"error": "invalid_client", "error_description": "The provided client secret is invalid."});
        assert!(google_verdict(&wrong)
            .unwrap_err()
            .to_string()
            .contains("isn't the client's secret"));
        let unknown = json!({"error": "invalid_client", "error_description": "The OAuth client was not found."});
        assert!(google_verdict(&unknown)
            .unwrap_err()
            .to_string()
            .contains("doesn't know that client ID"));
        // a right pair gets as far as the made-up refresh token
        assert!(google_verdict(
            &json!({"error": "invalid_grant", "error_description": "Bad Request"})
        )
        .is_ok());
        assert!(google_verdict(&json!({})).is_err());
        assert!(google_verdict(&json!({"error": "invalid_request"})).is_err());
    }
}
