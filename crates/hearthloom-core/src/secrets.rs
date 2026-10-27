//! The sealed-secrets regime (I of the 2026-07-12 batch): no plaintext
//! secrets on the data disk.
//!
//! - The MASTER KEY lives OUTSIDE the data directory: the
//!   `HEARTHLOOM_MASTER_KEY` env var (cloud nodes — a Fly secret), or
//!   the macOS login keychain (`security` CLI, service
//!   `hearthloom-master-key`). Key material of any shape is accepted
//!   and stretched to 32 bytes via SHA-256.
//! - Secrets are SEALED: `~/.hearthloom/.env` is replaced by
//!   `.env.sealed` (ChaCha20-Poly1305); the daemon decrypts in memory
//!   at boot and never writes plaintext back. Migration is automatic:
//!   plaintext .env + available master key → seal, then shred the
//!   plaintext (best-effort overwrite before unlink).
//! - With NO master key the plaintext .env still works (dev mode) —
//!   the status surfaces on Monitoring as a warning, not a failure.
//!
//! Honest scope (from the approved plan): at-rest sealing does not
//! stop a tricked RUNNING daemon — it holds the key. That threat is
//! what the RLM redaction guard below is for.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SEAL_MAGIC: &[u8] = b"HLSEAL1\n";
const KEYCHAIN_SERVICE: &str = "hearthloom-master-key";
const KEYCHAIN_ACCOUNT: &str = "hearthloom";

/// How secrets were loaded this boot — the Monitoring surface.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SecretsStatus {
    /// env | keychain | none — where the master key came from.
    pub master_key: String,
    /// A .env.sealed was decrypted in memory.
    pub sealed: bool,
    /// A PLAINTEXT .env was used (dev mode — warn on Monitoring).
    pub plaintext_fallback: bool,
    /// This boot migrated a plaintext .env into the sealed file.
    pub migrated: bool,
}

static STATUS: OnceLock<SecretsStatus> = OnceLock::new();

/// The status recorded by the last `load_sealed_env` call (defaults
/// when secrets loading never ran, e.g. in tests).
pub fn status() -> SecretsStatus {
    STATUS.get().cloned().unwrap_or_default()
}

fn derive_key(material: &str) -> Key {
    let mut hasher = Sha256::new();
    hasher.update(b"hearthloom-master-key:");
    hasher.update(material.trim().as_bytes());
    Key::clone_from_slice(&hasher.finalize())
}

/// Resolve the master key: env first (cloud posture), then the macOS
/// login keychain. Returns (key, source).
fn master_key() -> Option<(Key, &'static str)> {
    if let Ok(material) = std::env::var("HEARTHLOOM_MASTER_KEY") {
        if !material.trim().is_empty() {
            return Some((derive_key(&material), "env"));
        }
    }
    if cfg!(target_os = "macos") {
        let out = Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
            .output()
            .ok()?;
        if out.status.success() {
            let material = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !material.is_empty() {
                return Some((derive_key(&material), "keychain"));
            }
        }
    }
    None
}

/// Create a fresh random master key in the macOS login keychain (only
/// called when there are plaintext secrets to seal and no key exists —
/// the door locks itself the first time it can). ACL restricted to the
/// security tool our own reads go through.
fn create_keychain_key() -> Result<(Key, &'static str)> {
    let mut raw = [0u8; 32];
    use chacha20poly1305::aead::rand_core::RngCore;
    OsRng.fill_bytes(&mut raw);
    let material: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let out = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
            &material,
            "-T",
            "/usr/bin/security",
        ])
        .output()
        .context("running security add-generic-password")?;
    if !out.status.success() {
        bail!(
            "keychain key creation failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok((derive_key(&material), "keychain"))
}

/// Seal plaintext into the HLSEAL1 container: magic + nonce + box.
pub fn seal(key: &Key, plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(key);
    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
    let boxed = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| anyhow!("sealing failed"))?;
    let mut out = Vec::with_capacity(SEAL_MAGIC.len() + 12 + boxed.len());
    out.extend_from_slice(SEAL_MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&boxed);
    Ok(out)
}

/// Open an HLSEAL1 container.
pub fn unseal(key: &Key, sealed: &[u8]) -> Result<Vec<u8>> {
    let body = sealed
        .strip_prefix(SEAL_MAGIC)
        .ok_or_else(|| anyhow!("not a sealed-secrets file"))?;
    if body.len() < 12 {
        bail!("sealed file truncated");
    }
    let (nonce, boxed) = body.split_at(12);
    ChaCha20Poly1305::new(key)
        .decrypt(Nonce::from_slice(nonce), boxed)
        .map_err(|_| anyhow!("unseal failed — wrong master key?"))
}

