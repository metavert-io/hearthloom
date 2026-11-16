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
//!   `agent_runs_on` — a rewrite keeps the author date and a later one
//!   replaces the commit time, so an agent's rewrite in between leaves no
//!   time of its own). Only a record that says its
//!   parents and commit time (`git_commit_head`) can be judged: a commit
//!   an earlier version imported stays outside text. Git doesn't check an
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
                 commits an earlier version of Hearthloom imported (their records can't show \
                 those things). Git doesn't check author addresses: anyone whose commit gets \
                 into the repository can put yours on it",
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

/// The `committed:` and `parents:` lines of a commit's stored text (its
/// head ends at the first blank line). None unless each is there exactly
/// once and reads — so for every commit an earlier version stored, which
/// wrote neither: such a record can't say whether the commit is a merge,
/// or when it was made.
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
/// - a record without `committed:` and `parents:` lines (`git_commit_head`
///   — every commit an earlier version stored): whether it is a merge, and
///   when it was made, can't be read;
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
            .and_then(clean_address);
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
            out.extend(m["address"].as_str().and_then(clean_address));
            for a in m["aliases"].as_array().into_iter().flatten() {
                out.extend(a.as_str().and_then(clean_address));
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
            out.extend(e.as_str().and_then(clean_address));
        }
    }
    Ok(out)
}

fn clean_address(a: &str) -> Option<String> {
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

/// One run of one of the owner's agents: from its start to its end (None:
/// still going, or never closed).
struct AgentRun {
    task: String,
    from: String,
    to: Option<String>,
}

/// Every agent task that started running, on any of the owner's nodes
/// (`agent_tasks` syncs).
fn agent_runs_on(conn: &Connection) -> Result<Vec<AgentRun>> {
    let mut stmt = conn.prepare(
        "SELECT id, started_at, finished_at FROM agent_tasks WHERE started_at IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(AgentRun {
            task: r.get(0)?,
            from: r.get(1)?,
            to: r.get(2)?,
        })
    })?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
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

/// The owner's list and addresses, loaded once for a check.
pub(crate) struct OwnedWords {
    entries: HashSet<(String, String)>,
    identities: BTreeSet<String>,
    /// The owner's agents' runs: a commit made during one may be the
    /// agent's, under the owner's git identity.
    agent_runs: Vec<AgentRun>,
}

