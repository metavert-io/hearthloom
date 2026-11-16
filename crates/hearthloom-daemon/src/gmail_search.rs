//! Searching the whole Gmail mailbox where it lives (0.5.89).
//!
//! The node with the Gmail connection can ask Gmail itself: its search
//! (the Gmail search box's syntax, server-side, over All Mail — every
//! message the account holds, 2007 to now) and one message at a time,
//! read-only. Nothing is copied ahead of time and nothing goes stale, so a
//! Takeout file isn't needed for Ask, and no file has to follow the
//! owner's nodes around: Ask on another node reaches this source through
//! the hub (lookups.rs) like any archive.
//!
//! It appears as the virtual source `gmail` (adapter `gmail`), registered
//! while Gmail is connected here. Reads go through the same privacy check
//! as the archives (`virtual_source::{search,fetch}_with`): its messages
//! are the Gmail pull's `mbox` memories, keyed by the same Message-ID.
//! Each mailbox keeps one warm read-only session (its own, not the watch's
//! IDLE session), reopened on any error.
//!
//! With several mailboxes (0.5.98) the one `gmail` source searches them
//! all: hits come back newest first, one per Message-ID, each naming its
//! mailbox. A hit without a Message-ID is `uid:<n>` in the first mailbox
//! and `uid:<id>:<n>` in an added one. A mailbox Google refused is
//! skipped (and stops being tried) until it's saved again.
use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use hearthloom_adapters::gmail_imap::{
    is_auth_failure, is_message_id, FetchedMessage, FoundMessage, GmailSearch, SEARCH_TEXT_HEAD,
};
use serde_json::json;

use crate::mailboxes::{self, Mailbox};
use crate::virtual_source::{mail_key, Fetched, ItemKey, Privacy, VirtualHit};
use crate::AppState;

pub const SOURCE: &str = "gmail";
pub const ADAPTER: &str = "gmail";
/// Bytes of one message read for vfetch (attachments come after the text).
const FETCH_CAP: usize = 2 * 1024 * 1024;
/// Bytes of one message read for the privacy check when what was read
/// first was cut — more than any message Gmail accepts (50 MB, encoded).
/// A message longer still can't be checked, and counts as private.
const WHOLE_CAP: usize = 96 * 1024 * 1024;
/// Characters of a message's readable text vfetch returns.
const TEXT_CAP: usize = 12_000;

/// One warm session per mailbox (by id). Each is used under this lock, so
/// two searches never share a connection.
static SESSIONS: Mutex<BTreeMap<String, GmailSearch>> = Mutex::new(BTreeMap::new());

fn with_session<T>(mb: &Mailbox, f: impl Fn(&mut GmailSearch) -> Result<T>) -> Result<T> {
    mailboxes::check_refused(mb)?;
    let mut guard = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    // a warm session first; on any error, one fresh session
    if let Some(session) = guard.get_mut(&mb.id) {
        match f(session) {
            Ok(v) => return Ok(v),
            Err(_) => {
                if let Some(old) = guard.remove(&mb.id) {
                    old.logout();
                }
            }
        }
    }
    let mut session = GmailSearch::connect(&mb.address, mb.password()).map_err(|e| {
        if is_auth_failure(&e) {
            // stop trying it; its watch tells the owner (mailboxes.rs)
            mailboxes::refuse(None, mb, &e);
            anyhow!(
                "Gmail refused the sign-in for {} (reconnect it on the Integrations page)",
                mb.address
            )
        } else {
            e
        }
    })?;
    let out = f(&mut session);
    guard.insert(mb.id.clone(), session);
    out
}

/// Drop a mailbox's session (removed, or its password changed) — every
/// mailbox's with `None`.
pub fn reset(id: Option<&str>) {
    let mut guard = SESSIONS.lock().unwrap_or_else(|p| p.into_inner());
    let gone: Vec<GmailSearch> = match id {
        Some(id) => guard.remove(id).into_iter().collect(),
        None => std::mem::take(&mut *guard).into_values().collect(),
    };
    drop(guard);
    for old in gone {
        old.logout();
    }
}

/// Header fields and readable text of a (possibly partial) raw message.
struct Read {
    message_id: Option<String>,
    from: String,
    to: String,
    subject: String,
    date: String,
    text: String,
    links: String,
}

