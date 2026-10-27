//! Daemon-side ember-registry client — the registry spec's §5 install
//! contract (resolve → fetch → sha256 → publisher Ed25519 → registry
//! counter-sign), shared by the dashboard gallery, remote CLI installs,
//! and (later) provisioner auto-provisioning. Staging lands in the same
//! .staging dir as a zip upload; the EXISTING /v1/plugins/install
//! completes it, so the human scope grant is unchanged.
//!
//! Cloud policy is enforced HERE, not in the UI: on a cloud node the
//! index proxy always asks `context=cloud`, and stage() refuses embers
//! that aren't cloud-eligible + counter-signed, and any manifest whose
//! placement is `local`.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::AppState;

/// The ember registry (and host-update channel) this node uses: the
/// `HEARTHLOOM_REGISTRY_URL` setting — in `~/.hearthloom/.env`, the
/// environment, or a hosted image's defaults. Unset means none: no gallery,
/// no update check. A personal self-hosted node talks to no outside service
/// its owner didn't configure (Hearthlaunch is one compatible registry).
pub fn configured_registry() -> Option<String> {
    std::env::var("HEARTHLOOM_REGISTRY_URL")
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap_or_default()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// (v1 base, countersign public key) for a registry root, via its
/// /.well-known/hearth-registry; degrades to `<root>/registry/v1`.
async fn discover(root: &str) -> (String, Option<String>) {
    let root = root.trim_end_matches('/');
    match http()
        .get(format!("{root}/.well-known/hearth-registry"))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => {
            let doc: Value = res.json().await.unwrap_or_default();
            (
                doc.get("base")
                    .and_then(|b| b.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| format!("{root}/registry/v1")),
                doc.pointer("/counterSign/publicKey")
                    .and_then(|k| k.as_str())
                    .filter(|k| !k.is_empty())
                    .map(String::from),
            )
        }
        _ => (format!("{root}/registry/v1"), None),
    }
}

fn is_cloud() -> bool {
    crate::monitor::node_kind() == "cloud"
}

/// Staging dirs THIS process verified (signature + countersignature on
/// a hosted node). A hosted node installs only from these — a marker
/// file inside the staged package would be forgeable by the zip
/// itself, an in-memory set is not. Lost on restart: re-stage.
fn verified_stages() -> &'static std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

pub fn is_verified_stage(path: &std::path::Path) -> bool {
    let Ok(canon) = path.canonicalize() else { return false };
    verified_stages().lock().unwrap().contains(&canon)
}

