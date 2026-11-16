//! Git adapter: a repo's history becomes episodes (commit = episode;
//! author, repo, and touched files become entities + edges).
//!
//! Shells out to `git log` rather than binding libgit2 — zero extra
//! dependencies, and the porcelain format is stable. `%aI` gives strict
//! ISO-8601 author time *with the author's UTC offset*, which is exactly
//! what the episode log's tz_offset field wants.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use hearthloom_core::{EdgeAssertion, EntityRef, EpisodeDraft, EventTime, TimeUncertainty};

/// Files beyond this per commit are listed in the body but not minted as
/// Artifact entities (mass renames/vendoring would flood the graph).
const MAX_FILE_ENTITIES: usize = 20;

/// Record separator at the *start* of each commit so `--name-only` file
/// lists stay attached to their own commit when splitting.
const PRETTY: &str = "--pretty=format:%x1e%H%x1f%an%x1f%ae%x1f%aI%x1f%s%x1f%b%x1f";

/// Walk a repository's history into episode drafts. `scope: None` lets the
/// store infer each episode's scope (known project > identity > personal).
pub fn ingest(repo_path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let repo_path = repo_path
        .canonicalize()
        .with_context(|| format!("no such path: {}", repo_path.display()))?;
    let repo_name = repo_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo")
        .to_string();

    // Repo identity: origin URL if there is one, else the canonical path.
    // A credential saved in the URL never enters the identity, the body or
    // the Project's attribute.
    let (attr_kind, repo_identity) = match git(&repo_path, &["remote", "get-url", "origin"]) {
        Ok(url) if !url.trim().is_empty() => ("repo-url", without_credentials(url.trim())),
        _ => ("repo-path", repo_path.display().to_string()),
    };

    let log = git(&repo_path, &["log", "--no-color", "--name-only", PRETTY])
        .with_context(|| format!("git log failed in {}", repo_path.display()))?;

    let mut drafts = Vec::new();
    for record in log.split('\x1e').skip(1) {
        match parse_commit(record, &repo_name, &repo_identity, attr_kind, scope) {
            Ok(draft) => drafts.push(draft),
            Err(err) => tracing::warn!("skipping unparseable commit: {err}"),
        }
    }
    if drafts.is_empty() {
        return Err(anyhow!("no commits found in {}", repo_path.display()));
    }
    Ok(drafts)
}