/* ————— node-sealed small secrets (ember secrets, 2026-10-23) —————
Sealed with the master key when this node has one. A node without
one (a hosted node whose operator hasn't provisioned
HEARTHLOOM_MASTER_KEY) seals with a per-node key file at the home
root instead — never exported, 0600 — and says so (`node_seal_source`)
so the UI can warn. That is weaker than a key held off the data
disk, and is labeled as such rather than pretending. */

const SEAL_MAGIC_NODE: &[u8] = b"HLSEALN\n";
pub const NODE_SEAL_KEY_FILE: &str = ".ember-secrets.key";

fn node_file_key(home_root: &Path, create: bool) -> Result<Key> {
    let path = home_root.join(NODE_SEAL_KEY_FILE);
    if let Ok(material) = std::fs::read_to_string(&path) {
        if !material.trim().is_empty() {
            return Ok(derive_key(&material));
        }
    }
    if !create {
        bail!("this node's seal key is missing ({})", path.display());
    }
    let mut raw = [0u8; 32];
    use chacha20poly1305::aead::rand_core::RngCore;
    OsRng.fill_bytes(&mut raw);
    let material: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(home_root)?;
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        use std::io::Write;
        match opts.open(&path) {
            Ok(mut f) => f.write_all(material.as_bytes())?,
            // lost a race with another writer: use theirs
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return node_file_key(home_root, false)
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(derive_key(&material))
}

/// Which key seals node secrets here: "master-key" or "node-key".
pub fn node_seal_source() -> &'static str {
    if master_key().is_some() {
        "master-key"
    } else {
        "node-key"
    }
}

/// Seal a small secret for THIS node (master key, else the node key).
pub fn seal_node(home_root: &Path, plaintext: &[u8]) -> Result<Vec<u8>> {
    seal_node_with(home_root, plaintext, master_key().map(|(k, _)| k))
}

fn seal_node_with(home_root: &Path, plaintext: &[u8], master: Option<Key>) -> Result<Vec<u8>> {
    if let Some(key) = master {
        return seal(&key, plaintext);
    }
    let key = node_file_key(home_root, true)?;
    let mut out = seal(&key, plaintext)?;
    out.splice(..SEAL_MAGIC.len(), SEAL_MAGIC_NODE.iter().copied());
    Ok(out)
}

/// Open a secret sealed by `seal_node` (either key, by its magic).
pub fn unseal_node(home_root: &Path, sealed: &[u8]) -> Result<Vec<u8>> {
    if let Some(body) = sealed.strip_prefix(SEAL_MAGIC_NODE) {
        let key = node_file_key(home_root, false)?;
        let mut container = SEAL_MAGIC.to_vec();
        container.extend_from_slice(body);
        return unseal(&key, &container);
    }
    let (key, _) = master_key()
        .ok_or_else(|| anyhow!("sealed with the master key, which this node doesn't have"))?;
    unseal(&key, sealed)
}

/// Best-effort shred: overwrite with zeros, sync, unlink. (APFS may
/// still keep copy-on-write blocks; the point is that casual copies of
/// the data directory stop carrying the plaintext.)
fn shred(path: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path) {
        let zeros = vec![0u8; meta.len() as usize];
        let _ = std::fs::write(path, &zeros);
    }
    std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))
}

/// The boot path, called from Config::load with the home root
/// Upsert environment variables into the on-disk secrets store, honoring
/// the sealed regime: decrypt current content → replace the given keys →
/// re-seal (or plaintext fallback when no master key). Also updates the
/// live process env. This is the ONLY correct way to persist a secret now
/// that .env is sealed — a naive `.env` rewrite would clobber the sealed
/// file and lose every other secret on the next boot.
pub fn set_env_vars(home_root: &Path, updates: &[(&str, &str)]) -> Result<()> {
    let plain_path = home_root.join(".env");
    let sealed_path = home_root.join(".env.sealed");
    let key = master_key();
    // Current content: prefer the sealed file when we can open it.
    let current = if let Some((ref k, _)) = key {
        if sealed_path.exists() {
            String::from_utf8(unseal(k, &std::fs::read(&sealed_path)?)?)?
        } else if plain_path.exists() {
            std::fs::read_to_string(&plain_path)?
        } else {
            String::new()
        }
    } else if plain_path.exists() {
        std::fs::read_to_string(&plain_path)?
    } else {
        String::new()
    };
    let is_update = |line: &str| {
        updates
            .iter()
            .any(|(k, _)| line.split('=').next().map(str::trim) == Some(*k))
    };
    let mut lines: Vec<String> = current
        .lines()
        .filter(|l| !is_update(l))
        .map(String::from)
        .collect();
    for (k, v) in updates {
        lines.push(format!("{k}={v}"));
        std::env::set_var(k, v);
    }
    let body = lines.join("\n") + "\n";
    if let Some((ref k, _)) = key {
        std::fs::write(&sealed_path, seal(k, body.as_bytes())?)?;
        if plain_path.exists() {
            let _ = shred(&plain_path);
        }
    } else {
        std::fs::write(&plain_path, &body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&plain_path, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(())
}

/// `existing` with every `KEY=value` line of `update` applied: keys in
/// `update` replace their old line, new keys are appended, everything else
/// (other keys, comments) is kept in order.
pub fn merge_env(existing: &str, update: &str) -> String {
    let key_of = |line: &str| -> Option<String> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        line.split_once('=')
            .map(|(k, _)| k.trim().trim_start_matches("export ").to_string())
    };
    let updated: Vec<String> = update.lines().filter_map(key_of).collect();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| key_of(l).is_none_or(|k| !updated.contains(&k)))
        .map(String::from)
        .collect();
    lines.extend(
        update
            .lines()
            .filter(|l| key_of(l).is_some())
            .map(String::from),
    );
    lines.join("\n") + "\n"
}

