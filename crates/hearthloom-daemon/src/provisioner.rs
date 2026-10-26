//! The provisioner control plane + killswitch (design locked 2026-07-16).
//!
//! Structurally optional: the whole surface exists only when
//! HEARTHLOOM_PROVISIONER_SECRET is set at the MACHINE level (Fly
//! secrets) — absent, the routes answer 404 and no killswitch can ever
//! engage. Control tokens are the trusted-issuer HMAC shape with
//! purpose="control", a short exp, and a jti replay cache — verified
//! independently of (and above) PAT logic: no API key, admin included,
//! can reach this surface. Tokens ride the X-Provisioner-Token header so
//! the bearer path never sees them.
//!
//! The killswitch itself is a cached flag (the auth_locked pattern);
//! while active the HTTP surface shrinks to the allowlist in
//! `killswitch_allows` and everything else answers 402 carrying the
//! activator message + link.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::AppState;
use hearthloom_core::store::Actor;

const KEY_ACTIVE: &str = "killswitch.active";
const KEY_MESSAGE: &str = "killswitch.message";
const KEY_LINK: &str = "killswitch.link";
const KEY_ALLOW_EXPORT: &str = "killswitch.allow_export";
const KEY_LINK_DOMAINS: &str = "provisioner.link_domains";
/// Replay window = max token exp we accept.
const MAX_TOKEN_AGE_SECS: u64 = 300;

pub fn secret() -> Option<String> {
    std::env::var("HEARTHLOOM_PROVISIONER_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// jti replay cache — a control token is single-use within its window.
static SEEN_JTI: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

/// Verify a control token: HMAC-SHA256 `payload_b64.sig_b64` (the
/// trusted-issuer shape) with claims {purpose:"control", exp, jti, sub}.
/// Returns the issuer subject.
pub fn verify_control_token(token: &str) -> Result<String> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    let secret = secret().ok_or_else(|| anyhow!("no provisioner is configured"))?;
    let (payload_b64, sig_b64) = token
        .split_once('.')
        .ok_or_else(|| anyhow!("malformed control token"))?;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = engine.decode(payload_b64).map_err(|_| anyhow!("malformed control token"))?;
    let sig = engine.decode(sig_b64).map_err(|_| anyhow!("malformed control token"))?;
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow!("bad provisioner secret"))?;
    mac.update(&payload);
    mac.verify_slice(&sig).map_err(|_| anyhow!("control signature mismatch"))?;
    let claims: Value = serde_json::from_slice(&payload)?;
    if claims.get("purpose").and_then(|p| p.as_str()) != Some("control") {
        return Err(anyhow!("not a control token"));
    }
    let now = chrono::Utc::now().timestamp();
    let exp = claims.get("exp").and_then(|e| e.as_i64()).unwrap_or(0);
    if exp < now || exp > now + MAX_TOKEN_AGE_SECS as i64 {
        return Err(anyhow!("control token expired or over-long"));
    }
    let jti = claims
        .get("jti")
        .and_then(|j| j.as_str())
        .filter(|j| !j.is_empty())
        .ok_or_else(|| anyhow!("control token has no jti"))?;
    {
        let mut guard = SEEN_JTI.lock().unwrap();
        let cache = guard.get_or_insert_with(HashMap::new);
        cache.retain(|_, at| at.elapsed() < Duration::from_secs(MAX_TOKEN_AGE_SECS));
        if cache.insert(jti.to_string(), Instant::now()).is_some() {
            return Err(anyhow!("control token replayed"));
        }
    }
    Ok(claims
        .get("sub")
        .and_then(|s| s.as_str())
        .unwrap_or("provisioner")
        .to_string())
}

/* ————— killswitch state (cached like auth_locked) ————— */

pub fn load_killswitch(store: &hearthloom_core::Store) -> bool {
    store
        .consolidation_state(KEY_ACTIVE)
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false)
}

