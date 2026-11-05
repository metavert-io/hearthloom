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
//! One warm read-only session is reused (its own, not the watch's IDLE
//! session) and reopened on any error.
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use hearthloom_adapters::gmail_imap::{is_auth_failure, FoundMessage, GmailSearch};
use serde_json::json;

use crate::virtual_source::VirtualHit;
use crate::AppState;

pub const SOURCE: &str = "gmail";
pub const ADAPTER: &str = "gmail";
/// Bytes of one message read for vfetch (attachments come after the text).
const FETCH_CAP: usize = 2 * 1024 * 1024;
/// Characters of a message's readable text vfetch returns.
const TEXT_CAP: usize = 12_000;

static SESSION: Mutex<Option<GmailSearch>> = Mutex::new(None);

fn with_session<T>(f: impl Fn(&mut GmailSearch) -> Result<T>) -> Result<T> {
    let (user, password) = crate::gmail_pull::credentials()
        .ok_or_else(|| anyhow!("Gmail isn't connected on this node"))?;
    let mut guard = SESSION.lock().unwrap_or_else(|p| p.into_inner());
    // a warm session first; on any error, one fresh session
    if let Some(session) = guard.as_mut() {
        match f(session) {
            Ok(v) => return Ok(v),
            Err(_) => {
                if let Some(old) = guard.take() {
                    old.logout();
                }
            }
        }
    }
    let mut session = GmailSearch::connect(&user, &password).map_err(|e| {
        if is_auth_failure(&e) {
            anyhow!("Gmail refused the sign-in (reconnect Gmail on the Integrations page)")
        } else {
            e
        }
    })?;
    let out = f(&mut session);
    *guard = Some(session);
    out
}

/// Drop the session (Gmail disconnected or its password changed).
pub fn reset() {
    if let Some(old) = SESSION.lock().unwrap_or_else(|p| p.into_inner()).take() {
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

fn hit(found: &FoundMessage) -> VirtualHit {
    let mut raw = found.header.clone();
    raw.extend_from_slice(b"\r\n");
    raw.extend_from_slice(&found.text_head);
    let r = read(&raw);
    let path = r
        .message_id
        .clone()
        .unwrap_or_else(|| format!("uid:{}", found.uid));
    let preview: String = r.text.chars().take(160).collect();
    VirtualHit {
        path,
        snippet: format!(
            "{} | {} | {} | {}{}",
            r.date,
            r.from,
            r.subject,
            found.labels.join(","),
            if preview.is_empty() {
                String::new()
            } else {
                format!(" | text: {preview}…")
            }
        ),
        text: Some(format!(
            "{}\n{}\n{}\n{}\n{}",
            r.from, r.to, r.subject, r.text, r.links
        )),
    }
}

/// Gmail's own search, newest first.
pub fn search(query: &str, limit: usize) -> Result<Vec<VirtualHit>> {
    if query.trim().is_empty() {
        bail!("give Gmail something to search for");
    }
    let found = with_session(|s| s.search(query, limit))?;
    Ok(found.iter().map(hit).collect())
}

/// One message, as text: who, when, subject, its readable text, links.
pub fn fetch(path: &str) -> Result<String> {
    let raw = with_session(|s| s.fetch(path, FETCH_CAP))?
        .ok_or_else(|| anyhow!("no message {path:?} in Gmail"))?;
    let r = read(&raw);
    let text: String = r.text.chars().take(TEXT_CAP).collect();
    Ok(format!(
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
    ))
}

/// Keep the `gmail` source registered while Gmail is connected here.
pub fn ensure_registered(state: &AppState) {
    let Some((user, _)) = crate::gmail_pull::credentials() else {
        return;
    };
    let known = state
        .store
        .list_virtual_sources()
        .unwrap_or_default()
        .into_iter()
        .any(|d| d.name == SOURCE && d.config["account"].as_str() == Some(user.as_str()));
    if !known {
        let _ = state.store.register_virtual_source(
            &hearthloom_core::Actor::new("system:gmail"),
            SOURCE,
            ADAPTER,
            &json!({"account": user, "trust": "imported"}),
            &["search".into(), "fetch".into()],
        );
    }
}

/// The account this node searches, when Gmail is connected here.
pub fn account() -> Option<String> {
    crate::gmail_pull::credentials().map(|(user, _)| user)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let h = hit(&found);
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
        assert!(h.text.unwrap().contains("portal.neutral.example/clients"));
    }
}