impl OwnedWords {
    pub(crate) fn load(conn: &Connection) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT source, key FROM owned_sources")?;
        let entries = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<HashSet<_>, _>>()?;
        let (identities, agent_runs) = if entries.iter().any(|(s, _)| author_checked(s)) {
            (owner_emails_on(conn)?, agent_runs_on(conn)?)
        } else {
            (BTreeSet::new(), Vec::new())
        };
        Ok(OwnedWords {
            entries,
            identities,
            agent_runs,
        })
    }

    /// An agent task that was running at any time between these two (in
    /// either order), if any (a task never closed runs on). Times are RFC
    /// 3339 UTC with a `Z`, so they compare as text.
    fn agent_run_between(&self, a: &str, b: &str) -> Option<&str> {
        let (from, to) = if a <= b { (a, b) } else { (b, a) };
        self.agent_runs
            .iter()
            .find(|r| r.from.as_str() <= to && r.to.as_deref().is_none_or(|end| from <= end))
            .map(|r| r.task.as_str())
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
    pub(crate) fn covers(&self, c: &Candidate) -> bool {
        self.judge(c).counts
    }

    pub(crate) fn judge(&self, c: &Candidate) -> Judgement {
        let mut j = Judgement {
            source: c.source.clone(),
            key: None,
            entry: None,
            counts: false,
            why: None,
        };
        if ownable(&c.source).is_none() {
            j.why = not_ownable_reason(&c.source);
            return j;
        }
        j.key = key_of_ref(&c.source, &c.source_ref);
        let Some(key) = &j.key else {
            j.why = Some(match c.source.as_str() {
                "git" => "its repository has no remote address to name it by".into(),
                _ => "its address can't be read".into(),
            });
            return j;
        };
        if !self.entries.contains(&(c.source.clone(), key.clone())) {
            j.why = Some("not marked as yours".into());
            return j;
        }
        j.entry = Some(owned_id(&c.source, key));
        if !matches!(c.trust.as_str(), "self" | "imported") {
            j.why = Some(format!("it was let in with trust {}", c.trust));
            return j;
        }
        match &c.writer {
            Some(w) if *w == importer(&c.source) => {}
            Some(w) => {
                j.why = Some(format!(
                    "it was written by {w}, not the {} import",
                    c.source
                ));
                return j;
            }
            None => {
                j.why = Some("who first wrote it isn't known".into());
                return j;
            }
        }
        if author_checked(&c.source) {
            let author = c
                .head
                .as_deref()
                .and_then(|head| git_author(&c.source_ref, head));
            match author {
                None => {
                    j.why = Some("its author can't be read".into());
                    return j;
                }
                Some(a) if self.identities.is_empty() => {
                    j.why = Some(format!(
                        "by {a}, and none of your own addresses is known yet"
                    ));
                    return j;
                }
                Some(a) if !self.identities.contains(&a) => {
                    j.why = Some(format!("by {a}, not one of your addresses"));
                    return j;
                }
                Some(_) => {}
            }
            let head = c.head.as_deref().unwrap_or_default();
            if let Some(concern) = git_commit_concern(head, &self.identities) {
                j.why = Some(concern);
                return j;
            }
            // an agent working in the owner's checkout commits under the
            // owner's git identity (an agent's run is never the owner's
            // words). An amend, rebase or cherry-pick keeps the author date
            // and sets the commit time, and the next one sets it again: an
            // agent's rewrite in between leaves no time of its own, so a
            // run anywhere from the author date to the commit time counts
            let (Some(authored), Some(record)) = (c.when.as_deref(), git_commit_head(head)) else {
                j.why = Some("when it was made isn't known".into());
                return j;
            };
            if let Some(task) = self.agent_run_between(authored, &record.committed) {
                j.why = Some(format!(
                    "made or rewritten while one of your agents was at work (task {task})"
                ));
                return j;
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
                    return j;
                }
                Some(host) if host != *key => {
                    j.why = Some(format!("{host} served it, not {key}"));
                    return j;
                }
                Some(_) => {}
            }
        }
        j.counts = true;
        j
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
        let now = now_utc();
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
        super::journal_row(&conn, actor, "owned_source", "owned_sources", id, "delete")?;
        conn.execute("DELETE FROM owned_sources WHERE id = ?1", [id])?;
        record_mutation(
            &conn,
            actor,
            "owned_source_removed",
            &format!("owned_source_removed:{id}:{}", now_utc()),
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
    pub fn owned_source_stored(&self) -> Result<std::collections::HashMap<String, usize>> {
        let entries = self.owned_sources()?;
        let sources: BTreeSet<&str> = entries
            .iter()
            .filter_map(|e| ownable(&e.source).map(|s| s.source))
            .collect();
        let mut out = std::collections::HashMap::new();
        if sources.is_empty() {
            return Ok(out);
        }
        let ids: std::collections::HashMap<(&str, &str), &str> = entries
            .iter()
            .map(|e| ((e.source.as_str(), e.key.as_str()), e.id.as_str()))
            .collect();
        let list = sources
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(",");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT source, source_ref FROM episodes
             WHERE source IN ({list}) AND superseded_by IS NULL"
        ))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let source: String = row.get(0)?;
            let source_ref: String = row.get(1)?;
            if let Some(key) = key_of_ref(&source, &source_ref) {
                if let Some(id) = ids.get(&(source.as_str(), key.as_str())) {
                    *out.entry(id.to_string()).or_insert(0) += 1;
                }
            }
        }
        Ok(out)
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
        Ok(Some(OwnedWords::load(&conn)?.judge(&candidate)))
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
}
