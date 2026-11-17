//! "This source is mine" (v68): the owner's list of sources, or parts of a
//! source, whose text counts as the owner's own words although the source
//! is one other people can write (`provenance::OTHERS_WORDS_SOURCES`).
//!
//! An entry names a source and a KEY that source defines for the part that
//! is the owner's:
//!
//! - `git`: a repository, by its remote address (`git_repo_key`: host and
//!   path, lowercased, no user name, no `.git`). Within it only commits
//!   whose author address is one of the owner's own (`owner_emails`)
//!   count, and of those not a merge, not one co-written with anyone else
//!   (a `Co-authored-by:` trailer — Claude Code adds one) and not one
//!   made or rewritten while one of the owner's agents may have been at it:
//!   a run of one of their agents anywhere from the commit's author date
//!   to its commit time keeps it out (`git_commit_concern`,
//!   `agent_run_between` — a rewrite keeps the author date and a later one
//!   replaces the commit time, so an agent's rewrite in between leaves no
//!   time of its own). Only a record that says its
//!   parents and commit time (`git_commit_head`) can be judged: a commit
//!   an earlier version imported, or one at a shallow clone's cut-off
//!   (`GIT_PARENTS_NOT_FETCHED`), stays outside text. Git doesn't check an
//!   author address: anyone whose commit reaches the repository can write
//!   the owner's on it, so the owner marks only repositories whose history
//!   they control.
//! - `substack`: a publication, by its host (`substack_host_key`). Every
//!   post under that host counts, guest posts included; the adapter keeps
//!   a post only under the host that served it, and records that host
//!   (`substack_served_from`) — a post an earlier version imported, which
//!   recorded none, stays outside text.
//!
//! A source joins by adding an `OwnableSource` row plus its key reader
//! (`key_of_ref`) and, when its items can have different authors, an
//! author reader (`author_of`). Only sources where one author per item can
//! be checked, or that one person writes whole, may join; `NOT_OWNABLE`
//! says why the others can't.
//!
//! Stored trust never changes: the list is applied when the taint checks
//! read (`first_untrusted_episode`, `entity_names_unverified` — via
//! `OwnedWords`). Every doubt keeps an episode outside text: a writer
//! other than the source's own importer, an unknown writer, a ref or
//! author line that can't be read, no known address of the owner's.
//!
//! The list is owner-only (the daemon refuses every other key, and a
//! locked vault for additions), mirrored between the owner's nodes like
//! the mail rules (operational mirror, LWW by `updated_at`) and journaled
//! (`op_journal`, kind `owned_source`: the journal leg carries removals).
use std::collections::{BTreeSet, HashSet};

use anyhow::{anyhow, bail, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// A source the owner may mark as theirs.
pub struct OwnableSource {
    pub source: &'static str,
    /// What one entry names.
    pub names: &'static str,
    /// What the owner gives to name it.
    pub given_as: &'static str,
    /// Which of its items then count as the owner's words.
    pub counts: &'static str,
}

pub const OWNABLE_SOURCES: &[OwnableSource] = &[
    OwnableSource {
        source: "git",
        names: "a repository",
        given_as: "its remote address (https://host/owner/repo or git@host:owner/repo)",
        counts: "commits whose author address is one of your own, except merges, commits \
                 co-written with anyone else (a Co-authored-by line, which Claude Code adds), \
                 commits with one of your agents at work anywhere between when they were \
                 first written and their last rewrite, and \
                 commits an earlier version of Hearthloom imported or a shallow clone's oldest \
                 (their records can't show those things). Git doesn't check author \
                 addresses: anyone whose commit gets into the repository can put yours on it",
    },
    OwnableSource {
        source: "substack",
        names: "a publication",
        given_as: "its address (name.substack.com or its own domain)",
        counts: "every post published under that address, guest posts included, except posts \
                 an earlier version of Hearthloom imported (it didn't check which host served \
                 them)",
    },
];

/// Sources that can never be marked as the owner's, and why: their items
/// mix authors, or carry text fetched from elsewhere.
pub const NOT_OWNABLE: &[(&str, &str)] = &[
    ("imessage", "a conversation holds other people's messages"),
    (
        "chrome",
        "page titles and addresses are written by the sites",
    ),
    (
        "claude-code",
        "a session holds fetched pages, tool output and the assistant's replies",
    ),
    ("openai", "a conversation holds the assistant's replies"),
    ("chatlog-", "a conversation holds the assistant's replies"),
    ("ics", "invitations are written by whoever sent them"),
    ("rewind", "screen captures hold whatever was on screen"),
    ("ios-scan", "scanned documents were written by other people"),
    (
        "ios-media",
        "titles are written by whoever published the media",
    ),
];

pub fn ownable(source: &str) -> Option<&'static OwnableSource> {
    OWNABLE_SOURCES.iter().find(|s| s.source == source)
}

/// Why a source can't be marked as the owner's (None: it can).
pub fn not_ownable_reason(source: &str) -> Option<String> {
    if ownable(source).is_some() {
        return None;
    }
    let listed = NOT_OWNABLE.iter().find(|(s, _)| {
        if let Some(prefix) = s.strip_suffix('-') {
            source.starts_with(&format!("{prefix}-"))
        } else {
            *s == source
        }
    });
    let allowed = OWNABLE_SOURCES
        .iter()
        .map(|s| s.source)
        .collect::<Vec<_>>()
        .join(", ");
    Some(match listed {
        Some((_, why)) => format!("{source} can't be marked as yours: {why}"),
        None => format!(
            "{source} can't be marked as yours: only sources with one checkable author per \
             item, or one author for the whole source, can ({allowed})"
        ),
    })
}

/// The key an entry is stored under, from what the owner gave.
pub fn owned_key(source: &str, given: &str) -> Result<String> {
    if let Some(why) = not_ownable_reason(source) {
        bail!(why);
    }
    match source {
        "git" => git_repo_key(given).ok_or_else(|| {
            anyhow!(
                "that isn't a repository's remote address — give it as \
                 https://host/owner/repo or git@host:owner/repo (a repository with no \
                 remote can't be named the same way on every node)"
            )
        }),
        "substack" => substack_host_key(given).ok_or_else(|| {
            anyhow!("that isn't a publication's address — give it as name.substack.com or its own domain")
        }),
        other => bail!("{other} has no key reader"),
    }
}

/// The deterministic id of an entry: every node that adds the same entry
/// makes the same row.
pub fn owned_id(source: &str, key: &str) -> String {
    format!(
        "os_{}",
        &crate::ids::content_hash(format!("{source}\n{key}").as_bytes())[..16]
    )
}

/* ————— the keys each source defines ————— */

/// A repository's key: `host/path` of its remote address, lowercased,
/// without user name or password, default port, trailing `/` or `.git`.
/// A local path (`/…`, `file://…`) has no remote address: None.
pub fn git_repo_key(address: &str) -> Option<String> {
    let a = address.trim();
    if a.is_empty()
        || a.len() > 2048
        || a.chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return None;
    }
    let (host, path) = if let Some((scheme, rest)) = a.split_once("://") {
        let default_port = match scheme.to_ascii_lowercase().as_str() {
            "https" => ":443",
            "http" => ":80",
            "ssh" | "git+ssh" | "ssh+git" => ":22",
            "git" => ":9418",
            _ => return None,
        };
        // The authority ends at the first `/`, `?` or `#`, as git, curl and
        // the git adapter's `without_credentials` read it — so
        // `https://a.example#@b.example/o/r` is a.example's, not b.example's.
        let (authority, tail) = rest.split_at(rest.find(['/', '?', '#'])?);
        let path = tail.strip_prefix('/').unwrap_or("");
        let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = hostport.strip_suffix(default_port).unwrap_or(hostport);
        (host, path.split(['?', '#']).next().unwrap_or(""))
    } else if let Some((before, path)) = a.split_once(':').filter(|(b, _)| !b.contains('/')) {
        // scp-like: [user@]host:path
        (before.rsplit_once('@').map_or(before, |(_, h)| h), path)
    } else {
        // host/path, as a browser shows it
        let (host, path) = a.split_once('/')?;
        if !host.contains('.') || host.contains('@') {
            return None;
        }
        (host, path)
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty()
        || host.starts_with('.')
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    {
        return None;
    }
    let mut path = path.trim_matches('/').to_lowercase();
    if let Some(stripped) = path.strip_suffix(".git") {
        path = stripped.trim_end_matches('/').to_string();
    }
    if path.is_empty()
        || path
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    Some(format!("{host}/{path}"))
}

