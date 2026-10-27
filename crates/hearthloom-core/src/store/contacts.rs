//! The device address-book store (v51) — the identity backbone. One row
//! per contact key (CNContact identifier) holding the LATEST card; the
//! apply step resolves each card into a Person entity attribute-first
//! (an email/phone owns the Person — the same resolution every adapter
//! already rides), so the whole Person directory gets seeded and every
//! other source's entity resolution improves.
//!
//! A card CHANGE re-mints the contact episode under the next revision
//! ref (`<key>#r<n>` — THE SUPERSESSION PATTERN), so the entity picks up
//! new attributes while the record stays append-only. Peer-synced via
//! /v1/sync/contacts, LWW by updated_at (mutable directory rows — the
//! grants-leg precedent, not the telemetry one).

use anyhow::Result;
use serde_json::{json, Value};

use super::Store;
use crate::ids::content_hash;
use crate::time::now_utc;

/// One incoming card (the HTTP layer deserializes into this).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ContactCardIn {
    pub key: String,
    pub name: String,
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub phones: Vec<String>,
    #[serde(default)]
    pub org: Option<String>,
    #[serde(default)]
    pub birthday: Option<String>,
    #[serde(default)]
    pub postal: Vec<String>,
}

impl ContactCardIn {
    /// Order-independent fingerprint of the card's content.
    pub fn fingerprint(&self) -> String {
        let mut emails = self.emails.clone();
        let mut phones = self.phones.clone();
        let mut postal = self.postal.clone();
        emails.sort();
        phones.sort();
        postal.sort();
        content_hash(
            format!(
                "{}\x1f{}\x1f{}\x1f{}\x1f{}\x1f{}",
                self.name,
                emails.join(","),
                phones.join(","),
                self.org.as_deref().unwrap_or(""),
                self.birthday.as_deref().unwrap_or(""),
                postal.join(","),
            )
            .as_bytes(),
        )
    }
}

/// A stored card awaiting entity/episode apply.
#[derive(Debug, Clone)]
pub struct PendingContact {
    pub key: String,
    pub name: String,
    pub emails: Vec<String>,
    pub phones: Vec<String>,
    pub org: Option<String>,
    pub birthday: Option<String>,
    pub postal: Vec<String>,
    pub revision: i64,
}

impl Store {
    /// Upsert cards — new keys insert, changed cards bump revision and
    /// reset `applied`, unchanged cards are no-ops. Returns
    /// (new, updated, unchanged).
    pub fn contacts_upsert(
        &self,
        device: &str,
        cards: &[ContactCardIn],
    ) -> Result<(usize, usize, usize)> {
        let (mut new, mut updated, mut unchanged) = (0usize, 0usize, 0usize);
        for chunk in cards.chunks(500) {
            {
                let mut conn = self.lock();
                let tx = conn.transaction()?;
                for c in chunk {
                    if c.name.trim().is_empty() {
                        continue;
                    }
                    let hash = c.fingerprint();
                    let existing: Option<(String, i64)> = tx
                        .query_row(
                            "SELECT content_hash, revision FROM contact_card WHERE key = ?1",
                            [&c.key],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .map(Some)
                        .or_else(|e| match e {
                            rusqlite::Error::QueryReturnedNoRows => Ok(None),
                            e => Err(e),
                        })?;
                    let now = now_utc();
                    match existing {
                        None => {
                            tx.execute(
                                "INSERT INTO contact_card (key, name, emails, phones, org,
                                     birthday, postal, content_hash, revision, applied,
                                     device, updated_at, ingest_time)
                                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,1,0,?9,?10,?10)",
                                rusqlite::params![
                                    c.key,
                                    c.name.trim(),
                                    serde_json::to_string(&c.emails)?,
                                    serde_json::to_string(&c.phones)?,
                                    c.org,
                                    c.birthday,
                                    serde_json::to_string(&c.postal)?,
                                    hash,
                                    device,
                                    now
                                ],
                            )?;
                            new += 1;
                        }
                        Some((old_hash, _)) if old_hash == hash => unchanged += 1,
                        Some((_, rev)) => {
                            tx.execute(
                                "UPDATE contact_card SET name=?2, emails=?3, phones=?4,
                                     org=?5, birthday=?6, postal=?7, content_hash=?8,
                                     revision=?9, applied=0, device=?10, updated_at=?11
                                 WHERE key=?1",
                                rusqlite::params![
                                    c.key,
                                    c.name.trim(),
                                    serde_json::to_string(&c.emails)?,
                                    serde_json::to_string(&c.phones)?,
                                    c.org,
                                    c.birthday,
                                    serde_json::to_string(&c.postal)?,
                                    hash,
                                    rev + 1,
                                    device,
                                    now
                                ],
                            )?;
                            updated += 1;
                        }
                    }
                }
                tx.commit()?;
            }
            if cards.len() > 500 {
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
        }
        Ok((new, updated, unchanged))
    }

