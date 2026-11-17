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
const PRETTY: &str = "--pretty=format:%x1e%H%x1f%an%x1f%ae%x1f%aI%x1f%cI%x1f%P%x1f%s%x1f%b%x1f";

/// Walk a repository's history into episode drafts. `scope: None` lets the
/// store infer each episode's scope (known project > identity > personal).
pub fn ingest(repo_path: &Path, scope: Option<&str>) -> Result<Vec<EpisodeDraft>> {
    let read = ingest_new(repo_path, scope, None)?;
    if read.drafts.is_empty() {
        return Err(anyhow!("no commits found in {}", repo_path.display()));
    }
    Ok(read.drafts)
}

/// What [`ingest_new`] read.
pub struct GitRead {
    /// The commits not covered by the mark it was given.
    pub drafts: Vec<EpisodeDraft>,
    /// Where this read stopped (the commit it read up to, how much history
    /// the clone held, and the repo identity the refs were made with):
    /// give it to the next call.
    pub mark: String,
}

/// The commits of HEAD's history that `mark` (from an earlier call) didn't
/// cover. An unchanged HEAD reads nothing; a moved one reads `old..new`
/// (commits reachable now and not then — rebased and newly merged ones
/// included). With no mark, a mark this repo can't resolve (the old commit
/// is gone), a changed identity (its refs would differ), or a shallow
/// clone whose cut-off has moved since (history fetched deeper, or whole),
/// it reads the whole history.
pub fn ingest_new(repo_path: &Path, scope: Option<&str>, mark: Option<&str>) -> Result<GitRead> {
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

    let head = git(&repo_path, &["rev-parse", "--verify", "HEAD^{commit}"])
        .with_context(|| format!("no commits found in {}", repo_path.display()))?
        .trim()
        .to_string();
    if !is_sha(&head) {
        return Err(anyhow!("unexpected HEAD in {}", repo_path.display()));
    }
    let cut = shallow_cut(&repo_path);
    let history = history_held(cut.as_deref());
    let cut_off = cut_off_commits(cut.as_deref());
    let new_mark = format!("{head} {history} {repo_identity}");
    if mark == Some(new_mark.as_str()) {
        return Ok(GitRead {
            drafts: Vec::new(),
            mark: new_mark,
        });
    }
    let since = mark
        .and_then(|m| m.split_once(' '))
        .filter(|(old, rest)| {
            is_sha(old)
                && match rest.split_once(' ') {
                    Some((held, identity)) if *held == history && identity == repo_identity => true,
                    // a mark from before marks said how much history the
                    // clone held: good for a clone that holds all of it
                    _ => *rest == repo_identity && history == "full",
                }
        })
        .map(|(old, _)| old)
        .filter(|old| {
            git(
                &repo_path,
                &["cat-file", "-e", &format!("{old}^{{commit}}")],
            )
            .is_ok()
        });
    let range = match since {
        Some(old) => format!("{old}..{head}"),
        None => head.clone(),
    };

    let log = git(
        &repo_path,
        &["log", "--no-color", "--name-only", PRETTY, &range, "--"],
    )
    .with_context(|| format!("git log failed in {}", repo_path.display()))?;

    let mut drafts = Vec::new();
    for record in log.split('\x1e').skip(1) {
        match parse_commit(
            record,
            &repo_name,
            &repo_identity,
            attr_kind,
            scope,
            &cut_off,
        ) {
            Ok(draft) => drafts.push(draft),
            Err(err) => tracing::warn!("skipping unparseable commit: {err}"),
        }
    }
    Ok(GitRead {
        drafts,
        mark: new_mark,
    })
}

/// A shallow clone's list of cut-off commits (git's `shallow` file: one id
/// per line); None for a clone that holds its whole history.
fn shallow_cut(repo_path: &Path) -> Option<Vec<u8>> {
    git(repo_path, &["rev-parse", "--git-path", "shallow"])
        .ok()
        .map(|p| repo_path.join(p.trim()))
        .and_then(|p| std::fs::read(p).ok())
        .filter(|cut| !cut.iter().all(u8::is_ascii_whitespace))
}

/// How much history the clone holds: `full`, or for a shallow clone
/// `shallow:<digest of its cut-off commits>` — which changes whenever more
/// history is fetched.
fn history_held(cut: Option<&[u8]>) -> String {
    match cut {
        Some(cut) => format!("shallow:{}", &hearthloom_core::ids::content_hash(cut)[..16]),
        None => "full".into(),
    }
}