/// A publication's key: the host of its address, lowercased, without a
/// leading `www.` or a default port. Given by the owner with or without
/// `https://`; read from a post's ref only with it (`key_of_ref`).
pub fn substack_host_key(address: &str) -> Option<String> {
    let a = address.trim();
    let rest = match a.split_once("://") {
        Some((scheme, rest))
            if matches!(scheme.to_ascii_lowercase().as_str(), "https" | "http") =>
        {
            rest
        }
        Some(_) => return None,
        None => a,
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.contains('@') {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let host = host
        .strip_suffix(":443")
        .or_else(|| host.strip_suffix(":80"))
        .unwrap_or(&host);
    let host = host.strip_prefix("www.").unwrap_or(host);
    let ok = host.contains('.')
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    ok.then(|| host.to_string())
}

/// The key of the part of its source an episode belongs to, read from its
/// ref; None when the ref can't be read that way (it then never counts).
pub fn key_of_ref(source: &str, source_ref: &str) -> Option<String> {
    match source {
        // `<remote address>#<commit id>` (the git adapter's ref)
        "git" => {
            let (identity, sha) = source_ref.rsplit_once('#')?;
            is_commit_id(sha).then_some(())?;
            git_repo_key(identity)
        }
        // the post's canonical address
        "substack" => {
            let lower = source_ref.trim().to_ascii_lowercase();
            (lower.starts_with("https://") || lower.starts_with("http://")).then_some(())?;
            substack_host_key(source_ref)
        }
        _ => None,
    }
}

/// The part of its source a ref belongs to, as the start every ref of that
/// part shares (a range of the episodes' `(source, source_ref)` index), and
/// that part's key: a commit's ref up to its first `#` (its remote address,
/// when the address holds none), a post's up to the end of its host. Every
/// ref with that start has the part's key or none — except, for git, a ref
/// whose address itself holds a `#`: `git_repo_key` ends a URL's host and
/// path at a `#`, so its key is then the part's, none, or one that holds a
/// `#` and starts with the part's key; and when the part has no key, any
/// key (`stored_by_key` says which such parts can hold one). None: a ref
/// without such a start (it is looked at alone).
fn ref_part<'r>(source: &str, source_ref: &'r str) -> Option<(&'r str, Option<String>)> {
    match source {
        "git" => {
            let end = source_ref.find('#')?;
            Some((&source_ref[..=end], git_repo_key(&source_ref[..end])))
        }
        "substack" => {
            let host = source_ref.find("://")? + 3;
            let end = host + source_ref[host..].find(['/', '?', '#'])?;
            let start = &source_ref[..=end];
            Some((start, key_of_ref(source, start)))
        }
        _ => None,
    }
}

/// The first text past every one that starts with `start`, whose last
/// character is ASCII (`ref_part`'s): where its range ends.
fn part_end(start: &str) -> String {
    let mut end = start.as_bytes().to_vec();
    if let Some(last) = end.last_mut() {
        *last += 1;
    }
    String::from_utf8(end).expect("an ASCII character, raised by one, is ASCII")
}

fn is_commit_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The author address of a commit, read from the head of its stored text
/// as the git adapter writes it:
///
/// ```text
/// commit <id>
/// repo: <remote address>
/// author: <name> <<address>>
/// ```
///
/// The first two lines must match the ref (the text belongs to that
/// commit); git keeps `<` out of names and `>` out of addresses, so the
/// address is what sits between the line's first `<` and its final `>`.
/// Anything else: None.
pub fn git_author(source_ref: &str, head: &str) -> Option<String> {
    let (identity, sha) = source_ref.rsplit_once('#')?;
    let mut lines = head.lines();
    if lines.next()? != format!("commit {sha}") || lines.next()? != format!("repo: {identity}") {
        return None;
    }
    let rest = lines.next()?.strip_prefix("author: ")?;
    let open = rest.find('<')?;
    let address = rest[open + 1..].strip_suffix('>')?;
    let ok = address.matches('@').count() == 1
        && !address.starts_with('@')
        && !address.ends_with('@')
        && !address.contains(['<', '>'])
        && !address.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then(|| address.to_lowercase())
}

/// How much of a commit's stored text the judgement reads: all of it, up
/// to this many characters. A longer one can't be checked whole (a
/// trailer could sit past the cut), so it doesn't count.
pub const COMMIT_TEXT_CHARS: usize = 65_536;

/// What a commit's record says beyond its author, from the head lines the
/// git adapter writes after `author:` — `committed: <UTC time>` (when the
/// commit itself was made: an amend, a rebase or a cherry-pick keeps the
/// author date and sets this one) and `parents:` followed by their ids.
#[derive(Debug, Clone, PartialEq)]
pub struct GitCommitHead {
    /// RFC 3339 UTC, `…Z`.
    pub committed: String,
    pub parents: usize,
}

/// What the git adapter writes after `parents:` for a commit at a shallow
/// clone's cut-off, where git shows none of its parents (a merge there
/// would read as a first commit). Not an id, so `git_commit_head` reads no
/// such record, and the commit never counts.
pub const GIT_PARENTS_NOT_FETCHED: &str = "not fetched";

/// The `committed:` and `parents:` lines of a commit's stored text (its
/// head ends at the first blank line). None unless each is there exactly
/// once and reads — so for every commit an earlier version stored, which
/// wrote neither, and for one at a shallow clone's cut-off, whose parents
/// weren't fetched (`GIT_PARENTS_NOT_FETCHED`): such a record can't say
/// whether the commit is a merge, or when it was made.
pub fn git_commit_head(head: &str) -> Option<GitCommitHead> {
    let mut committed = None;
    let mut parents = None;
    for line in head.lines().take_while(|l| !l.is_empty()) {
        if let Some(at) = line.strip_prefix("committed: ") {
            let at = chrono::DateTime::parse_from_rfc3339(at.trim())
                .ok()?
                .to_utc()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string();
            if committed.replace(at).is_some() {
                return None;
            }
        } else if let Some(list) = line.strip_prefix("parents:") {
            if !(list.is_empty() || list.starts_with(' ')) {
                return None;
            }
            let ids: Vec<&str> = list.split_whitespace().collect();
            if !ids.iter().all(|id| is_commit_id(id)) || parents.replace(ids.len()).is_some() {
                return None;
            }
        }
    }
    Some(GitCommitHead {
        committed: committed?,
        parents: parents?,
    })
}

/// Subjects git, GitHub, GitLab and Bitbucket give a merge by default.
/// The rest of such a message (a pull request's title, a branch name) is
/// written by whoever opened the merged branch.
const MERGE_SUBJECTS: &[&str] = &[
    "Merge pull request #",
    "Merge branch ",
    "Merge branches ",
    "Merge remote-tracking branch ",
    "Merge tag ",
    "Merge commit ",
    "Merged in ",
];

/// Why a commit whose author address is the owner's still isn't the
/// owner's words alone, read from its stored text (None: nothing found):
///
/// - a record without readable `committed:` and `parents:` lines
///   (`git_commit_head` — every commit an earlier version stored, and one
///   at a shallow clone's cut-off): whether it is a merge, and when it was
///   made, can't be read;
/// - a merge (more than one parent, or a default merge subject): its
///   message comes from the merged branch;
/// - a `Co-authored-by:` trailer naming an address that isn't one of the
///   owner's, or none that can be read: someone else — a person, or an
///   agent such as Claude Code, which adds one — wrote it with them;
/// - a text too long to read whole (`COMMIT_TEXT_CHARS`).
///
/// The author line itself is whatever the commit's writer typed: git
/// doesn't check it, so this can't tell a forged author from the owner.
pub fn git_commit_concern(head: &str, identities: &BTreeSet<String>) -> Option<String> {
    if head.chars().count() > COMMIT_TEXT_CHARS {
        return Some("its text is too long to check whole".into());
    }
    let Some(record) = git_commit_head(head) else {
        let not_fetched = format!("parents: {GIT_PARENTS_NOT_FETCHED}");
        if head
            .lines()
            .take_while(|l| !l.is_empty())
            .any(|l| l == not_fetched)
        {
            return Some(
                "it was read from a shallow clone's oldest commits, so whether it is a merge \
                 can't be read"
                    .into(),
            );
        }
        return Some(
            "an earlier version imported it, and its record doesn't say whether it is a merge \
             or when it was made"
                .into(),
        );
    };
    let message: Vec<&str> = head.lines().skip_while(|l| !l.is_empty()).skip(1).collect();
    let subject = message.first().copied().unwrap_or("");
    if record.parents > 1 || MERGE_SUBJECTS.iter().any(|m| subject.starts_with(m)) {
        return Some("it is a merge: its message comes from the merged branch".into());
    }
    for line in &message {
        let line = line.trim();
        let Some(rest) = line
            .get(..15)
            .filter(|t| t.eq_ignore_ascii_case("co-authored-by:"))
            .map(|_| &line[15..])
        else {
            continue;
        };
        let address = rest
            .find('<')
            .and_then(|open| rest[open + 1..].strip_suffix('>'))
            .and_then(owner_address);
        match address {
            Some(a) if identities.contains(&a) => {}
            Some(a) => return Some(format!("co-written by {a}")),
            None => return Some("co-written by someone whose address can't be read".into()),
        }
    }
    None
}

/// The first line the substack adapter writes on a post: the address of
/// the host that served the archive (`served from: https://host`), which
/// the adapter checked the post's own address against. Its host's key, or
/// None — for every post an earlier version stored, which wrote no such
/// line (its first line is `article: …`) and kept a post under whatever
/// address the server's JSON gave.
pub fn substack_served_from(head: &str) -> Option<String> {
    let first = head.lines().next()?;
    let address = first.strip_prefix("served from: ")?.trim();
    let lower = address.to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")).then_some(())?;
    substack_host_key(address)
}

/// Does this source's text have one author per item to check?
fn author_checked(source: &str) -> bool {
    source == "git"
}

/// How much of an item's stored text the judgement reads (None: none):
/// a commit's whole text, a post's first line.
fn text_read(source: &str) -> Option<usize> {
    match source {
        "git" => Some(COMMIT_TEXT_CHARS + 1),
        "substack" => Some(SERVED_LINE_CHARS),
        _ => None,
    }
}

/// How much of a post's text holds its `served from:` line.
const SERVED_LINE_CHARS: usize = 1024;

/// The only writer whose episodes an entry can cover: the source's own
/// importer. On `/v1/ingest` only the owner may name an adapter (any other
/// key is recorded under its own name), and a peer's origin is believed
/// only from a peer key.
fn importer(source: &str) -> String {
    format!("adapter:{source}")
}

/* ————— the owner's addresses ————— */

/// Every address the owner's mail comes from, as the synced node list
/// shows it: each mailbox connected on any of the owner's nodes, and its
/// aliases (`roles.mailboxes`). The daemon's `mailboxes::owner_addresses`
/// adds this node's own mailboxes before they are first published.
pub(crate) fn owner_mailbox_addresses_on(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut stmt = conn.prepare("SELECT roles FROM nodes")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = BTreeSet::new();
    for roles in rows {
        let roles: serde_json::Value = serde_json::from_str(&roles?).unwrap_or_default();
        for m in roles["mailboxes"].as_array().into_iter().flatten() {
            out.extend(m["address"].as_str().and_then(owner_address));
            for a in m["aliases"].as_array().into_iter().flatten() {
                out.extend(a.as_str().and_then(owner_address));
            }
        }
    }
    Ok(out)
}

/// The owner's own addresses, the same on every node: their mail
/// addresses (`owner_mailbox_addresses_on`) and the addresses on their own
/// profile card (`network.profile`, edited only by the owner and mirrored
/// between their nodes). Not the addresses on the owner's Person entity:
/// those include whatever an imported item named that Person with.
pub(crate) fn owner_emails_on(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut out = owner_mailbox_addresses_on(conn)?;
    let card: Option<String> = conn
        .query_row(
            "SELECT value FROM consolidation_state WHERE key = 'network.profile'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(card) = card {
        let card: serde_json::Value = serde_json::from_str(&card).unwrap_or_default();
        for e in card["emails"].as_array().into_iter().flatten() {
            out.extend(e.as_str().and_then(owner_address));
        }
    }
    Ok(out)
}

/// An address as the owner's addresses are read (`owner_emails_on`):
/// trimmed and lowercased, or None for one that isn't an address (no `@`,
/// more than one, spaces or angle brackets) — such a one is never the
/// owner's, wherever it is written.
pub fn owner_address(a: &str) -> Option<String> {
    let a = a.trim().to_lowercase();
    let ok = a.matches('@').count() == 1
        && !a.starts_with('@')
        && !a.ends_with('@')
        && !a.contains(['<', '>'])
        && !a.chars().any(|c| c.is_whitespace() || c.is_control());
    ok.then_some(a)
}

/* ————— the read-time judgement ————— */

/// One episode as the judgement reads it.
pub(crate) struct Candidate {
    pub source: String,
    pub source_ref: String,
    pub trust: String,
    /// Who first wrote it (its recorded origin, or — written here before
    /// origins were kept — its own actor); None when unknown.
    pub writer: Option<String>,
    /// The start of its stored text, as much as `text_read` says (sources
    /// whose text the judgement reads only).
    pub head: Option<String>,
    /// When it happened (RFC 3339 UTC), if known.
    pub when: Option<String>,
}

/// The columns `Candidate::from_row` reads, starting at `at`, over an
/// `episodes` row aliased `e` joined with `candidate_joins`.
pub(crate) fn candidate_cols() -> String {
    let read: String = OWNABLE_SOURCES
        .iter()
        .filter_map(|s| {
            text_read(s.source).map(|n| {
                format!(
                    "WHEN e.source = '{}' THEN substr(ff.body, 1, {n}) ",
                    s.source
                )
            })
        })
        .collect();
    format!(
        "e.source, e.source_ref, e.trust,
         CASE WHEN oo.episode_id IS NOT NULL THEN oo.writer
              WHEN e.actor NOT LIKE 'peer:%' THEN e.actor END,
         CASE {read}END,
         e.event_time"
    )
}

/// The joins `candidate_cols` needs (the FTS row by its rowid pointer).
pub(crate) fn candidate_joins() -> &'static str {
    "LEFT JOIN episode_origin oo ON oo.episode_id = e.id
     LEFT JOIN episode_fts ff ON ff.rowid = e.fts_rowid /* all-episodes: provenance of any episode, retired included */"
}

impl Candidate {
    pub(crate) fn from_row(row: &rusqlite::Row, at: usize) -> rusqlite::Result<Self> {
        Ok(Candidate {
            source: row.get(at)?,
            source_ref: row.get(at + 1)?,
            trust: row.get(at + 2)?,
            writer: row.get(at + 3)?,
            head: row.get(at + 4)?,
            when: row.get(at + 5)?,
        })
    }
}

/// The longest one Claude Code run can last: the longest timeout an agent
/// can have (a day — the daemon's agents/ clamps `timeout_secs` to it, and
/// the run is stopped there) plus the five minutes the runner waits past it
/// before it calls the task failed.
pub(crate) const LONGEST_AGENT_RUN_SECS: i64 = 86_400 + 300;

/// The harness whose runs Hearthloom stops itself at their time limit:
/// Claude Code runs as the daemon's child and is killed there. Hermes and
/// OpenClaw run on their own — Hearthloom only waits on a request, or sends
/// the task — so nothing says a run there has stopped until it is closed.
const STOPPED_HARNESS: &str = "claude_code";

/// One run of one of the owner's agents never closed: from its start to the
/// latest it can end (None: it may still be going).
struct AgentRun {
    task: String,
    from: String,
    to: Option<String>,
}

/// A run that finished from ?1 to ?3 and started at or before ?2 — one
/// going at some time between ?1 and ?2 — if any (by
/// `idx_agent_tasks_finished`). With ?3 a day and five minutes past ?2
/// (`LONGEST_AGENT_RUN_SECS`), every run of a day or less going then
/// finished in that window: the first run that started and finished by ?2
/// answers, else only the runs that finished within the day after it are
/// read —
/// never the history before ?1 or after ?3, however long it grows.
const RUN_FINISHED_DURING: &str = "SELECT id FROM agent_tasks
     WHERE finished_at >= ?1 AND finished_at <= ?3 AND started_at <= ?2 LIMIT 1";

