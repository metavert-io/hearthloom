//! The login layer for remote postures (deployment-modes doctrine,
//! 2026-07-11). One admin user for now; argon2id password hashing;
//! DB-backed sessions whose tokens are stored hashed (the api_keys
//! discipline — plaintext exists only in the Set-Cookie).
//!
//! THE POSTURE RULE lives here: `auth_required()` is simply "does a
//! user row exist". Bootstrap creates the admin and the door locks in
//! the same gesture; there is no separate flag to misconfigure. (The
//! HEARTHLOOM_AUTH_OVERRIDE=open env escape hatch — recovery when a
//! password is lost — is honored in the daemon middleware, not here.)

use anyhow::{anyhow, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// Session lifetime: 30 days. Long-lived by design — the threat model
/// is other processes/devices, not a shared terminal.
pub const SESSION_DAYS: i64 = 30;

fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn expiry_from_now() -> String {
    (chrono::Utc::now() + chrono::Duration::days(SESSION_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

impl Store {
    /// Does any user exist? True = every request must authenticate.
    pub fn auth_required(&self) -> Result<bool> {
        let conn = self.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(n > 0)
    }

    /// Create the admin. Refuses when a user already exists — bootstrap
    /// is a first-run act; credential changes are a different, already-
    /// authenticated door (not built yet). Identity is an EMAIL — the
    /// natural key a provisioner's trusted-issuer logins also use.
    pub fn create_admin(&self, actor: &Actor, email: &str, password: &str) -> Result<()> {
        let email = email.trim().to_lowercase();
        let looks_like_email = email
            .split_once('@')
            .is_some_and(|(l, r)| !l.is_empty() && r.contains('.'));
        if !looks_like_email {
            return Err(anyhow!("that doesn't look like an email address"));
        }
        if password.len() < 8 {
            return Err(anyhow!("password must be at least 8 characters"));
        }
        let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow!("hashing failed: {e}"))?
            .to_string();
        let conn = self.lock();
        let existing: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        if existing > 0 {
            return Err(anyhow!("an admin already exists — bootstrap runs once"));
        }
        conn.execute(
            "INSERT INTO users (id, email, password_hash, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![
                format!("us_{}", uuid::Uuid::new_v4().simple()),
                email,
                hash,
                now_utc(),
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "admin_created",
            &format!("admin_created:{email}"),
            &serde_json::json!({"email": email}),
        )?;
        Ok(())
    }

    /// Verify credentials and mint a session. Returns the PLAINTEXT
    /// session token (for the cookie) — only its hash is stored.
    pub fn login(&self, email: &str, password: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let stored: Option<(String, String)> = conn
            .query_row(
                "SELECT id, password_hash FROM users WHERE email = ?1",
                [email.trim().to_lowercase()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((user_id, stored_hash)) = stored else {
            // Burn comparable time so an unknown email isn't a
            // timing oracle for enumeration.
            let _ = Argon2::default().verify_password(
                password.as_bytes(),
                &PasswordHash::new(
                    "$argon2id$v=19$m=19456,t=2,p=1$YWFhYWFhYWFhYWFhYWFhYQ$\
                     m0iFuX2VvNhP4a2WwPnJvfQtQ0hZQm3cJ4y3wV0d0dA",
                )
                .unwrap(),
            );
            return Ok(None);
        };
        let parsed =
            PasswordHash::new(&stored_hash).map_err(|e| anyhow!("corrupt password hash: {e}"))?;
        if Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_err()
        {
            return Ok(None);
        }
        let token = format!(
            "hs_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![hash_token(&token), user_id, now_utc(), expiry_from_now()],
        )?;
        Ok(Some(token))
    }

    /// Mint a session directly for a trusted-issuer subject (the
    /// the provisioner hook) — the caller has already verified the token
    /// signature. The subject must match the admin's EMAIL.
    pub fn login_as_issuer_subject(&self, subject: &str) -> Result<Option<String>> {
        let conn = self.lock();
        let user_id: Option<String> = conn
            .query_row(
                "SELECT id FROM users WHERE email = ?1",
                [subject.trim().to_lowercase()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(user_id) = user_id else {
            return Ok(None);
        };
        let token = format!(
            "hs_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![hash_token(&token), user_id, now_utc(), expiry_from_now()],
        )?;
        Ok(Some(token))
    }

    /// Mint a DB-backed session for the node's (single) admin user —
    /// the POST /v1/auth/session-from-key path, so a device holding
    /// only a scoped PAT (the iOS shell's Keychain credential) can
    /// (re)open the authenticated dashboard in a webview without
    /// re-prompting for a password. None when no user exists (open
    /// posture — no session needed).
    pub fn create_session_for_admin(&self) -> Result<Option<String>> {
        let conn = self.lock();
        let user_id: Option<String> = conn
            .query_row(
                "SELECT id FROM users ORDER BY created_at LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(user_id) = user_id else {
            return Ok(None);
        };
        let token = format!(
            "hs_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        conn.execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![hash_token(&token), user_id, now_utc(), expiry_from_now()],
        )?;
        Ok(Some(token))
    }

    /// The admin's email (None on an open-posture node) — the profile
    /// card's name fallback.
    pub fn admin_email(&self) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT email FROM users ORDER BY created_at LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Validate a session token: Some(email) when live and unexpired.
    pub fn session_user(&self, token: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT u.email FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.expires_at > ?2",
            rusqlite::params![hash_token(token), now_utc()],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn logout(&self, token: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            [hash_token(token)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (Store, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.db")).unwrap();
        (store, tmp)
    }

    #[test]
    fn test_posture_flips_on_bootstrap() {
        let (store, _tmp) = test_store();
        assert!(!store.auth_required().unwrap());
        store
            .create_admin(
                &Actor::new("human:test"),
                "jon@example.com",
                "hunter2hunter2",
            )
            .unwrap();
        assert!(store.auth_required().unwrap());
        // bootstrap runs once
        assert!(store
            .create_admin(&Actor::new("human:test"), "eve@example.com", "password123")
            .is_err());
    }

    #[test]
    fn test_login_session_logout() {
        let (store, _tmp) = test_store();
        store
            .create_admin(
                &Actor::new("human:test"),
                "jon@example.com",
                "hunter2hunter2",
            )
            .unwrap();
        assert!(store
            .login("jon@example.com", "wrong-password")
            .unwrap()
            .is_none());
        assert!(store
            .login("nobody@example.com", "hunter2hunter2")
            .unwrap()
            .is_none());
        let token = store
            .login("jon@example.com", "hunter2hunter2")
            .unwrap()
            .unwrap();
        assert!(token.starts_with("hs_"));
        assert_eq!(
            store.session_user(&token).unwrap().unwrap(),
            "jon@example.com"
        );
        store.logout(&token).unwrap();
        assert!(store.session_user(&token).unwrap().is_none());
        assert!(store.session_user("hs_forged").unwrap().is_none());
    }

    #[test]
    fn test_session_from_key_path_mints_admin_session() {
        // The POST /v1/auth/session-from-key store half: no user → None
        // (open posture); with an admin, the minted session validates as
        // that admin — the webview can render authenticated from a PAT.
        let (store, _tmp) = test_store();
        assert!(store.create_session_for_admin().unwrap().is_none());
        store
            .create_admin(
                &Actor::new("human:test"),
                "jon@example.com",
                "hunter2hunter2",
            )
            .unwrap();
        let token = store.create_session_for_admin().unwrap().unwrap();
        assert!(token.starts_with("hs_"));
        assert_eq!(
            store.session_user(&token).unwrap().unwrap(),
            "jon@example.com"
        );
    }

    #[test]
    fn test_short_password_refused() {
        let (store, _tmp) = test_store();
        assert!(store
            .create_admin(&Actor::new("human:test"), "jon@example.com", "short")
            .is_err());
    }

    #[test]
    fn test_non_email_refused() {
        let (store, _tmp) = test_store();
        assert!(store
            .create_admin(&Actor::new("human:test"), "jon", "hunter2hunter2")
            .is_err());
    }
}
