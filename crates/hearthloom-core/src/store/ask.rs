//! Ask-log: full traces of every deep query — the Ask panel's chat
//! history and the raw material for the QA/improvement loop. Failures
//! are as valuable as successes, so nothing is deleted automatically;
//! the ONE exception is `ask_delete`, the user's explicit, modal-
//! confirmed, permanent removal of a history item.

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use super::{Actor, Store};
use crate::time::now_utc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRecord {
    pub id: String,
    pub ts: String,
    pub actor: String,
    pub question: String,
    pub status: String,
    pub answer: Option<String>,
    pub error: Option<String>,
    pub citations: JsonValue,
    pub transcript: JsonValue,
    pub rounds: i64,
    pub duration_ms: Option<i64>,
    /// Root ask id of this record's conversation (None = standalone or
    /// itself the root).
    pub thread: Option<String>,
}

/// One conversation in the history sidebar: its FIRST turn (what the
/// user opened with), how many turns it has, and when it last moved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskConversation {
    #[serde(flatten)]
    pub first: AskRecord,
    pub turns: i64,
    pub last_ts: String,
    /// The most recent turn (for "did the last question fail?" and the
    /// debug-trace shortcut).
    pub last_id: String,
    pub last_status: String,
    /// Sealed (hidden while the vault is locked).
    pub private: bool,
    /// Sealed by hand only — no turn ran with the vault open, so it can
    /// be made public again.
    pub can_unseal: bool,
}

/// The locked-vault filter for ask_log. A conversation is private AS A
/// WHOLE: one turn asked with the vault open hides every turn of it —
/// the earlier ones are its context and the later ones can quote it.
/// (`ask_start` also stamps the flag across the thread; this form keeps
/// conversations recorded before that rule equally sealed.)
///
/// `sensitive` carries WHY: 1 = a turn ran with the vault open (its
/// answer may quote private episodes — permanent), 2 = sealed by hand
/// (`ask_set_private`; reversible while no turn is a 1). Every guard
/// tests `!= 0`, never `= 1`.
const LOCKED_GUARD: &str = " AND COALESCE(thread, id) NOT IN \
     (SELECT COALESCE(thread, id) FROM ask_log WHERE sensitive != 0)";

