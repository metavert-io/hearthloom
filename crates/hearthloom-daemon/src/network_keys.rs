//! Per-friend bearer keys: .env lines (0600), never the DB — the same
//! discipline as the peer key, keyed by a sanitized friend name.

use anyhow::{Context, Result};
use hearthloom_core::Home;

fn env_var_of(name: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("HEARTHLOOM_FRIEND_KEY_{safe}")
}

pub fn friend_key(name: &str) -> Option<String> {
    std::env::var(env_var_of(name))
        .ok()
        .filter(|k| !k.trim().is_empty())
}

pub fn store_friend_key(home: &Home, name: &str, key: &str) -> Result<()> {
    let var = env_var_of(name);
    let path = home.root().join(".env");
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with(&format!("{var}=")))
        .map(String::from)
        .collect();
    lines.push(format!("{var}={key}"));
    std::fs::write(&path, lines.join("\n") + "\n").context("writing .env")?;
    let _ = std::process::Command::new("chmod")
        .args(["600", &path.to_string_lossy()])
        .output();
    std::env::set_var(&var, key);
    Ok(())
}

pub fn remove_friend_key(home: &Home, name: &str) -> Result<()> {
    let var = env_var_of(name);
    let path = home.root().join(".env");
    let lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with(&format!("{var}=")))
        .map(String::from)
        .collect();
    std::fs::write(&path, lines.join("\n") + "\n")?;
    std::env::remove_var(&var);
    Ok(())
}