/// May a package that did NOT come through the registry install here?
/// Yes on a user's own machine; on a hosted node only when its operator
/// opted in (`HEARTHLOOM_ALLOW_UNREVIEWED_EMBERS=1` — a DIY cloud node
/// may run anything; a hearthlaunch tenant never sets it).
pub fn unreviewed_install_allowed() -> bool {
    !is_cloud()
        || std::env::var("HEARTHLOOM_ALLOW_UNREVIEWED_EMBERS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

/// Proxy the registry index for the dashboard gallery. Auto-appends
/// `context=cloud` on cloud nodes; 60s cache keyed by (root, q).
pub async fn index(root: Option<String>, q: &str) -> Result<Value> {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, (Instant, Value)>>> =
        OnceLock::new();
    let Some(root) = root.or_else(configured_registry) else {
        // No registry configured: an empty gallery, said plainly.
        return Ok(json!({"embers": [], "configured": false}));
    };
    let key = format!("{root}|{q}|{}", is_cloud());
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Some((at, value)) = cache.lock().unwrap().get(&key) {
        if at.elapsed() < Duration::from_secs(60) {
            return Ok(value.clone());
        }
    }
    let (base, _) = discover(&root).await;
    let mut url = format!("{base}/index?q={}", urlencode(q));
    if is_cloud() {
        url.push_str("&context=cloud");
    }
    let value: Value = http().get(url).send().await?.error_for_status()?.json().await?;
    let mut c = cache.lock().unwrap();
    if c.len() > 32 {
        c.clear();
    }
    c.insert(key, (Instant::now(), value.clone()));
    Ok(value)
}

pub struct Staged {
    pub path: std::path::PathBuf,
    pub name: String,
    pub version: String,
    pub scopes: Vec<String>,
    pub publisher: String,
    pub reviewed: bool,
    pub placement: String,
    pub requires_host: Option<String>,
    /// Public addresses the ember will serve (manifest `publish`).
    pub publishes: Vec<String>,
}

/// Download + verify + extract to .staging. The caller (dashboard/CLI)
/// shows the scopes and calls /v1/plugins/install to finish.
pub async fn stage(
    state: &AppState,
    root: Option<String>,
    name: &str,
    version: Option<String>,
) -> Result<Staged> {
    // Both end up in registry URLs: a name or version holding '/' or '..'
    // would address some other path on the registry host.
    if !crate::plugins::valid_ember_name(name) {
        bail!("{name:?} isn't an ember name (1-40 chars of [a-z0-9-])");
    }
    let root = root
        .or_else(configured_registry)
        .ok_or_else(|| anyhow!("no ember registry is configured (set HEARTHLOOM_REGISTRY_URL)"))?;
    let (base, org_key) = discover(&root).await;
    let http = http();
    let page: Value = http
        .get(format!("{base}/embers/{name}"))
        .send()
        .await
        .with_context(|| format!("couldn't reach the registry at {base}"))?
        .error_for_status()
        .map_err(|_| anyhow!("{name} isn't in this registry"))?
        .json()
        .await?;
    let cloud_eligible = page
        .pointer("/ember/cloudEligible")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_cloud() && !cloud_eligible {
        bail!("{name} isn't approved for hosted instances");
    }
    let version = match version {
        Some(v) => v,
        None => page
            .pointer("/ember/latestVersion")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("{name} has no released versions"))?
            .to_string(),
    };
    if !valid_version(&version) {
        bail!("{version:?} isn't a version");
    }
    let record: Value = http
        .get(format!("{base}/embers/{name}/{version}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let download = record
        .get("download")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("the version record carries no download URL"))?;
    let bytes = http.get(download).send().await?.error_for_status()?.bytes().await?;
    let digest = {
        use sha2::Digest;
        format!("{:x}", sha2::Sha256::digest(&bytes))
    };
    let want = record.get("artifactSha256").and_then(|v| v.as_str()).unwrap_or("");
    if digest != want {
        bail!("artifact hash mismatch (got {digest}, registry says {want}) — refusing");
    }

    let publisher = record
        .get("publisher")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sig = record.get("signature").cloned().unwrap_or_default();
    let key_id = sig.get("keyId").and_then(|v| v.as_str()).unwrap_or("");
    let pub_doc: Value = http
        .get(format!("{base}/publishers/{publisher}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let pubkey = pub_doc
        .get("keys")
        .and_then(|v| v.as_array())
        .and_then(|keys| {
            keys.iter()
                .find(|k| k.get("keyId").and_then(|v| v.as_str()) == Some(key_id))
        })
        .and_then(|k| k.get("publicKey"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("publisher {publisher} has no active key {key_id} — refusing"))?;
    let mut scopes: Vec<String> = record
        .get("scopes")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();
    scopes.sort();
    let payload = release_payload(
        name,
        &version,
        record.get("protocol").and_then(|v| v.as_i64()).unwrap_or(1),
        &scopes,
        &digest,
        &publisher,
    );
    verify_ed25519(pubkey, &payload, sig.get("sig").and_then(|v| v.as_str()).unwrap_or(""))
        .context("developer signature verification FAILED — refusing")?;

    let reviewed = match (record.get("countersign").filter(|c| !c.is_null()), &org_key) {
        (Some(cs), Some(org_key)) => {
            // The registry SIGNS approvedAt at RFC3339 second precision
            // but may SERVE it with fractional seconds (a database
            // round-trip) — try verbatim, then normalized (a registry's
            // serving-side echo bug).
            let served = cs.get("approvedAt").and_then(|v| v.as_str()).unwrap_or("");
            let operator = cs.get("operator").and_then(|v| v.as_str()).unwrap_or("");
            let sig = cs.get("sig").and_then(|v| v.as_str()).unwrap_or("");
            let normalized = chrono::DateTime::parse_from_rfc3339(served)
                .map(|t| t.with_timezone(&chrono::Utc).format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_else(|_| served.to_string());
            let ok = [served.to_string(), normalized]
                .iter()
                .any(|ts| verify_ed25519(org_key, &approval_payload(&digest, ts, operator), sig).is_ok());
            if !ok {
                bail!("registry counter-signature verification FAILED — refusing");
            }
            true
        }
        _ => false,
    };
    if is_cloud() && !reviewed {
        bail!("{name} {version} isn't counter-signed — hosted instances install only reviewed embers");
    }

    // extract + parse with the real loader; placement gate on cloud
    let st = state.clone();
    let bytes_vec = bytes.to_vec();
    let (path, manifest) = tokio::task::spawn_blocking(move || -> Result<_> {
        let dir = crate::plugins::stage_zip(&st, &bytes_vec)?;
        let manifest = crate::plugins::load_manifest(&dir)?;
        Ok((dir, manifest))
    })
    .await
    .map_err(anyhow::Error::from)??;
    if manifest.name != name {
        bail!("the package says it's {:?}, not {name:?} — refusing", manifest.name);
    }
    let placement = manifest.effective_placement().to_string();
    let requires_host = manifest.host_requirement();
    if is_cloud() && placement == "local" {
        bail!("{name} reads the user's own machine (placement: local) — it can't run on a hosted instance");
    }
    if let Ok(canon) = path.canonicalize() {
        verified_stages().lock().unwrap().insert(canon);
    }
    let publishes = manifest.publish.iter().map(|p| p.path.clone()).collect();
    Ok(Staged { path, name: name.into(), version, scopes, publisher, reviewed, placement, requires_host, publishes })
}

pub fn release_payload(
    name: &str,
    version: &str,
    protocol: i64,
    sorted_scopes: &[String],
    sha256_hex: &str,
    publisher: &str,
) -> String {
    format!(
        "hearth-ember-release.v1\nname={name}\nversion={version}\nprotocol={protocol}\nscopes={}\nsha256={sha256_hex}\npublisher={publisher}\n",
        sorted_scopes.join(","),
    )
}

pub fn approval_payload(sha256_hex: &str, approved_at: &str, operator: &str) -> String {
    format!("hearth-ember-approval.v1\nsha256={sha256_hex}\napprovedAt={approved_at}\noperator={operator}\n")
}

/// The `hearthloom` release block from the configured registry's
/// well-known doc (host self-update channel). Cached ~1h. None when no
/// registry is configured.
pub async fn host_release() -> Option<Value> {
    let registry = configured_registry()?;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    type Cached = Option<(Instant, Option<Value>)>;
    static CACHE: OnceLock<Mutex<Cached>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some((at, v)) = cache.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(3600) {
            return v.clone();
        }
    }
    let block = match http()
        .get(format!("{registry}/.well-known/hearth-registry"))
        .send()
        .await
    {
        Ok(res) if res.status().is_success() => res
            .json::<Value>()
            .await
            .ok()
            .and_then(|d| d.get("hearthloom").cloned())
            .filter(|v| !v.is_null()),
        _ => None,
    };
    *cache.lock().unwrap() = Some((Instant::now(), block.clone()));
    block
}

/// Version status for the dashboard/CLI: current vs latest, whether a
/// self-update is possible on this platform, and the download for it.
pub async fn version_info() -> Value {
    let current = env!("CARGO_PKG_VERSION");
    let block = host_release().await;
    let latest = block
        .as_ref()
        .and_then(|b| b.get("latest"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let update = latest
        .as_deref()
        .map(|l| crate::plugins::version_satisfies(l, &format!(">{current}")))
        .unwrap_or(false);
    let target = crate::plugins::host_target();
    let download = block
        .as_ref()
        .and_then(|b| b.pointer(&format!("/downloads/{target}")))
        .cloned();
    let can_self_update =
        update && !is_cloud() && target != "unknown" && download.is_some();
    json!({
        "current": current,
        "latest": latest,
        "update_available": update,
        "min_supported": block.as_ref().and_then(|b| b.get("minSupported")).cloned(),
        "notes_url": block.as_ref().and_then(|b| b.get("notesUrl")).cloned(),
        "target": target,
        "can_self_update": can_self_update,
        // where updates come from (None = no channel configured), and
        // whether this node is a hosted instance (the dashboard words its
        // upgrade advice from this, never from the hostname)
        "channel": configured_registry(),
        "hosted": is_cloud(),
        // self-update verifies a release signature only with a pinned key
        "signature_pinned": update_signing_key().is_some(),
        "download": download,
    })
}

/// The release-signing public key (base64 Ed25519) a self-update must
/// verify, from `HEARTHLOOM_UPDATE_SIGNING_KEY`. Unset: hash check only.
pub fn update_signing_key() -> Option<String> {
    std::env::var("HEARTHLOOM_UPDATE_SIGNING_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// What a host release signature covers.
pub fn host_release_payload(version: &str, target: &str, sha256_hex: &str) -> String {
    format!("hearthloom-release.v1\nversion={version}\ntarget={target}\nsha256={sha256_hex}\n")
}

pub fn verify_ed25519(pubkey_b64: &str, payload: &str, sig_b64: &str) -> Result<()> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let engine = base64::engine::general_purpose::STANDARD;
    let key_bytes: [u8; 32] = engine
        .decode(pubkey_b64)
        .context("bad public key encoding")?
        .try_into()
        .map_err(|_| anyhow!("public key isn't 32 bytes"))?;
    let sig_bytes: [u8; 64] = engine
        .decode(sig_b64)
        .context("bad signature encoding")?
        .try_into()
        .map_err(|_| anyhow!("signature isn't 64 bytes"))?;
    VerifyingKey::from_bytes(&key_bytes)?
        .verify(payload.as_bytes(), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| anyhow!("signature does not match the payload"))
}

/// A release version as the registry names it (e.g. `0.7.1`,
/// `1.0.0-rc.1+build.5`): 1-64 chars of [0-9A-Za-z.+-], no "..".
fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && !version.contains("..")
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
}

#[cfg(test)]
mod tests {
    use super::valid_version;
    use crate::plugins::valid_ember_name;

    #[test]
    fn test_only_plain_names_and_versions_reach_a_registry_url() {
        for ok in ["0.7.1", "1.0.0-rc.1+build.5", "2"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in ["", "../x", "0.1/../../admin", "1.0 ", "a?b", "1..2"] {
            assert!(!valid_version(bad), "{bad:?}");
        }
        assert!(valid_ember_name("hpresence") && valid_ember_name("h-tesla2"));
        for bad in ["", "-x", "../x", "a/b", "Caps", "x?y=1"] {
            assert!(!valid_ember_name(bad), "{bad:?}");
        }
    }
}