/// A run that lasted more than a day, as many digits long in whole days as
/// ?3 (1 to 6), that finished from ?1 to ?4 and started at or before ?2, if
/// any: by `idx_agent_tasks_span`, which holds only the runs that may have
/// lasted more than a day (a Claude Code run is stopped within the day; a
/// run on a harness Hearthloom doesn't stop can go on longer), by that
/// length and when they finished. A run of ?3 digits going by ?2 finished
/// less than 10^?3 days past it, so with ?4 that time only the runs of its
/// length that finished then are read — never the later history. Its first
/// three conditions are the index's, word for word (store/mod.rs, v73), or
/// the index isn't used.
const LONG_RUN_DURING: &str = "SELECT id FROM agent_tasks
     WHERE started_at IS NOT NULL
       AND IFNULL(julianday(finished_at) - julianday(started_at), 2) > 1
       AND IFNULL(min(length(CAST(julianday(finished_at) - julianday(started_at) AS INTEGER)), 7), 7) = ?3
       AND finished_at >= ?1 AND finished_at <= ?4 AND started_at <= ?2 LIMIT 1";

/// The same among the runs whose length the index gives as 7: times that
/// can't be read, or a million days or more apart — none a clock wrote, so
/// they are read with no bound on when they finished.
const ODD_RUN_DURING: &str = "SELECT id FROM agent_tasks
     WHERE started_at IS NOT NULL
       AND IFNULL(julianday(finished_at) - julianday(started_at), 2) > 1
       AND IFNULL(min(length(CAST(julianday(finished_at) - julianday(started_at) AS INTEGER)), 7), 7) = 7
       AND finished_at >= ?1 AND started_at <= ?2 LIMIT 1";

/// Runs never closed, with the harness each started on
/// (`agent_tasks.harness`, kept as the run starts — never the agent's
/// setting now, which the owner can change while a run goes on).
const RUNS_NOT_CLOSED: &str = "SELECT id, started_at, updated_at, harness FROM agent_tasks
     WHERE finished_at IS NULL AND started_at IS NOT NULL";

/// The runs of the owner's agents, on any of their nodes (`agent_tasks`
/// syncs), that were never closed — its agent deleted mid-run, its node
/// gone. Such a run ended no later than the longest run can last past its
/// latest stamp (a run writes one as it starts, acts or reads) when it
/// started on a harness Hearthloom stops itself (`STOPPED_HARNESS`). Any
/// other — Hermes, OpenClaw, a run started again on another harness, or
/// one that started before the harness was kept — may still be going.
fn open_runs_on(conn: &Connection) -> Result<Vec<AgentRun>> {
    let mut stmt = conn.prepare(RUNS_NOT_CLOSED)?;
    let open = stmt.query_map([], |r| {
        let from: String = r.get(1)?;
        let updated: String = r.get(2)?;
        let harness: Option<String> = r.get(3)?;
        let to = if harness.as_deref() == Some(STOPPED_HARNESS) {
            latest_possible_end(std::cmp::max(&from, &updated))
        } else {
            None
        };
        Ok(AgentRun {
            task: r.get(0)?,
            from,
            to,
        })
    })?;
    open.collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
}

/// The latest an unfinished run can end, given its latest stamp (None: a
/// stamp that can't be read).
fn latest_possible_end(stamp: &str) -> Option<String> {
    time_after(stamp, LONGEST_AGENT_RUN_SECS)
}

/// The time `secs` after a stamp, written as stored times are (RFC 3339 UTC,
/// whole seconds); past the year 9999, which no stored time can be in
/// (SQLite's date functions read none later), the last second of it. None:
/// a stamp that can't be read.
fn time_after(stamp: &str, secs: i64) -> Option<String> {
    use chrono::Datelike;
    let at = chrono::DateTime::parse_from_rfc3339(stamp.trim())
        .ok()?
        .to_utc();
    Some(
        match at.checked_add_signed(chrono::Duration::seconds(secs)) {
            Some(end) if end.year() <= 9999 => end.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            _ => "9999-12-31T23:59:59Z".into(),
        },
    )
}

/// What the owner's list says about one episode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Judgement {
    pub source: String,
    /// The key of the part of its source it belongs to (None: unreadable).
    pub key: Option<String>,
    /// The entry that names that part, if the owner listed it.
    pub entry: Option<String>,
    /// Does it count as the owner's own words?
    pub counts: bool,
    /// Why not, when it doesn't.
    pub why: Option<String>,
}

/// The owner's list, loaded once for a check; the owner's addresses are
/// read the first time an episode needs them (an owner's commit in a marked
/// repository), and their agents' runs only for such a commit: the runs
/// never closed once per check, and one look for a finished run during the
/// commit's own time.
pub(crate) struct OwnedWords<'c> {
    conn: &'c Connection,
    entries: HashSet<(String, String)>,
    identities: std::cell::OnceCell<BTreeSet<String>>,
    /// The owner's agents' runs never closed: a commit made during one may
    /// be the agent's, under the owner's git identity.
    open_runs: std::cell::OnceCell<Vec<AgentRun>>,
    /// How many runs this check has read.
    #[cfg(test)]
    runs_read: std::cell::Cell<usize>,
}

impl<'c> OwnedWords<'c> {
    pub(crate) fn load(conn: &'c Connection) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT source, key FROM owned_sources")?;
        let entries = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        Ok(OwnedWords {
            conn,
            entries,
            identities: std::cell::OnceCell::new(),
            open_runs: std::cell::OnceCell::new(),
            #[cfg(test)]
            runs_read: std::cell::Cell::new(0),
        })
    }

    fn identities(&self) -> Result<&BTreeSet<String>> {
        if let Some(known) = self.identities.get() {
            return Ok(known);
        }
        let read = owner_emails_on(self.conn)?;
        Ok(self.identities.get_or_init(|| read))
    }

    /// An agent task that was running at any time between these two (in
    /// either order), if any. Times are RFC 3339 UTC with a `Z`, so they
    /// compare as text. A run never closed is looked for among those read
    /// for this check; a finished one in two looks: a run of a day or less
    /// among those that finished from the earlier time to a day past the
    /// later one (`RUN_FINISHED_DURING`), and a longer one among those that
    /// lasted more than a day, a length at a time (`long_run_between`). The
    /// rest of the task history is never read.
    fn agent_run_between(&self, a: &str, b: &str) -> Result<Option<String>> {
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        let open = match self.open_runs.get() {
            Some(open) => open,
            None => {
                let read = open_runs_on(self.conn)?;
                #[cfg(test)]
                self.runs_read.set(self.runs_read.get() + read.len());
                self.open_runs.get_or_init(|| read)
            }
        };
        if let Some(run) = open
            .iter()
            .find(|r| r.from.as_str() <= to && r.to.as_deref().is_none_or(|end| from <= end))
        {
            return Ok(Some(run.task.clone()));
        }
        // `judge` passes only times that read as times
        let until = latest_possible_end(to).ok_or_else(|| anyhow!("unreadable time {to:?}"))?;
        let mut finished: Option<String> = self
            .conn
            .query_row(RUN_FINISHED_DURING, [from, to, &until], |r| r.get(0))
            .optional()?;
        if finished.is_none() {
            finished = self.long_run_between(from, to)?;
        }
        #[cfg(test)]
        self.runs_read
            .set(self.runs_read.get() + usize::from(finished.is_some()));
        Ok(finished)
    }

    /// A run that lasted more than a day and was going at some time from
    /// `from` to `to`, if any. Its length in whole days has 1 to 6 digits
    /// (`LONG_RUN_DURING`, one look per count: a run under 10^d days that
    /// started by `to` finished by 10^d days past it, so a look reads only
    /// the runs of that length that finished then), or it is one no clock
    /// wrote (`ODD_RUN_DURING`).
    fn long_run_between(&self, from: &str, to: &str) -> Result<Option<String>> {
        let mut by_length = self.conn.prepare_cached(LONG_RUN_DURING)?;
        for digits in 1..=6u32 {
            let until = time_after(to, 10_i64.pow(digits) * 86_400)
                .ok_or_else(|| anyhow!("unreadable time {to:?}"))?;
            let found: Option<String> = by_length
                .query_row(rusqlite::params![from, to, digits, until], |r| r.get(0))
                .optional()?;
            if found.is_some() {
                return Ok(found);
            }
        }
        Ok(self
            .conn
            .query_row(ODD_RUN_DURING, [from, to], |r| r.get(0))
            .optional()?)
    }

    /// How many runs this check has read (for tests).
    #[cfg(test)]
    fn runs_read(&self) -> usize {
        self.runs_read.get()
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.entries.iter().any(|(s, _)| ownable(s).is_some())
    }

    /// `'git','substack'` — the listed sources this build can judge (names
    /// from `OWNABLE_SOURCES`, never from a row), for an SQL `IN`.
    pub(crate) fn sources_sql(&self) -> String {
        let list: Vec<String> = OWNABLE_SOURCES
            .iter()
            .filter(|s| self.entries.iter().any(|(src, _)| src == s.source))
            .map(|s| format!("'{}'", s.source))
            .collect();
        if list.is_empty() {
            "NULL".into()
        } else {
            list.join(",")
        }
    }

    /// Does an entry make this episode the owner's own words?
    pub(crate) fn covers(&self, c: &Candidate) -> Result<bool> {
        Ok(self.judge(c)?.counts)
    }

    pub(crate) fn judge(&self, c: &Candidate) -> Result<Judgement> {
        let mut j = Judgement {
            source: c.source.clone(),
            key: None,
            entry: None,
            counts: false,
            why: None,
        };
        if ownable(&c.source).is_none() {
            j.why = not_ownable_reason(&c.source);
            return Ok(j);
        }
        j.key = key_of_ref(&c.source, &c.source_ref);
        let Some(key) = &j.key else {
            j.why = Some(match c.source.as_str() {
                "git" => "its repository has no remote address to name it by".into(),
                _ => "its address can't be read".into(),
            });
            return Ok(j);
        };
        if !self.entries.contains(&(c.source.clone(), key.clone())) {
            j.why = Some("not marked as yours".into());
            return Ok(j);
        }
        j.entry = Some(owned_id(&c.source, key));
        if !matches!(c.trust.as_str(), "self" | "imported") {
            j.why = Some(format!("it was let in with trust {}", c.trust));
            return Ok(j);
        }
        match &c.writer {
            Some(w) if *w == importer(&c.source) => {}
            Some(w) => {
                j.why = Some(format!(
                    "it was written by {w}, not the {} import",
                    c.source
                ));
                return Ok(j);
            }
            None => {
                j.why = Some("who first wrote it isn't known".into());
                return Ok(j);
            }
        }
        if author_checked(&c.source) {
            let author = c
                .head
                .as_deref()
                .and_then(|head| git_author(&c.source_ref, head));
            let Some(author) = author else {
                j.why = Some("its author can't be read".into());
                return Ok(j);
            };
            // read only now: most episodes never get this far
            let identities = self.identities()?;
            if identities.is_empty() {
                j.why = Some(format!(
                    "by {author}, and none of your own addresses is known yet"
                ));
                return Ok(j);
            }
            if !identities.contains(&author) {
                j.why = Some(format!("by {author}, not one of your addresses"));
                return Ok(j);
            }
            let head = c.head.as_deref().unwrap_or_default();
            if let Some(concern) = git_commit_concern(head, identities) {
                j.why = Some(concern);
                return Ok(j);
            }
            // an agent working in the owner's checkout commits under the
            // owner's git identity (an agent's run is never the owner's
            // words). An amend, rebase or cherry-pick keeps the author date
            // and sets the commit time, and the next one sets it again: an
            // agent's rewrite in between leaves no time of its own, so a
            // run anywhere from the author date to the commit time counts
            let authored = c
                .when
                .as_deref()
                .filter(|at| latest_possible_end(at).is_some());
            let (Some(authored), Some(record)) = (authored, git_commit_head(head)) else {
                j.why = Some("when it was made isn't known".into());
                return Ok(j);
            };
            if let Some(task) = self.agent_run_between(authored, &record.committed)? {
                j.why = Some(format!(
                    "made or rewritten while one of your agents was at work (task {task})"
                ));
                return Ok(j);
            }
        }
        if c.source == "substack" {
            // the adapter keeps a post only under the host that served it
            // and says which host that was; an earlier version did neither
            match c.head.as_deref().and_then(substack_served_from) {
                None => {
                    j.why = Some(
                        "an earlier version imported it without checking that this \
                         publication served it"
                            .into(),
                    );
                    return Ok(j);
                }
                Some(host) if host != *key => {
                    j.why = Some(format!("{host} served it, not {key}"));
                    return Ok(j);
                }
                Some(_) => {}
            }
        }
        j.counts = true;
        Ok(j)
    }
}

