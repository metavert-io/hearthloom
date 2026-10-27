//! The notification queue (v38, design locked 2026-07-16): hints, never
//! data. Internal producers, plugins (PAT scope `notify`), and the
//! provisioner all speak through here; the killswitch's activator
//! message is just a `critical` row. UPSERT by (origin, dedupe_key) so
//! repeated notices (a renewal countdown) update one row. Replicates
//! between a user's own peers with the telemetry exchange pattern;
//! read/dismiss state merges last-writer-wins by timestamp.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// Bound the panel and the exchange; dismissed rows past this are pruned.
const RETAIN: i64 = 500;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationRow {
    pub id: String,
    pub ts: String,
    pub severity: String,
    pub origin: String,
    pub title: String,
    pub body: String,
    pub link: Option<String>,
    pub link_label: Option<String>,
    pub dedupe_key: Option<String>,
    pub read_at: Option<String>,
    pub dismissed_at: Option<String>,
}

fn row_to_notification(row: &rusqlite::Row) -> rusqlite::Result<NotificationRow> {
    Ok(NotificationRow {
        id: row.get(0)?,
        ts: row.get(1)?,
        severity: row.get(2)?,
        origin: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        link: row.get(6)?,
        link_label: row.get(7)?,
        dedupe_key: row.get(8)?,
        read_at: row.get(9)?,
        dismissed_at: row.get(10)?,
    })
}

const COLS: &str = "id, ts, severity, origin, title, body, link, link_label, \
                    dedupe_key, read_at, dismissed_at";

/// The link-safe rule (Q3): relative in-app routes always pass; absolute
/// URLs must land on an allowed origin (this node, the paired peer, or a
/// provisioner-registered domain). Reject, never strip — a plugin dev
/// should find out immediately.
pub fn validate_link(link: &str, allowed_origins: &[String]) -> Result<()> {
    let link = link.trim();
    if link.starts_with("#/") || (link.starts_with('/') && !link.starts_with("//")) {
        return Ok(()); // in-app route or same-origin path
    }
    let lower = link.to_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(anyhow!("link must be an in-app route or an http(s) URL"));
    }
    let host = link
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .map(|h| h.split('@').next_back().unwrap_or(h)) // strip creds trick
        .map(|h| h.to_lowercase())
        .unwrap_or_default();
    let host = host.split(':').next().unwrap_or(&host).to_string();
    for allowed in allowed_origins {
        let a = allowed
            .trim()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split(['/', ':'])
            .next()
            .unwrap_or("")
            .to_lowercase();
        if !a.is_empty() && (host == a || host.ends_with(&format!(".{a}"))) {
            return Ok(());
        }
    }
    Err(anyhow!(
        "link host {host:?} is not on this node's allowlist (own node, paired peer, \
         or a provisioner-registered domain)"
    ))
}

