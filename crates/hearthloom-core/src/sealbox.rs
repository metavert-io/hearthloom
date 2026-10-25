//! Anonymous sealed boxes — the Phase-5 key-sync primitive.
//!
//! A libsodium `crypto_box_seal`-style construction: encrypt a small secret
//! (the LLM API key) to a recipient's X25519 PUBLIC key so that ONLY the
//! holder of the matching private key can open it. The sender needs no shared
//! secret and stays anonymous; the ciphertext is opaque to anyone on the wire
//! — including a peer-scoped credential that isn't the intended recipient.
//! This lets the operational mirror carry the LLM key end-to-end opaque over
//! the (already TLS) peer channel, so a user keys ONE node by hand and its
//! counterpart adopts the key without it ever traversing in the clear.
//!
//! Construction (both ends under our control, so we fix the details):
//!   seal:  eph = random X25519 keypair
//!          shared = X25519(eph_secret, recipient_public)
//!          key    = SHA-256(shared ‖ eph_public ‖ recipient_public)
//!          nonce  = SHA-256("hearthloom-sealbox" ‖ eph_public ‖ recipient_public)[..24]
//!          out    = eph_public(32) ‖ XChaCha20Poly1305(key, nonce, plaintext)
//! A per-message ephemeral key makes the (deterministic) nonce safe: the
//! symmetric key never repeats, so neither does the (key, nonce) pair.

use anyhow::{anyhow, bail, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

const NONCE_DOMAIN: &[u8] = b"hearthloom-sealbox";

/// 32 random bytes for a node's long-lived X25519 secret. Store this sealed
/// (never synced); publish only its `public_from_secret`.
pub fn generate_secret() -> [u8; 32] {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut raw = [0u8; 32];
    chacha20poly1305::aead::OsRng.fill_bytes(&mut raw);
    // X25519 clamps internally on use; storing the raw seed is fine.
    raw
}

/// The public key to publish for a stored secret.
pub fn public_from_secret(secret: &[u8; 32]) -> [u8; 32] {
    let s = StaticSecret::from(*secret);
    PublicKey::from(&s).to_bytes()
}

fn kdf(shared: &[u8; 32], eph_pub: &[u8; 32], recipient_pub: &[u8; 32]) -> Key {
    let mut h = Sha256::new();
    h.update(shared);
    h.update(eph_pub);
    h.update(recipient_pub);
    *Key::from_slice(&h.finalize())
}

fn nonce(eph_pub: &[u8; 32], recipient_pub: &[u8; 32]) -> XNonce {
    let mut h = Sha256::new();
    h.update(NONCE_DOMAIN);
    h.update(eph_pub);
    h.update(recipient_pub);
    let full = h.finalize();
    *XNonce::from_slice(&full[..24])
}

/// Seal `plaintext` to `recipient_public`. Output is `eph_public(32) ‖ ct`.
pub fn seal(recipient_public: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    let eph = StaticSecret::from(generate_secret());
    let eph_pub = PublicKey::from(&eph).to_bytes();
    let recipient = PublicKey::from(*recipient_public);
    let shared = eph.diffie_hellman(&recipient).to_bytes();
    let cipher = XChaCha20Poly1305::new(&kdf(&shared, &eph_pub, recipient_public));
    let ct = cipher
        .encrypt(&nonce(&eph_pub, recipient_public), plaintext)
        .map_err(|_| anyhow!("sealbox encrypt failed"))?;
    let mut out = Vec::with_capacity(32 + ct.len());
    out.extend_from_slice(&eph_pub);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Open a sealed box with the recipient's secret. Fails (rather than
/// returning garbage) if this isn't the intended recipient or the box is
/// tampered/truncated.
pub fn open(recipient_secret: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < 32 + 16 {
        bail!("sealed box truncated");
    }
    let mut eph_pub = [0u8; 32];
    eph_pub.copy_from_slice(&sealed[..32]);
    let secret = StaticSecret::from(*recipient_secret);
    let recipient_pub = PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&PublicKey::from(eph_pub)).to_bytes();
    let cipher = XChaCha20Poly1305::new(&kdf(&shared, &eph_pub, &recipient_pub));
    cipher
        .decrypt(&nonce(&eph_pub, &recipient_pub), &sealed[32..])
        .map_err(|_| anyhow!("sealbox open failed — not the recipient, or tampered"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let secret = generate_secret();
        let public = public_from_secret(&secret);
        let msg = b"sk-ant-a-real-looking-key-0123456789";
        let sealed = seal(&public, msg).unwrap();
        assert_ne!(&sealed[32..], &msg[..], "ciphertext must not equal plaintext");
        assert_eq!(open(&secret, &sealed).unwrap(), msg);
    }

    #[test]
    fn wrong_recipient_cannot_open() {
        let alice = generate_secret();
        let bob = generate_secret();
        let sealed = seal(&public_from_secret(&alice), b"secret").unwrap();
        // Bob (a different keypair) can't open a box sealed to Alice.
        assert!(open(&bob, &sealed).is_err());
        // Alice can.
        assert_eq!(open(&alice, &sealed).unwrap(), b"secret");
    }

    #[test]
    fn tamper_is_detected() {
        let secret = generate_secret();
        let mut sealed = seal(&public_from_secret(&secret), b"secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(open(&secret, &sealed).is_err());
    }

    #[test]
    fn each_seal_is_fresh() {
        // Ephemeral key per call → different ciphertext each time (no nonce reuse).
        let public = public_from_secret(&generate_secret());
        let a = seal(&public, b"same").unwrap();
        let b = seal(&public, b"same").unwrap();
        assert_ne!(a, b);
    }
}