    /// Cards whose latest revision hasn't minted its episode yet.
    pub fn contacts_pending_apply(&self, limit: usize) -> Result<Vec<PendingContact>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT key, name, emails, phones, org, birthday, postal, revision
             FROM contact_card WHERE applied = 0 ORDER BY updated_at LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(PendingContact {
                key: row.get(0)?,
                name: row.get(1)?,
                emails: serde_json::from_str(&row.get::<_, String>(2)?).unwrap_or_default(),
                phones: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
                org: row.get(4)?,
                birthday: row.get(5)?,
                postal: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
                revision: row.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    pub fn contacts_mark_applied(&self, key: &str, revision: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE contact_card SET applied = 1 WHERE key = ?1 AND revision = ?2",
            rusqlite::params![key, revision],
        )?;
        Ok(())
    }

    /// (count, last_ingest_at) for the ingest-status surface.
    pub fn contacts_stats(&self) -> Result<(i64, Option<String>)> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COUNT(*), MAX(ingest_time) FROM contact_card",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(Into::into)
    }

    /// Peer-sync export: cards past an updated_at watermark.
    pub fn contacts_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT key, name, emails, phones, org, birthday, postal, content_hash,
                    revision, applied, device, updated_at
             FROM contact_card WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2",
        )?;
        let items: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(json!({
                    "key": row.get::<_, String>(0)?, "name": row.get::<_, String>(1)?,
                    "emails": row.get::<_, String>(2)?, "phones": row.get::<_, String>(3)?,
                    "org": row.get::<_, Option<String>>(4)?,
                    "birthday": row.get::<_, Option<String>>(5)?,
                    "postal": row.get::<_, String>(6)?,
                    "content_hash": row.get::<_, String>(7)?,
                    "revision": row.get::<_, i64>(8)?,
                    "applied": row.get::<_, i64>(9)?,
                    "device": row.get::<_, String>(10)?,
                    "updated_at": row.get::<_, String>(11)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(
            &conn,
            "synced_at",
            "FROM contact_card WHERE synced_at > ?1",
            since,
            limit,
        )?;
        Ok(json!({ "items": items, "next": next }))
    }

    /// Apply a peer's card page — LWW by updated_at (a newer local edit
    /// survives), keeping the HIGHER revision so supersession refs never
    /// go backwards. The receiver keeps its own `applied` progress when
    /// content matches; a genuinely newer card re-opens apply.
    pub fn contacts_apply(&self, batch: &Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        for r in batch
            .get("items")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(key), Some(name), Some(updated_at), Some(hash)) = (
                r.get("key").and_then(|v| v.as_str()),
                r.get("name").and_then(|v| v.as_str()),
                r.get("updated_at").and_then(|v| v.as_str()),
                r.get("content_hash").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let local: Option<(String, String, i64)> = tx
                .query_row(
                    "SELECT updated_at, content_hash, revision FROM contact_card WHERE key = ?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
            let rev = r.get("revision").and_then(|v| v.as_i64()).unwrap_or(1);
            match local {
                Some((local_at, local_hash, local_rev)) => {
                    if local_hash == hash || local_at.as_str() >= updated_at {
                        continue; // same content or newer local edit wins
                    }
                    applied += tx.execute(
                        "UPDATE contact_card SET name=?2, emails=?3, phones=?4, org=?5,
                             birthday=?6, postal=?7, content_hash=?8, revision=?9,
                             applied=0, device=?10, updated_at=?11 WHERE key=?1",
                        rusqlite::params![
                            key,
                            name,
                            r.get("emails").and_then(|v| v.as_str()).unwrap_or("[]"),
                            r.get("phones").and_then(|v| v.as_str()).unwrap_or("[]"),
                            r.get("org").and_then(|v| v.as_str()),
                            r.get("birthday").and_then(|v| v.as_str()),
                            r.get("postal").and_then(|v| v.as_str()).unwrap_or("[]"),
                            hash,
                            rev.max(local_rev + 1),
                            r.get("device").and_then(|v| v.as_str()).unwrap_or("peer"),
                            updated_at
                        ],
                    )?;
                }
                None => {
                    applied += tx.execute(
                        "INSERT INTO contact_card (key, name, emails, phones, org, birthday,
                             postal, content_hash, revision, applied, device, updated_at,
                             ingest_time)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,0,?10,?11,?12)",
                        rusqlite::params![
                            key,
                            name,
                            r.get("emails").and_then(|v| v.as_str()).unwrap_or("[]"),
                            r.get("phones").and_then(|v| v.as_str()).unwrap_or("[]"),
                            r.get("org").and_then(|v| v.as_str()),
                            r.get("birthday").and_then(|v| v.as_str()),
                            r.get("postal").and_then(|v| v.as_str()).unwrap_or("[]"),
                            hash,
                            rev,
                            r.get("device").and_then(|v| v.as_str()).unwrap_or("peer"),
                            updated_at,
                            now_utc()
                        ],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(key: &str, name: &str, email: &str) -> ContactCardIn {
        ContactCardIn {
            key: key.into(),
            name: name.into(),
            emails: vec![email.into()],
            phones: vec![],
            org: None,
            birthday: None,
            postal: vec![],
        }
    }

    #[test]
    fn test_upsert_change_detection_and_revisions() {
        let store = Store::open_in_memory().unwrap();
        let c = card("c1", "Ada Lovelace", "ada@example.com");
        assert_eq!(
            store
                .contacts_upsert("iPhone", std::slice::from_ref(&c))
                .unwrap(),
            (1, 0, 0)
        );
        // identical re-POST: unchanged, still applied=0 (not yet minted)
        assert_eq!(
            store
                .contacts_upsert("iPhone", std::slice::from_ref(&c))
                .unwrap(),
            (0, 0, 1)
        );
        let pending = store.contacts_pending_apply(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].revision, 1);
        store.contacts_mark_applied("c1", 1).unwrap();
        assert!(store.contacts_pending_apply(10).unwrap().is_empty());
        // a changed card bumps revision and re-opens apply
        let mut c2 = c.clone();
        c2.org = Some("Analytical Engines Ltd".into());
        assert_eq!(store.contacts_upsert("iPhone", &[c2]).unwrap(), (0, 1, 0));
        let pending = store.contacts_pending_apply(10).unwrap();
        assert_eq!(pending[0].revision, 2);
    }

    #[test]
    fn test_peer_lww_roundtrip() {
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        a.contacts_upsert("iPhone", &[card("c1", "Ada", "ada@example.com")])
            .unwrap();
        let page = a.contacts_since("", 100).unwrap();
        assert_eq!(b.contacts_apply(&page).unwrap(), 1);
        assert_eq!(
            b.contacts_apply(&page).unwrap(),
            0,
            "same content is a no-op"
        );
        // the receiver's copy is pending apply (it must mint its own episode)
        assert_eq!(b.contacts_pending_apply(10).unwrap().len(), 1);
    }
}