/// A remote URL without the credentials a URL can carry. A `scheme://` URL
/// loses its user-info (a token can sit in the user name as well as the
/// password) — except over ssh, where the user name (`git@`) is part of
/// the address and only a password goes. An scp-style `user@host:path`
/// keeps its user name and loses a `:password`. Anything else (a local
/// path, a URL without user-info) is returned as it is.
fn without_credentials(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        let (authority, tail) = match rest.find(['/', '?', '#']) {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        let Some((userinfo, host)) = authority.rsplit_once('@') else {
            return url.to_string();
        };
        let ssh = ["ssh", "git+ssh", "ssh+git"]
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s));
        let user = userinfo.split(':').next().unwrap_or("");
        return if ssh && !user.is_empty() {
            format!("{scheme}://{user}@{host}{tail}")
        } else {
            format!("{scheme}://{host}{tail}")
        };
    }
    // scp-style: [user@]host:path
    if let Some((userinfo, rest)) = url.split_once('@') {
        if !userinfo.contains('/') {
            if let Some((user, _)) = userinfo.split_once(':') {
                return format!("{user}@{rest}");
            }
        }
    }
    url.to_string()
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("running git")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_commit(
    record: &str,
    repo_name: &str,
    repo_identity: &str,
    attr_kind: &str,
    scope: Option<&str>,
) -> Result<EpisodeDraft> {
    let mut fields = record.splitn(7, '\x1f');
    let sha = fields.next().unwrap_or("").trim();
    let author_name = fields.next().unwrap_or("").trim().to_string();
    let author_email = fields
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
        .to_string();
    let author_date = fields.next().unwrap_or("").trim();
    let subject = fields.next().unwrap_or("").trim().to_string();
    let body_msg = fields.next().unwrap_or("").trim().to_string();
    // After the final \x1f: blank line(s) + the --name-only file list.
    let files: Vec<String> = fields
        .next()
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect();

    if sha.is_empty() || author_date.is_empty() {
        return Err(anyhow!("malformed record"));
    }
    let event_time = EventTime::parse_rfc3339(author_date, TimeUncertainty::Exact)?;

    let mut body = String::new();
    body.push_str(&format!("commit {sha}\nrepo: {repo_identity}\n"));
    body.push_str(&format!("author: {author_name} <{author_email}>\n\n"));
    body.push_str(&subject);
    if !body_msg.is_empty() {
        body.push_str("\n\n");
        body.push_str(&body_msg);
    }
    if !files.is_empty() {
        body.push_str("\n\nfiles:\n");
        for file in &files {
            body.push_str(&format!("  {file}\n"));
        }
    }

    // Entity 0: author. Entity 1: repo. 2..: touched files (capped).
    let author_display = if author_name.is_empty() {
        author_email.clone()
    } else {
        author_name.clone()
    };
    let mut entities = vec![
        EntityRef {
            entity_type: "Person".into(),
            name: author_display,
            attributes: if author_email.is_empty() {
                vec![]
            } else {
                vec![("email".into(), author_email.clone())]
            },
            role: "author".into(),
        },
        EntityRef {
            entity_type: "Project".into(),
            name: repo_name.into(),
            attributes: vec![(attr_kind.into(), repo_identity.into())],
            role: "repo".into(),
        },
    ];
    let mut edges = vec![EdgeAssertion {
        src: 0,
        relation: "contributes_to".into(),
        dst: 1,
    }];
    for file in files.iter().take(MAX_FILE_ENTITIES) {
        entities.push(EntityRef {
            entity_type: "Artifact".into(),
            name: format!("{repo_name}/{file}"),
            attributes: vec![],
            role: "touched".into(),
        });
        edges.push(EdgeAssertion {
            src: 1,
            relation: "contains".into(),
            dst: entities.len() - 1,
        });
    }

    Ok(EpisodeDraft {
        source: "git".into(),
        source_ref: format!("{repo_identity}#{sha}"),
        kind: "commit".into(),
        title: subject,
        body,
        event_time,
        // A history holds every contributor's messages, and an author line
        // is whatever its writer typed: commits are not taken as the
        // owner's own words (core `OTHERS_WORDS_SOURCES`).
        trust: "imported".into(),
        scope: scope.map(String::from),
        entities,
        edges,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a throwaway repo with two commits and ingest it.
    #[test]
    fn test_ingest_real_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let run = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test Author")
                .env("GIT_AUTHOR_EMAIL", "author@example.com")
                .env("GIT_COMMITTER_NAME", "Test Author")
                .env("GIT_COMMITTER_EMAIL", "author@example.com")
                .env("GIT_AUTHOR_DATE", "2026-03-15T10:00:00-04:00")
                .env("GIT_COMMITTER_DATE", "2026-03-15T10:00:00-04:00")
                .output()
                .unwrap();
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        run(&["init", "-q"]);
        fs::write(repo.join("a.txt"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "first commit"]);
        fs::write(repo.join("b.txt"), "world").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "add b\n\nlonger body here"]);

        let drafts = ingest(repo, Some("work")).unwrap();
        assert_eq!(drafts.len(), 2);

        // Newest first from git log.
        let newest = &drafts[0];
        assert_eq!(newest.kind, "commit");
        assert_eq!(newest.title, "add b");
        assert!(newest.body.contains("longer body here"));
        assert!(newest.body.contains("b.txt"));
        assert_eq!(newest.trust, "imported");
        assert_eq!(newest.event_time.tz_offset_minutes, Some(-240));
        assert_eq!(newest.entities[0].entity_type, "Person");
        assert_eq!(
            newest.entities[0].attributes[0],
            ("email".to_string(), "author@example.com".to_string())
        );
        assert_eq!(newest.entities[1].entity_type, "Project");
        // touched file entity present
        assert!(newest
            .entities
            .iter()
            .any(|e| e.entity_type == "Artifact" && e.name.ends_with("/b.txt")));
        // contributes_to + contains edges
        assert!(newest.edges.len() >= 2);

        // source_ref is unique per commit.
        assert_ne!(drafts[0].source_ref, drafts[1].source_ref);

        // A credential saved in the origin URL is stored nowhere.
        run(&[
            "remote",
            "add",
            "origin",
            "https://someone:tok-SECRET-123@git.example.com/team/repo.git",
        ]);
        let drafts = ingest(repo, None).unwrap();
        for d in &drafts {
            assert!(!d.body.contains("SECRET"), "{}", d.body);
            assert!(!d.source_ref.contains("SECRET"), "{}", d.source_ref);
            for e in &d.entities {
                assert!(e.attributes.iter().all(|(_, v)| !v.contains("SECRET")));
            }
        }
        assert!(drafts[0]
            .source_ref
            .starts_with("https://git.example.com/team/repo.git#"));
        assert_eq!(
            drafts[0].entities[1].attributes[0],
            (
                "repo-url".to_string(),
                "https://git.example.com/team/repo.git".to_string()
            )
        );
    }

    #[test]
    fn test_remote_urls_lose_their_credentials_only() {
        for (given, kept) in [
            (
                "https://user:tok@github.example/o/r.git",
                "https://github.example/o/r.git",
            ),
            (
                "https://tok@github.example/o/r.git",
                "https://github.example/o/r.git",
            ),
            (
                "https://u:p@ss@host.example:8443/r",
                "https://host.example:8443/r",
            ),
            (
                "https://host.example/@team/r.git",
                "https://host.example/@team/r.git",
            ),
            (
                "ssh://git:pw@host.example/r.git",
                "ssh://git@host.example/r.git",
            ),
            (
                "ssh://git@host.example/r.git",
                "ssh://git@host.example/r.git",
            ),
            ("git@github.example:o/r.git", "git@github.example:o/r.git"),
            (
                "git:pw@github.example:o/r.git",
                "git@github.example:o/r.git",
            ),
            ("/Users/you/code/r", "/Users/you/code/r"),
            ("/srv/repos/r@v2", "/srv/repos/r@v2"),
            ("file:///srv/repos/r.git", "file:///srv/repos/r.git"),
        ] {
            assert_eq!(without_credentials(given), kept, "{given}");
        }
    }
}
