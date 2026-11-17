//! Mail threads and the owner's mail rules (v63, 0.5.92) — the action
//! half of mail: what is waiting on the owner.
//!
//! The memory half (archive + Core-strata episodes) is unchanged. Each new
//! message the Gmail pull sees is `observe`d: it joins a thread by its
//! headers (References / In-Reply-To / Message-ID — provider-neutral; the
//! thread id is a hash of the root message id, so every node derives the
//! same one) and moves the thread's state. Only mail worth a decision
//! becomes a thread: Primary-category mail from someone else, replies in
//! a thread that exists, and whatever an owner rule names. Newsletters and
//! notifications never become rows.
//!
//! States: `new` (waiting for triage) → `needs_reply` | `scheduling` |
//! `opportunity` (open: on the Desk) | `fyi` | `ignore`; the owner's reply
//! makes it `replied`, the owner marks `done`; a new message from someone
//! else reopens it. A thread whose mail a privacy rule covers, or whose
//! mail has a private memory, is `private` — hidden while the vault is
//! locked, never sent to a model. A message found private when it is seen
//! again (its memory arrived private after it joined), or when triage looks
//! its thread's memories up before sorting it, makes its thread private
//! then.
//!
//! Owner rules are owner-authored only (mail itself can never write one):
//! deterministic matchers decide first (sender, domain, subject, list);
//! `plain` rules are the owner's words the triage model follows.
//! `mail_threads` and `mail_rules` mirror between the owner's nodes and are
//! journaled; `mail_thread_messages` is the observing node's own index.
use anyhow::{anyhow, bail, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

pub const MAIL_STATES: &[&str] = &[
    "new",
    "needs_reply",
    "scheduling",
    "opportunity",
    "fyi",
    "ignore",
    "replied",
    "done",
];
/// What waits on the owner.
pub const OPEN_STATES: &[&str] = &["needs_reply", "scheduling", "opportunity"];
pub const RULE_KINDS: &[&str] = &["from", "domain", "subject", "list", "plain"];
pub const RULE_ACTIONS: &[&str] = &[
    "needs_reply",
    "scheduling",
    "opportunity",
    "fyi",
    "ignore",
    "triage",
    "draft",
];

/// One message as the pull saw it.
#[derive(Debug, Clone, Default)]
pub struct MailSeen {
    pub account: String,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// "Name <address>"
    pub from: String,
    pub from_addr: String,
    pub subject: String,
    /// RFC 3339
    pub date: Option<String>,
    pub from_me: bool,
    /// Gmail's Primary category (or Starred/Important): a candidate
    pub primary: bool,
    pub list_id: Option<String>,
    /// the start of its text (≤300 chars)
    pub snippet: String,
    /// a privacy rule covers it, or its memory is labelled private — or
    /// `held`
    pub private: bool,
    /// Nothing says it's private, but its privacy can't be told: mail is
    /// held (a private promotion an earlier version kept can't be told
    /// apart from it). It reads as `private`, and is never recorded:
    /// `mail_observe` refuses it.
    pub held: bool,
    /// Gmail's labels for it (what an ember's watch can name)
    pub labels: Vec<String>,
    /// its readable text, for embers' watches (not stored on the thread)
    pub text: String,
    /// The keys its memory is stored under: the mail import's key for its
    /// Message-ID header (which can differ from `message_id`, a MIME
    /// parser's reading of it), and `message_id`. Empty when its memory is
    /// keyed by a hash of its text. Its privacy is read again by these
    /// once the pull has made the memory, and by triage before a model
    /// reads its thread (kept in this node's thread index, v70 — not on
    /// the thread).
    pub memory_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MailThread {
    pub id: String,
    pub account: String,
    pub subject: String,
    pub last_from: String,
    pub last_from_me: bool,
    pub last_message_id: Option<String>,
    pub last_at: Option<String>,
    pub message_count: i64,
    pub snippet: String,
    pub state: String,
    pub why: Option<String>,
    pub confidence: Option<f64>,
    pub due_by: Option<String>,
    pub rule_id: Option<String>,
    pub scope: String,
    pub triaged_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MailRule {
    pub id: String,
    pub kind: String,
    pub pattern: String,
    pub action: String,
    pub note: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

const THREAD_COLS: &str = "id, account, subject, last_from, last_from_me, last_message_id, \
     last_at, message_count, snippet, state, why, confidence, due_by, rule_id, scope, \
     triaged_at, created_at, updated_at";

fn thread_from_row(r: &rusqlite::Row) -> rusqlite::Result<MailThread> {
    Ok(MailThread {
        id: r.get(0)?,
        account: r.get(1)?,
        subject: r.get(2)?,
        last_from: r.get(3)?,
        last_from_me: r.get::<_, i64>(4)? != 0,
        last_message_id: r.get(5)?,
        last_at: r.get(6)?,
        message_count: r.get(7)?,
        snippet: r.get(8)?,
        state: r.get(9)?,
        why: r.get(10)?,
        confidence: r.get(11)?,
        due_by: r.get(12)?,
        rule_id: r.get(13)?,
        scope: r.get(14)?,
        triaged_at: r.get(15)?,
        created_at: r.get(16)?,
        updated_at: r.get(17)?,
    })
}

/// The thread id every node derives from a thread's root message id.
/// SQL over a row's `account` for the mailboxes one node handles (0.5.98):
/// the ones in `accounts`, and with `unwatched` also any account not in
/// that list of watched mailboxes (one no live node watches, or "" from
/// before 0.5.98). Parameters are numbered from `?{first}`.
pub(crate) fn account_filter(
    accounts: &[String],
    unwatched: Option<&[String]>,
    first: usize,
) -> (String, Vec<rusqlite::types::Value>) {
    fn list(vals: &[String], params: &mut Vec<rusqlite::types::Value>, next: &mut usize) -> String {
        vals.iter()
            .map(|v| {
                params.push(v.trim().to_lowercase().into());
                *next += 1;
                format!("?{}", *next - 1)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
    let mut params = Vec::new();
    let mut next = first;
    let mut parts = Vec::new();
    if !accounts.is_empty() {
        parts.push(format!(
            "lower(account) IN ({})",
            list(accounts, &mut params, &mut next)
        ));
    }
    match unwatched {
        Some([]) => parts.push("1".into()),
        Some(watched) => parts.push(format!(
            "lower(account) NOT IN ({})",
            list(watched, &mut params, &mut next)
        )),
        None => {}
    }
    if parts.is_empty() {
        ("0".into(), params)
    } else {
        (format!("({})", parts.join(" OR ")), params)
    }
}

/// Make a thread private, journaled; false when it already was. Nothing
/// makes a thread un-private.
fn make_private(
    conn: &rusqlite::Connection,
    actor: &Actor,
    thread: &str,
    now: &str,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE mail_threads SET scope = 'private', updated_at = ?2
         WHERE id = ?1 AND scope != 'private'",
        rusqlite::params![thread, now],
    )?;
    if changed == 0 {
        return Ok(false);
    }
    super::journal_row(conn, actor, "mail_thread", "mail_threads", thread, "update")?;
    Ok(true)
}

pub fn thread_id_for(root: &str) -> String {
    format!(
        "mt_{}",
        &crate::ids::content_hash(root.trim().as_bytes())[..16]
    )
}

fn rule_matches(rule: &MailRule, seen: &MailSeen) -> bool {
    let p = rule.pattern.to_lowercase();
    match rule.kind.as_str() {
        "from" => seen.from.to_lowercase().contains(&p),
        "domain" => seen
            .from_addr
            .to_lowercase()
            .rsplit_once('@')
            .is_some_and(|(_, d)| d == p || d.ends_with(&format!(".{p}"))),
        "subject" => seen.subject.to_lowercase().contains(&p),
        "list" => seen
            .list_id
            .as_deref()
            .is_some_and(|l| l.to_lowercase().contains(&p)),
        _ => false,
    }
}

impl Store {
    /// The SQL tail every reader of threads a viewer sees adds: the vault's
    /// guard, and while mail is held (`mail_hidden`) nothing at all for a
    /// viewer whose vault is locked — any thread can hold the message the
    /// hold is for, and the hold relabels nothing.
    fn mail_thread_guard(&self) -> String {
        let mut guard = self.scope_guard("scope");
        if self.mail_hidden() {
            guard.push_str(" AND 0");
        }
        guard
    }

    /// The owner's deterministic rule for this message, if any (first
    /// added wins).
    fn mail_rule_for(&self, seen: &MailSeen) -> Result<Option<MailRule>> {
        Ok(self
            .mail_rules()?
            .into_iter()
            .filter(|r| r.enabled && r.kind != "plain")
            .find(|r| rule_matches(r, seen)))
    }

    /// Record one message: its thread and the thread's state. Returns the
    /// thread id when this message created or changed a thread. Seeing the
    /// same message again changes nothing — unless it is private now (its
    /// memory arrived private since, or a rule covers it): then the thread
    /// it joined becomes private too. Nothing here makes a thread
    /// un-private.
    pub fn mail_observe(&self, actor: &Actor, seen: &MailSeen) -> Result<Option<String>> {
        if seen.held {
            bail!("mail is held: a message whose privacy can't be told is not recorded");
        }
        if seen.message_id.trim().is_empty() {
            return Ok(None);
        }
        let rule = self.mail_rule_for(seen)?;
        let now = now_utc();
        let conn = self.lock();
        let refs = serde_json::to_string(&seen.memory_refs)?;
        let known: Option<String> = conn
            .query_row(
                "SELECT thread_id FROM mail_thread_messages WHERE message_id = ?1",
                [&seen.message_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(thread) = known {
            // a message recorded before the index kept its memory's keys
            conn.execute(
                "UPDATE mail_thread_messages SET memory_refs = ?2
                 WHERE message_id = ?1 AND memory_refs IS NULL",
                [&seen.message_id, &refs],
            )?;
            if !seen.private || !make_private(&conn, actor, &thread, &now)? {
                return Ok(None);
            }
            return Ok(Some(thread));
        }
        // the thread a reply belongs to: a message it answers that we have
        let mut thread: Option<String> = None;
        for id in seen.references.iter().rev().chain(seen.in_reply_to.iter()) {
            thread = conn
                .query_row(
                    "SELECT thread_id FROM mail_thread_messages WHERE message_id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if thread.is_some() {
                break;
            }
        }
        let root = seen
            .references
            .first()
            .or(seen.in_reply_to.as_ref())
            .unwrap_or(&seen.message_id)
            .clone();
        let id = thread.unwrap_or_else(|| thread_id_for(&root));
        let existing: Option<(String, String)> = conn
            .query_row(
                "SELECT state, scope FROM mail_threads WHERE id = ?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let rule_state = rule
            .as_ref()
            .map(|r| r.action.as_str())
            .filter(|a| *a != "triage");
        let snippet: String = seen.snippet.chars().take(300).collect();
        let scope = if seen.private { "private" } else { "personal" };
        match existing {
            None => {
                // only mail worth a decision becomes a thread
                let candidate = !seen.from_me
                    && (seen.primary || rule.is_some())
                    && rule_state != Some("ignore");
                if !candidate {
                    return Ok(None);
                }
                conn.execute(
                    &format!(
                        "INSERT INTO mail_threads ({THREAD_COLS}) VALUES
                         (?1, ?2, ?3, ?4, 0, ?5, ?6, 1, ?7, ?8, ?9, NULL, NULL, ?10, ?11, NULL, ?12, ?12)"
                    ),
                    rusqlite::params![
                        id,
                        seen.account,
                        seen.subject,
                        seen.from,
                        seen.message_id,
                        seen.date,
                        snippet,
                        rule_state.unwrap_or("new"),
                        rule.as_ref().map(|r| format!("your rule: {} {}", r.kind, r.pattern)),
                        rule.as_ref().map(|r| r.id.clone()),
                        scope,
                        now,
                    ],
                )?;
            }
            Some((state, old_scope)) => {
                let next = if seen.from_me {
                    if OPEN_STATES.contains(&state.as_str()) || state == "new" {
                        "replied".to_string()
                    } else {
                        state.clone()
                    }
                } else if state == "ignore" && rule_state.is_none() {
                    state.clone()
                } else {
                    // someone wrote again: it needs a look again
                    rule_state.unwrap_or("new").to_string()
                };
                let scope = if old_scope == "private" {
                    "private"
                } else {
                    scope
                };
                conn.execute(
                    "UPDATE mail_threads SET last_from = ?2, last_from_me = ?3, last_message_id = ?4,
                         last_at = COALESCE(?5, last_at), message_count = message_count + 1,
                         snippet = CASE WHEN ?3 = 1 THEN snippet ELSE ?6 END,
                         state = ?7, scope = ?8, updated_at = ?9,
                         why = CASE WHEN ?7 = state THEN why ELSE NULL END
                     WHERE id = ?1",
                    rusqlite::params![
                        id,
                        seen.from,
                        seen.from_me as i64,
                        seen.message_id,
                        seen.date,
                        snippet,
                        next,
                        scope,
                        now,
                    ],
                )?;
            }
        }
        conn.execute(
            "INSERT OR IGNORE INTO mail_thread_messages (message_id, thread_id, memory_refs)
             VALUES (?1, ?2, ?3)",
            [&seen.message_id, &id, &refs],
        )?;
        super::journal_row(&conn, actor, "mail_thread", "mail_threads", &id, "update")?;
        Ok(Some(id))
    }

    /// Threads in these states (all when empty), newest first, as the
    /// vault allows.
    pub fn mail_threads(&self, states: &[&str], limit: usize) -> Result<Vec<MailThread>> {
        let guard = self.mail_thread_guard();
        let conn = self.lock();
        let filter = if states.is_empty() {
            String::new()
        } else {
            format!(
                " AND state IN ({})",
                states
                    .iter()
                    .filter(|s| MAIL_STATES.contains(s))
                    .map(|s| format!("'{s}'"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT {THREAD_COLS} FROM mail_threads WHERE 1=1{filter}{guard}
             ORDER BY COALESCE(due_by, '9999'), last_at DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], thread_from_row)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    pub fn mail_thread(&self, id: &str) -> Result<Option<MailThread>> {
        let guard = self.mail_thread_guard();
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {THREAD_COLS} FROM mail_threads WHERE id = ?1{guard}"),
            [id],
            thread_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Threads waiting for triage — never private ones (they never go to
    /// a model), oldest first.
    pub fn mail_threads_to_triage(&self, limit: usize) -> Result<Vec<MailThread>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {THREAD_COLS} FROM mail_threads WHERE state = 'new' AND scope != 'private'
             ORDER BY last_at LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], thread_from_row)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Threads waiting for triage that this node sorts (0.5.98): those of
    /// the mailboxes in `accounts`, and with `unwatched` also those of a
    /// mailbox none of the watched ones (`account_filter`).
    pub fn mail_threads_to_triage_for(
        &self,
        limit: usize,
        accounts: &[String],
        unwatched: Option<&[String]>,
    ) -> Result<Vec<MailThread>> {
        self.mail_threads_to_triage_from(0, limit, accounts, unwatched)
    }

    /// `mail_threads_to_triage_for`, past the first `skip` of them: triage
    /// pages past threads that wait (their mail can't be checked here yet).
    pub fn mail_threads_to_triage_from(
        &self,
        skip: usize,
        limit: usize,
        accounts: &[String],
        unwatched: Option<&[String]>,
    ) -> Result<Vec<MailThread>> {
        let conn = self.lock();
        let (filter, params) = account_filter(accounts, unwatched, 3);
        let mut stmt = conn.prepare(&format!(
            "SELECT {THREAD_COLS} FROM mail_threads
             WHERE state = 'new' AND scope != 'private' AND {filter}
             ORDER BY last_at, id LIMIT ?1 OFFSET ?2"
        ))?;
        let mut all: Vec<rusqlite::types::Value> =
            vec![(limit as i64).into(), (skip as i64).into()];
        all.extend(params);
        let rows = stmt.query_map(rusqlite::params_from_iter(all), thread_from_row)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Triage's verdict on a thread still waiting for one.
    pub fn mail_classify(
        &self,
        actor: &Actor,
        id: &str,
        state: &str,
        why: &str,
        confidence: f64,
        due_by: Option<&str>,
    ) -> Result<bool> {
        if !MAIL_STATES.contains(&state) {
            bail!("unknown mail state {state:?}");
        }
        let now = now_utc();
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE mail_threads SET state = ?2, why = ?3, confidence = ?4, due_by = ?5,
                 triaged_at = ?6, updated_at = ?6
             WHERE id = ?1 AND state = 'new'",
            rusqlite::params![id, state, why, confidence, due_by, now],
        )?;
        if changed > 0 {
            super::journal_row(&conn, actor, "mail_thread", "mail_threads", id, "update")?;
        }
        Ok(changed > 0)
    }

    /// Make a thread private: one of its messages was found private after
    /// it joined. True when this changed it.
    pub fn mail_thread_make_private(&self, actor: &Actor, id: &str) -> Result<bool> {
        let now = now_utc();
        let conn = self.lock();
        make_private(&conn, actor, id, &now)
    }

    /// The thread a message already joined on this node (its own index),
    /// if any.
    pub fn mail_thread_of_message(&self, message_id: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT thread_id FROM mail_thread_messages WHERE message_id = ?1",
            [message_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// The messages this node has seen in a thread (its own index).
    pub fn mail_thread_message_ids(&self, thread_id: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT message_id FROM mail_thread_messages WHERE thread_id = ?1")?;
        let rows = stmt.query_map([thread_id], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// The messages this node has seen in a thread, each with the keys its
    /// memory can be stored under (`MailSeen::memory_refs`; empty: keyed by
    /// a hash of its text; None: recorded before v70 kept them, or
    /// unreadable — the message id alone is known).
    pub fn mail_thread_message_keys(
        &self,
        thread_id: &str,
    ) -> Result<Vec<(String, Option<Vec<String>>)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT message_id, memory_refs FROM mail_thread_messages WHERE thread_id = ?1",
        )?;
        let rows = stmt.query_map([thread_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, refs) = row?;
            let refs = refs.and_then(|r| serde_json::from_str::<Vec<String>>(&r).ok());
            out.push((id, refs));
        }
        Ok(out)
    }

    /// Keep the keys a message's memory can be stored under, for a message
    /// recorded before the index kept them (`memory_refs` NULL); a message
    /// that has them keeps them.
    pub fn mail_thread_message_set_refs(&self, message_id: &str, refs: &[String]) -> Result<()> {
        let refs = serde_json::to_string(refs)?;
        let conn = self.lock();
        conn.execute(
            "UPDATE mail_thread_messages SET memory_refs = ?2
             WHERE message_id = ?1 AND memory_refs IS NULL",
            [message_id, refs.as_str()],
        )?;
        Ok(())
    }

    /// The owner moves a thread (done, not needed, needs a reply after all…).
    pub fn mail_set_state(&self, actor: &Actor, id: &str, state: &str) -> Result<MailThread> {
        if !MAIL_STATES.contains(&state) {
            bail!("a thread's state is one of {}", MAIL_STATES.join(", "));
        }
        let now = now_utc();
        {
            let conn = self.lock();
            let changed = conn.execute(
                "UPDATE mail_threads SET state = ?2, updated_at = ?3,
                     why = CASE WHEN ?2 = state THEN why ELSE 'you said so' END
                 WHERE id = ?1",
                rusqlite::params![id, state, now],
            )?;
            if changed == 0 {
                bail!("no mail thread {id:?}");
            }
            record_mutation(
                &conn,
                actor,
                "mail_thread_state",
                &format!("mail_thread_state:{id}:{now}"),
                &json!({"id": id, "state": state}),
            )?;
            super::journal_row(&conn, actor, "mail_thread", "mail_threads", id, "update")?;
        }
        self.mail_thread(id)?
            .ok_or_else(|| anyhow!("no mail thread {id:?}"))
    }

    /// How many threads are in each state (as the vault allows).
    pub fn mail_counts(&self) -> Result<Vec<(String, i64)>> {
        let guard = self.mail_thread_guard();
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT state, COUNT(*) FROM mail_threads WHERE 1=1{guard} GROUP BY state"
        ))?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /* ————— the owner's rules ————— */

    pub fn mail_rules(&self) -> Result<Vec<MailRule>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, pattern, action, note, enabled, created_at FROM mail_rules
             ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MailRule {
                id: r.get(0)?,
                kind: r.get(1)?,
                pattern: r.get(2)?,
                action: r.get(3)?,
                note: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
                created_at: r.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Add an owner rule. `plain` rules are instructions for triage
    /// (their action is `triage`).
    pub fn mail_rule_add(
        &self,
        actor: &Actor,
        kind: &str,
        pattern: &str,
        action: &str,
        note: Option<&str>,
    ) -> Result<MailRule> {
        if !RULE_KINDS.contains(&kind) {
            bail!("a rule matches one of: {}", RULE_KINDS.join(", "));
        }
        // a plain rule is the owner's words for the sorter (`triage`) or
        // for reply drafts (`draft`, 0.5.94)
        let action = match (kind, action) {
            ("plain", "draft") => "draft",
            ("plain", _) => "triage",
            (_, "draft") => bail!("only a plain rule guides drafts"),
            (_, a) => a,
        };
        if !RULE_ACTIONS.contains(&action) {
            bail!("a rule does one of: {}", RULE_ACTIONS.join(", "));
        }
        let pattern = pattern.trim();
        if pattern.len() < 2 || pattern.chars().count() > 500 {
            bail!("give the rule something to match (2–500 characters)");
        }
        let id = format!("mr_{}", uuid_ish());
        let now = now_utc();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO mail_rules (id, kind, pattern, action, note, enabled, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)",
            rusqlite::params![id, kind, pattern, action, note, now],
        )?;
        record_mutation(
            &conn,
            actor,
            "mail_rule_added",
            &format!("mail_rule_added:{id}"),
            &json!({"id": id, "kind": kind, "pattern": pattern, "action": action}),
        )?;
        super::journal_row(&conn, actor, "mail_rule", "mail_rules", &id, "create")?;
        Ok(MailRule {
            id,
            kind: kind.into(),
            pattern: pattern.into(),
            action: action.into(),
            note: note.map(String::from),
            enabled: true,
            created_at: now,
        })
    }

    pub fn mail_rule_remove(&self, actor: &Actor, id: &str) -> Result<bool> {
        let conn = self.lock();
        super::journal_op(&conn, actor, "mail_rule", id, "delete", None)?;
        let gone = conn.execute("DELETE FROM mail_rules WHERE id = ?1", [id])?;
        if gone > 0 {
            record_mutation(
                &conn,
                actor,
                "mail_rule_removed",
                &format!("mail_rule_removed:{id}"),
                &json!({"id": id}),
            )?;
        }
        Ok(gone > 0)
    }
}

fn uuid_ish() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    crate::ids::content_hash(format!("{t}:{:?}", std::thread::current().id()).as_bytes())[..12]
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(&tmp.path().join("t.db")).unwrap();
        (s, tmp)
    }

    fn msg(id: &str, from: &str, primary: bool) -> MailSeen {
        MailSeen {
            account: "me@x.example".into(),
            message_id: id.into(),
            from: format!("{from} <{}@ex.example>", from.to_lowercase()),
            from_addr: format!("{}@ex.example", from.to_lowercase()),
            subject: "the lease".into(),
            date: Some("2026-11-05T10:00:00Z".into()),
            primary,
            snippet: "Can you send it by Friday?".into(),
            ..Default::default()
        }
    }

    #[test]
    fn test_threads_follow_replies_and_only_decisions_become_rows() {
        let (s, _t) = store();
        let a = Actor::new("adapter:test");
        // a newsletter (not Primary) never becomes a thread
        assert!(s
            .mail_observe(&a, &msg("n1@x", "News", false))
            .unwrap()
            .is_none());
        // a Primary message from someone else does
        let t = s
            .mail_observe(&a, &msg("m1@x", "Ann", true))
            .unwrap()
            .unwrap();
        assert_eq!(s.mail_thread(&t).unwrap().unwrap().state, "new");
        // seen again: nothing changes
        assert!(s
            .mail_observe(&a, &msg("m1@x", "Ann", true))
            .unwrap()
            .is_none());
        // my reply joins it and marks it replied
        let mut mine = msg("m2@x", "Me", false);
        mine.from_me = true;
        mine.in_reply_to = Some("m1@x".into());
        mine.references = vec!["m1@x".into()];
        assert_eq!(
            s.mail_observe(&a, &mine).unwrap().as_deref(),
            Some(t.as_str())
        );
        s.mail_classify(&a, &t, "needs_reply", "send it by Friday", 0.9, None)
            .unwrap();
        // (classify only touches threads still waiting: it was replied)
        assert_eq!(s.mail_thread(&t).unwrap().unwrap().state, "replied");
        // her answer reopens it, threaded by References
        let mut back = msg("m3@x", "Ann", false);
        back.references = vec!["m1@x".into(), "m2@x".into()];
        assert_eq!(
            s.mail_observe(&a, &back).unwrap().as_deref(),
            Some(t.as_str())
        );
        let th = s.mail_thread(&t).unwrap().unwrap();
        assert_eq!((th.state.as_str(), th.message_count), ("new", 3));
        // the id is the same on every node: a hash of the root
        assert_eq!(t, thread_id_for("m1@x"));
    }

    /// Several mailboxes (0.5.98): a node sorts only the threads of the
    /// mailboxes it handles — plus, as the fallback node, those of a
    /// mailbox nobody watches — and a draft remembers its thread's mailbox.
    #[test]
    fn test_each_node_takes_only_its_mailboxes_threads_and_drafts() {
        let (s, _t) = store();
        let a = Actor::new("adapter:test");
        let mut ids = std::collections::BTreeMap::new();
        for (account, mid) in [
            ("Me@X.example", "h1@x"),
            ("jon@work.example", "w1@x"),
            ("", "o1@x"),
        ] {
            let mut m = msg(mid, "Ann", true);
            m.account = account.into();
            ids.insert(mid, s.mail_observe(&a, &m).unwrap().unwrap());
        }
        let sorted = |mine: &[&str], watched: Option<&[&str]>| -> Vec<String> {
            let mine: Vec<String> = mine.iter().map(|x| x.to_string()).collect();
            let watched: Option<Vec<String>> =
                watched.map(|w| w.iter().map(|x| x.to_string()).collect());
            let mut got: Vec<String> = s
                .mail_threads_to_triage_for(10, &mine, watched.as_deref())
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect();
            got.sort();
            got
        };
        let want = |list: &[&str]| -> Vec<String> {
            let mut v: Vec<String> = list.iter().map(|m| ids[m].clone()).collect();
            v.sort();
            v
        };
        // case doesn't matter
        assert_eq!(sorted(&["me@x.example"], None), want(&["h1@x"]));
        // the fallback node also takes what no watched mailbox covers
        assert_eq!(
            sorted(
                &["me@x.example"],
                Some(&["me@x.example", "jon@work.example"])
            ),
            want(&["h1@x", "o1@x"])
        );
        assert_eq!(sorted(&[], Some(&[])), want(&["h1@x", "w1@x", "o1@x"]));
        assert!(sorted(&[], None).is_empty());

        // a draft of the work thread is for the work mailbox
        let owner = Actor::new("human:owner");
        let d = s
            .mail_draft_request(&owner, Some(&ids["w1@x"]), None, None, false, None)
            .unwrap();
        assert_eq!(d.account, "jon@work.example");
        let waiting = |mine: &[&str], watched: Option<&[&str]>| -> usize {
            let mine: Vec<String> = mine.iter().map(|x| x.to_string()).collect();
            let watched: Option<Vec<String>> =
                watched.map(|w| w.iter().map(|x| x.to_string()).collect());
            s.mail_drafts_waiting_for(10, &mine, watched.as_deref())
                .unwrap()
                .len()
        };
        assert_eq!(waiting(&["jon@work.example"], None), 1);
        assert_eq!(waiting(&["me@x.example"], None), 0);
        assert_eq!(waiting(&["me@x.example"], Some(&["me@x.example"])), 1);
    }

    #[test]
    fn test_owner_rules_decide_first_and_private_mail_never_reaches_triage() {
        let (s, _t) = store();
        let a = Actor::new("human:test");
        s.mail_rule_add(&a, "domain", "ex.example", "fyi", None)
            .unwrap();
        let t = s
            .mail_observe(&a, &msg("r1@x", "Bo", false))
            .unwrap()
            .unwrap();
        let th = s.mail_thread(&t).unwrap().unwrap();
        assert_eq!(
            th.state, "fyi",
            "a rule can make a thread of non-Primary mail"
        );
        assert!(th.why.unwrap().contains("your rule"));
        let ignore = s
            .mail_rule_add(&a, "subject", "unsubscribe", "ignore", None)
            .unwrap();
        let mut spam = msg("s1@x", "Ads", true);
        spam.subject = "Unsubscribe here".into();
        spam.from_addr = "ads@other.example".into();
        spam.from = "Ads <ads@other.example>".into();
        // the first rule added wins: domain doesn't match, subject does
        assert!(
            s.mail_observe(&a, &spam).unwrap().is_none(),
            "ignored mail isn't a row"
        );
        assert!(s.mail_rule_remove(&a, &ignore.id).unwrap());
        assert!(
            s.mail_rule_add(&a, "plain", "x", "fyi", None).is_err(),
            "too short"
        );
        let plain = s
            .mail_rule_add(
                &a,
                "plain",
                "Anything from my accountant needs a reply",
                "fyi",
                None,
            )
            .unwrap();
        assert_eq!(plain.action, "triage");

        let mut private = msg("p1@x", "Cy", true);
        private.private = true;
        private.from_addr = "cy@third.example".into();
        let p = s.mail_observe(&a, &private).unwrap().unwrap();
        assert!(s
            .mail_threads_to_triage(10)
            .unwrap()
            .iter()
            .all(|t| t.id != p));
        assert!(
            s.mail_thread(&p).unwrap().is_none(),
            "hidden while the vault is locked"
        );
        s.vault_unlock(5);
        assert_eq!(s.mail_thread(&p).unwrap().unwrap().scope, "private");
    }

    /// A message seen again once it is private (its memory arrived private
    /// after it joined) makes the thread it joined private, journaled for
    /// the other nodes; triage no longer takes the thread, and seeing the
    /// message again not private leaves it private.
    #[test]
    fn test_a_message_found_private_later_makes_its_thread_private() {
        let (s, _t) = store();
        let a = Actor::new("adapter:test");
        let t = s
            .mail_observe(&a, &msg("m1@x", "Ann", true))
            .unwrap()
            .unwrap();
        assert_eq!(s.mail_threads_to_triage(10).unwrap().len(), 1);

        let mut now_private = msg("m1@x", "Ann", true);
        now_private.private = true;
        assert_eq!(
            s.mail_observe(&a, &now_private).unwrap().as_deref(),
            Some(t.as_str())
        );
        assert!(s.mail_threads_to_triage(10).unwrap().is_empty());
        assert!(s
            .mail_threads_to_triage_for(10, &[], Some(&[]))
            .unwrap()
            .is_empty());
        let journaled: String = s
            .lock()
            .query_row(
                "SELECT post_image FROM op_journal
                 WHERE entity_kind = 'mail_thread' AND entity_id = ?1
                 ORDER BY seq DESC LIMIT 1",
                [&t],
                |r| r.get(0),
            )
            .unwrap();
        assert!(journaled.contains("\"private\""), "{journaled}");

        // already private: nothing changes, and nothing makes it un-private
        assert!(s.mail_observe(&a, &now_private).unwrap().is_none());
        assert!(s
            .mail_observe(&a, &msg("m1@x", "Ann", true))
            .unwrap()
            .is_none());
        s.vault_unlock(5);
        let th = s.mail_thread(&t).unwrap().unwrap();
        assert_eq!((th.scope.as_str(), th.message_count), ("private", 1));
    }
}