/// (~/.hearthloom). Returns the .env content to apply, if any:
/// 1. master key + plaintext .env → SEAL it, shred the plaintext,
///    return the content (migration boot).
/// 2. master key + .env.sealed → decrypt in memory.
/// 3. no master key, plaintext .env on macOS → create a keychain key
///    and go to (1) — the first boot that CAN lock the door, does.
/// 4. no master key → plaintext fallback (dev mode, warned).
pub fn load_sealed_env(home_root: &Path) -> Option<String> {
    let plain_path = home_root.join(".env");
    let sealed_path = home_root.join(".env.sealed");
    let mut status = SecretsStatus::default();

    let mut key = master_key();
    if key.is_none() && cfg!(target_os = "macos") && plain_path.exists() {
        match create_keychain_key() {
            Ok(k) => {
                tracing::info!("secrets: created a master key in the login keychain");
                key = Some(k);
            }
            Err(err) => tracing::warn!("secrets: keychain key creation failed: {err:#}"),
        }
    }

    let content = match key {
        Some((key, source)) => {
            status.master_key = source.into();
            if plain_path.exists() {
                // Seal, then shred the plaintext. When a sealed file
                // already exists the plaintext is MERGED into it (its lines
                // win per key): writing a fresh `.env` with one new setting
                // must add that setting, never replace every sealed secret.
                match std::fs::read_to_string(&plain_path)
                    .map_err(anyhow::Error::from)
                    .and_then(|plain| {
                        let content = if sealed_path.exists() {
                            let existing =
                                String::from_utf8(unseal(&key, &std::fs::read(&sealed_path)?)?)?;
                            merge_env(&existing, &plain)
                        } else {
                            plain
                        };
                        let sealed = seal(&key, content.as_bytes())?;
                        std::fs::write(&sealed_path, &sealed)?;
                        // Paranoia before the irreversible step: the
                        // file we just wrote must decrypt back to the
                        // exact plaintext, or the plaintext stays.
                        let readback = std::fs::read(&sealed_path)?;
                        if unseal(&key, &readback)? != content.as_bytes() {
                            bail!("seal verification failed — keeping the plaintext");
                        }
                        shred(&plain_path)?;
                        Ok(content)
                    }) {
                    Ok(content) => {
                        tracing::info!(
                            "secrets: sealed {} → .env.sealed and shredded the plaintext",
                            plain_path.display()
                        );
                        status.sealed = true;
                        status.migrated = true;
                        Some(content)
                    }
                    Err(err) => {
                        tracing::warn!("secrets: sealing failed, using plaintext: {err:#}");
                        status.plaintext_fallback = true;
                        std::fs::read_to_string(&plain_path).ok()
                    }
                }
            } else if sealed_path.exists() {
                match std::fs::read(&sealed_path)
                    .map_err(anyhow::Error::from)
                    .and_then(|sealed| unseal(&key, &sealed))
                {
                    Ok(bytes) => {
                        status.sealed = true;
                        Some(String::from_utf8_lossy(&bytes).into_owned())
                    }
                    Err(err) => {
                        tracing::warn!("secrets: {err:#}");
                        None
                    }
                }
            } else {
                None
            }
        }
        None => {
            if plain_path.exists() {
                status.plaintext_fallback = true;
                status.master_key = "none".into();
                std::fs::read_to_string(&plain_path).ok()
            } else {
                if sealed_path.exists() {
                    tracing::warn!(
                        "secrets: .env.sealed present but no master key available \
                         (HEARTHLOOM_MASTER_KEY or the login keychain)"
                    );
                }
                status.master_key = "none".into();
                None
            }
        }
    };
    let _ = STATUS.set(status);
    content
}

