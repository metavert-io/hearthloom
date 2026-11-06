//! Reply drafts (v65, 0.5.94): asked for on any of the owner's nodes,
//! written where Gmail is watched, saved in the mailbox's Drafts folder.
//! Nothing here sends mail: a draft is what the owner reviews and sends
//! from their mail app.
//!
//! A row is a request first (`requested`, with the owner's words for what
//! it should say, or an ember's finished text), then `writing` on the node
//! that drafts, then `saved` (with the folder and the draft's Message-ID)
//! or `failed` (why). Rows mirror and journal like the other mail tables,
//! so the request reaches the drafting node and the outcome comes back.
use anyhow::{bail, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

pub const DRAFT_STATES: &[&str] = &["requested", "writing", "saved", "failed"];
/// What an owner's request may say, at most.
pub const SAY_CHARS: usize = 2000;
/// An ember's finished text, at most.
pub const BODY_CHARS: usize = 20_000;

#[derive(Debug, Clone, Default, Serialize)]
pub struct MailDraft {
    pub id: String,
    pub thread_id: Option<String>,
    /// the Message-ID answered (set by the drafter when the owner asked
    /// for a thread)
    pub reply_to: Option<String>,
    pub requested_by: String,
    /// the owner's words for what it should say
    pub say: Option<String>,
    pub reply_all: bool,
    pub body: Option<String>,
    /// what the draft leaves for the owner to fill in
    pub placeholders: Vec<String>,
    pub state: String,
    pub error: Option<String>,
    /// the node that wrote it
    pub node: Option<String>,
    pub folder: Option<String>,
    pub draft_message_id: Option<String>,
    pub recipients: Vec<String>,
    pub subject: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const COLS: &str = "id, thread_id, reply_to, requested_by, say, reply_all, body, placeholders, state, error, node, folder, draft_message_id, recipients, subject, created_at, updated_at";

fn list(json_text: Option<String>) -> Vec<String> {
    json_text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn row_to_draft(r: &rusqlite::Row) -> rusqlite::Result<MailDraft> {
    Ok(MailDraft {
        id: r.get(0)?,
        thread_id: r.get(1)?,
        reply_to: r.get(2)?,
        requested_by: r.get(3)?,
        say: r.get(4)?,
        reply_all: r.get::<_, i64>(5)? != 0,
        body: r.get(6)?,
        placeholders: list(r.get(7)?),
        state: r.get(8)?,
        error: r.get(9)?,
        node: r.get(10)?,
        folder: r.get(11)?,
        draft_message_id: r.get(12)?,
        recipients: list(r.get(13)?),
        subject: r.get(14)?,
        created_at: r.get(15)?,
        updated_at: r.get(16)?,
    })
}

/// What the drafter records once it has written (or failed).
#[derive(Debug, Clone, Default)]
pub struct DraftOutcome {
    pub reply_to: Option<String>,
    pub body: Option<String>,
    pub placeholders: Vec<String>,
    pub folder: Option<String>,
    pub draft_message_id: Option<String>,
    pub recipients: Vec<String>,
    pub subject: Option<String>,
    pub error: Option<String>,
}

/// Drafts of a thread a privacy rule covers since stay out of sight.
const NOT_PRIVATE: &str =
    "(thread_id IS NULL OR thread_id NOT IN (SELECT id FROM mail_threads WHERE scope = 'private'))";

impl Store {
    /// Ask for a draft. `thread_id` for the owner's own request (the
    /// drafter picks the message to answer); `reply_to` + `body` for an
    /// ember's finished text.
    #[allow(clippy::too_many_arguments)]
    pub fn mail_draft_request(
        &self,
        actor: &Actor,
        thread_id: Option<&str>,
        reply_to: Option<&str>,
        say: Option<&str>,
        reply_all: bool,
        body: Option<&str>,
    ) -> Result<MailDraft> {
        if thread_id.is_none() && reply_to.is_none() {
            bail!("say which thread or message to answer");
        }
        let say = say.map(str::trim).filter(|s| !s.is_empty());
        if say.is_some_and(|s| s.chars().count() > SAY_CHARS) {
            bail!("what it should say runs past {SAY_CHARS} characters");
        }
        let body = body.map(str::trim).filter(|s| !s.is_empty());
        if body.is_some_and(|b| b.chars().count() > BODY_CHARS) {
            bail!("a draft runs past {BODY_CHARS} characters");
        }
        if let Some(t) = thread_id {
            let scope: Option<String> = self
                .lock()
                .query_row("SELECT scope FROM mail_threads WHERE id = ?1", [t], |r| {
                    r.get(0)
                })
                .optional()?;
            match scope.as_deref() {
                None => bail!("no thread {t}"),
                Some("private") => {
                    bail!("that thread is private: it never goes to a model, so it gets no draft")
                }
                _ => {}
            }
        }
        let id = format!("md_{}", uuid::Uuid::new_v4().simple());
        let now = now_utc();
        let conn = self.lock();
        conn.execute(
            &format!(
                "INSERT INTO mail_drafts ({COLS}) VALUES
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, '[]', 'requested', NULL, NULL, NULL, NULL, '[]', NULL, ?8, ?8)"
            ),
            rusqlite::params![
                id,
                thread_id,
                reply_to,
                actor.actor,
                say,
                reply_all as i64,
                body,
                now
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "mail_draft_requested",
            &format!("mail_draft_requested:{id}"),
            &json!({"id": id, "thread": thread_id, "reply_to": reply_to}),
        )?;
        super::journal_row(&conn, actor, "mail_draft", "mail_drafts", &id, "create")?;
        conn.query_row(
            &format!("SELECT {COLS} FROM mail_drafts WHERE id = ?1"),
            [&id],
            row_to_draft,
        )
        .map_err(Into::into)
    }

    pub fn mail_draft(&self, id: &str) -> Result<Option<MailDraft>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {COLS} FROM mail_drafts WHERE id = ?1 AND {NOT_PRIVATE}"),
            [id],
            row_to_draft,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Drafts, newest first: of one thread, or asked for by one writer, or all.
    pub fn mail_drafts(
        &self,
        thread_id: Option<&str>,
        requested_by: Option<&str>,
        limit: usize,
    ) -> Result<Vec<MailDraft>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM mail_drafts
             WHERE (?1 IS NULL OR thread_id = ?1) AND (?2 IS NULL OR requested_by = ?2) AND {NOT_PRIVATE}
             ORDER BY created_at DESC LIMIT ?3"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![thread_id, requested_by, limit.clamp(1, 500) as i64],
            row_to_draft,
        )?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Requests waiting for the drafter, oldest first.
    pub fn mail_drafts_waiting(&self, limit: usize) -> Result<Vec<MailDraft>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM mail_drafts WHERE state = 'requested' ORDER BY created_at LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit as i64], row_to_draft)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Drafts a node took and hasn't finished (a restart cut them off).
    pub fn mail_drafts_writing(&self, node: &str, before: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id FROM mail_drafts WHERE state = 'writing' AND node = ?1 AND updated_at < ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![node, before], |r| r.get(0))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// How many drafts this writer asked for since `since` (RFC 3339).
    pub fn mail_drafts_since(&self, requested_by: &str, since: &str) -> Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM mail_drafts WHERE requested_by = ?1 AND created_at >= ?2",
            rusqlite::params![requested_by, since],
            |r| r.get(0),
        )?)
    }

    /// The drafter takes a request: `requested` → `writing` (false when it
    /// was taken or withdrawn meanwhile).
    pub fn mail_draft_take(&self, actor: &Actor, id: &str, node: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "UPDATE mail_drafts SET state = 'writing', node = ?2, updated_at = ?3
             WHERE id = ?1 AND state = 'requested'",
            rusqlite::params![id, node, now_utc()],
        )?;
        if n > 0 {
            super::journal_row(&conn, actor, "mail_draft", "mail_drafts", id, "update")?;
        }
        Ok(n > 0)
    }

    /// The drafter's outcome: `saved` when it reached the Drafts folder,
    /// `failed` with why.
    pub fn mail_draft_finish(&self, actor: &Actor, id: &str, out: &DraftOutcome) -> Result<()> {
        let state = if out.error.is_some() {
            "failed"
        } else {
            "saved"
        };
        if !DRAFT_STATES.contains(&state) {
            bail!("unknown draft state");
        }
        let conn = self.lock();
        conn.execute(
            "UPDATE mail_drafts SET state = ?2, reply_to = COALESCE(?3, reply_to),
                 body = COALESCE(?4, body), placeholders = ?5, folder = ?6,
                 draft_message_id = ?7, recipients = ?8, subject = ?9, error = ?10,
                 updated_at = ?11
             WHERE id = ?1",
            rusqlite::params![
                id,
                state,
                out.reply_to,
                out.body,
                serde_json::to_string(&out.placeholders)?,
                out.folder,
                out.draft_message_id,
                serde_json::to_string(&out.recipients)?,
                out.subject,
                out.error,
                now_utc()
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "mail_draft_finished",
            &format!("mail_draft_finished:{id}"),
            &json!({"id": id, "state": state}),
        )?;
        super::journal_row(&conn, actor, "mail_draft", "mail_drafts", id, "update")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mail::MailSeen;

    #[test]
    fn test_a_draft_goes_requested_writing_saved_and_private_threads_get_none() {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(&tmp.path().join("t.db")).unwrap();
        let a = Actor::new("human:owner");
        let seen = |id: &str, private: bool| MailSeen {
            account: "me@x.example".into(),
            message_id: id.into(),
            from: "Dana <dana@fund.example>".into(),
            from_addr: "dana@fund.example".into(),
            subject: "Seed round".into(),
            primary: true,
            private,
            ..Default::default()
        };
        let open = s.mail_observe(&a, &seen("m1@x", false)).unwrap().unwrap();
        let hidden = s.mail_observe(&a, &seen("p1@x", true)).unwrap().unwrap();
        assert!(s
            .mail_draft_request(&a, Some(&hidden), None, None, false, None)
            .is_err());
        assert!(s
            .mail_draft_request(&a, Some("mt_nope"), None, None, false, None)
            .is_err());
        let d = s
            .mail_draft_request(&a, Some(&open), None, Some("yes, Tuesday"), false, None)
            .unwrap();
        assert_eq!(d.state, "requested");
        assert_eq!(s.mail_drafts_waiting(10).unwrap().len(), 1);
        assert!(s.mail_draft_take(&a, &d.id, "mini").unwrap());
        assert!(
            !s.mail_draft_take(&a, &d.id, "laptop").unwrap(),
            "taken once"
        );
        assert!(s.mail_drafts_waiting(10).unwrap().is_empty());
        s.mail_draft_finish(
            &a,
            &d.id,
            &DraftOutcome {
                reply_to: Some("m1@x".into()),
                body: Some("Tuesday works.".into()),
                folder: Some("[Gmail]/Drafts".into()),
                recipients: vec!["Dana <dana@fund.example>".into()],
                ..Default::default()
            },
        )
        .unwrap();
        let got = s.mail_draft(&d.id).unwrap().unwrap();
        assert_eq!(
            (got.state.as_str(), got.node.as_deref()),
            ("saved", Some("mini"))
        );
        assert_eq!(s.mail_drafts(Some(&open), None, 10).unwrap().len(), 1);
        assert_eq!(
            s.mail_drafts_since("human:owner", "2000-01-01T00:00:00Z")
                .unwrap(),
            1
        );
    }
}
