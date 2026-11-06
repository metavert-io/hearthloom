//! The one-time setup code (0.5.96): proof that whoever creates a node's
//! first login can reach the node's own disk.
//!
//! While a node has no login and no provisioner, it keeps a random code in
//! `<home>/setup-code` (0600). `POST /v1/auth/bootstrap` takes it, and the
//! file goes once the login exists. On the machine itself, the first-run
//! wizard reads it through `GET /v1/auth/setup-code`, which answers only a
//! connection from this machine's own loopback (not a Host header: a
//! remote client can write any) — a local caller already controls a node
//! with no login, so that costs nothing. Anyone else types it in from the
//! machine (`hearthloom auth setup-code`). A hosted node is set up by its
//! provisioner's control token instead (`auth_bootstrap`).
use std::path::PathBuf;

use crate::AppState;

const FILE: &str = "setup-code";
/// No 0/O or 1/I/L: read off a terminal and typed into a browser.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

pub fn path(state: &AppState) -> PathBuf {
    state.home.root().join(FILE)
}

fn fresh() -> String {
    let bytes: Vec<u8> = uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .chain(uuid::Uuid::new_v4().as_bytes().iter())
        .copied()
        .collect();
    let chars: Vec<char> = bytes
        .iter()
        .take(20)
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    chars
        .chunks(5)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

fn normal(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// Whether this node still needs its first login set up with a code.
pub fn needed(state: &AppState) -> bool {
    crate::provisioner::secret().is_none() && !state.store.auth_required().unwrap_or(true)
}

/// The code, made when the node needs one. Never logged.
pub fn ensure(state: &AppState) -> Option<String> {
    if !needed(state) {
        return None;
    }
    at(&path(state))
}

/// The code in this file, made first if there is none.
fn at(p: &std::path::Path) -> Option<String> {
    if let Ok(code) = std::fs::read_to_string(p) {
        if !normal(&code).is_empty() {
            return Some(code.trim().to_string());
        }
    }
    let code = fresh();
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(p)?;
        f.write_all(code.as_bytes())?;
        f.write_all(b"\n")
    };
    match write() {
        Ok(()) => Some(code),
        Err(err) => {
            tracing::warn!("couldn't write the setup code: {err}");
            None
        }
    }
}

/// Whether `given` is this node's code (case, spaces and dashes aside).
pub fn check(state: &AppState, given: Option<&str>) -> bool {
    matches_code(ensure(state).as_deref(), given)
}

fn matches_code(code: Option<&str>, given: Option<&str>) -> bool {
    let (Some(given), Some(code)) = (given, code) else {
        return false;
    };
    let (a, b) = (normal(given), normal(code));
    // the same length, compared without stopping early
    a.len() == b.len()
        && !a.is_empty()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The login exists: the code is spent.
pub fn consume(state: &AppState) {
    let _ = std::fs::remove_file(path(state));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_code_reads_easily_and_compares_loosely() {
        let c = fresh();
        assert_eq!(c.len(), 23, "{c}");
        assert!(normal(&c).bytes().all(|b| ALPHABET.contains(&b)));
        assert_ne!(fresh(), fresh());
        assert_eq!(normal(" abcde-fghjk "), "ABCDEFGHJK");
    }

    #[test]
    fn test_a_code_is_made_once_checked_and_spent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(FILE);
        let code = at(&p).unwrap();
        assert_eq!(at(&p).as_deref(), Some(code.as_str()), "kept");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let ok = |given: Option<&str>| matches_code(Some(&code), given);
        assert!(ok(Some(&code.to_lowercase().replace('-', " "))));
        assert!(!ok(Some("AAAAA-AAAAA-AAAAA-AAAAA")));
        assert!(!ok(None));
        assert!(!ok(Some("")));
        assert!(
            !matches_code(None, Some(&code)),
            "no code here: nothing matches"
        );
    }
}