pub fn set_killswitch(
    state: &AppState,
    sub: &str,
    active: bool,
    message: &str,
    link: Option<&str>,
    allow_export: Option<bool>,
) -> Result<()> {
    state.store.set_consolidation_state(KEY_ACTIVE, if active { "1" } else { "0" })?;
    state.store.set_consolidation_state(KEY_MESSAGE, message)?;
    state.store.set_consolidation_state(KEY_LINK, link.unwrap_or(""))?;
    if let Some(allow) = allow_export {
        state
            .store
            .set_consolidation_state(KEY_ALLOW_EXPORT, if allow { "1" } else { "0" })?;
    }
    state
        .killswitch
        .store(active, std::sync::atomic::Ordering::Relaxed);
    // the transition is its own notification — an audit trail that
    // survives reactivation
    let _ = state.store.notify(
        &Actor::new(format!("provisioner:{sub}")),
        "provisioner",
        if active { "critical" } else { "info" },
        if active { "This node has been restricted" } else { "Full access restored" },
        message,
        link.filter(|l| !l.is_empty()).map(|l| (l, None)),
        Some("killswitch"),
    );
    Ok(())
}

/// The 402 payload every blocked request receives.
pub fn killswitch_body(store: &hearthloom_core::Store) -> Value {
    let get = |k: &str| store.consolidation_state(k).ok().flatten().unwrap_or_default();
    json!({
        "error": "this node is restricted by its provisioner",
        "killswitch": true,
        "message": get(KEY_MESSAGE),
        "link": Some(get(KEY_LINK)).filter(|l| !l.is_empty()),
    })
}

/// The shrunken surface while the killswitch is active: notifications,
/// key list/revoke, auth in/out, the shell, health, the way out — and
/// EXPORT, unconditionally (Jon 2026-07-18): a user mid-shutdown or in
/// a grace period can always take their data. The vault toggle rides
/// along because starting an export requires an unlocked vault.
/// Minting keys is deliberately absent.
pub fn killswitch_allows(path: &str, method: &axum::http::Method) -> bool {
    use axum::http::Method;
    path == "/"
        || path.starts_with("/v1/export/")
        || path == "/v1/vault"
        || path.starts_with("/assets/")
        || path == "/health"
        // ember public files (a vehicle maker's key check must keep
        // passing through a grace period, or pairing breaks)
        || path.starts_with("/.well-known/")
        || path == "/v1/auth/login"
        || path == "/v1/auth/logout"
        || path == "/v1/auth/me"
        || (path == "/v1/notifications" && method == Method::GET)
        || (path.starts_with("/v1/notifications/") && method == Method::POST)
        || path == "/v1/notifications/read-all"
        || (path == "/v1/keys" && method == Method::GET)
        || (path.starts_with("/v1/keys/") && path.ends_with("/revoke"))
        || path == "/v1/keys/self-revoke"
        || path.starts_with("/v1/provisioner/")
}

/* ————— link allowlist + producers ————— */

/// Origins a notification link may point at: this node, the paired
/// peer, and provisioner-registered domains.
pub fn allowed_origins(state: &AppState) -> Vec<String> {
    let mut origins = vec![
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        crate::sync::hostname(),
    ];
    if let Ok(Some((_, url))) = state.store.peer() {
        origins.push(url);
    }
    if let Some(domains) = state.store.consolidation_state(KEY_LINK_DOMAINS).ok().flatten() {
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&domains) {
            origins.extend(list);
        }
    }
    origins
}

pub fn set_link_domains(state: &AppState, domains: &[String]) -> Result<()> {
    state
        .store
        .set_consolidation_state(KEY_LINK_DOMAINS, &serde_json::to_string(domains)?)
}

