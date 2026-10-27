//! Phase-5 key sync glue: this node's X25519 sealbox keypair and the
//! seal/open helpers used by the operational mirror.
//!
//! The SECRET lives only in `~/.hearthloom/.env` (sealed with the rest of the
//! secrets, never synced); the PUBLIC key is published in consolidation_state
//! `sealbox.public` so a paired peer can fetch it and seal this node's LLM key
//! to us. See `hearthloom_core::sealbox`.

use anyhow::{anyhow, Result};
use base64::Engine;
use hearthloom_core::store::Store;
use hearthloom_core::{sealbox, secrets, Home};

const SECRET_ENV: &str = "HEARTHLOOM_SEALBOX_SECRET";
/// consolidation_state key holding this node's published public key (b64).
pub const PUBLIC_KEY: &str = "sealbox.public";

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn decode32(s: &str) -> Option<[u8; 32]> {
    let bytes = b64().decode(s.trim()).ok()?;
    (bytes.len() == 32).then(|| {
        let mut a = [0u8; 32];
        a.copy_from_slice(&bytes);
        a
    })
}

/// This node's secret, if one has been generated. Read from the process env
/// (populated from the sealed .env at boot / at generation).
pub fn local_secret() -> Option<[u8; 32]> {
    std::env::var(SECRET_ENV).ok().as_deref().and_then(decode32)
}

/// Ensure a keypair exists and return this node's PUBLIC key bytes. Generates
/// one on first call (persisting the secret to the sealed .env and publishing
/// the public key), then is idempotent. Cheap enough to call per request.
pub fn ensure_keypair(home: &Home, store: &Store) -> Result<[u8; 32]> {
    if let Some(secret) = local_secret() {
        let public = sealbox::public_from_secret(&secret);
        // Republish if the stored public drifted or is absent (e.g. a fresh DB).
        let want = b64().encode(public);
        if store.consolidation_state(PUBLIC_KEY)?.as_deref() != Some(want.as_str()) {
            store.set_consolidation_state(PUBLIC_KEY, &want)?;
        }
        return Ok(public);
    }
    // First run: mint, persist the secret (sealed), publish the public key.
    let secret = sealbox::generate_secret();
    let secret_b64 = b64().encode(secret);
    secrets::set_env_vars(home.root(), &[(SECRET_ENV, &secret_b64)])?;
    std::env::set_var(SECRET_ENV, &secret_b64);
    let public = sealbox::public_from_secret(&secret);
    store.set_consolidation_state(PUBLIC_KEY, &b64().encode(public))?;
    Ok(public)
}

/// Seal `plaintext` to a base64 X25519 public key, returning base64 ciphertext.
pub fn seal_to(recipient_public_b64: &str, plaintext: &[u8]) -> Result<String> {
    let public =
        decode32(recipient_public_b64).ok_or_else(|| anyhow!("bad recipient public key"))?;
    Ok(b64().encode(sealbox::seal(&public, plaintext)?))
}

/// Open a base64 sealed box with this node's secret.
pub fn open_local(sealed_b64: &str) -> Result<Vec<u8>> {
    let secret = local_secret().ok_or_else(|| anyhow!("no local sealbox secret"))?;
    let sealed = b64()
        .decode(sealed_b64.trim())
        .map_err(|_| anyhow!("sealed key is not base64"))?;
    sealbox::open(&secret, &sealed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_roundtrips() {
        // The b64 seal_to → raw sealbox::open path a receiver walks, minus
        // the env/Store lookups (covered by the crypto tests in sealbox).
        let secret = sealbox::generate_secret();
        let public_b64 = b64().encode(sealbox::public_from_secret(&secret));
        let sealed_b64 = seal_to(&public_b64, b"sk-ant-secret").unwrap();
        let opened = sealbox::open(&secret, &b64().decode(sealed_b64).unwrap()).unwrap();
        assert_eq!(opened, b"sk-ant-secret");
    }

    #[test]
    fn bad_public_key_rejected() {
        assert!(seal_to("not-base64!!", b"x").is_err());
        assert!(seal_to(&b64().encode([0u8; 16]), b"x").is_err()); // wrong length
    }
}