fn read(raw: &[u8]) -> Read {
    let parsed = mail_parser::MessageParser::default().parse(raw);
    let addr = |a: Option<mail_parser::AddressList>| -> String {
        a.map(|list| {
            list.mailboxes()
                .map(|x| match (x.name(), x.address()) {
                    (Some(n), Some(e)) => format!("{n} <{e}>"),
                    (None, Some(e)) => e.to_string(),
                    (Some(n), None) => n.to_string(),
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
    };
    let (text, links) = crate::mbox_text::message_text(raw);
    match parsed {
        Some(m) => Read {
            message_id: m.message_id().map(String::from),
            from: addr(m.from()),
            to: addr(m.to()),
            subject: m.subject().unwrap_or("").to_string(),
            date: m.date().map(|d| d.to_rfc3339()).unwrap_or_default(),
            text,
            links,
        },
        None => Read {
            message_id: None,
            from: String::new(),
            to: String::new(),
            subject: String::new(),
            date: String::new(),
            text,
            links,
        },
    }
}

/// A hit's path when the message has no Message-ID.
fn uid_path(mb: Option<&Mailbox>, uid: u32) -> String {
    match mb {
        Some(mb) if !mb.first => format!("uid:{}:{uid}", mb.id),
        _ => format!("uid:{uid}"),
    }
}

/// `uid:<n>` (the first mailbox) or `uid:<id>:<n>` → (mailbox id, uid).
pub(crate) fn parse_uid_path(path: &str) -> Option<(Option<&str>, u32)> {
    let rest = path.strip_prefix("uid:")?;
    match rest.split_once(':') {
        Some((id, uid)) => Some((Some(id), uid.parse().ok()?)),
        None => Some((None, rest.parse().ok()?)),
    }
}

/// The key a message's memory carries (`mail_key`: its Message-ID as the
/// import reads it, and as `parsed` spells it), plus its uid path in the
/// mailbox it was read from — the path a promotion made before promotions
/// kept the Message-ID was stored under.
fn key_of(raw: &[u8], parsed: Option<&str>, uid_path: String) -> ItemKey {
    match mail_key(&String::from_utf8_lossy(raw), parsed) {
        ItemKey::Refs(mut refs) => {
            refs.push(uid_path);
            ItemKey::Refs(refs)
        }
        other => other,
    }
}

/// `mailbox` is where it was found (its uid path); `label` names it in the
/// snippet (several mailboxes searched).
fn hit(found: &FoundMessage, mailbox: Option<&Mailbox>, label: bool) -> VirtualHit {
    let mut raw = found.header.clone();
    raw.extend_from_slice(b"\r\n");
    raw.extend_from_slice(&found.text_head);
    let r = read(&raw);
    let at = uid_path(mailbox, found.uid);
    let path = r.message_id.clone().unwrap_or_else(|| at.clone());
    let preview: String = r.text.chars().take(160).collect();
    // the privacy rules read the whole message: a text the search cut is
    // read whole, by its uid, before the vault lets the hit through
    let privacy = if found.text_head.len() < SEARCH_TEXT_HEAD {
        Privacy::Message {
            labels: found.labels.join(", "),
            raw: raw.clone(),
        }
    } else {
        Privacy::ReadAt(at.clone())
    };
    VirtualHit {
        path,
        snippet: format!(
            "{} | {} | {} | {}{}{}",
            r.date,
            r.from,
            r.subject,
            found.labels.join(","),
            mailbox
                .filter(|_| label)
                .map(|mb| format!(" | in {}", mb.address))
                .unwrap_or_default(),
            if preview.is_empty() {
                String::new()
            } else {
                format!(" | text: {preview}…")
            }
        ),
        privacy: Some(privacy),
        key: key_of(&found.header, r.message_id.as_deref(), at),
    }
}

/// When a hit was sent, for merging mailboxes newest first.
fn sent_at(found: &FoundMessage) -> i64 {
    mail_parser::MessageParser::default()
        .parse_headers(&found.header)
        .and_then(|m| m.date().map(|d| d.to_timestamp()))
        .unwrap_or(0)
}

/// Gmail's own search over every mailbox here, newest first, one hit per
/// message. A mailbox that can't be reached is skipped while others answer.
pub fn search(query: &str, limit: usize) -> Result<Vec<VirtualHit>> {
    if query.trim().is_empty() {
        bail!("give Gmail something to search for");
    }
    let boxes: Vec<Mailbox> = mailboxes::all()
        .into_iter()
        .filter(|mb| !mailboxes::refused(&mb.id))
        .collect();
    if boxes.is_empty() {
        bail!("Gmail isn't connected on this node (or it refused the sign-in)");
    }
    let several = boxes.len() > 1;
    let mut found = Vec::new();
    let mut first_err = None;
    for mb in &boxes {
        match with_session(mb, |s| s.search(query, limit)) {
            Ok(list) => found.extend(list.into_iter().map(|f| (sent_at(&f), mb.clone(), f))),
            Err(err) => {
                tracing::warn!("gmail search: {}: {err:#}", mb.address);
                first_err.get_or_insert(err);
            }
        }
    }
    if found.is_empty() {
        if let Some(err) = first_err {
            return Err(err);
        }
    }
    Ok(merge(found, several, limit))
}

/// Newest first, one hit per path (a message both mailboxes hold).
fn merge(
    mut found: Vec<(i64, Mailbox, FoundMessage)>,
    several: bool,
    limit: usize,
) -> Vec<VirtualHit> {
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut seen = std::collections::BTreeSet::new();
    found
        .iter()
        .map(|(_, mb, f)| hit(f, Some(mb), several))
        .filter(|h| seen.insert(h.path.clone()))
        .take(limit)
        .collect()
}

/// Whether a raw message's own Message-ID is `id` (angle brackets aside,
/// ASCII case ignored). Gmail's `rfc822msgid:` lookup is a search, so what
/// it returns is checked to be the message asked for.
fn carries(raw: &[u8], id: &str) -> bool {
    mail_parser::MessageParser::default()
        .parse_headers(raw)
        .and_then(|m| m.message_id().map(String::from))
        .is_some_and(|own| {
            own.trim_matches(|c| c == '<' || c == '>')
                .eq_ignore_ascii_case(id.trim_matches(|c| c == '<' || c == '>'))
        })
}

/// One message, as text — who, when, subject, its readable text, links —
/// and the key its memory carries, whatever path reached it (a `uid:` path
/// names none, and another parser may spell the Message-ID differently
/// from the import): the vault check needs the message's identity, not
/// only the path asked for.
pub(crate) fn fetch(path: &str) -> Result<Fetched> {
    let not_found = || anyhow!("no message {path:?} in Gmail");
    if parse_uid_path(path).is_none() && !is_message_id(path) {
        return Err(not_found());
    }
    let boxes = mailboxes::all();
    let (mb, message) = match parse_uid_path(path) {
        // a uid means something in one mailbox only
        Some((id, uid)) => {
            let mb =
                uid_mailbox(&boxes, id).ok_or_else(|| anyhow!("no mailbox here holds {path:?}"))?;
            let found = with_session(mb, |s| s.fetch(&format!("uid:{uid}"), FETCH_CAP))?;
            (mb, found)
        }
        // a Message-ID: whichever mailbox has it
        None => {
            let mut out = None;
            let mut first_err = None;
            for mb in boxes.iter().filter(|mb| !mailboxes::refused(&mb.id)) {
                match with_session(mb, |s| s.fetch(path, FETCH_CAP)) {
                    Ok(Some(m)) if carries(&m.raw, path) => {
                        out = Some((mb, m));
                        break;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        first_err.get_or_insert(err);
                    }
                }
            }
            match (out, first_err) {
                (Some((mb, m)), _) => (mb, Some(m)),
                (None, Some(err)) => return Err(err),
                (None, None) => return Err(not_found()),
            }
        }
    };
    let message = message.ok_or_else(not_found)?;
    Ok(as_text(&message, Some(mb), FETCH_CAP))
}

/// The mailbox a uid path names: an added one by its id, else the first.
fn uid_mailbox<'a>(boxes: &'a [Mailbox], id: Option<&str>) -> Option<&'a Mailbox> {
    match id {
        Some(id) => boxes.iter().find(|mb| mb.id == id),
        None => boxes.iter().find(|mb| mb.first).or(boxes.first()),
    }
}

/// What the privacy rules read for the message at this uid path, read
/// whole: None when it can't be read, or is longer than `WHOLE_CAP` — a
/// message that can't be read whole counts as private.
pub(crate) fn read_whole(at: &str) -> Option<String> {
    let (id, uid) = parse_uid_path(at)?;
    let boxes = mailboxes::all();
    let mb = uid_mailbox(&boxes, id)?;
    let message = match with_session(mb, |s| s.fetch(&format!("uid:{uid}"), WHOLE_CAP)) {
        Ok(Some(message)) => message,
        Ok(None) => return None,
        Err(err) => {
            tracing::warn!("gmail: reading {at} whole for the privacy check: {err:#}");
            return None;
        }
    };
    match as_text(&message, Some(mb), WHOLE_CAP).privacy {
        Privacy::Message { raw, labels } => {
            Some(format!("{}{labels}", crate::mbox_text::privacy_text(&raw)))
        }
        _ => None,
    }
}

/// A fetched message as vfetch shows it, with the key its memory carries
/// and what the privacy rules read: the whole message, or — when the read
/// stopped at `cap` — where to read it whole from.
pub(crate) fn as_text(message: &FetchedMessage, mailbox: Option<&Mailbox>, cap: usize) -> Fetched {
    let raw = &message.raw;
    let r = read(raw);
    let at = uid_path(mailbox, message.uid);
    let key = key_of(raw, r.message_id.as_deref(), at.clone());
    let privacy = if raw.len() < cap {
        Privacy::Message {
            raw: raw.clone(),
            labels: message.labels.join(", "),
        }
    } else {
        Privacy::ReadAt(at)
    };
    let text: String = r.text.chars().take(TEXT_CAP).collect();
    let shown = format!(
        "From: {}\nTo: {}\nDate: {}\nSubject: {}\n\n{}{}",
        r.from,
        r.to,
        r.date,
        r.subject,
        text,
        if r.links.is_empty() {
            String::new()
        } else {
            format!("\n\nLinks: {}", r.links)
        }
    );
    Fetched {
        shown,
        key,
        privacy,
    }
}

/// Keep the `gmail` source registered while Gmail is connected here, naming
/// every mailbox it searches (`account` stays the first, for older readers).
pub fn ensure_registered(state: &AppState) {
    let accounts = accounts();
    let Some(first) = accounts.first() else {
        return;
    };
    let known = state
        .store
        .list_virtual_sources()
        .unwrap_or_default()
        .into_iter()
        .any(|d| {
            d.name == SOURCE
                && d.config["account"].as_str() == Some(first.as_str())
                && d.config["accounts"] == json!(accounts)
        });
    if !known {
        let _ = state.store.register_virtual_source(
            &hearthloom_core::Actor::new("system:gmail"),
            SOURCE,
            ADAPTER,
            &json!({"account": first, "accounts": accounts, "trust": "imported"}),
            &["search".into(), "fetch".into()],
        );
    }
}

/// The mailboxes this node searches (addresses as saved), first one first.
pub fn accounts() -> Vec<String> {
    mailboxes::all().into_iter().map(|mb| mb.address).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_uid_path_names_its_mailbox() {
        assert_eq!(parse_uid_path("uid:42"), Some((None, 42)));
        assert_eq!(
            parse_uid_path("uid:1a2b3c4d:42"),
            Some((Some("1a2b3c4d"), 42))
        );
        assert_eq!(parse_uid_path("p1@neutral.example"), None);
        assert_eq!(parse_uid_path("uid:soon"), None);
    }

    /// Hits from two mailboxes: newest first, a message both hold once,
    /// each naming its mailbox, and a uid path that says which one.
    #[test]
    fn test_several_mailboxes_merge_newest_first_once_each() {
        let work = "jon@work.example";
        let id = mailboxes::id_for(work).to_uppercase();
        let boxes = mailboxes::from_vars(vec![
            ("GMAIL_IMAP_USER".into(), "me@mail.example".into()),
            ("GMAIL_IMAP_PASSWORD".into(), "p".into()),
            (format!("HEARTHLOOM_MAIL_{id}_USER"), work.into()),
            (format!("HEARTHLOOM_MAIL_{id}_PASSWORD"), "p".into()),
        ]);
        let found = |uid: u32, msgid: Option<&str>, date: &str| FoundMessage {
            uid,
            labels: vec![],
            header: format!(
                "From: a@x.example\r\nSubject: s{uid}\r\nDate: {date}\r\n{}",
                msgid
                    .map(|m| format!("Message-ID: <{m}>\r\n"))
                    .unwrap_or_default()
            )
            .into_bytes(),
            text_head: vec![],
        };
        let (old, dup, plain) = (
            found(1, Some("both@x"), "Mon, 5 Jan 2026 10:00:00 -0500"),
            found(7, Some("both@x"), "Mon, 5 Jan 2026 10:00:00 -0500"),
            found(9, None, "Tue, 6 Jan 2026 09:00:00 +0100"),
        );
        let list = vec![
            (sent_at(&old), boxes[0].clone(), old),
            (sent_at(&dup), boxes[1].clone(), dup),
            (sent_at(&plain), boxes[1].clone(), plain),
        ];
        let hits = merge(list, true, 10);
        assert_eq!(hits.len(), 2, "the message both hold, once");
        assert_eq!(
            hits[0].path,
            format!("uid:{}:9", boxes[1].id),
            "newest first"
        );
        assert!(hits[0].snippet.contains("| in jon@work.example"));
        assert_eq!(hits[1].path, "both@x");
        assert_eq!(merge(Vec::new(), true, 10).len(), 0);
    }

    /// A Message-ID lookup returns only the message asked for, and a read
    /// reports the message's own id whatever path reached it.
    #[test]
    fn test_a_fetch_is_the_message_asked_for() {
        let raw = b"From: Bank <alerts@bank.example>\r\nSubject: statement\r\n\
Message-ID: <Stmt-7@bank.example>\r\n\r\nYour balance.\r\n";
        assert!(carries(raw, "Stmt-7@bank.example"));
        assert!(carries(raw, "<stmt-7@BANK.example>"));
        assert!(!carries(raw, "p1@neutral.example"));
        assert!(!carries(
            b"From: a@x\r\n\r\nno id\r\n",
            "p1@neutral.example"
        ));
        let message = FetchedMessage {
            uid: 7,
            labels: vec!["Inbox".into()],
            raw: raw.to_vec(),
        };
        let fetched = as_text(&message, None, FETCH_CAP);
        assert!(fetched.shown.contains("Subject: statement"));
        assert_eq!(
            fetched.key,
            ItemKey::Refs(vec!["Stmt-7@bank.example".into(), "uid:7".into()])
        );
        assert_eq!(
            fetched.privacy,
            Privacy::Message {
                raw: raw.to_vec(),
                labels: "Inbox".into()
            }
        );
        // a read that stopped at the cap is read whole before it's judged
        assert_eq!(
            as_text(&message, None, raw.len()).privacy,
            Privacy::ReadAt("uid:7".into())
        );
        // a path that isn't a uid or one Message-ID is never looked up
        let err = fetch("p1@neutral.example OR from:bank.example")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("no message"), "{err}");
    }

    #[test]
    fn test_a_found_message_reads_as_a_hit() {
        let found = FoundMessage {
            uid: 42,
            labels: vec!["Inbox".into(), "Important".into()],
            header: b"From: =?UTF-8?Q?Susan_M=C3=BCller?= <susan@neutral.example>\r\n\
To: Jon <jon@x.example>\r\nSubject: Your client portal\r\n\
Date: Mon, 5 Jan 2026 10:00:00 -0500\r\nMessage-ID: <p1@neutral.example>\r\n\
Content-Type: text/plain; charset=utf-8\r\n"
                .to_vec(),
            text_head: b"Log in at https://portal.neutral.example/clients to upload.\r\n".to_vec(),
        };
        let h = hit(&found, None, false);
        assert_eq!(h.path, "p1@neutral.example");
        assert!(
            h.snippet.contains("Susan Müller <susan@neutral.example>"),
            "{}",
            h.snippet
        );
        assert!(h.snippet.contains("Your client portal"));
        assert!(h
            .snippet
            .contains("text: Log in at https://portal.neutral.example/clients"));
        assert!(matches!(
            &h.privacy,
            Some(Privacy::Message { raw, labels })
                if labels == "Inbox, Important"
                    && String::from_utf8_lossy(raw).contains("portal.neutral.example/clients")
        ));
        assert_eq!(
            h.key,
            ItemKey::Refs(vec!["p1@neutral.example".into(), "uid:42".into()])
        );

        // a text the search read only the start of: read whole by its uid
        let long = FoundMessage {
            text_head: vec![b'x'; SEARCH_TEXT_HEAD],
            ..found
        };
        assert_eq!(
            hit(&long, None, false).privacy,
            Some(Privacy::ReadAt("uid:42".into()))
        );
    }
}