/// The commits at a shallow clone's cut-off: git shows them with no
/// parents, whatever they had (a merge there reads as a first commit).
fn cut_off_commits(cut: Option<&[u8]>) -> std::collections::HashSet<String> {
    String::from_utf8_lossy(cut.unwrap_or_default())
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect()
}

/// A full commit id (SHA-1 or SHA-256), lowercase hex.
fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A remote URL without the credentials a URL can carry. A `scheme://` URL
/// loses its user-info (a token can sit in the user name as well as the
/// password) — except over ssh, where the user name (`git@`) is part of
/// the address and only a password goes. Anything else is returned as it
/// is: an scp-style `[user@]host:path` carries no password (git reads all
/// before its first `:` as the user and host, and an `@` after it belongs
/// to the path), nor does a local path or a URL without user-info.
/// Earlier versions kept the address as given: commits stored then carry
/// it in their refs (the daemon's `local_import::prepare` knows them).
pub fn without_credentials(url: &str) -> String {
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
    cut_off: &std::collections::HashSet<String>,
) -> Result<EpisodeDraft> {
    let mut fields = record.splitn(9, '\x1f');
    let sha = fields.next().unwrap_or("").trim();
    let author_name = fields.next().unwrap_or("").trim().to_string();
    let author_email = fields
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
        .to_string();
    let author_date = fields.next().unwrap_or("").trim();
    // when the commit itself was made: an amend, rebase or cherry-pick
    // keeps the author date and sets this one
    let committed = chrono::DateTime::parse_from_rfc3339(fields.next().unwrap_or("").trim())
        .ok()
        .map(|t| t.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string());
    let parents: Vec<&str> = fields.next().unwrap_or("").split_whitespace().collect();
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
    body.push_str(&format!("author: {author_name} <{author_email}>\n"));
    // When the commit was made, and its parents (more than one: a merge,
    // whose message comes from the merged branch). Core store/owned.rs
    // counts a commit as the owner's words only when its record says both.
    // At a shallow clone's cut-off git shows no parents whatever the commit
    // had, so the record says they weren't fetched — and it never counts
    // (the record stays as first stored, even once more history arrives).
    if let Some(committed) = &committed {
        body.push_str(&format!("committed: {committed}\n"));
    }
    body.push_str("parents:");
    if cut_off.contains(&sha.to_ascii_lowercase()) {
        body.push(' ');
        body.push_str(hearthloom_core::store::GIT_PARENTS_NOT_FETCHED);
    } else {
        for parent in &parents {
            body.push(' ');
            body.push_str(parent);
        }
    }
    body.push_str("\n\n");
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
        // owner's own words (core `OTHERS_WORDS_SOURCES`) — except, read
        // at check time, commits by the owner's own addresses in a
        // repository the owner marked as theirs (core store/owned.rs reads
        // this ref, the body's header lines — the first three, then
        // `committed:` and `parents:` — and the message's trailers; keep
        // their format).
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

        // "This source is mine" reads this adapter's ref and author line
        // (core store/owned.rs): every commit names its repository's key
        // and its author's address there.
        for d in &drafts {
            assert_eq!(
                hearthloom_core::store::key_of_ref("git", &d.source_ref).as_deref(),
                Some("git.example.com/team/repo")
            );
            assert_eq!(
                hearthloom_core::store::git_author(&d.source_ref, &d.body).as_deref(),
                Some("author@example.com")
            );
        }
    }

    /// Every commit's head names its parents, so a merge is known as one
    /// whatever its message says: its message comes from the merged branch,
    /// so "this source is mine" never counts it (core store/owned.rs
    /// `git_commit_concern`).
    #[test]
    fn test_a_merge_commit_is_marked_as_one() {
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
                .output()
                .unwrap();
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        run(&["init", "-q"]);
        fs::write(repo.join("a.txt"), "a").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "one"]);
        run(&["checkout", "-q", "-b", "topic"]);
        fs::write(repo.join("b.txt"), "b").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "on the branch"]);
        run(&["checkout", "-q", "-"]);
        fs::write(repo.join("c.txt"), "c").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "two"]);
        run(&[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "fold the branch in",
            "topic",
        ]);

        let drafts = ingest(repo, None).unwrap();
        assert_eq!(drafts.len(), 4);
        let mine: std::collections::BTreeSet<String> =
            ["author@example.com".to_string()].into_iter().collect();
        for d in &drafts {
            let merge = d.title == "fold the branch in";
            let head = hearthloom_core::store::git_commit_head(&d.body).unwrap();
            let parents = match d.title.as_str() {
                "one" => 0,
                "fold the branch in" => 2,
                _ => 1,
            };
            assert_eq!(head.parents, parents, "{}", d.body);
            assert_eq!(
                hearthloom_core::store::git_commit_concern(&d.body, &mine).is_some(),
                merge,
                "{}",
                d.title
            );
            // the author line still reads
            assert_eq!(
                hearthloom_core::store::git_author(&d.source_ref, &d.body).as_deref(),
                Some("author@example.com")
            );
        }
    }

    /// An amend keeps the author date and sets the commit time. A commit
    /// the owner wrote one day and an agent reworded during its run the
    /// next keeps yesterday's author date — and still doesn't count as the
    /// owner's words in a repository they marked: its record carries the
    /// commit time, and the check reads it (core store/owned.rs). Nor once
    /// the owner rewrites it again after the run: the run falls between its
    /// author date and its commit time.
    #[test]
    fn test_a_commit_amended_during_an_agent_run_stays_outside_text() {
        use hearthloom_core::store::Store;
        use hearthloom_core::{Actor, Home, IngestBatch};
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str], author_date: Option<&str>, commit_date: &str| {
            let mut cmd = Command::new("git");
            cmd.arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Owner Name")
                .env("GIT_AUTHOR_EMAIL", "me@own.example")
                .env("GIT_COMMITTER_NAME", "Owner Name")
                .env("GIT_COMMITTER_EMAIL", "me@own.example")
                .env("GIT_COMMITTER_DATE", commit_date)
                .env_remove("GIT_AUTHOR_DATE");
            if let Some(at) = author_date {
                cmd.env("GIT_AUTHOR_DATE", at);
            }
            let output = cmd.output().unwrap();
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        let day = "2026-11-01T09:00:00Z";
        run(&["init", "-q"], None, day);
        run(
            &[
                "remote",
                "add",
                "origin",
                "https://git.example.com/owner/project.git",
            ],
            None,
            day,
        );
        fs::write(repo.join("a.txt"), "a").unwrap();
        run(&["add", "."], None, day);
        run(&["commit", "-q", "-m", "a change"], Some(day), day);
        let original = ingest(&repo, None).unwrap().remove(0);
        // reworded the next day, during the agent's run below
        run(
            &["commit", "-q", "--amend", "-m", "a change, reworded"],
            None,
            "2026-11-02T10:30:00Z",
        );
        let amended = ingest(&repo, None).unwrap().remove(0);
        assert_eq!(amended.title, "a change, reworded");
        assert_eq!(amended.event_time.utc.as_deref(), Some(day));
        let head = hearthloom_core::store::git_commit_head(&amended.body).unwrap();
        assert_eq!(head.committed, "2026-11-02T10:30:00Z");
        assert_eq!(head.parents, 0);

        let home = Home::at(tmp.path().join("home"));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:git".into(),
                    session_id: None,
                    episodes: vec![original.clone(), amended.clone()],
                },
            )
            .unwrap();
        store
            .set_consolidation_state(
                "network.profile",
                &serde_json::json!({"emails": ["me@own.example"]}).to_string(),
            )
            .unwrap();
        store
            .owned_source_add(
                &Actor::new("human:owner"),
                "git",
                "https://git.example.com/owner/project",
                "session",
            )
            .unwrap();
        let judge = |d: &EpisodeDraft| {
            let id = hearthloom_core::ids::episode_id("git", &d.source_ref);
            store.owned_judgement(&id).unwrap().unwrap()
        };
        assert!(judge(&amended).counts, "{:?}", judge(&amended).why);

        let at = |t: &str| serde_json::Value::String(t.into());
        store
            .agents_sync_apply(&serde_json::json!({"tasks": [{
                "id": "tk_agent", "request": "r", "brief": "b", "classes": "[]",
                "sensitive": 0, "status": "done", "actions": 0,
                "inputs": "[]", "outputs": "[]",
                "created_at": at("2026-11-02T10:00:00Z"),
                "updated_at": at("2026-11-02T11:00:00Z"),
                "started_at": at("2026-11-02T10:00:00Z"),
                "finished_at": at("2026-11-02T11:00:00Z"),
            }]}))
            .unwrap();
        let j = judge(&amended);
        assert!(!j.counts);
        assert!(
            j.why.as_deref().unwrap_or("").contains("tk_agent"),
            "{:?}",
            j.why
        );
        // the commit as the owner first made it still counts
        assert!(judge(&original).counts, "{:?}", judge(&original).why);

        // rewritten again the day after the run (a rebase or a cherry-pick
        // sets the commit time the same way): neither time it keeps falls
        // in the run, and the agent's words are still in it
        run(
            &["commit", "-q", "--amend", "--no-edit"],
            None,
            "2026-11-03T09:00:00Z",
        );
        let rewritten = ingest(&repo, None).unwrap().remove(0);
        assert_eq!(rewritten.title, "a change, reworded");
        assert_eq!(rewritten.event_time.utc.as_deref(), Some(day));
        assert_eq!(
            hearthloom_core::store::git_commit_head(&rewritten.body)
                .unwrap()
                .committed,
            "2026-11-03T09:00:00Z"
        );
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:git".into(),
                    session_id: None,
                    episodes: vec![rewritten.clone()],
                },
            )
            .unwrap();
        let j = judge(&rewritten);
        assert!(!j.counts);
        assert!(
            j.why.as_deref().unwrap_or("").contains("tk_agent"),
            "{:?}",
            j.why
        );
    }

    /// A read given the previous read's mark reads only the commits since;
    /// an unchanged HEAD reads nothing, and a mark that can't be resolved
    /// here (or was made under another identity) reads everything.
    #[test]
    fn test_a_read_covers_only_commits_past_its_mark() {
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
                .output()
                .unwrap();
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        let commit = |file: &str, message: &str| {
            fs::write(repo.join(file), message).unwrap();
            run(&["add", "."]);
            run(&["commit", "-q", "-m", message]);
        };
        let titles = |read: &GitRead| -> Vec<String> {
            let mut t: Vec<String> = read.drafts.iter().map(|d| d.title.clone()).collect();
            t.sort();
            t
        };
        run(&["init", "-q"]);
        commit("a.txt", "one");
        commit("b.txt", "two");

        let first = ingest_new(repo, None, None).unwrap();
        assert_eq!(titles(&first), ["one", "two"]);
        let same = ingest_new(repo, None, Some(&first.mark)).unwrap();
        assert!(same.drafts.is_empty());
        assert_eq!(same.mark, first.mark);

        commit("c.txt", "three");
        let next = ingest_new(repo, None, Some(&first.mark)).unwrap();
        assert_eq!(titles(&next), ["three"]);
        assert_ne!(next.mark, first.mark);

        // a rewritten tip: the new commit is read, the kept ones aren't
        run(&["commit", "-q", "--amend", "-m", "three, reworded"]);
        let amended = ingest_new(repo, None, Some(&next.mark)).unwrap();
        assert_eq!(titles(&amended), ["three, reworded"]);

        // an old commit that isn't here, or another identity: everything
        let gone = format!(
            "{} {}",
            "0".repeat(40),
            first.mark.split_once(' ').unwrap().1
        );
        assert_eq!(ingest_new(repo, None, Some(&gone)).unwrap().drafts.len(), 3);
        let elsewhere = format!(
            "{} /some/other/path",
            amended.mark.split_once(' ').unwrap().0
        );
        assert_eq!(
            ingest_new(repo, None, Some(&elsewhere))
                .unwrap()
                .drafts
                .len(),
            3
        );
        assert_eq!(
            ingest_new(repo, None, Some("junk")).unwrap().drafts.len(),
            3
        );
        // a mark from before marks named the history held: still good
        // for a clone that holds all of it
        let (head, rest) = amended.mark.split_once(' ').unwrap();
        let identity = rest.strip_prefix("full ").unwrap();
        let older = format!("{head} {identity}");
        let read = ingest_new(repo, None, Some(&older)).unwrap();
        assert!(read.drafts.is_empty());
        assert_eq!(read.mark, amended.mark);
    }

    /// Fetching more of a shallow clone's history, with HEAD where it was,
    /// reads the commits that arrived.
    #[test]
    fn test_a_shallow_clone_fetched_deeper_reads_the_new_history() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let clone = tmp.path().join("clone");
        let git_in = |dir: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test Author")
                .env("GIT_AUTHOR_EMAIL", "author@example.com")
                .env("GIT_COMMITTER_NAME", "Test Author")
                .env("GIT_COMMITTER_EMAIL", "author@example.com")
                .output()
                .unwrap();
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        fs::create_dir_all(&source).unwrap();
        git_in(&source, &["init", "-q"]);
        for message in ["one", "two", "three"] {
            fs::write(source.join(format!("{message}.txt")), message).unwrap();
            git_in(&source, &["add", "."]);
            git_in(&source, &["commit", "-q", "-m", message]);
        }
        let url = format!("file://{}", source.display());
        git_in(
            tmp.path(),
            &["clone", "-q", "--depth", "1", &url, clone.to_str().unwrap()],
        );

        let shallow = ingest_new(&clone, None, None).unwrap();
        assert_eq!(shallow.drafts.len(), 1);
        assert!(ingest_new(&clone, None, Some(&shallow.mark))
            .unwrap()
            .drafts
            .is_empty());

        git_in(&clone, &["fetch", "-q", "--deepen=1"]);
        let deeper = ingest_new(&clone, None, Some(&shallow.mark)).unwrap();
        assert_eq!(deeper.drafts.len(), 2, "HEAD and the commit fetched");
        assert_ne!(deeper.mark, shallow.mark);

        git_in(&clone, &["fetch", "-q", "--unshallow"]);
        let whole = ingest_new(&clone, None, Some(&deeper.mark)).unwrap();
        assert_eq!(whole.drafts.len(), 3);
        assert!(ingest_new(&clone, None, Some(&whole.mark))
            .unwrap()
            .drafts
            .is_empty());
    }

    /// At a shallow clone's cut-off git shows a commit with no parents,
    /// whatever it had: a merge there with a subject of its own would read
    /// as a first commit. Its record says its parents weren't fetched, so
    /// it never counts as the owner's words (core store/owned.rs) — and a
    /// commit below the cut-off reads as usual.
    #[test]
    fn test_a_merge_at_a_shallow_clones_cut_off_is_never_read_as_a_first_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source");
        let clone = tmp.path().join("clone");
        let git_in = |dir: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Test Author")
                .env("GIT_AUTHOR_EMAIL", "author@example.com")
                .env("GIT_COMMITTER_NAME", "Test Author")
                .env("GIT_COMMITTER_EMAIL", "author@example.com")
                .output()
                .unwrap();
            assert!(output.status.success(), "git {:?}: {:?}", args, output);
        };
        fs::create_dir_all(&source).unwrap();
        git_in(&source, &["init", "-q"]);
        fs::write(source.join("a.txt"), "a").unwrap();
        git_in(&source, &["add", "."]);
        git_in(&source, &["commit", "-q", "-m", "one"]);
        git_in(&source, &["checkout", "-q", "-b", "topic"]);
        fs::write(source.join("b.txt"), "b").unwrap();
        git_in(&source, &["add", "."]);
        git_in(&source, &["commit", "-q", "-m", "on the branch"]);
        git_in(&source, &["checkout", "-q", "-"]);
        fs::write(source.join("c.txt"), "c").unwrap();
        git_in(&source, &["add", "."]);
        git_in(&source, &["commit", "-q", "-m", "two"]);
        git_in(
            &source,
            &["merge", "-q", "--no-ff", "-m", "Add the new page", "topic"],
        );
        let url = format!("file://{}", source.display());
        git_in(
            tmp.path(),
            &["clone", "-q", "--depth", "1", &url, clone.to_str().unwrap()],
        );

        let mine: std::collections::BTreeSet<String> =
            ["author@example.com".to_string()].into_iter().collect();
        let read = ingest_new(&clone, None, None).unwrap();
        assert_eq!(read.drafts.len(), 1);
        let merge = &read.drafts[0];
        assert_eq!(merge.title, "Add the new page");
        let concern = hearthloom_core::store::git_commit_concern(&merge.body, &mine);
        assert!(
            concern.as_deref().unwrap_or("").contains("shallow clone"),
            "{concern:?}\n{}",
            merge.body
        );
        assert!(
            merge.body.contains("\nparents: not fetched\n"),
            "{}",
            merge.body
        );
        assert_eq!(hearthloom_core::store::git_commit_head(&merge.body), None);

        // fetched deeper: the merge reads with both parents, the commits
        // now at the cut-off don't
        git_in(&clone, &["fetch", "-q", "--deepen=1"]);
        let deeper = ingest_new(&clone, None, Some(&read.mark)).unwrap();
        for d in &deeper.drafts {
            let head = hearthloom_core::store::git_commit_head(&d.body);
            match d.title.as_str() {
                "Add the new page" => assert_eq!(head.unwrap().parents, 2),
                _ => assert_eq!(head, None, "{}", d.body),
            }
        }
        assert!(deeper.drafts.iter().any(|d| d.title == "Add the new page"));
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
            // an `@` after an scp-style address's `:` is the path's
            ("host.example:repo@v2", "host.example:repo@v2"),
            ("git@host.example:o/r@v2", "git@host.example:o/r@v2"),
            (r"C:\repos\proj@v2", r"C:\repos\proj@v2"),
            ("/Users/you/code/r", "/Users/you/code/r"),
            ("/srv/repos/r@v2", "/srv/repos/r@v2"),
            ("file:///srv/repos/r.git", "file:///srv/repos/r.git"),
        ] {
            assert_eq!(without_credentials(given), kept, "{given}");
        }
    }
}