/* ————— the list ————— */

/// One entry of the owner's list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnedSourceRow {
    pub id: String,
    pub source: String,
    pub key: String,
    /// Who set it (`session`, `key:<name>`, or the node it came from).
    pub set_by: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One change to the list, from the operational journal: an entry added
/// (`create`) or taken off (`delete`), or the owner's addresses changed
/// (`addresses`: what was added and removed, and where).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnedSourceChange {
    pub op: String,
    pub source: Option<String>,
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    /// Where an address change was made (`profile card`, `aliases of …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    pub actor: String,
    pub ts: String,
}

/// The journal entity an address change is recorded under (kind
/// `owned_source`, op `addresses`: the journal leg carries it to the
/// owner's other nodes; only `delete` entries act there).
const ADDRESSES_ENTITY: &str = "addresses";

fn row_of(r: &rusqlite::Row) -> rusqlite::Result<OwnedSourceRow> {
    Ok(OwnedSourceRow {
        id: r.get(0)?,
        source: r.get(1)?,
        key: r.get(2)?,
        set_by: r.get(3)?,
        created_at: r.get(4)?,
        updated_at: r.get(5)?,
    })
}

const ROW_COLS: &str = "id, source, key, set_by, created_at, updated_at";

impl Store {
    /// The owner's list, oldest first.
    pub fn owned_sources(&self) -> Result<Vec<OwnedSourceRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ROW_COLS} FROM owned_sources ORDER BY created_at, id"
        ))?;
        let rows = stmt.query_map([], row_of)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// The entry for what the owner gave, if listed: (key, entry).
    pub fn owned_source_lookup(
        &self,
        source: &str,
        given: &str,
    ) -> Result<(String, Option<OwnedSourceRow>)> {
        let key = owned_key(source, given)?;
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!("SELECT {ROW_COLS} FROM owned_sources WHERE id = ?1"),
                [owned_id(source, &key)],
                row_of,
            )
            .optional()?;
        Ok((key, row))
    }

    /// Mark part of a source as the owner's. The caller has checked that
    /// the owner asked, with the vault open. Returns the entry and whether
    /// it is new (adding a listed one changes nothing).
    pub fn owned_source_add(
        &self,
        actor: &Actor,
        source: &str,
        given: &str,
        set_by: &str,
    ) -> Result<(OwnedSourceRow, bool)> {
        let key = owned_key(source, given)?;
        let id = owned_id(source, &key);
        let conn = self.lock();
        if let Some(row) = conn
            .query_row(
                &format!("SELECT {ROW_COLS} FROM owned_sources WHERE id = ?1"),
                [&id],
                row_of,
            )
            .optional()?
        {
            return Ok((row, false));
        }
        // Stamped after the entry's last removal (here or received): a
        // tombstone at or after a row's stamp removes it on every node, and
        // stamps have whole seconds — a re-add in the removal's second would
        // stand here and be refused everywhere else.
        let removed: Option<String> = conn.query_row(
            "SELECT MAX(ts) FROM op_journal
             WHERE entity_kind = 'owned_source' AND entity_id = ?1 AND op = 'delete'",
            [&id],
            |r| r.get(0),
        )?;
        let now = match removed {
            Some(removed) => super::repair::stamp_after(&removed),
            None => now_utc(),
        };
        conn.execute(
            "INSERT INTO owned_sources (id, source, key, set_by, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            rusqlite::params![id, source, key, set_by, now],
        )?;
        record_mutation(
            &conn,
            actor,
            "owned_source_added",
            &format!("owned_source_added:{id}:{now}"),
            &json!({"id": id, "source": source, "key": key, "set_by": set_by}),
        )?;
        super::journal_row(&conn, actor, "owned_source", "owned_sources", &id, "create")?;
        let row = OwnedSourceRow {
            id,
            source: source.into(),
            key,
            set_by: set_by.into(),
            created_at: now.clone(),
            updated_at: now,
        };
        Ok((row, true))
    }

    /// Take an entry off the list (its text is outside text again). The
    /// journal tombstone, written first, carries the removal to the
    /// owner's other nodes.
    pub fn owned_source_remove(&self, actor: &Actor, id: &str) -> Result<Option<OwnedSourceRow>> {
        let conn = self.lock();
        let Some(row) = conn
            .query_row(
                &format!("SELECT {ROW_COLS} FROM owned_sources WHERE id = ?1"),
                [id],
                row_of,
            )
            .optional()?
        else {
            return Ok(None);
        };
        // never before the row's own stamp (a re-add can be stamped a
        // second ahead): a tombstone removes a row only at or after it
        let ts = std::cmp::max(now_utc(), row.updated_at.clone());
        let image = super::row_as_json(&conn, "owned_sources", id)?;
        super::journal_op_at(
            &conn,
            actor,
            "owned_source",
            id,
            "delete",
            image.as_ref(),
            &ts,
        )?;
        conn.execute("DELETE FROM owned_sources WHERE id = ?1", [id])?;
        record_mutation(
            &conn,
            actor,
            "owned_source_removed",
            &format!("owned_source_removed:{id}:{ts}"),
            &json!({"id": id, "source": row.source, "key": row.key}),
        )?;
        Ok(Some(row))
    }

    /// The latest changes to the list, newest first — on this node or
    /// received from another of the owner's nodes (`peer:<name>`).
    pub fn owned_source_history(&self, limit: usize) -> Result<Vec<OwnedSourceChange>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT op, post_image, actor, ts FROM op_journal
             WHERE entity_kind = 'owned_source' ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            let image: Option<String> = r.get(1)?;
            let image: serde_json::Value = image
                .and_then(|i| serde_json::from_str(&i).ok())
                .unwrap_or_default();
            let list = |k: &str| -> Vec<String> {
                image[k]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a.as_str().map(String::from))
                    .collect()
            };
            Ok(OwnedSourceChange {
                op: r.get(0)?,
                source: image["source"].as_str().map(String::from),
                key: image["key"].as_str().map(String::from),
                added: list("added"),
                removed: list("removed"),
                place: image["place"].as_str().map(String::from),
                actor: r.get(2)?,
                ts: r.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Record a change to the owner's addresses (the profile card's, a
    /// mailbox's aliases) in the list's history: they decide whose commits
    /// in a marked repository count. Nothing is recorded when nothing
    /// changed.
    pub fn owned_addresses_changed(
        &self,
        actor: &Actor,
        place: &str,
        before: &BTreeSet<String>,
        after: &BTreeSet<String>,
        set_by: &str,
    ) -> Result<()> {
        let added: Vec<&String> = after.difference(before).collect();
        let removed: Vec<&String> = before.difference(after).collect();
        if added.is_empty() && removed.is_empty() {
            return Ok(());
        }
        let image = json!({"place": place, "added": added, "removed": removed, "set_by": set_by});
        let conn = self.lock();
        record_mutation(
            &conn,
            actor,
            "owner_addresses_changed",
            &format!("owner_addresses_changed:{place}:{}", now_utc()),
            &image,
        )?;
        super::journal_op(
            &conn,
            actor,
            "owned_source",
            ADDRESSES_ENTITY,
            "addresses",
            Some(&image),
        )
    }

    /// How many memories here each entry's key names (by entry id; entries
    /// naming none are absent): an entry that names nothing stored — a
    /// publication marked by its substack.com address whose posts are
    /// filed under its own domain — changes nothing, and the list says so.
    /// Counted as the viewer sees the record: private memories only with
    /// their vault open.
    pub fn owned_source_stored(&self) -> Result<std::collections::HashMap<String, usize>> {
        let entries = self.owned_sources()?;
        let mut keys: std::collections::BTreeMap<&str, BTreeSet<String>> = Default::default();
        for e in &entries {
            keys.entry(e.source.as_str())
                .or_default()
                .insert(e.key.clone());
        }
        let mut stored = std::collections::HashMap::new();
        for (source, keys) in keys {
            for (key, n) in self.stored_by_key(source, &keys)?.0 {
                stored.insert((source.to_string(), key), n);
            }
        }
        Ok(entries
            .into_iter()
            .filter_map(|e| stored.get(&(e.source, e.key)).map(|n| (e.id, *n)))
            .collect())
    }

    /// How many memories here one entry's key names (see
    /// `owned_source_stored`).
    pub fn owned_entry_stored(&self, entry: &OwnedSourceRow) -> Result<usize> {
        let keys = BTreeSet::from([entry.key.clone()]);
        Ok(self
            .stored_by_key(&entry.source, &keys)?
            .0
            .get(&entry.key)
            .copied()
            .unwrap_or(0))
    }

    /// How many memories here of `source` each of `keys` names (keys naming
    /// none are absent), and the database's work for it (virtual machine
    /// steps). The source's refs are read a part at a time — a repository's
    /// commits, a publication's posts: each part is a range of the
    /// `(source, source_ref)` index (`ref_part`) — and a part is read only
    /// when its key is asked for; of every other part only its first ref is
    /// looked at, to step past it. So a count costs what the keys name plus
    /// one look per part, never every memory of the source.
    fn stored_by_key(
        &self,
        source: &str,
        keys: &BTreeSet<String>,
    ) -> Result<(std::collections::HashMap<String, usize>, i64)> {
        use rusqlite::StatementStatus::VmStep;
        let mut stored = std::collections::HashMap::new();
        if ownable(source).is_none() || keys.is_empty() {
            return Ok((stored, 0));
        }
        // a commit whose remote address holds a `#` sits in the part of the
        // address before it, under a key that holds a `#` (`ref_part`). Such
        // a key starts with that part's key — `git_repo_key` reads the
        // address up to the `#` as it reads the whole, only shorter — or the
        // part has none: then its start reads as an address once the `#`
        // follows it (its path was `.`, `..`, `.git` or empty), or it holds
        // no `/` or `:` (`x#y@host:path` — a user name with a `#` — whose
        // key needn't hold one). A key is looked for only in a part that
        // may hold it: never in other repositories, or local paths
        let hashed: Vec<&str> = keys
            .iter()
            .filter(|k| k.contains('#'))
            .map(String::as_str)
            .collect();
        let may_hold = |start: &str, key: Option<String>| match key {
            Some(k) => keys.contains(&k) || hashed.iter().any(|h| h.starts_with(k.as_str())),
            None => {
                source == "git"
                    && (!start.contains(['/', ':'])
                        || (!hashed.is_empty() && git_repo_key(start).is_some()))
            }
        };
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        // the next ref at or past a point, whatever its memory (only to
        // step from one part to the next: what is counted is read below)
        let mut next_from = conn.prepare(
            "SELECT source_ref FROM episodes /* all-episodes: finds where the next part of a source starts; nothing read here is counted */
             WHERE source = ?1 AND source_ref >= ?2 ORDER BY source_ref LIMIT 1",
        )?;
        let mut next_after = conn.prepare(
            "SELECT source_ref FROM episodes /* all-episodes: finds where the next part of a source starts; nothing read here is counted */
             WHERE source = ?1 AND source_ref > ?2 ORDER BY source_ref LIMIT 1",
        )?;
        let mut part = conn.prepare(&format!(
            "SELECT source_ref FROM episodes
             WHERE source = ?1 AND source_ref >= ?2 AND source_ref < ?3
               AND superseded_by IS NULL{guard}"
        ))?;
        let mut alone = conn.prepare(&format!(
            "SELECT source_ref FROM episodes
             WHERE source = ?1 AND source_ref = ?2 AND superseded_by IS NULL{guard}"
        ))?;
        let mut tally = |rows: &mut rusqlite::Rows| -> Result<()> {
            while let Some(row) = rows.next()? {
                let source_ref: String = row.get(0)?;
                if let Some(key) = key_of_ref(source, &source_ref).filter(|k| keys.contains(k)) {
                    *stored.entry(key).or_insert(0) += 1;
                }
            }
            Ok(())
        };
        // where the next look starts: at a part's end, or just past a ref
        let mut at: Option<(String, bool)> = Some((String::new(), true));
        while let Some((point, inclusive)) = at.take() {
            let next = if inclusive {
                &mut next_from
            } else {
                &mut next_after
            };
            let Some(first): Option<String> = next
                .query_row(rusqlite::params![source, point], |r| r.get(0))
                .optional()?
            else {
                break;
            };
            match ref_part(source, &first) {
                Some((start, key)) => {
                    let end = part_end(start);
                    if may_hold(start, key) {
                        tally(&mut part.query(rusqlite::params![source, start, end])?)?;
                    }
                    at = Some((end, true));
                }
                None => {
                    // a ref alone is read only when its own key is asked for
                    if key_of_ref(source, &first).is_some_and(|k| keys.contains(&k)) {
                        tally(&mut alone.query(rusqlite::params![source, first])?)?;
                    }
                    at = Some((first, false));
                }
            }
        }
        let work = [&next_from, &next_after, &part, &alone]
            .iter()
            .map(|s| i64::from(s.get_status(VmStep)))
            .sum();
        Ok((stored, work))
    }

    /// Does the list name a source whose items are judged by their author
    /// (a repository)? Then the owner's addresses decide what counts.
    pub fn owned_list_reads_addresses(&self) -> Result<bool> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT DISTINCT source FROM owned_sources")?;
        let sources = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(sources.iter().any(|s| author_checked(s)))
    }

    /// The owner's own addresses (see `owner_emails_on`).
    pub fn owner_emails(&self) -> Result<BTreeSet<String>> {
        owner_emails_on(&self.lock())
    }

    /// The owner's mail addresses from the synced node list (see
    /// `owner_mailbox_addresses_on`).
    pub fn owner_mailbox_addresses(&self) -> Result<BTreeSet<String>> {
        owner_mailbox_addresses_on(&self.lock())
    }

    /// What the owner's list says about one episode of an ownable source
    /// (None: another source, or no such episode). For the owner's view.
    pub fn owned_judgement(&self, episode_id: &str) -> Result<Option<Judgement>> {
        let conn = self.lock();
        let candidate = conn
            .query_row(
                &format!(
                    "SELECT {} FROM episodes e /* all-episodes: the owner's view of any memory */ {}
                     WHERE e.id = ?1",
                    candidate_cols(),
                    candidate_joins()
                ),
                [episode_id],
                |r| Candidate::from_row(r, 0),
            )
            .optional()?;
        let Some(candidate) = candidate.filter(|c| ownable(&c.source).is_some()) else {
            return Ok(None);
        };
        Ok(Some(OwnedWords::load(&conn)?.judge(&candidate)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EntityRef, EpisodeDraft, IngestBatch};
    use crate::paths::Home;
    use crate::time::{EventTime, TimeUncertainty};

    const SHA_A: &str = "1111111111111111111111111111111111111111";
    const SHA_B: &str = "2222222222222222222222222222222222222222";
    const PARENT: &str = "3333333333333333333333333333333333333333";
    const REPO: &str = "https://git.example.com/owner/project.git";

    fn commit(sha: &str, name: &str, email: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "git".into(),
            source_ref: format!("{REPO}#{sha}"),
            kind: "commit".into(),
            title: format!("change by {name}"),
            body: format!(
                "commit {sha}\nrepo: {REPO}\nauthor: {name} <{email}>\n\
                 committed: 2026-11-01T00:00:00Z\nparents: {PARENT}\n\nchange by {name}"
            ),
            event_time: EventTime::parse_rfc3339("2026-11-01T00:00:00Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: "imported".into(),
            scope: Some("work".into()),
            entities: vec![EntityRef {
                entity_type: "Person".into(),
                name: name.into(),
                attributes: vec![],
                role: "author".into(),
            }],
            edges: vec![],
        }
    }

    /// A post as the substack adapter writes it, served by the host of its
    /// own address.
    fn post(url: &str) -> EpisodeDraft {
        let served = url.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
        post_served(url, &format!("served from: {served}\n"))
    }

    /// A post whose text begins with `first` (an earlier version wrote
    /// none: its text begins `article: …`).
    fn post_served(url: &str, first: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "substack".into(),
            source_ref: url.into(),
            kind: "article".into(),
            title: "a post".into(),
            body: format!("{first}article: a post\nurl: {url}\n\nwords"),
            event_time: EventTime::parse_rfc3339("2026-11-01T00:00:00Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: "imported".into(),
            scope: Some("work".into()),
            entities: vec![],
            edges: vec![],
        }
    }

    fn node(tmp: &std::path::Path, name: &str) -> (Home, Store) {
        let home = Home::at(tmp.join(name));
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        (home, store)
    }

    fn apply(store: &Store, home: &Home, actor: &str, episodes: Vec<EpisodeDraft>) {
        store
            .apply_batch(
                home,
                &IngestBatch {
                    actor: actor.into(),
                    session_id: None,
                    episodes,
                },
            )
            .unwrap();
    }

    fn set_card(store: &Store, emails: &[&str]) {
        store
            .set_consolidation_state(
                "network.profile",
                &json!({"name": "Owner", "emails": emails}).to_string(),
            )
            .unwrap();
    }

    fn untrusted(store: &Store, ids: &[String]) -> Option<String> {
        store.first_untrusted_episode(ids).unwrap()
    }

    fn person(store: &Store, name: &str) -> String {
        store
            .lock()
            .query_row("SELECT id FROM entities WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn test_repository_keys_compare_host_and_path_without_case_or_credentials() {
        let key = Some("github.example/owner/repo".to_string());
        for given in [
            "https://github.example/owner/repo.git",
            "https://GitHub.Example/Owner/Repo",
            "https://user:token@github.example/owner/repo.git/",
            "https://github.example:443/owner/repo",
            "ssh://git@github.example/owner/repo.git",
            "ssh://git@github.example:22/owner/repo",
            "git@github.example:owner/repo.git",
            "git@github.example:Owner/Repo",
            "github.example/owner/repo",
        ] {
            assert_eq!(git_repo_key(given), key, "{given}");
        }
        assert_eq!(
            git_repo_key("ssh://git@github.example:2222/owner/repo").as_deref(),
            Some("github.example:2222/owner/repo")
        );
        for refused in [
            "/Users/you/code/repo",
            "file:///srv/repo.git",
            "repo",
            "https://github.example/",
            "https://github.example/owner/../repo",
            "C:\\code\\repo",
            "",
            // fetched from evil.example: its authority ends at `#` or `?`
            "https://evil.example#@github.example/owner/repo",
            "https://evil.example?@github.example/owner/repo",
            "ssh://evil.example#@github.example/owner/repo.git",
        ] {
            assert_eq!(git_repo_key(refused), None, "{refused}");
        }
        assert_eq!(
            key_of_ref(
                "git",
                &format!("https://evil.example#@github.example/owner/repo#{SHA_A}")
            ),
            None
        );
        assert_eq!(
            key_of_ref("git", &format!("{REPO}#{SHA_A}")).as_deref(),
            Some("git.example.com/owner/project")
        );
        assert_eq!(key_of_ref("git", &format!("{REPO}#main")), None);
        assert_eq!(key_of_ref("git", &format!("/home/you/repo#{SHA_A}")), None);
    }

    #[test]
    fn test_publication_keys_are_hosts() {
        for given in [
            "owner.substack.com",
            "https://owner.substack.com/",
            "https://www.owner.substack.com/p/a-post",
            "HTTPS://Owner.Substack.com:443",
        ] {
            assert_eq!(
                substack_host_key(given).as_deref(),
                Some("owner.substack.com"),
                "{given}"
            );
        }
        for refused in ["ftp://owner.substack.com", "localhost", "a@b.example", ""] {
            assert_eq!(substack_host_key(refused), None, "{refused}");
        }
        // a post's ref is read only as a full address
        assert_eq!(key_of_ref("substack", "a-post-slug"), None);
    }

    #[test]
    fn test_a_commit_author_is_read_only_from_a_matching_head() {
        let r = format!("{REPO}#{SHA_A}");
        let head = |sha: &str, repo: &str, author: &str| {
            format!("commit {sha}\nrepo: {repo}\nauthor: {author}\n\nsubject")
        };
        assert_eq!(
            git_author(&r, &head(SHA_A, REPO, "Owner Name <me@own.example>")).as_deref(),
            Some("me@own.example")
        );
        // the text of another commit, or another repository
        assert_eq!(
            git_author(&r, &head(SHA_B, REPO, "O <me@own.example>")),
            None
        );
        assert_eq!(
            git_author(
                &r,
                &head(SHA_A, "https://elsewhere.example/r", "O <me@own.example>")
            ),
            None
        );
        // no address, or one that doesn't read as one
        for author in [
            "Owner Name",
            "O <>",
            "O <a<b@own.example>",
            "O <me@own.example> x",
        ] {
            assert_eq!(git_author(&r, &head(SHA_A, REPO, author)), None, "{author}");
        }
    }

    #[test]
    fn test_only_sources_with_a_checkable_author_can_be_marked() {
        for source in [
            "imessage",
            "chrome",
            "claude-code",
            "openai",
            "chatlog-claude",
            "chatlog-chatgpt",
            "ics",
            "rewind",
            "ios-scan",
            "ios-media",
            "mbox",
            "desk",
        ] {
            let err = owned_key(source, "anything").unwrap_err().to_string();
            assert!(err.contains("can't be marked as yours"), "{source}: {err}");
        }
        assert!(owned_key("imessage", "x")
            .unwrap_err()
            .to_string()
            .contains("other people's messages"));
        assert!(owned_key("git", "/Users/you/repo").is_err());
        assert!(owned_key("git", REPO).is_ok());
        assert!(owned_key("substack", "owner.substack.com").is_ok());
    }

    /// An owned repository: the owner's commits stop tainting, another
    /// author's commits in the same repository still taint, and taking the
    /// entry off the list brings the taint back.
    #[test]
    fn test_an_owned_repository_counts_only_the_owners_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        apply(
            &store,
            &home,
            "adapter:git",
            vec![
                commit(SHA_A, "Owner Name", "me@own.example"),
                commit(SHA_B, "Ann Contributor", "ann@else.example"),
            ],
        );
        set_card(&store, &["Me@Own.example"]);
        let mine = crate::ids::episode_id("git", &format!("{REPO}#{SHA_A}"));
        let theirs = crate::ids::episode_id("git", &format!("{REPO}#{SHA_B}"));
        let owner = person(&store, "Owner Name");
        let ann = person(&store, "Ann Contributor");

        // not listed: both commits are outside text
        assert_eq!(
            untrusted(&store, std::slice::from_ref(&mine)),
            Some(mine.clone())
        );
        assert!(store
            .entity_names_unverified(std::slice::from_ref(&owner))
            .unwrap()
            .is_some());

        let actor = Actor::new("human:owner");
        let (entry, new) = store
            .owned_source_add(
                &actor,
                "git",
                "git@git.example.com:Owner/Project.git",
                "session",
            )
            .unwrap();
        assert!(new);
        assert_eq!(entry.key, "git.example.com/owner/project");
        assert!(
            !store
                .owned_source_add(&actor, "git", REPO, "session")
                .unwrap()
                .1
        );

        assert_eq!(untrusted(&store, std::slice::from_ref(&mine)), None);
        assert_eq!(
            store
                .entity_names_unverified(std::slice::from_ref(&owner))
                .unwrap(),
            None
        );
        assert_eq!(
            untrusted(&store, &[mine.clone(), theirs.clone()]),
            Some(theirs.clone())
        );
        assert_eq!(
            store
                .entity_names_unverified(std::slice::from_ref(&ann))
                .unwrap(),
            Some(ann.clone())
        );
        let j = store.owned_judgement(&theirs).unwrap().unwrap();
        assert!(!j.counts);
        assert!(j.why.unwrap().contains("ann@else.example"));
        assert!(store.owned_judgement(&mine).unwrap().unwrap().counts);

        // the stored trust never moved
        assert_eq!(store.episode(&mine).unwrap().unwrap().trust, "imported");

        store
            .owned_source_remove(&actor, &entry.id)
            .unwrap()
            .unwrap();
        assert_eq!(untrusted(&store, std::slice::from_ref(&mine)), Some(mine));
        let history = store.owned_source_history(10).unwrap();
        assert_eq!(
            history.iter().map(|h| h.op.as_str()).collect::<Vec<_>>(),
            ["delete", "create"]
        );
        assert_eq!(
            history[0].key.as_deref(),
            Some("git.example.com/owner/project")
        );
    }

    /// Every doubt keeps a commit outside text: no known address of the
    /// owner's, a writer other than the git import, an unknown writer.
    #[test]
    fn test_an_owned_repository_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        let actor = Actor::new("human:owner");
        store
            .owned_source_add(&actor, "git", REPO, "session")
            .unwrap();
        apply(
            &store,
            &home,
            "adapter:git",
            vec![commit(SHA_A, "Owner Name", "me@own.example")],
        );
        let mine = crate::ids::episode_id("git", &format!("{REPO}#{SHA_A}"));
        // no address of the owner's is known
        assert_eq!(
            untrusted(&store, std::slice::from_ref(&mine)),
            Some(mine.clone())
        );
        // a mailbox on any of the owner's nodes is one
        store
            .publish_node(
                "nd_other",
                "other",
                "local",
                &json!({"mailboxes": [{"address": "me@own.example", "aliases": []}]}),
                "0",
            )
            .unwrap();
        assert_eq!(untrusted(&store, std::slice::from_ref(&mine)), None);

        // the same commit written by anyone but the git import
        apply(
            &store,
            &home,
            "agent:key:helper",
            vec![commit(SHA_B, "Owner Name", "me@own.example")],
        );
        let forged = crate::ids::episode_id("git", &format!("{REPO}#{SHA_B}"));
        assert_eq!(
            untrusted(&store, std::slice::from_ref(&forged)),
            Some(forged.clone())
        );
        // a writer that isn't known
        store
            .lock()
            .execute(
                "UPDATE episode_origin SET writer = NULL WHERE episode_id = ?1",
                [&mine],
            )
            .unwrap();
        assert_eq!(untrusted(&store, std::slice::from_ref(&mine)), Some(mine));
    }

    /// A commit by the owner's address in an owned repository still isn't
    /// the owner's words alone when it is a merge (more than one parent, or
    /// a default merge subject), co-written with another address (Claude
    /// Code's trailer included), made or rewritten while one of the owner's
    /// agents was running (a run anywhere from its author date to its
    /// commit time), too long to read whole, or stored by an
    /// earlier version whose record names neither its parents nor its
    /// commit time.
    #[test]
    fn test_owner_commits_that_carry_other_words_stay_outside_text() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        store
            .owned_source_add(&Actor::new("human:owner"), "git", REPO, "session")
            .unwrap();
        set_card(&store, &["me@own.example"]);
        let head = |at: &str| format!("committed: {at}\nparents: {PARENT}\n");
        let day = "2026-11-01T00:00:00Z";
        let cases: Vec<(String, &str, &str, bool)> = vec![
            // (header lines after the author's, message, author time, counts)
            (head(day), "a plain change", day, true),
            (
                format!("committed: {day}\nparents: {SHA_A} {SHA_B}\n"),
                "fold the branch in",
                day,
                false,
            ),
            (
                head(day),
                "Merge pull request #12 from someone/branch\n\nTheir title",
                day,
                false,
            ),
            (
                head(day),
                "a change\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
                day,
                false,
            ),
            (
                head(day),
                "a change\n\nco-authored-by: Ann <ann@else.example>",
                day,
                false,
            ),
            (head(day), "a change\n\nCo-authored-by: Ann", day, false),
            (
                head(day),
                "a change\n\nCo-authored-by: Me Again <me@own.example>",
                day,
                true,
            ),
            // during the agent's run below
            (
                head("2026-11-02T10:30:00Z"),
                "a change",
                "2026-11-02T10:30:00Z",
                false,
            ),
            // after it ended
            (
                head("2026-11-02T12:00:00Z"),
                "a change",
                "2026-11-02T12:00:00Z",
                true,
            ),
            // authored the day before, rewritten (an amend keeps the author
            // date) during the run — also with its time written in an offset
            (
                head("2026-11-02T10:30:00Z"),
                "a change, reworded",
                day,
                false,
            ),
            (
                head("2026-11-02T06:30:00-04:00"),
                "a change, reworded",
                day,
                false,
            ),
            // stored by an earlier version: no parents, no commit time —
            // a plain change, and a merge whose subject is its own
            (String::new(), "a plain change", day, false),
            (String::new(), "fold the branch in", day, false),
            // a record that says its parents twice can't be read
            (
                format!("{}parents: {SHA_A} {SHA_B}\n", head(day)),
                "fold the branch in",
                day,
                false,
            ),
            // authored the day before the run and rewritten the day after
            // (a rebase or cherry-pick of an amend made during the run
            // keeps neither time of the run)
            (
                head("2026-11-03T09:00:00Z"),
                "a change, reworded",
                day,
                false,
            ),
            // rewritten before the run began
            (
                head("2026-11-01T23:00:00Z"),
                "a change, reworded",
                day,
                true,
            ),
        ];
        let long = format!("a change\n\n{}", "x".repeat(COMMIT_TEXT_CHARS));
        let long_head = head(day);
        let mut drafts = Vec::new();
        let mut expected = Vec::new();
        for (n, (header, message, at, counts)) in cases
            .iter()
            .map(|(h, m, t, c)| (h.as_str(), *m, *t, *c))
            .chain(std::iter::once((
                long_head.as_str(),
                long.as_str(),
                day,
                false,
            )))
            .enumerate()
        {
            let sha = format!("{:040x}", n + 16);
            let mut d = commit(&sha, "Owner Name", "me@own.example");
            d.body = format!(
                "commit {sha}\nrepo: {REPO}\nauthor: Owner Name <me@own.example>\n{header}\n{message}"
            );
            d.event_time = EventTime::parse_rfc3339(at, TimeUncertainty::Exact).unwrap();
            expected.push((crate::ids::episode_id("git", &d.source_ref), counts, n));
            drafts.push(d);
        }
        apply(&store, &home, "adapter:git", drafts);
        store
            .lock()
            .execute(
                "INSERT INTO agent_tasks (id, request, brief, status, created_at, updated_at,
                                          started_at, finished_at)
                 VALUES ('tk_agent', 'r', 'b', 'done', ?1, ?2, ?1, ?2)",
                ["2026-11-02T10:00:00Z", "2026-11-02T11:00:00Z"],
            )
            .unwrap();
        for (id, counts, n) in &expected {
            let j = store.owned_judgement(id).unwrap().unwrap();
            assert_eq!(j.counts, *counts, "case {n}: {:?}", j.why);
            assert_eq!(
                untrusted(&store, std::slice::from_ref(id)).is_none(),
                *counts,
                "case {n}"
            );
        }
        let why = |n: usize| {
            store
                .owned_judgement(&expected[n].0)
                .unwrap()
                .unwrap()
                .why
                .unwrap_or_default()
        };
        for n in [9, 10, 14] {
            assert!(why(n).contains("tk_agent"), "{}", why(n));
        }
        for n in [11, 12] {
            assert!(why(n).contains("earlier version"), "{}", why(n));
        }
        // a task still running covers what came after its start
        store
            .lock()
            .execute(
                "UPDATE agent_tasks SET finished_at = NULL, status = 'running'",
                [],
            )
            .unwrap();
        let after = &expected[8];
        let j = store.owned_judgement(&after.0).unwrap().unwrap();
        assert!(!j.counts);
        assert!(j.why.unwrap().contains("tk_agent"));
    }

    /// A commit by the owner's address in an owned repository, made at
    /// `at` (author date and commit time).
    fn commit_at(sha: &str, at: &str) -> EpisodeDraft {
        let mut d = commit(sha, "Owner Name", "me@own.example");
        d.body = format!(
            "commit {sha}\nrepo: {REPO}\nauthor: Owner Name <me@own.example>\n\
             committed: {at}\nparents: {PARENT}\n\na change"
        );
        d.event_time = EventTime::parse_rfc3339(at, TimeUncertainty::Exact).unwrap();
        d
    }

    /// A task that started and was never closed — its agent deleted
    /// mid-run, or its node gone — keeps commits out, when it started on a
    /// harness Hearthloom stops itself (Claude Code), only as long as a run
    /// can last after its latest stamp, and one still writing stamps as long
    /// as it does. A run Hearthloom doesn't stop (OpenClaw, Hermes), or one
    /// whose harness wasn't kept, keeps them out until it is closed. The
    /// harness is the run's own: its agent, gone or not, isn't asked.
    #[test]
    fn test_a_run_never_closed_ends_only_where_hearthloom_stops_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        store
            .owned_source_add(&Actor::new("human:owner"), "git", REPO, "session")
            .unwrap();
        set_card(&store, &["me@own.example"]);
        let times = [
            "2026-11-02T12:00:00Z", // within a day of the run's start
            "2026-11-05T12:00:00Z", // three days on
        ];
        let drafts: Vec<EpisodeDraft> = times
            .iter()
            .enumerate()
            .map(|(n, at)| commit_at(&format!("{:040x}", n + 1), at))
            .collect();
        let ids: Vec<String> = drafts
            .iter()
            .map(|d| crate::ids::episode_id("git", &d.source_ref))
            .collect();
        apply(&store, &home, "adapter:git", drafts);
        let run = |sql: &str, params: &[&str]| {
            store
                .lock()
                .execute(sql, rusqlite::params_from_iter(params))
                .unwrap();
        };
        // its agent was deleted mid-run: known nowhere now
        run(
            "INSERT INTO agent_tasks (id, agent_id, request, brief, status, created_at,
                                      updated_at, started_at, harness)
             VALUES ('tk_stuck', 'ag_gone', 'r', 'b', 'running', ?1, ?1, ?1, 'claude_code')",
            &["2026-11-02T10:00:00Z"],
        );
        let counts = |id: &String| store.owned_judgement(id).unwrap().unwrap();
        let during = counts(&ids[0]);
        assert!(!during.counts);
        assert!(during.why.unwrap().contains("tk_stuck"));
        assert!(counts(&ids[1]).counts, "{:?}", counts(&ids[1]).why);
        assert_eq!(untrusted(&store, std::slice::from_ref(&ids[1])), None);

        // a harness that runs on its own: the job may still be going
        for harness in ["openclaw", "hermes"] {
            run("UPDATE agent_tasks SET harness = ?1", &[harness]);
            let later = counts(&ids[1]);
            assert!(!later.counts, "{harness}");
            assert!(later.why.unwrap().contains("tk_stuck"), "{harness}");
            assert_eq!(
                untrusted(&store, std::slice::from_ref(&ids[1])),
                Some(ids[1].clone()),
                "{harness}"
            );
        }
        // a run whose harness wasn't kept (it started before they were):
        // it can't be told
        run("UPDATE agent_tasks SET harness = NULL", &[]);
        assert!(!counts(&ids[1]).counts);
        run("UPDATE agent_tasks SET harness = 'claude_code'", &[]);
        assert!(counts(&ids[1]).counts, "{:?}", counts(&ids[1]).why);

        // the task wrote a stamp that morning (it read something): it may
        // still have been at work
        run(
            "UPDATE agent_tasks SET updated_at = '2026-11-05T09:00:00Z'",
            &[],
        );
        assert!(!counts(&ids[1]).counts);
    }

    /// A run keeps the harness it started on: an agent the owner moves from
    /// OpenClaw to Claude Code while its OpenClaw job is still out doesn't
    /// give that job an end, so a commit made days later still stays out.
    /// A task started again on another harness keeps none.
    #[test]
    fn test_a_run_keeps_the_harness_it_started_on() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        let owner = Actor::new("human:owner");
        store
            .owned_source_add(&owner, "git", REPO, "session")
            .unwrap();
        set_card(&store, &["me@own.example"]);
        let agent_in = |harness: &str| crate::store::AgentIn {
            name: "Helper".into(),
            harness: harness.into(),
            config: json!({}),
            enabled: None,
        };
        let agent = store
            .save_agent(&owner, None, &agent_in("openclaw"))
            .unwrap();
        let task = store
            .create_agent_task(&owner, None, Some(&agent.id), "r", "b", &[], None, false)
            .unwrap();
        store
            .start_agent_task_run(&owner, &task.id, "openclaw", None, None)
            .unwrap();
        // the run began on 2 November and has written nothing since
        store
            .lock()
            .execute(
                "UPDATE agent_tasks SET started_at = ?1, updated_at = ?1",
                ["2026-11-02T10:00:00Z"],
            )
            .unwrap();
        let later = commit_at(&format!("{:040x}", 7), "2026-11-05T12:00:00Z");
        let id = crate::ids::episode_id("git", &later.source_ref);
        apply(&store, &home, "adapter:git", vec![later]);
        let judged = || store.owned_judgement(&id).unwrap().unwrap();
        assert!(!judged().counts);

        // the owner moves the agent to Claude Code; the OpenClaw job goes on
        store
            .save_agent(&owner, Some(&agent.id), &agent_in("claude_code"))
            .unwrap();
        let still = judged();
        assert!(!still.counts, "the agent's setting now ended the run");
        assert!(still.why.unwrap().contains(&task.id));

        // started again, on Claude Code: which harness ran it can't be told
        store
            .start_agent_task_run(&owner, &task.id, "claude_code", None, None)
            .unwrap();
        let harness = || -> Option<String> {
            store
                .lock()
                .query_row("SELECT harness FROM agent_tasks", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(harness(), None);
        // a task first started on Claude Code keeps it
        let first = store
            .create_agent_task(&owner, None, Some(&agent.id), "r", "b", &[], None, false)
            .unwrap();
        store
            .start_agent_task_run(&owner, &first.id, "claude_code", None, None)
            .unwrap();
        let kept: Option<String> = store
            .lock()
            .query_row(
                "SELECT harness FROM agent_tasks WHERE id = ?1",
                [&first.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept.as_deref(), Some("claude_code"));
    }

    /// Judging a commit reads only a run that was going while it was made
    /// (by the index on when runs finished): never the task history that
    /// ended before it was written, nor the runs that started after its
    /// commit time — an old commit reads none of the history since, however
    /// long it grows.
    #[test]
    fn test_judging_a_commit_reads_only_a_run_going_while_it_was_made() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        store
            .owned_source_add(&Actor::new("human:owner"), "git", REPO, "session")
            .unwrap();
        set_card(&store, &["me@own.example"]);
        let drafts = vec![
            commit_at(&format!("{:040x}", 1), "2026-11-05T12:00:00Z"),
            // before the owner had agents
            commit_at(&format!("{:040x}", 2), "2020-01-01T12:00:00Z"),
            commit_at(&format!("{:040x}", 3), "2025-06-10T12:30:00Z"),
        ];
        let ids: Vec<String> = drafts
            .iter()
            .map(|d| crate::ids::episode_id("git", &d.source_ref))
            .collect();
        apply(&store, &home, "adapter:git", drafts);
        {
            let conn = store.lock();
            // a long history before the recent commit, and one run after it
            for day in 0..500 {
                let start = format!("2025-06-{:02}T12:00:00Z", 1 + day % 28);
                let end = format!("2025-06-{:02}T13:00:00Z", 1 + day % 28);
                conn.execute(
                    "INSERT INTO agent_tasks (id, request, brief, status, created_at,
                                              updated_at, started_at, finished_at)
                     VALUES (?1, 'r', 'b', 'done', ?2, ?3, ?2, ?3)",
                    rusqlite::params![format!("tk_old_{day}"), start, end],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO agent_tasks (id, request, brief, status, created_at, updated_at,
                                          started_at, finished_at)
                 VALUES ('tk_new', 'r', 'b', 'done', ?1, ?2, ?1, ?2)",
                ["2026-11-06T10:00:00Z", "2026-11-06T11:00:00Z"],
            )
            .unwrap();
        }
        let conn = store.lock();
        let candidate = |id: &str| {
            conn.query_row(
                &format!(
                    "SELECT {} FROM episodes e {} WHERE e.id = ?1",
                    candidate_cols(),
                    candidate_joins()
                ),
                [id],
                |r| Candidate::from_row(r, 0),
            )
            .unwrap()
        };
        let owned = OwnedWords::load(&conn).unwrap();
        let recent = owned.judge(&candidate(&ids[0])).unwrap();
        assert!(recent.counts, "{:?}", recent.why);
        assert_eq!(owned.runs_read(), 0, "the run after it isn't read");
        let old = owned.judge(&candidate(&ids[1])).unwrap();
        assert!(old.counts, "{:?}", old.why);
        assert_eq!(owned.runs_read(), 0, "the history since isn't read");
        let during = owned.judge(&candidate(&ids[2])).unwrap();
        assert!(!during.counts);
        assert!(during.why.unwrap().contains("tk_old_"));
        assert!(owned.judge(&candidate(&ids[0])).unwrap().counts);
        assert_eq!(owned.runs_read(), 1);

        // years more history after the old commit: still none of it read
        let later_runs = |conn: &Connection, from: usize, to: usize| {
            conn.execute(
                "WITH RECURSIVE n(i) AS (SELECT ?1 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < ?2)
                 INSERT INTO agent_tasks (id, request, brief, status, created_at,
                                          updated_at, started_at, finished_at)
                 SELECT 'tk_later_' || i, 'r', 'b', 'done', s, f, s, f FROM (
                     SELECT i,
                            printf('2026-%02d-%02dT08:00:00Z', 1 + i % 10, 1 + i % 28) AS s,
                            printf('2026-%02d-%02dT09:00:00Z', 1 + i % 10, 1 + i % 28) AS f
                     FROM n)",
                rusqlite::params![from as i64, to as i64],
            )
            .unwrap();
        };
        later_runs(&conn, 0, 2_000);
        let owned = OwnedWords::load(&conn).unwrap();
        assert!(owned.judge(&candidate(&ids[1])).unwrap().counts);
        assert_eq!(owned.runs_read(), 0);
        drop(conn);

        // nor looked at: the work of judging the old commit stays the same
        // however many runs finished after it
        let old_commit_work = || {
            super::super::trace::work_of(&store, || {
                assert!(store.owned_judgement(&ids[1]).unwrap().unwrap().counts);
            })
            .steps_on("agent_tasks")
        };
        let before = old_commit_work();
        later_runs(&store.lock(), 2_000, 20_000);
        let after = old_commit_work();
        assert!(after <= before + 10, "{before} steps, then {after}");

        // nor the runs after it that lasted more than a day: two-day and
        // twenty-two-day runs years later aren't looked at either
        let later_long_runs = |conn: &Connection, from: usize, to: usize| {
            conn.execute(
                "WITH RECURSIVE n(i) AS (SELECT ?1 UNION ALL SELECT i + 1 FROM n WHERE i + 1 < ?2)
                 INSERT INTO agent_tasks (id, request, brief, status, created_at,
                                          updated_at, started_at, finished_at)
                 SELECT 'tk_long_' || i, 'r', 'b', 'done', s, f, s, f FROM (
                     SELECT i,
                            printf('2026-%02d-01T08:00:00Z', 1 + i % 10) AS s,
                            printf('2026-%02d-%02dT08:00:00Z', 1 + i % 10, 3 + 20 * (i % 2)) AS f
                     FROM n)",
                rusqlite::params![from as i64, to as i64],
            )
            .unwrap();
        };
        later_long_runs(&store.lock(), 0, 2_000);
        let before = old_commit_work();
        later_long_runs(&store.lock(), 2_000, 20_000);
        let after = old_commit_work();
        assert!(after <= before + 10, "{before} steps, then {after}");

        // each read is a search of an index, never a pass over the table
        let conn = store.lock();
        let at = || rusqlite::types::Value::Text("2020-01-01T12:00:00Z".into());
        let until = || rusqlite::types::Value::Text("2020-01-02T12:05:00Z".into());
        let digits = rusqlite::types::Value::Integer(1);
        for (sql, params, index) in [
            (
                RUN_FINISHED_DURING,
                vec![at(), at(), until()],
                "idx_agent_tasks_finished",
            ),
            (
                LONG_RUN_DURING,
                vec![at(), at(), digits, until()],
                "idx_agent_tasks_span",
            ),
            (ODD_RUN_DURING, vec![at(), at()], "idx_agent_tasks_span"),
            (RUNS_NOT_CLOSED, vec![], "idx_agent_tasks_finished"),
        ] {
            let mut plan = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let steps: Vec<String> = plan
                .query_map(rusqlite::params_from_iter(params), |r| r.get(3))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert!(
                steps
                    .iter()
                    .any(|s| s.contains(&format!("USING INDEX {index}"))),
                "{sql}: {steps:?}"
            );
            assert!(
                !steps
                    .iter()
                    .any(|s| s.starts_with("SCAN agent_tasks") || s.starts_with("SCAN t")),
                "{sql}: {steps:?}"
            );
            // a look at the runs of one length reads only those that
            // finished in its window: bounded on both ends
            if sql == LONG_RUN_DURING {
                assert!(
                    steps
                        .iter()
                        .any(|s| s.contains("finished_at>? AND finished_at<?")),
                    "{sql}: {steps:?}"
                );
            }
        }
        drop(conn);

        // a run that went on for more than a day is still found, however
        // long it lasted: one of a day and four minutes that ended the day
        // after the old commit, and one that went on for years
        let judged = || store.owned_judgement(&ids[1]).unwrap().unwrap();
        let run = |id: &str, start: &str, end: &str| {
            store
                .lock()
                .execute(
                    "INSERT INTO agent_tasks (id, request, brief, status, created_at,
                                              updated_at, started_at, finished_at)
                     VALUES (?1, 'r', 'b', 'done', ?2, ?3, ?2, ?3)",
                    [id, start, end],
                )
                .unwrap();
        };
        run("tk_day", "2020-01-01T11:00:00Z", "2020-01-02T11:04:00Z");
        assert!(judged().why.unwrap().contains("tk_day"));
        store
            .lock()
            .execute("DELETE FROM agent_tasks WHERE id = 'tk_day'", [])
            .unwrap();
        assert!(judged().counts);
        // and one of each other length: weeks, months, years, and one whose
        // end can't be read
        for (id, start, end) in [
            ("tk_weeks", "2019-12-25T09:00:00Z", "2020-01-16T09:00:00Z"),
            ("tk_months", "2019-06-01T09:00:00Z", "2020-10-01T09:00:00Z"),
            ("tk_years", "2019-12-01T09:00:00Z", "2026-02-01T09:00:00Z"),
            ("tk_unreadable", "2019-12-31T09:00:00Z", "unknown"),
        ] {
            run(id, start, end);
            assert!(judged().why.unwrap().contains(id), "{id}");
            store
                .lock()
                .execute("DELETE FROM agent_tasks WHERE id = ?1", [id])
                .unwrap();
            assert!(judged().counts, "{id}");
        }
    }

    /// What the list doesn't name is checked in one look, and the owner's
    /// agents' runs are read only for a commit that gets as far as needing
    /// them — never when another episode already answers. A runs table
    /// that can't be read leaves a commit outside text.
    #[test]
    fn test_agent_runs_are_read_only_for_a_commit_that_needs_them() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        store
            .owned_source_add(&Actor::new("human:owner"), "git", REPO, "session")
            .unwrap();
        set_card(&store, &["me@own.example"]);
        apply(
            &store,
            &home,
            "adapter:git",
            vec![
                commit(SHA_A, "Owner Name", "me@own.example"),
                commit(SHA_B, "Ann Contributor", "ann@else.example"),
            ],
        );
        apply(
            &store,
            &home,
            "adapter:substack",
            vec![post("https://someone.substack.com/p/two")],
        );
        let mine = crate::ids::episode_id("git", &format!("{REPO}#{SHA_A}"));
        let theirs = crate::ids::episode_id("git", &format!("{REPO}#{SHA_B}"));
        let post = crate::ids::episode_id("substack", "https://someone.substack.com/p/two");
        assert_eq!(untrusted(&store, std::slice::from_ref(&mine)), None);

        store
            .lock()
            .execute("ALTER TABLE agent_tasks RENAME TO agent_tasks_away", [])
            .unwrap();
        assert_eq!(
            untrusted(&store, &[mine.clone(), post.clone()]),
            Some(post.clone())
        );
        assert_eq!(
            untrusted(&store, std::slice::from_ref(&theirs)),
            Some(theirs)
        );
        assert!(store
            .first_untrusted_episode(std::slice::from_ref(&mine))
            .is_err());
    }

    /// The count of stored memories an entry names is the viewer's: a
    /// private commit counts only with their vault open.
    #[test]
    fn test_stored_counts_leave_out_private_memories_while_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        let mut private = commit(SHA_B, "Owner Name", "me@own.example");
        private.scope = Some("private".into());
        apply(
            &store,
            &home,
            "adapter:git",
            vec![commit(SHA_A, "Owner Name", "me@own.example"), private],
        );
        let (entry, _) = store
            .owned_source_add(&Actor::new("human:owner"), "git", REPO, "session")
            .unwrap();
        assert_eq!(
            store.owned_source_stored().unwrap().get(&entry.id),
            Some(&1)
        );
        assert_eq!(store.owned_entry_stored(&entry).unwrap(), 1);
        store.vault_unlock(5);
        assert_eq!(
            store.owned_source_stored().unwrap().get(&entry.id),
            Some(&2)
        );
        assert_eq!(store.owned_entry_stored(&entry).unwrap(), 2);
    }

    /// Counting an entry reads the repositories that may name it and steps
    /// past every other one with one look: the database's work doesn't grow
    /// with other repositories' commits, or a local checkout's — also for
    /// an address that holds a `#`. It still finds the entry under every
    /// spelling of its address, an address that holds a `#`, and one whose
    /// user name holds one.
    #[test]
    fn test_counting_an_entry_steps_past_what_cannot_name_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        let at = |identity: &str, n: usize| {
            let mut d = commit(&format!("{n:040x}"), "Owner Name", "me@own.example");
            d.source_ref = format!("{identity}#{n:040x}");
            d
        };
        let others = |from: usize, many: usize| {
            (0..many)
                .flat_map(|n| {
                    [
                        at("https://git.example.com/owner/other", from + 3 * n),
                        at("https://elsewhere.example/owner/project", from + 3 * n + 1),
                        // a checkout with no remote address
                        at("/home/user/project", from + 3 * n + 2),
                    ]
                })
                .collect::<Vec<_>>()
        };
        let mut drafts = vec![
            at(REPO, 1),
            at("git@Git.Example.com:Owner/Project.git", 2),
            at("ssh://git@git.example.com:22/OWNER/project", 3),
            // a path with a letter the database doesn't lowercase
            at("https://git.example.com/Owner/CAFÉ.git", 4),
            // an address that holds a `#`: in the part of the one before it
            at("git.example.com:owner/project#x", 5),
            // a user name that holds a `#`: in a part with no key
            at("me#1@git.example.com:owner/project", 6),
        ];
        drafts.extend(others(100, 20));
        apply(&store, &home, "adapter:git", drafts);
        let actor = Actor::new("human:owner");
        let (entry, _) = store
            .owned_source_add(&actor, "git", REPO, "session")
            .unwrap();
        let count = |entry: &OwnedSourceRow| {
            let (stored, work) = store
                .stored_by_key(&entry.source, &BTreeSet::from([entry.key.clone()]))
                .unwrap();
            (stored.get(&entry.key).copied().unwrap_or(0), work)
        };
        let (stored, work) = count(&entry);
        assert_eq!(stored, 4);

        // many more commits of the other repositories: no more work
        apply(&store, &home, "adapter:git", others(1_000, 200));
        let (stored, more_work) = count(&entry);
        assert_eq!(stored, 4);
        assert!(more_work < work + 20, "{work} → {more_work}");

        let (cafe, _) = store
            .owned_source_add(
                &actor,
                "git",
                "https://git.example.com/owner/café",
                "session",
            )
            .unwrap();
        assert_eq!(count(&cafe).0, 1);
        let (hash, _) = store
            .owned_source_add(&actor, "git", "git.example.com:owner/project#x", "session")
            .unwrap();
        let (stored, hash_work) = count(&hash);
        assert_eq!(stored, 1);
        // an address that holds a `#` reads only the parts that may hold it
        apply(&store, &home, "adapter:git", others(5_000, 200));
        let (stored, more_hash_work) = count(&hash);
        assert_eq!(stored, 1);
        assert!(
            more_hash_work < hash_work + 20,
            "{hash_work} → {more_hash_work}"
        );
        let stored = store.owned_source_stored().unwrap();
        assert_eq!(stored.get(&entry.id), Some(&4));
        assert_eq!(stored.get(&cafe.id), Some(&1));
        assert_eq!(stored.get(&hash.id), Some(&1));
        assert_eq!(store.owned_entry_stored(&entry).unwrap(), 4);
    }

    /// A publication's posts are counted by host, a host at a time, and
    /// under every spelling of its address.
    #[test]
    fn test_counting_a_publication_steps_past_other_hosts() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        let mut drafts = vec![
            post("https://owner.example/p/one"),
            post("https://www.Owner.example/p/two"),
            post("https://owner.example:443?p=3"),
            post("https://owner.example"),
            post("https://owner.example.evil/p/four"),
        ];
        for n in 0..20 {
            drafts.push(post(&format!("https://someone.substack.com/p/{n}")));
        }
        apply(&store, &home, "adapter:substack", drafts);
        let (entry, _) = store
            .owned_source_add(&Actor::new("human:owner"), "substack", "owner.example", "s")
            .unwrap();
        let keys = BTreeSet::from([entry.key.clone()]);
        let (stored, work) = store.stored_by_key("substack", &keys).unwrap();
        assert_eq!(stored.get("owner.example"), Some(&4));
        let more: Vec<EpisodeDraft> = (20..220)
            .map(|n| post(&format!("https://someone.substack.com/p/{n}")))
            .collect();
        apply(&store, &home, "adapter:substack", more);
        let (stored, more_work) = store.stored_by_key("substack", &keys).unwrap();
        assert_eq!(stored.get("owner.example"), Some(&4));
        assert!(more_work < work + 20, "{work} → {more_work}");
    }

    #[test]
    fn test_an_owned_publication_counts_by_host() {
        let tmp = tempfile::tempdir().unwrap();
        let (home, store) = node(tmp.path(), "laptop");
        apply(
            &store,
            &home,
            "adapter:substack",
            vec![
                post("https://owner.substack.com/p/one"),
                post("https://someone.substack.com/p/two"),
            ],
        );
        let one = crate::ids::episode_id("substack", "https://owner.substack.com/p/one");
        let two = crate::ids::episode_id("substack", "https://someone.substack.com/p/two");
        assert_eq!(
            untrusted(&store, std::slice::from_ref(&one)),
            Some(one.clone())
        );
        store
            .owned_source_add(
                &Actor::new("human:owner"),
                "substack",
                "https://www.owner.substack.com",
                "session",
            )
            .unwrap();
        assert_eq!(untrusted(&store, std::slice::from_ref(&one)), None);
        assert_eq!(untrusted(&store, &[one, two.clone()]), Some(two));

        // A post filed under the marked host by an earlier version, which
        // took the address from whatever server it was given — here one
        // that named the owner's publication while serving its own text —
        // and a post whose record says another host served it: neither
        // counts.
        apply(
            &store,
            &home,
            "adapter:substack",
            vec![
                post_served("https://owner.substack.com/p/forged", ""),
                post_served(
                    "https://owner.substack.com/p/elsewhere",
                    "served from: https://other.example\n",
                ),
            ],
        );
        for (slug, why) in [
            ("forged", "earlier version"),
            ("elsewhere", "other.example served it"),
        ] {
            let id =
                crate::ids::episode_id("substack", &format!("https://owner.substack.com/p/{slug}"));
            assert_eq!(
                untrusted(&store, std::slice::from_ref(&id)),
                Some(id.clone()),
                "{slug}"
            );
            let j = store.owned_judgement(&id).unwrap().unwrap();
            assert!(!j.counts);
            assert!(j.why.as_deref().unwrap_or("").contains(why), "{:?}", j.why);
        }

        // the list says how many stored posts each entry names: one marked
        // by an address no post is filed under names none
        let (elsewhere, _) = store
            .owned_source_add(
                &Actor::new("human:owner"),
                "substack",
                "owner.example",
                "session",
            )
            .unwrap();
        let stored = store.owned_source_stored().unwrap();
        let owner = owned_id("substack", "owner.substack.com");
        assert_eq!(stored.get(&owner), Some(&3));
        assert_eq!(stored.get(&elsewhere.id), None);
    }

    /// The list reaches the owner's other node through the operational
    /// mirror, and a removal through the journal leg.
    #[test]
    fn test_the_list_reaches_a_peer_and_so_does_a_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let (laptop_home, laptop) = node(tmp.path(), "laptop");
        let (hub_home, hub) = node(tmp.path(), "hub");
        let (entry, _) = laptop
            .owned_source_add(&Actor::new("human:owner"), "git", REPO, "session")
            .unwrap();
        // the commit reaches the hub too, with its origin
        apply(
            &laptop,
            &laptop_home,
            "adapter:git",
            vec![commit(SHA_A, "Owner Name", "me@own.example")],
        );
        set_card(&laptop, &["me@own.example"]);
        set_card(&hub, &["me@own.example"]);
        let page = laptop.sync_export(&laptop_home, 0, 50).unwrap();
        hub.apply_batch_from(
            &hub_home,
            &IngestBatch {
                actor: "peer:laptop".into(),
                session_id: None,
                episodes: page.episodes,
            },
            &page.origins,
        )
        .unwrap();
        let mine = crate::ids::episode_id("git", &format!("{REPO}#{SHA_A}"));
        assert_eq!(
            untrusted(&hub, std::slice::from_ref(&mine)),
            Some(mine.clone())
        );

        let mirror = laptop.export_operational(&laptop_home).unwrap();
        assert!(hub.apply_operational(&hub_home, &mirror).unwrap() >= 1);
        assert_eq!(hub.owned_sources().unwrap(), vec![entry.clone()]);
        assert_eq!(untrusted(&hub, std::slice::from_ref(&mine)), None);
        // applying it again changes nothing
        assert_eq!(hub.apply_operational(&hub_home, &mirror).unwrap(), 0);

        // removed on the laptop: the journal carries it, and the mirror
        // doesn't bring the old copy back
        laptop
            .owned_source_remove(&Actor::new("human:owner"), &entry.id)
            .unwrap();
        let journal = laptop.journal_export(0, 100).unwrap();
        hub.apply_journal(&Actor::new("peer:laptop"), &journal)
            .unwrap();
        assert!(hub.owned_sources().unwrap().is_empty());
        let stale = json!({"owned_sources": [serde_json::to_value(&entry).unwrap()]});
        hub.apply_operational(&hub_home, &stale).unwrap();
        assert!(hub.owned_sources().unwrap().is_empty());
        assert_eq!(untrusted(&hub, std::slice::from_ref(&mine)), Some(mine));
        assert!(hub
            .owned_source_history(5)
            .unwrap()
            .iter()
            .any(|h| h.op == "delete" && h.actor == "peer:laptop"));
    }

    /// Taken off and marked again within the removal's second (stamps have
    /// whole seconds; here the removal is stamped a little ahead, as it can
    /// be in the same second or on a node whose clock runs ahead): the
    /// entry stands on the peer too, whichever leg arrives first, and
    /// taking it off again right away reaches the peer as well.
    #[test]
    fn test_marking_again_right_after_a_removal_reaches_a_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let (laptop_home, laptop) = node(tmp.path(), "laptop");
        let (hub_home, hub) = node(tmp.path(), "hub");
        let (other_home, other) = node(tmp.path(), "other");
        let owner = Actor::new("human:owner");
        let (entry, _) = laptop
            .owned_source_add(&owner, "git", REPO, "session")
            .unwrap();
        let mirror = laptop.export_operational(&laptop_home).unwrap();
        hub.apply_operational(&hub_home, &mirror).unwrap();
        other.apply_operational(&other_home, &mirror).unwrap();

        laptop.owned_source_remove(&owner, &entry.id).unwrap();
        let ahead = (chrono::Utc::now() + chrono::Duration::seconds(3))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        laptop
            .lock()
            .execute(
                "UPDATE op_journal SET ts = ?1 WHERE entity_kind = 'owned_source' AND op = 'delete'",
                [&ahead],
            )
            .unwrap();
        let (again, new) = laptop
            .owned_source_add(&owner, "git", REPO, "session")
            .unwrap();
        assert!(new);

        let journal = laptop.journal_export(0, 100).unwrap();
        let mirror = laptop.export_operational(&laptop_home).unwrap();
        let peer = Actor::new("peer:laptop");
        // the journal's removal first, then the mirror's copy
        hub.apply_journal(&peer, &journal).unwrap();
        hub.apply_operational(&hub_home, &mirror).unwrap();
        // the other way round
        other.apply_operational(&other_home, &mirror).unwrap();
        other.apply_journal(&peer, &journal).unwrap();
        for (name, store) in [("hub", &hub), ("other", &other)] {
            assert_eq!(
                store.owned_sources().unwrap(),
                vec![again.clone()],
                "{name}"
            );
        }
        assert!(again.updated_at > ahead, "{} vs {ahead}", again.updated_at);

        // taken off again at once: the tombstone is stamped no earlier than
        // the entry, so it removes it on the peers too
        laptop.owned_source_remove(&owner, &again.id).unwrap();
        let journal = laptop.journal_export(0, 100).unwrap();
        let mirror = laptop.export_operational(&laptop_home).unwrap();
        for (home, store) in [(&hub_home, &hub), (&other_home, &other)] {
            store.apply_journal(&peer, &journal).unwrap();
            store.apply_operational(home, &mirror).unwrap();
            assert!(store.owned_sources().unwrap().is_empty());
        }
        assert!(laptop.owned_sources().unwrap().is_empty());
    }
}