/// Internal producer helper: system notifications never fail the
/// caller's path, and every creation publishes a bus hint so the peer's
/// bell rings in seconds (poll reconciles).
pub fn notify_system(state: &AppState, severity: &str, title: &str, body: &str, dedupe: Option<&str>) {
    let _ = state.store.notify(
        &Actor::new("system:daemon"),
        "system:daemon",
        severity,
        title,
        body,
        None,
        dedupe,
    );
    crate::bus::touch(state, "notification.created");
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use hmac::{Hmac, Mac};

    fn mint(secret: &str, claims: &Value) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let payload = serde_json::to_vec(claims).unwrap();
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&payload);
        let sig = mac.finalize().into_bytes();
        format!("{}.{}", engine.encode(&payload), engine.encode(sig))
    }

    #[test]
    fn test_control_token_purpose_exp_jti_and_replay() {
        std::env::set_var("HEARTHLOOM_PROVISIONER_SECRET", "test-secret");
        let now = chrono::Utc::now().timestamp();
        let good = mint(
            "test-secret",
            &json!({"purpose": "control", "exp": now + 60, "jti": "j1", "sub": "provisioner"}),
        );
        assert_eq!(verify_control_token(&good).unwrap(), "provisioner");
        // replay of the same jti is refused
        assert!(verify_control_token(&good).is_err());
        // wrong purpose (a LOGIN token must never control the node)
        let login = mint(
            "test-secret",
            &json!({"purpose": "login", "exp": now + 60, "jti": "j2", "sub": "x"}),
        );
        assert!(verify_control_token(&login).is_err());
        // expired / over-long exp / bad signature
        let expired = mint("test-secret", &json!({"purpose": "control", "exp": now - 5, "jti": "j3"}));
        assert!(verify_control_token(&expired).is_err());
        let overlong = mint(
            "test-secret",
            &json!({"purpose": "control", "exp": now + 999_999, "jti": "j4"}),
        );
        assert!(verify_control_token(&overlong).is_err());
        let forged = mint("wrong-secret", &json!({"purpose": "control", "exp": now + 60, "jti": "j5"}));
        assert!(verify_control_token(&forged).is_err());
        std::env::remove_var("HEARTHLOOM_PROVISIONER_SECRET");
    }

    #[test]
    fn test_killswitch_allowlist_is_narrow() {
        use axum::http::Method;
        // the survivors
        for (m, p) in [
            (Method::GET, "/"),
            (Method::GET, "/health"),
            (Method::GET, "/assets/app.js"),
            (Method::POST, "/v1/auth/login"),
            (Method::GET, "/v1/auth/me"),
            (Method::GET, "/v1/notifications"),
            (Method::POST, "/v1/notifications/nf_1/read"),
            (Method::GET, "/v1/keys"),
            (Method::POST, "/v1/keys/k1/revoke"),
            (Method::POST, "/v1/provisioner/killswitch"),
            // the data-liberty guarantee: export (and the vault unlock
            // it requires) ALWAYS survives the killswitch
            (Method::POST, "/v1/export/start"),
            (Method::GET, "/v1/export/status"),
            (Method::GET, "/v1/export/download"),
            (Method::POST, "/v1/vault"),
        ] {
            assert!(killswitch_allows(p, &m), "{m} {p} should survive");
        }
        // the blocked: every data/write surface, INCLUDING key minting,
        // sync, search, MCP, bus
        for (m, p) in [
            (Method::GET, "/v1/search"),
            (Method::GET, "/v1/recall"),
            (Method::POST, "/v1/keys"),
            (Method::POST, "/v1/ingest"),
            (Method::GET, "/v1/sync/episodes"),
            (Method::POST, "/v1/sync/operational"),
            (Method::GET, "/v1/bus/stream"),
            (Method::POST, "/mcp"),
            (Method::GET, "/v1/tasks"),
            (Method::GET, "/brain/index"),
            (Method::POST, "/v1/notifications"),
            (Method::POST, "/v1/auth/bootstrap"),
        ] {
            assert!(!killswitch_allows(p, &m), "{m} {p} must be blocked");
        }
    }
}
