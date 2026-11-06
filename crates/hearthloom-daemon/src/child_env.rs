//! What a program the daemon starts may see of the daemon's environment
//! (0.5.84).
//!
//! The daemon loads its sealed secrets into its own environment (core
//! config.rs `apply_dotenv`): the AI provider's key, the Gmail app
//! password, every agent's key and token, and on a hosted node the
//! provisioner's and the sign-in issuer's secrets. A child process
//! inherits all of it unless told otherwise. Until 0.5.84 an agent run
//! (and every command it ran) and an ember program under the supervisor
//! did: an ember could read the secrets that sign the owner in, which made
//! its key's limits moot, and `claude -p` billed an API key it found there
//! instead of the owner's subscription. So a program started for an agent
//! or an ember starts from an EMPTY environment plus this allowlist, and
//! is handed what it needs (its URL, its key file) explicitly.

/// What a child may inherit: where programs and the user's home are, who
/// the user is, the locale and the clock. None of it is a secret.
/// `SSH_AUTH_SOCK` is a socket path, not a key: an agent's `git` over ssh
/// works as it did.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TERM",
    "TZ",
    "LANG",
    "SSH_AUTH_SOCK",
];

fn allowed(name: &str) -> bool {
    INHERITED.contains(&name) || name.starts_with("LC_")
}

fn filter(vars: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    vars.into_iter().filter(|(k, _)| allowed(k)).collect()
}

/// The daemon's environment, cut down to what a child may see. Use as
/// `.env_clear().envs(child_env::inherited())`.
pub fn inherited() -> Vec<(String, String)> {
    filter(
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secrets_never_reach_a_child() {
        let parent = [
            ("PATH", "/usr/bin"),
            ("HOME", "/Users/x"),
            ("LC_CTYPE", "UTF-8"),
            ("ANTHROPIC_API_KEY", "sk-ant-…"),
            ("GMAIL_IMAP_PASSWORD", "app password"),
            ("HEARTHLOOM_MAIL_1A2B3C4D_PASSWORD", "app password"),
            ("HEARTHLOOM_AGENT_AG1_KEY", "hl_…"),
            ("HEARTHLOOM_PROVISIONER_SECRET", "…"),
            ("HEARTHLOOM_TRUSTED_ISSUER_SECRET", "…"),
            ("HEARTHLOOM_MASTER_KEY", "…"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "…"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let kept: Vec<String> = filter(parent).into_iter().map(|(k, _)| k).collect();
        assert_eq!(kept, ["PATH", "HOME", "LC_CTYPE"]);
    }

    /// Every program started for an agent or an ember clears what it
    /// inherits; a new spawn in these files must too.
    #[test]
    fn test_agent_and_ember_programs_start_clean() {
        for (file, src) in [
            ("sidecar.rs", include_str!("sidecar.rs")),
            (
                "agents/claude_code.rs",
                include_str!("agents/claude_code.rs"),
            ),
            (
                "agents/claude_pool.rs",
                include_str!("agents/claude_pool.rs"),
            ),
            ("mail_triage.rs", include_str!("mail_triage.rs")),
        ] {
            let starts = src.matches("Command::new(").count();
            let clears = src.matches(".env_clear()").count();
            assert!(
                starts > 0,
                "{file} starts no program any more: update this test"
            );
            assert!(
                clears >= starts,
                "{file}: every Command::new must .env_clear() and add child_env::inherited()"
            );
        }
    }
}