impl Store {
    fn ask_guard(&self) -> &'static str {
        if self.private_visible() {
            ""
        } else {
            LOCKED_GUARD
        }
    }

    /// Was this ask run while the vault was open (its answer may quote
    /// private episodes)? The keep-this-answer flow scopes accordingly.
    /// Unguarded on purpose — callers gate access via ask_get first.
    pub fn ask_sensitive(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let flag: Option<i64> = conn
            .query_row("SELECT sensitive FROM ask_log WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(flag.unwrap_or(0) != 0)
    }

    /// Open a trace when a question arrives. `thread` is the root ask id
    /// when this is a follow-up in a conversation. Returns the ask id.
    pub fn ask_start(&self, actor: &Actor, question: &str, thread: Option<&str>) -> Result<String> {
        let id = format!("ask_{}", uuid::Uuid::new_v4().simple());
        // If the vault is unlocked as the ask begins, its retrieval may
        // surface private episodes — flag it so it hides again while locked
        // (unlock windows dwarf query duration, so start-state ≈ run-state).
        let open = self.private_visible();
        let conn = self.lock();
        // A conversation is private as a whole. Joining one that is
        // already sensitive makes this turn sensitive too; and a turn
        // asked with the vault open seals every EARLIER turn of its
        // conversation — they are its context, and the history must not
        // show half a private chat once the vault relocks.
        // How the conversation is already sealed, if it is: 1 (a
        // vault-open turn) outranks 2 (by hand).
        let existing: i64 = match thread {
            Some(root) => conn.query_row(
                "SELECT COALESCE(MIN(NULLIF(sensitive, 0)), 0) FROM ask_log
                 WHERE id = ?1 OR thread = ?1",
                [root],
                |row| row.get(0),
            )?,
            None => 0,
        };
        let sensitive = if open { 1 } else { existing };
        conn.execute(
            "INSERT INTO ask_log (id, ts, actor, question, sensitive, thread)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, now_utc(), actor.actor, question, sensitive, thread],
        )?;
        if let (true, Some(root)) = (open, thread) {
            // A vault-open turn makes the whole conversation permanently
            // private — including one that was only hand-sealed before.
            conn.execute(
                "UPDATE ask_log SET sensitive = 1
                 WHERE (id = ?1 OR thread = ?1) AND sensitive != 1",
                [root],
            )?;
        }
        Ok(id)
    }

    /// Close a trace with either an answer or an error.
    #[allow(clippy::too_many_arguments)]
    pub fn ask_finish(
        &self,
        id: &str,
        answer: Option<&str>,
        error: Option<&str>,
        citations: &JsonValue,
        transcript: &JsonValue,
        rounds: usize,
        duration_ms: i64,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE ask_log SET status = ?2, answer = ?3, error = ?4, citations = ?5,
                 transcript = ?6, rounds = ?7, duration_ms = ?8
             WHERE id = ?1",
            rusqlite::params![
                id,
                if error.is_some() { "error" } else { "ok" },
                answer,
                error,
                citations.to_string(),
                transcript.to_string(),
                rounds as i64,
                duration_ms,
            ],
        )?;
        Ok(())
    }

    /// History page for the sidebar: ONE entry per conversation, most
    /// recently active first. A follow-up is part of its conversation,
    /// never its own entry. `search` matches any turn's question or
    /// answer. Vault-guarded: while locked, a conversation with any
    /// sensitive turn does not appear at all (no "N hidden").
    pub fn ask_history(
        &self,
        offset: usize,
        limit: usize,
        search: Option<&str>,
    ) -> Result<Vec<AskConversation>> {
        let guard = self.ask_guard();
        let conn = self.lock();
        let pattern = search.map(|s| format!("%{}%", s.replace('%', "")));
        // `first`/`last` pick by (ts, rowid) so a conversation whose root
        // turn was deleted still lists under its earliest surviving turn.
        let mut stmt = conn.prepare(&format!(
            "WITH visible AS (
                 SELECT rowid AS rid, COALESCE(thread, id) AS root, * FROM ask_log
                 WHERE 1=1{guard}
             ),
             convo AS (
                 SELECT root, COUNT(*) AS turns, MAX(ts) AS last_ts,
                        MAX(sensitive != 0) AS private, MAX(sensitive = 1) AS hard
                 FROM visible GROUP BY root
                 HAVING ?3 IS NULL
                     OR SUM(question LIKE ?3 OR COALESCE(answer, '') LIKE ?3) > 0
             )
             SELECT f.id, f.ts, f.actor, f.question, f.status, f.answer, f.error,
                    f.citations, f.transcript, f.rounds, f.duration_ms, f.thread,
                    c.turns, c.last_ts, l.id, l.status, c.private, c.hard
             FROM convo c
             JOIN visible f ON f.rid = (SELECT rid FROM visible v WHERE v.root = c.root
                                        ORDER BY v.ts ASC, v.rid ASC LIMIT 1)
             JOIN visible l ON l.rid = (SELECT rid FROM visible v WHERE v.root = c.root
                                        ORDER BY v.ts DESC, v.rid DESC LIMIT 1)
             ORDER BY c.last_ts DESC, l.rid DESC LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![limit as i64, offset as i64, pattern],
            |row| {
                Ok(AskConversation {
                    first: row_to_record(row)?,
                    turns: row.get(12)?,
                    last_ts: row.get(13)?,
                    last_id: row.get(14)?,
                    last_status: row.get(15)?,
                    private: row.get(16)?,
                    can_unseal: row.get::<_, bool>(16)? && !row.get::<_, bool>(17)?,
                })
            },
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// `(private, can_unseal)` for the conversation `id` belongs to, or
    /// None when it isn't readable (gone, or sealed while locked).
    pub fn ask_privacy(&self, id: &str) -> Result<Option<(bool, bool)>> {
        let Some(record) = self.ask_get(id)? else {
            return Ok(None);
        };
        let root = record.thread.unwrap_or(record.id);
        let conn = self.lock();
        let (private, hard): (bool, bool) = conn.query_row(
            "SELECT COALESCE(MAX(sensitive != 0), 0), COALESCE(MAX(sensitive = 1), 0)
             FROM ask_log WHERE id = ?1 OR thread = ?1",
            [&root],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Some((private, private && !hard)))
    }

    /// Seal or unseal a whole conversation BY HAND — no question needed.
    /// Sealing works in either lock state (locked, the conversation then
    /// vanishes at once). Unsealing needs the vault open, and is refused
    /// for a conversation with a vault-open turn: its answers may quote
    /// private material, so it stays private. Returns the new
    /// `(private, can_unseal)`, or None when the conversation isn't
    /// readable. Recorded in the mutations log.
    pub fn ask_set_private(
        &self,
        actor: &Actor,
        id: &str,
        private: bool,
    ) -> Result<Option<(bool, bool)>> {
        let Some((was_private, can_unseal)) = self.ask_privacy(id)? else {
            return Ok(None);
        };
        let record = self.ask_get(id)?.ok_or_else(|| anyhow::anyhow!("ask vanished"))?;
        let root = record.thread.unwrap_or(record.id);
        if private == was_private {
            return Ok(Some((was_private, can_unseal)));
        }
        if !private {
            if !self.private_visible() {
                anyhow::bail!("unlock the vault to make a conversation public again");
            }
            if !can_unseal {
                anyhow::bail!(
                    "this conversation includes a question asked with the vault open — \
                     its answers may quote private material, so it stays private"
                );
            }
        }
        {
            let conn = self.lock();
            if private {
                conn.execute(
                    "UPDATE ask_log SET sensitive = 2
                     WHERE (id = ?1 OR thread = ?1) AND sensitive = 0",
                    [&root],
                )?;
            } else {
                conn.execute(
                    "UPDATE ask_log SET sensitive = 0
                     WHERE (id = ?1 OR thread = ?1) AND sensitive = 2",
                    [&root],
                )?;
            }
        }
        self.record_event(
            actor,
            if private { "ask_sealed" } else { "ask_unsealed" },
            &serde_json::json!({ "id": root }),
        )?;
        Ok(Some((private, private)))
    }

    /// Permanently delete a WHOLE conversation — every turn of the one
    /// `id` belongs to (the history lists conversations, so that is what
    /// its delete removes). Vault-guarded like `ask_delete`: locked, a
    /// sensitive conversation reads as not-found. Returns turns removed.
    pub fn ask_delete_conversation(&self, actor: &Actor, id: &str) -> Result<usize> {
        let Some(record) = self.ask_get(id)? else {
            return Ok(0);
        };
        let root = record.thread.clone().unwrap_or_else(|| record.id.clone());
        let guard = self.ask_guard();
        let deleted = {
            let conn = self.lock();
            conn.execute(
                &format!("DELETE FROM ask_log WHERE (id = ?1 OR thread = ?1){guard}"),
                [&root],
            )?
        };
        if deleted > 0 {
            self.record_event(
                actor,
                "ask_deleted",
                &serde_json::json!({ "id": root, "turns": deleted }),
            )?;
        }
        Ok(deleted)
    }

    /// Permanently delete one ask trace (user-invoked, modal-confirmed).
    /// While the vault is locked a sensitive ask reads as not-found here
    /// too — you cannot delete what the lock hides (zero leakage).
    /// Returns false when nothing matched. Recorded in the mutations log.
    pub fn ask_delete(&self, actor: &Actor, id: &str) -> Result<bool> {
        let guard = self.ask_guard();
        let deleted = {
            let conn = self.lock();
            conn.execute(&format!("DELETE FROM ask_log WHERE id = ?1{guard}"), [id])? > 0
        };
        if deleted {
            self.record_event(actor, "ask_deleted", &serde_json::json!({ "id": id }))?;
        }
        Ok(deleted)
    }

    /// Every turn of the conversation `id` belongs to (itself included),
    /// oldest first. Vault-guarded like ask_get: while locked, sensitive
    /// turns silently vanish from the replay (zero leakage), and an
    /// invisible entry point returns an empty thread.
    pub fn ask_thread(&self, id: &str) -> Result<Vec<AskRecord>> {
        let Some(record) = self.ask_get(id)? else {
            return Ok(Vec::new());
        };
        let root = record.thread.clone().unwrap_or_else(|| record.id.clone());
        let guard = self.ask_guard();
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, ts, actor, question, status, answer, error, citations,
                    transcript, rounds, duration_ms, thread
             FROM ask_log
             WHERE (id = ?1 OR thread = ?1){guard}
             ORDER BY ts ASC, rowid ASC"
        ))?;
        let rows = stmt.query_map([&root], row_to_record)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Record why this turn read content the user didn't write (the
    /// Ask outbound guard). A conversation stays tainted from then on.
    pub fn ask_set_tainted(&self, id: &str, reason: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("UPDATE ask_log SET tainted = ?2 WHERE id = ?1", rusqlite::params![id, reason])?;
        Ok(())
    }

    /// The first recorded taint among these turns, if any. Reads past the
    /// vault guard on purpose: a sealed turn's taint must still count.
    pub fn ask_turns_tainted(&self, ids: &[String]) -> Result<Option<String>> {
        let conn = self.lock();
        for id in ids {
            let reason: Option<String> = conn
                .query_row("SELECT tainted FROM ask_log WHERE id = ?1", [id], |row| row.get(0))
                .optional()?
                .flatten();
            if reason.is_some() {
                return Ok(reason);
            }
        }
        Ok(None)
    }

    pub fn ask_get(&self, id: &str) -> Result<Option<AskRecord>> {
        // Locked, a sensitive ask reads as not-found (its transcript can
        // quote private episodes).
        let guard = self.ask_guard();
        let conn = self.lock();
        conn.query_row(
            &format!(
                "SELECT id, ts, actor, question, status, answer, error, citations,
                        transcript, rounds, duration_ms, thread
                 FROM ask_log WHERE id = ?1{guard}"
            ),
            [id],
            row_to_record,
        )
        .optional()
        .map_err(Into::into)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<AskRecord> {
    let parse = |value: Option<String>| -> JsonValue {
        value
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(JsonValue::Array(vec![]))
    };
    Ok(AskRecord {
        id: row.get(0)?,
        ts: row.get(1)?,
        actor: row.get(2)?,
        question: row.get(3)?,
        status: row.get(4)?,
        answer: row.get(5)?,
        error: row.get(6)?,
        citations: parse(row.get(7)?),
        transcript: parse(row.get(8)?),
        rounds: row.get(9)?,
        duration_ms: row.get(10)?,
        thread: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_ask_trace_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");
        let id = store.ask_start(&actor, "what happened in march?", None).unwrap();

        let running = store.ask_get(&id).unwrap().unwrap();
        assert_eq!(running.status, "running");

        store
            .ask_finish(
                &id,
                Some("things happened"),
                None,
                &json!(["ep_1"]),
                &json!([{"round": 1, "tool": "search"}]),
                3,
                4200,
            )
            .unwrap();
        let done = store.ask_get(&id).unwrap().unwrap();
        assert_eq!(done.status, "ok");
        assert_eq!(done.rounds, 3);
        assert_eq!(done.citations, json!(["ep_1"]));

        // Failures are traces too.
        let id2 = store.ask_start(&actor, "unparseable question", None).unwrap();
        store
            .ask_finish(&id2, None, Some("model returned prose"), &json!([]), &json!([]), 1, 900)
            .unwrap();

        // History: newest first, searchable.
        let all = store.ask_history(0, 10, None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].first.question, "unparseable question");
        let hits = store.ask_history(0, 10, Some("march")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(store.ask_history(1, 1, None).unwrap().len(), 1);
    }

    #[test]
    fn test_ask_thread_replay_and_vault_gating() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        // Root + a failed turn + a retry, chained on the root id.
        let root = store.ask_start(&actor, "rhr trends?", None).unwrap();
        store
            .ask_finish(&root, Some("here are trends"), None, &json!([]), &json!([]), 2, 900)
            .unwrap();
        let failed = store.ask_start(&actor, "what predicted the gaps?", Some(&root)).unwrap();
        store
            .ask_finish(&failed, None, Some("credit balance too low"), &json!([]), &json!([]), 0, 10)
            .unwrap();
        let retry = store.ask_start(&actor, "try again", Some(&root)).unwrap();

        // Thread replays oldest-first from ANY member.
        for entry in [&root, &failed, &retry] {
            let thread = store.ask_thread(entry).unwrap();
            assert_eq!(thread.len(), 3, "from {entry}");
            assert_eq!(thread[0].id, root);
            assert_eq!(thread[1].status, "error");
            assert_eq!(thread[2].question, "try again");
        }

        // The history lists the CONVERSATION once, under its opening
        // question, however many follow-ups it has.
        let standalone = store.ask_start(&actor, "something else", None).unwrap();
        let history = store.ask_history(0, 10, None).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].first.id, standalone);
        assert_eq!(history[1].first.id, root);
        assert_eq!(history[1].first.question, "rhr trends?");
        assert_eq!(history[1].turns, 3);
        assert_eq!(history[1].last_id, retry);
        // Search finds a conversation by ANY of its turns.
        let hits = store.ask_history(0, 10, Some("predicted")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].first.id, root);

        // One turn asked with the vault open seals the WHOLE conversation:
        // while locked no turn of it is reachable, by any route.
        store.vault_unlock(15);
        let secret = store.ask_start(&actor, "private follow-up", Some(&root)).unwrap();
        assert_eq!(store.ask_thread(&root).unwrap().len(), 4);
        assert_eq!(store.ask_history(0, 10, None).unwrap()[0].turns, 4);
        store.vault_lock();
        assert!(store.ask_thread(&root).unwrap().is_empty());
        for id in [&root, &failed, &retry, &secret] {
            assert!(store.ask_get(id).unwrap().is_none(), "{id} hidden while locked");
            assert!(store.ask_sensitive(id).unwrap(), "{id} stamped sensitive");
        }
        let history = store.ask_history(0, 10, None).unwrap();
        assert_eq!(history.len(), 1, "only the unrelated conversation remains");
        assert_eq!(history[0].first.id, standalone);
        assert!(store.ask_history(0, 10, Some("rhr")).unwrap().is_empty());
        // The raw sql handle (what Ask itself reads) is sealed the same way.
        let (_, rows) = store.read_query("SELECT id FROM ask_log", 50).unwrap();
        assert_eq!(rows.len(), 1);
        // Locked, it can't be deleted either; and a turn added to a sealed
        // conversation while locked stays sealed with it.
        assert_eq!(store.ask_delete_conversation(&actor, &root).unwrap(), 0);
        let late = store.ask_start(&actor, "sneaking in", Some(&root)).unwrap();
        assert!(store.ask_get(&late).unwrap().is_none());

        store.vault_unlock(15);
        assert_eq!(store.ask_thread(&root).unwrap().len(), 5);
        assert_eq!(store.ask_history(0, 10, None).unwrap().len(), 2);
        // Deleting a conversation removes every turn of it.
        assert_eq!(store.ask_delete_conversation(&actor, &retry).unwrap(), 5);
        assert!(store.ask_thread(&root).unwrap().is_empty());
        assert_eq!(store.ask_history(0, 10, None).unwrap().len(), 1);
    }

    #[test]
    fn test_seal_by_hand_and_unseal_rules() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");
        let root = store.ask_start(&actor, "what should I get her?", None).unwrap();
        let follow = store.ask_start(&actor, "something smaller", Some(&root)).unwrap();
        assert_eq!(store.ask_privacy(&root).unwrap(), Some((false, false)));

        // Sealed while LOCKED: gone at once, by every route.
        assert_eq!(store.ask_set_private(&actor, &follow, true).unwrap(), Some((true, true)));
        assert!(store.ask_get(&root).unwrap().is_none());
        assert!(store.ask_history(0, 10, None).unwrap().is_empty());
        assert_eq!(store.ask_privacy(&root).unwrap(), None);
        // …and can't be unsealed blind.
        assert_eq!(store.ask_set_private(&actor, &root, false).unwrap(), None);
        // A turn added while locked stays inside the seal.
        let late = store.ask_start(&actor, "one more", Some(&root)).unwrap();
        assert!(store.ask_get(&late).unwrap().is_none());

        // Unlocked: listed as private, hand-sealed ⇒ reversible.
        store.vault_unlock(15);
        let listed = &store.ask_history(0, 10, None).unwrap()[0];
        assert!(listed.private && listed.can_unseal);
        assert_eq!(store.ask_set_private(&actor, &root, false).unwrap(), Some((false, false)));
        store.vault_lock();
        assert_eq!(store.ask_thread(&root).unwrap().len(), 3, "public again");

        // Sealed by hand, then a vault-open question: permanent.
        store.ask_set_private(&actor, &root, true).unwrap();
        store.vault_unlock(15);
        store.ask_start(&actor, "use the private notes", Some(&root)).unwrap();
        assert_eq!(store.ask_privacy(&root).unwrap(), Some((true, false)));
        let err = store.ask_set_private(&actor, &root, false).unwrap_err().to_string();
        assert!(err.contains("stays private"), "{err}");
        let listed = &store.ask_history(0, 10, None).unwrap()[0];
        assert!(listed.private && !listed.can_unseal);
        store.vault_lock();
        assert!(store.ask_history(0, 10, None).unwrap().is_empty());
    }

    /// Conversations recorded BEFORE turns were stamped thread-wide (a
    /// lone sensitive follow-up on a public root) are sealed just the same.
    #[test]
    fn test_legacy_mixed_thread_is_sealed_whole() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");
        let root = store.ask_start(&actor, "public opener", None).unwrap();
        let follow = store.ask_start(&actor, "later private turn", Some(&root)).unwrap();
        {
            let conn = store.lock();
            conn.execute("UPDATE ask_log SET sensitive = 1 WHERE id = ?1", [&follow]).unwrap();
        }
        assert!(store.ask_get(&root).unwrap().is_none());
        assert!(store.ask_history(0, 10, None).unwrap().is_empty());
        let (_, rows) = store.read_query("SELECT id FROM ask_log", 50).unwrap();
        assert!(rows.is_empty());
        store.vault_unlock(15);
        assert_eq!(store.ask_thread(&root).unwrap().len(), 2);
    }

    #[test]
    fn test_ask_delete_permanent_and_vault_gated() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");
        let safe = store.ask_start(&actor, "public", None).unwrap();
        store.vault_unlock(15);
        let sensitive = store.ask_start(&actor, "private", None).unwrap();
        store.vault_lock();

        // Locked: a sensitive ask is invisible to deletion too.
        assert!(!store.ask_delete(&actor, &sensitive).unwrap());
        // A visible one deletes permanently; re-delete is a no-op.
        assert!(store.ask_delete(&actor, &safe).unwrap());
        assert!(store.ask_get(&safe).unwrap().is_none());
        assert!(!store.ask_delete(&actor, &safe).unwrap());
        // Unlocked, the sensitive one can be deleted.
        store.vault_unlock(15);
        assert!(store.ask_delete(&actor, &sensitive).unwrap());
        assert!(store.ask_history(0, 10, None).unwrap().is_empty());
    }

    #[test]
    fn test_vault_gates_sensitive_asks() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        // Locked (default): a normal ask is never sensitive.
        let safe = store.ask_start(&actor, "public question", None).unwrap();

        // An ask begun while unlocked is flagged sensitive.
        store.vault_unlock(15);
        let private = store.ask_start(&actor, "private question", None).unwrap();
        // Unlocked, both are visible.
        assert_eq!(store.ask_history(0, 10, None).unwrap().len(), 2);
        assert!(store.ask_get(&private).unwrap().is_some());

        // Locked, the sensitive ask vanishes from list AND detail; the
        // safe one stays. Zero leakage: no count of what's hidden.
        store.vault_lock();
        let visible = store.ask_history(0, 10, None).unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].first.id, safe);
        assert!(store.ask_get(&private).unwrap().is_none());
        assert!(store.ask_get(&safe).unwrap().is_some());
    }
}