/// Redact secret-shaped tokens (Anthropic keys, Hearthloom PATs,
/// provisioner tokens) from text headed anywhere untrusted — every RLM
/// tool result passes through here before the model sees it. A tricked
/// running daemon must not be able to quote its own credentials.
pub fn redact_secrets(text: &str) -> String {
    const PREFIXES: [&str; 3] = ["sk-ant-", "hl_", "hs_"];
    let tail = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        for prefix in PREFIXES {
            if let Some(stripped) = rest.strip_prefix(prefix) {
                // A real credential has a long random tail; short
                // matches ("hl_x") are prose, not secrets.
                let run = stripped.chars().take_while(|&c| tail(c)).count();
                if run >= 12 {
                    out.push_str("[redacted-secret]");
                    rest = &stripped[stripped
                        .char_indices()
                        .nth(run)
                        .map(|(i, _)| i)
                        .unwrap_or(stripped.len())..];
                    continue 'outer;
                }
            }
        }
        let mut chars = rest.chars();
        out.push(chars.next().unwrap());
        rest = chars.as_str();
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_a_new_plaintext_env_merges_into_the_sealed_one() {
        let existing = "ANTHROPIC_API_KEY=sk-old\n# a comment\nGMAIL_IMAP_USER=me@example.com\n";
        let update = "HEARTHLOOM_HOST=0.0.0.0\nANTHROPIC_API_KEY=sk-new\n";
        let merged = merge_env(existing, update);
        assert!(
            merged.contains("GMAIL_IMAP_USER=me@example.com"),
            "{merged}"
        );
        assert!(merged.contains("# a comment"));
        assert!(
            merged.contains("ANTHROPIC_API_KEY=sk-new") && !merged.contains("sk-old"),
            "{merged}"
        );
        assert!(merged.contains("HEARTHLOOM_HOST=0.0.0.0"));
    }

    use super::*;

    #[test]
    fn test_seal_roundtrip_and_wrong_key() {
        let key = derive_key("some master material");
        let sealed = seal(&key, b"ANTHROPIC_API_KEY=sk-ant-abc123\n").unwrap();
        assert!(sealed.starts_with(SEAL_MAGIC));
        assert_eq!(
            unseal(&key, &sealed).unwrap(),
            b"ANTHROPIC_API_KEY=sk-ant-abc123\n"
        );
        let wrong = derive_key("other material");
        assert!(unseal(&wrong, &sealed).is_err());
        assert!(unseal(&key, b"not sealed at all").is_err());
    }

    #[test]
    fn test_redact_secrets() {
        let text = "key sk-ant-api03-AbCdEf123456789012345 and pat hl_0123456789abcdef99 \
                    and hs_zzzzyyyyxxxxwwww11 but hl_ok stays, so does plain prose";
        let red = redact_secrets(text);
        assert!(!red.contains("sk-ant-api03"));
        assert!(!red.contains("hl_0123456789abcdef99"));
        assert!(!red.contains("hs_zzzz"));
        assert_eq!(red.matches("[redacted-secret]").count(), 3);
        assert!(red.contains("hl_ok stays"));
        assert_eq!(redact_secrets("no secrets here"), "no secrets here");
    }

    #[test]
    fn test_node_seal_roundtrip_both_keys() {
        let home = std::env::temp_dir().join(format!("hl-seal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // node-key path (no master key)
        let sealed = seal_node_with(&home, b"ta-secret.abc", None).unwrap();
        assert!(sealed.starts_with(SEAL_MAGIC_NODE));
        assert!(
            !sealed.windows(13).any(|w| w == b"ta-secret.abc"),
            "no plaintext at rest"
        );
        assert_eq!(unseal_node(&home, &sealed).unwrap(), b"ta-secret.abc");
        let key_file = home.join(NODE_SEAL_KEY_FILE);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // a second seal reuses the same key
        let again = seal_node_with(&home, b"x", None).unwrap();
        assert_eq!(unseal_node(&home, &again).unwrap(), b"x");
        // master-key path carries the ordinary magic
        let master = derive_key("test-master");
        let m = seal_node_with(&home, b"y", Some(master)).unwrap();
        assert!(m.starts_with(SEAL_MAGIC));
        assert_eq!(unseal(&master, &m).unwrap(), b"y");
        // losing the node key loses node-sealed secrets (by design)
        std::fs::remove_file(&key_file).unwrap();
        assert!(unseal_node(&home, &sealed).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }
}