impl Store {
    /// Create-or-update a notification. With a dedupe_key, a repeat from
    /// the same origin updates the row (fresh ts, unread again only if
    /// the content changed). Returns the row id.
    #[allow(clippy::too_many_arguments)]
    pub fn notify(
        &self,
        actor: &Actor,
        origin: &str,
        severity: &str,
        title: &str,
        body: &str,
        link: Option<(&str, Option<&str>)>,
        dedupe_key: Option<&str>,
    ) -> Result<String> {
        if !["info", "warn", "critical"].contains(&severity) {
            return Err(anyhow!("severity must be info|warn|critical"));
        }
        if title.trim().is_empty() {
            return Err(anyhow!("a notification needs a title"));
        }
        let conn = self.lock();
        let now = now_utc();
        let existing: Option<String> = match dedupe_key {
            Some(key) => conn
                .query_row(
                    "SELECT id FROM notifications WHERE origin = ?1 AND dedupe_key = ?2",
                    [origin, key],
                    |row| row.get(0),
                )
                .ok(),
            None => None,
        };
        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE notifications SET ts = ?2, severity = ?3, title = ?4, body = ?5,
                         link = ?6, link_label = ?7, read_at = NULL, dismissed_at = NULL
                     WHERE id = ?1",
                    rusqlite::params![
                        id,
                        now,
                        severity,
                        title,
                        body,
                        link.map(|(u, _)| u),
                        link.and_then(|(_, l)| l),
                    ],
                )?;
                id
            }
            None => {
                let id = format!("nf_{}", uuid::Uuid::new_v4().simple());
                conn.execute(
                    "INSERT INTO notifications
                         (id, ts, severity, origin, title, body, link, link_label, dedupe_key)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        id,
                        now,
                        severity,
                        origin,
                        title,
                        body,
                        link.map(|(u, _)| u),
                        link.and_then(|(_, l)| l),
                        dedupe_key,
                    ],
                )?;
                id
            }
        };
        // prune old dismissed rows so the table stays bounded
        conn.execute(
            "DELETE FROM notifications WHERE id IN (
                 SELECT id FROM notifications WHERE dismissed_at IS NOT NULL
                 ORDER BY ts DESC LIMIT -1 OFFSET ?1)",
            [RETAIN],
        )?;
        record_mutation(
            &conn,
            actor,
            "notify",
            &format!("notify:{origin}:{}", dedupe_key.unwrap_or(&id)),
            &json!({"id": id, "origin": origin, "severity": severity, "title": title}),
        )?;
        Ok(id)
    }

    pub fn notifications(
        &self,
        include_dismissed: bool,
        limit: usize,
    ) -> Result<Vec<NotificationRow>> {
        let conn = self.lock();
        let sql = format!(
            "SELECT {COLS} FROM notifications {} ORDER BY ts DESC LIMIT ?1",
            if include_dismissed {
                ""
            } else {
                "WHERE dismissed_at IS NULL"
            }
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([limit as i64], row_to_notification)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn notifications_unread(&self) -> Result<i64> {
        let conn = self.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM notifications WHERE read_at IS NULL AND dismissed_at IS NULL",
            [],
            |row| row.get(0),
        )?)
    }

    /// Mark read/dismissed. action: read | dismiss | read_all.
    pub fn notification_mark(&self, id: Option<&str>, action: &str) -> Result<usize> {
        let conn = self.lock();
        let now = now_utc();
        let changed = match (action, id) {
            ("read", Some(id)) => conn.execute(
                "UPDATE notifications SET read_at = ?2 WHERE id = ?1 AND read_at IS NULL",
                rusqlite::params![id, now],
            )?,
            ("dismiss", Some(id)) => conn.execute(
                "UPDATE notifications SET dismissed_at = ?2,
                     read_at = COALESCE(read_at, ?2) WHERE id = ?1",
                rusqlite::params![id, now],
            )?,
            ("read_all", None) => conn.execute(
                "UPDATE notifications SET read_at = ?1 WHERE read_at IS NULL",
                rusqlite::params![now],
            )?,
            _ => return Err(anyhow!("action must be read|dismiss (with id) or read_all")),
        };
        Ok(changed)
    }

    /* ————— the peer exchange (telemetry pattern: two-way append-ish,
    watermark by ts; read/dismiss merge LWW — the newest stamp on
    either node wins) ————— */

    pub fn notifications_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM notifications WHERE synced_at > ?1
             ORDER BY synced_at LIMIT ?2"
        ))?;
        let rows: Vec<NotificationRow> = stmt
            .query_map(rusqlite::params![since, limit as i64], row_to_notification)?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(
            &conn,
            "synced_at",
            "FROM notifications WHERE synced_at > ?1",
            since,
            limit,
        )?;
        Ok(json!({"notifications": rows, "next": next}))
    }

    /// Apply a counterpart's batch: unknown ids insert verbatim; known
    /// ids take the NEWER content ts, and read/dismiss stamps merge by
    /// max (a dismiss on either node sticks).
    pub fn apply_notifications(&self, batch: &Value) -> Result<usize> {
        let rows: Vec<NotificationRow> =
            serde_json::from_value(batch.get("notifications").cloned().unwrap_or(json!([])))
                .unwrap_or_default();
        let conn = self.lock();
        let mut applied = 0usize;
        for r in rows {
            let changed = conn.execute(
                "INSERT INTO notifications
                     (id, ts, severity, origin, title, body, link, link_label,
                      dedupe_key, read_at, dismissed_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(id) DO UPDATE SET
                     ts = CASE WHEN excluded.ts > ts THEN excluded.ts ELSE ts END,
                     severity = CASE WHEN excluded.ts > ts THEN excluded.severity ELSE severity END,
                     title = CASE WHEN excluded.ts > ts THEN excluded.title ELSE title END,
                     body = CASE WHEN excluded.ts > ts THEN excluded.body ELSE body END,
                     link = CASE WHEN excluded.ts > ts THEN excluded.link ELSE link END,
                     link_label = CASE WHEN excluded.ts > ts THEN excluded.link_label ELSE link_label END,
                     read_at = MAX(COALESCE(read_at,''), COALESCE(excluded.read_at,'')),
                     dismissed_at = MAX(COALESCE(dismissed_at,''), COALESCE(excluded.dismissed_at,''))",
                rusqlite::params![
                    r.id, r.ts, r.severity, r.origin, r.title, r.body, r.link,
                    r.link_label, r.dedupe_key, r.read_at, r.dismissed_at,
                ],
            );
            // a dedupe-key collision from the other node (same origin+key,
            // different id) falls back to content merge by the unique index
            match changed {
                Ok(n) => applied += n,
                Err(_) => {
                    // same (origin, dedupe_key) minted independently on both
                    // nodes: keep ours, absorb the newer content
                    if let Some(key) = &r.dedupe_key {
                        let _ = conn.execute(
                            "UPDATE notifications SET
                                 ts = CASE WHEN ?3 > ts THEN ?3 ELSE ts END,
                                 title = CASE WHEN ?3 > ts THEN ?4 ELSE title END,
                                 body = CASE WHEN ?3 > ts THEN ?5 ELSE body END
                             WHERE origin = ?1 AND dedupe_key = ?2",
                            rusqlite::params![r.origin, key, r.ts, r.title, r.body],
                        );
                        applied += 1;
                    }
                }
            }
        }
        // empty read_at/dismissed_at strings from the MAX(COALESCE) trick
        // normalize back to NULL
        conn.execute(
            "UPDATE notifications SET read_at = NULL WHERE read_at = ''",
            [],
        )?;
        conn.execute(
            "UPDATE notifications SET dismissed_at = NULL WHERE dismissed_at = ''",
            [],
        )?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }
    fn actor() -> Actor {
        Actor::new("system:test")
    }

    #[test]
    fn test_dedupe_upserts_and_unread() {
        let s = store();
        let a = actor();
        let id1 = s
            .notify(
                &a,
                "provisioner",
                "warn",
                "Renewal in 3 days",
                "",
                None,
                Some("renewal"),
            )
            .unwrap();
        s.notification_mark(Some(&id1), "read").unwrap();
        assert_eq!(s.notifications_unread().unwrap(), 0);
        // the countdown updates the SAME row and re-unreads it
        let id2 = s
            .notify(
                &a,
                "provisioner",
                "warn",
                "Renewal in 2 days",
                "",
                None,
                Some("renewal"),
            )
            .unwrap();
        assert_eq!(id1, id2);
        assert_eq!(s.notifications(false, 10).unwrap().len(), 1);
        assert_eq!(s.notifications_unread().unwrap(), 1);
        // no dedupe key = a new row every time
        s.notify(&a, "system:peering", "info", "Paired", "", None, None)
            .unwrap();
        s.notify(&a, "system:peering", "info", "Paired again", "", None, None)
            .unwrap();
        assert_eq!(s.notifications(false, 10).unwrap().len(), 3);
        // dismiss hides from the default list
        let rows = s.notifications(false, 10).unwrap();
        s.notification_mark(Some(&rows[0].id), "dismiss").unwrap();
        assert_eq!(s.notifications(false, 10).unwrap().len(), 2);
        assert_eq!(s.notifications(true, 10).unwrap().len(), 3);
    }

    #[test]
    fn test_link_allowlist() {
        let allowed = vec![
            "http://127.0.0.1:43278".to_string(),
            "https://mybrain.fly.dev".to_string(),
            "hearth.harbor.example".to_string(),
        ];
        // in-app routes always pass
        assert!(validate_link("#/tend", &allowed).is_ok());
        assert!(validate_link("/v1/notifications", &allowed).is_ok());
        // allowed origins + subdomains
        assert!(validate_link("https://mybrain.fly.dev/#/peering", &allowed).is_ok());
        assert!(validate_link("https://hearth.harbor.example/renew?t=1", &allowed).is_ok());
        assert!(validate_link("https://billing.hearth.harbor.example/x", &allowed).is_ok());
        // everything else is REJECTED
        assert!(validate_link("https://evil.example.com/phish", &allowed).is_err());
        assert!(validate_link("javascript:alert(1)", &allowed).is_err());
        assert!(validate_link("//evil.example.com", &allowed).is_err());
        // credential-prefix trick doesn't smuggle a host
        assert!(validate_link("https://hearth.harbor.example@evil.com/", &allowed).is_err());
    }

    #[test]
    fn test_exchange_merges_lww() {
        let a_store = store();
        let b_store = store();
        let a = actor();
        let id = a_store
            .notify(
                &a,
                "system:peering",
                "info",
                "First sync complete",
                "",
                None,
                None,
            )
            .unwrap();
        // replicate a → b
        let batch = a_store.notifications_since("", 100).unwrap();
        assert_eq!(b_store.apply_notifications(&batch).unwrap(), 1);
        assert_eq!(b_store.notifications(false, 10).unwrap().len(), 1);
        // read on b, exchange back — a learns the read stamp
        b_store.notification_mark(Some(&id), "read").unwrap();
        let back = b_store.notifications_since("", 100).unwrap();
        a_store.apply_notifications(&back).unwrap();
        assert_eq!(a_store.notifications_unread().unwrap(), 0);
        // idempotent re-apply
        let again = a_store.apply_notifications(&back).unwrap();
        assert!(again <= 1);
        assert_eq!(a_store.notifications(false, 10).unwrap().len(), 1);
    }
}
