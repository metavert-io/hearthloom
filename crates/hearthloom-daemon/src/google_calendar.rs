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
const SCOPES: &str = "openid email https://www.googleapis.com/auth/calendar.freebusy https://www.googleapis.com/auth/calendar.calendarlist.readonly";
/// Where Google sends the browser after consent. Nothing needs to answer
/// there: the owner copies the address it ended on.
pub const REDIRECT_URI: &str = "http://127.0.0.1:43279/hearthloom-calendar";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/calendar/v3";

pub const KEY_DAYS: &str = "calendar.days";
pub const KEY_HOURS: &str = "calendar.hours";
pub const KEY_TZ: &str = "calendar.tz";
pub const KEY_CALENDARS: &str = "calendar.calendars";
pub const KEY_NOTICE: &str = "calendar.notice_hours";
const DEFAULT_DAYS: &str = "mon,tue,wed,thu,fri";
const DEFAULT_HOURS: &str = "09:00-18:00";
const DEFAULT_NOTICE: i64 = 12;

struct Pending {
    state: String,
    verifier: String,
    started: Instant,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn client() -> Option<(String, String)> {
    Some((env(ENV_CLIENT_ID)?, env(ENV_CLIENT_SECRET)?))
}

pub fn connected() -> bool {
    client().is_some() && env(ENV_REFRESH).is_some()
}

fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Save the owner's Desktop-app client (sealed, live at once).
pub fn set_client(state: &AppState, id: &str, secret: &str) -> Result<()> {
    let (id, secret) = (id.trim(), secret.trim());
    if !id.ends_with(".apps.googleusercontent.com") || id.contains(char::is_whitespace) {
        bail!("that isn't a Google OAuth client ID (it ends in .apps.googleusercontent.com)");
    }
    if secret.len() < 10 || secret.contains(char::is_whitespace) {
        bail!("that doesn't look like the client secret");
    }
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[(ENV_CLIENT_ID, id), (ENV_CLIENT_SECRET, secret)],
    )
}

/// The address to open in a browser to connect.
pub fn start() -> Result<String> {
    let (id, _) = client()
        .ok_or_else(|| anyhow!("set up the Google client first (hearthloom calendar setup)"))?;
    let state = random();
    let verifier = random();
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let url = reqwest::Url::parse_with_params(
        AUTH_URL,
        &[
            ("client_id", id.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ],
    )?;
    *PENDING.lock().unwrap_or_else(|p| p.into_inner()) = Some(Pending {
        state,
        verifier,
        started: Instant::now(),
    });
    Ok(url.to_string())
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
    let pending = PENDING
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .ok_or_else(|| anyhow!("no sign-in is waiting here — start again"))?;
    if pending.started.elapsed() > Duration::from_secs(15 * 60) {
        bail!("that sign-in took too long — start again");
    }
    if pending.state != got_state {
        bail!("that address belongs to a different sign-in — start again");
    }
    let (id, secret) = client().ok_or_else(|| anyhow!("the Google client isn't set up"))?;
    let res = reqwest::Client::new()
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(30))
        .form(&[
            ("code", code.as_str()),
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("grant_type", "authorization_code"),
            ("code_verifier", pending.verifier.as_str()),
        ])
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "Google refused the code: {}",
            body["error_description"]
                .as_str()
                .or(body["error"].as_str())
                .unwrap_or("unknown")
        );
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
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[(ENV_REFRESH, refresh), (ENV_ACCOUNT, &account)],
    )?;
    if let (Some(token), Some(secs)) = (body["access_token"].as_str(), body["expires_in"].as_u64())
    {
        *TOKEN.lock().unwrap_or_else(|p| p.into_inner()) = Some((
            token.to_string(),
            Instant::now() + Duration::from_secs(secs.saturating_sub(60)),
        ));
    }
    crate::agents::publish_self(state);
    Ok(account)
}

pub fn disconnect(state: &AppState) -> Result<()> {
    *TOKEN.lock().unwrap_or_else(|p| p.into_inner()) = None;
    hearthloom_core::secrets::set_env_vars(
        state.home.root(),
        &[(ENV_REFRESH, ""), (ENV_ACCOUNT, "")],
    )?;
    crate::agents::publish_self(state);
    Ok(())
}

async fn access_token() -> Result<String> {
    if let Some((t, until)) = TOKEN.lock().unwrap_or_else(|p| p.into_inner()).clone() {
        if Instant::now() < until {
            return Ok(t);
        }
    }
    let (id, secret) = client().ok_or_else(|| anyhow!("the Google client isn't set up"))?;
    let refresh = env(ENV_REFRESH).ok_or_else(|| anyhow!("Google Calendar isn't connected"))?;
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
    Ok(token)
}

async fn api_get(path: &str) -> Result<Value> {
    let res = reqwest::Client::new()
        .get(format!("{API}{path}"))
        .bearer_auth(access_token().await?)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "Google Calendar: {}",
            body["error"]["message"]
                .as_str()
                .unwrap_or("request failed")
        );
    }
    Ok(body)
}

/// The account's calendars: id, name, primary, shown in Google Calendar,
/// time zone.
pub async fn calendars() -> Result<Vec<Value>> {
    let body =
        api_get("/users/me/calendarList?minAccessRole=freeBusyReader&maxResults=250").await?;
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
    ids: &[String],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<(DateTime<Utc>, DateTime<Utc>)>> {
    let res = reqwest::Client::new()
        .post(format!("{API}/freeBusy"))
        .bearer_auth(access_token().await?)
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
        bail!(
            "Google Calendar: {}",
            body["error"]["message"]
                .as_str()
                .unwrap_or("free/busy failed")
        );
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
    let cals = calendars().await?;
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
    let blocks = busy(&ids, now, now + chrono::Duration::days(days + 1)).await?;
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

pub fn status(state: &AppState) -> Value {
    let h = hours(state, None);
    json!({
        "client": client().is_some(),
        "connected": connected(),
        "account": env(ENV_ACCOUNT),
        "days": h.days.iter().map(|d| d.to_string().to_lowercase()).collect::<Vec<_>>(),
        "hours": format!("{}-{}", h.start.format("%H:%M"), h.end.format("%H:%M")),
        "time_zone": setting(state, KEY_TZ),
        "calendars": setting(state, KEY_CALENDARS),
        "notice_hours": h.notice_hours,
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
    Ok(())
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
}
