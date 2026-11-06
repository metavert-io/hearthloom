//! Embers watching the owner's mail (v64, 0.5.93).
//!
//! An ember granted `mail.watch` registers watches — a sender, a domain,
//! words in the subject or text, Gmail labels, the triage class — and
//! the node that sees the mail queues each message a watch matches for
//! that ember alone (`mail_matches`). The ember hears `mail.matched` on
//! the bus and reads its queue with a cursor. How much of a message it
//! then sees is the owner's choice, per ember or for all of them
//! (`mail_ember_views`): `headers` (sender, subject, date, class),
//! `snippet` (the default: plus the first 300 characters of the text) or
//! `full` (the readable text). The view is applied when the ember reads,
//! so changing it changes what is already queued too.
//!
//! Private mail never matches (the daemon checks before queueing, and
//! again at delivery — a privacy rule added later still holds a match
//! back). Watches and views mirror and journal like mail rules; the
//! queue rides its own id-keyed leg, so an ember gets its matches on
//! whichever of the owner's nodes it runs.
use anyhow::{bail, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

pub const VIEWS: &[&str] = &["headers", "snippet", "full"];
pub const DEFAULT_VIEW: &str = "snippet";
pub const DEFAULT_CHARS: i64 = 300;
/// The text a match keeps (what `full` can show at most).
pub const STORED_CHARS: usize = 8000;
pub const MAX_WATCHES: usize = 20;
/// Classes a watch may wait for (the triage labels).
pub const WATCH_CLASSES: &[&str] = &["needs_reply", "scheduling", "opportunity", "fyi", "ignore"];
/// The view row every ember without its own uses.
pub const ALL_EMBERS: &str = "*";
/// Matches kept per ember.
const QUEUE_KEEP: i64 = 2000;

/// What a watch looks for. Every field given must match (any one of its
/// values); at least one field is required.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchFilter {
    /// in the sender's name or address
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
    /// the sender's domain (or a subdomain of it)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<String>,
    /// in the subject
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subject: Vec<String>,
    /// in the subject or the text
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<String>,
    /// Gmail labels ("Category Personal", "Starred", a label of the owner's)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// only once triage gave the thread one of these classes
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<String>,
}

/// One message, as a watch sees it.
pub struct WatchInput<'a> {
    pub from: &'a str,
    pub from_addr: &'a str,
    pub subject: &'a str,
    pub text: &'a str,
    pub labels: &'a [String],
}

impl WatchFilter {
    pub fn validate(&self) -> Result<()> {
        let lists = [
            ("from", &self.from),
            ("domains", &self.domains),
            ("subject", &self.subject),
            ("words", &self.words),
            ("labels", &self.labels),
            ("classes", &self.classes),
        ];
        if lists.iter().all(|(_, l)| l.is_empty()) {
            bail!("a watch needs something to look for: from, domains, subject, words, labels or classes");
        }
        for (field, list) in lists {
            if list.len() > 20 {
                bail!("{field}: at most 20 values");
            }
            for v in list {
                let n = v.trim().chars().count();
                if !(2..=200).contains(&n) {
                    bail!("{field}: each value is 2–200 characters");
                }
            }
        }
        for c in &self.classes {
            if !WATCH_CLASSES.contains(&c.as_str()) {
                bail!("classes are: {}", WATCH_CLASSES.join(", "));
            }
        }
        Ok(())
    }

    /// Whether it waits for triage.
    pub fn needs_class(&self) -> bool {
        !self.classes.is_empty()
    }

    pub fn matches(&self, m: &WatchInput, class: Option<&str>) -> bool {
        let low = |s: &str| s.trim().to_lowercase();
        let any_in = |list: &[String], hay: &str| {
            let hay = hay.to_lowercase();
            list.iter().any(|v| hay.contains(&low(v)))
        };
        if !self.from.is_empty() && !any_in(&self.from, m.from) {
            return false;
        }
        if !self.domains.is_empty() {
            let domain = m
                .from_addr
                .rsplit_once('@')
                .map(|(_, d)| d.to_lowercase())
                .unwrap_or_default();
            let hit = self.domains.iter().any(|d| {
                let d = low(d);
                let d = d.trim_start_matches('@');
                !d.is_empty() && (domain == d || domain.ends_with(&format!(".{d}")))
            });
            if !hit {
                return false;
            }
        }
        if !self.subject.is_empty() && !any_in(&self.subject, m.subject) {
            return false;
        }
        if !self.words.is_empty() && !any_in(&self.words, m.subject) && !any_in(&self.words, m.text)
        {
            return false;
        }
        if !self.labels.is_empty()
            && !self
                .labels
                .iter()
                .any(|l| m.labels.iter().any(|x| x.eq_ignore_ascii_case(l.trim())))
        {
            return false;
        }
        if !self.classes.is_empty() && !class.is_some_and(|c| self.classes.iter().any(|x| x == c)) {
            return false;
        }
        true
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MailWatch {
    pub id: String,
    pub plugin: String,
    pub name: String,
    pub filter: WatchFilter,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmberView {
    /// an ember's name, or `*` for every ember without its own
    pub ember: String,
    pub view: String,
    pub chars: i64,
}

/// A message queued for an ember.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MailMatch {
    pub seq: i64,
    pub id: String,
    pub plugin: String,
    pub watch: String,
    pub watch_name: String,
    pub thread_id: Option<String>,
    pub message_id: String,
    pub sender: String,
    pub subject: String,
    pub date: Option<String>,
    pub labels: Vec<String>,
    pub class: Option<String>,
    pub why: Option<String>,
    pub text: String,
    pub created_at: String,
}

pub fn watch_id(plugin: &str, name: &str) -> String {
    format!(
        "mw_{}",
        &crate::ids::content_hash(format!("{plugin}\n{name}").as_bytes())[..16]
    )
}

fn match_id(plugin: &str, watch: &str, message_id: &str) -> String {
    format!(
        "mm_{}",
        &crate::ids::content_hash(format!("{plugin}\n{watch}\n{message_id}").as_bytes())[..20]
    )
}

fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (1..=60).contains(&n)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " _.-".contains(c))
}

const MATCH_COLS: &str = "seq, id, plugin, watch, watch_name, thread_id, message_id, sender, subject, date, labels, class, why, text, created_at";

fn row_to_match(r: &rusqlite::Row) -> rusqlite::Result<MailMatch> {
    let labels: String = r.get(10)?;
    Ok(MailMatch {
        seq: r.get(0)?,
        id: r.get(1)?,
        plugin: r.get(2)?,
        watch: r.get(3)?,
        watch_name: r.get(4)?,
        thread_id: r.get(5)?,
        message_id: r.get(6)?,
        sender: r.get(7)?,
        subject: r.get(8)?,
        date: r.get(9)?,
        labels: labels
            .split(',')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        class: r.get(11)?,
        why: r.get(12)?,
        text: r.get(13)?,
        created_at: r.get(14)?,
    })
}

impl Store {
    /* ————— watches (an ember's own) ————— */

    pub fn mail_watches(&self, plugin: Option<&str>) -> Result<Vec<MailWatch>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, plugin, name, filter, enabled, created_at, updated_at FROM mail_watches
             WHERE ?1 IS NULL OR plugin = ?1 ORDER BY plugin, name",
        )?;
        let rows = stmt.query_map([plugin], |r| {
            let filter: String = r.get(3)?;
            Ok(MailWatch {
                id: r.get(0)?,
                plugin: r.get(1)?,
                name: r.get(2)?,
                // a filter this build can't read matches nothing
                filter: serde_json::from_str(&filter).unwrap_or_default(),
                enabled: r.get::<_, i64>(4)? != 0,
                created_at: r.get(5)?,
                updated_at: r.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Add or change one of an ember's watches (by its name).
    pub fn mail_watch_set(
        &self,
        actor: &Actor,
        plugin: &str,
        name: &str,
        filter: &WatchFilter,
    ) -> Result<MailWatch> {
        let name = name.trim();
        if !valid_name(name) {
            bail!("a watch's name is 1–60 letters, digits, spaces, '_', '.' or '-'");
        }
        filter.validate()?;
        let id = watch_id(plugin, name);
        let existing = self
            .mail_watches(Some(plugin))?
            .into_iter()
            .find(|w| w.id == id);
        if let Some(w) = &existing {
            if w.enabled && &w.filter == filter {
                return Ok(w.clone());
            }
        } else if self.mail_watches(Some(plugin))?.len() >= MAX_WATCHES {
            bail!("an ember keeps at most {MAX_WATCHES} watches");
        }
        let now = now_utc();
        let filter_json = serde_json::to_string(filter)?;
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO mail_watches (id, plugin, name, filter, enabled, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)
                 ON CONFLICT(id) DO UPDATE SET filter = ?4, enabled = 1, updated_at = ?5",
                rusqlite::params![id, plugin, name, filter_json, now],
            )?;
            record_mutation(
                &conn,
                actor,
                "mail_watch_set",
                &format!("mail_watch_set:{id}:{now}"),
                &json!({"id": id, "plugin": plugin, "name": name, "filter": filter}),
            )?;
            let op = if existing.is_some() {
                "update"
            } else {
                "create"
            };
            super::journal_row(&conn, actor, "mail_watch", "mail_watches", &id, op)?;
        }
        Ok(MailWatch {
            id,
            plugin: plugin.into(),
            name: name.into(),
            filter: filter.clone(),
            enabled: true,
            created_at: existing
                .map(|w| w.created_at)
                .unwrap_or_else(|| now.clone()),
            updated_at: now,
        })
    }

    pub fn mail_watch_remove(&self, actor: &Actor, plugin: &str, name: &str) -> Result<bool> {
        let id = watch_id(plugin, name.trim());
        let conn = self.lock();
        let known: bool = conn
            .query_row(
                "SELECT 1 FROM mail_watches WHERE id = ?1",
                [&id],
                |_| Ok(()),
            )
            .is_ok();
        if !known {
            return Ok(false);
        }
        super::journal_op(&conn, actor, "mail_watch", &id, "delete", None)?;
        conn.execute("DELETE FROM mail_watches WHERE id = ?1", [&id])?;
        conn.execute("DELETE FROM mail_matches WHERE watch = ?1", [&id])?;
        record_mutation(
            &conn,
            actor,
            "mail_watch_removed",
            &format!("mail_watch_removed:{id}"),
            &json!({"id": id, "plugin": plugin}),
        )?;
        Ok(true)
    }

    /// An ember is gone: its watches, its own view and its queue go too.
    pub fn mail_watches_forget(&self, actor: &Actor, plugin: &str) -> Result<usize> {
        let names: Vec<String> = self
            .mail_watches(Some(plugin))?
            .into_iter()
            .map(|w| w.name)
            .collect();
        for name in &names {
            self.mail_watch_remove(actor, plugin, name)?;
        }
        self.mail_ember_view_clear(actor, plugin)?;
        let conn = self.lock();
        conn.execute("DELETE FROM mail_matches WHERE plugin = ?1", [plugin])?;
        Ok(names.len())
    }

    /* ————— how much an ember sees (the owner's) ————— */

    pub fn mail_ember_views(&self) -> Result<Vec<EmberView>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id, view, chars FROM mail_ember_views ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(EmberView {
                ember: r.get(0)?,
                view: r.get(1)?,
                chars: r.get(2)?,
            })
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// What one ember sees: its own setting, else the one for every
    /// ember, else the default (a snippet of 300 characters).
    pub fn mail_ember_view(&self, plugin: &str) -> Result<EmberView> {
        let views = self.mail_ember_views()?;
        let pick = views
            .iter()
            .find(|v| v.ember == plugin)
            .or_else(|| views.iter().find(|v| v.ember == ALL_EMBERS));
        Ok(match pick {
            Some(v) if VIEWS.contains(&v.view.as_str()) => v.clone(),
            // a view this build doesn't know shows the least
            Some(v) => EmberView {
                ember: v.ember.clone(),
                view: "headers".into(),
                chars: 0,
            },
            None => EmberView {
                ember: ALL_EMBERS.into(),
                view: DEFAULT_VIEW.into(),
                chars: DEFAULT_CHARS,
            },
        })
    }

    pub fn mail_ember_view_set(
        &self,
        actor: &Actor,
        ember: &str,
        view: &str,
        chars: Option<i64>,
    ) -> Result<EmberView> {
        if !VIEWS.contains(&view) {
            bail!("an ember sees one of: {}", VIEWS.join(", "));
        }
        if ember != ALL_EMBERS && !valid_name(ember) {
            bail!("no ember {ember:?}");
        }
        let chars = match view {
            "snippet" => chars.unwrap_or(DEFAULT_CHARS),
            "full" => STORED_CHARS as i64,
            _ => 0,
        };
        if view == "snippet" && !(40..=4000).contains(&chars) {
            bail!("a snippet is 40–4000 characters");
        }
        let now = now_utc();
        let conn = self.lock();
        let same: bool = conn
            .query_row(
                "SELECT 1 FROM mail_ember_views WHERE id = ?1 AND view = ?2 AND chars = ?3",
                rusqlite::params![ember, view, chars],
                |_| Ok(()),
            )
            .is_ok();
        if !same {
            conn.execute(
                "INSERT INTO mail_ember_views (id, view, chars, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(id) DO UPDATE SET view = ?2, chars = ?3, updated_at = ?4",
                rusqlite::params![ember, view, chars, now],
            )?;
            record_mutation(
                &conn,
                actor,
                "mail_ember_view_set",
                &format!("mail_ember_view_set:{ember}:{now}"),
                &json!({"ember": ember, "view": view, "chars": chars}),
            )?;
            super::journal_row(
                &conn,
                actor,
                "mail_ember_view",
                "mail_ember_views",
                ember,
                "update",
            )?;
        }
        Ok(EmberView {
            ember: ember.into(),
            view: view.into(),
            chars,
        })
    }

    /// Back to the setting for every ember.
    pub fn mail_ember_view_clear(&self, actor: &Actor, ember: &str) -> Result<bool> {
        let conn = self.lock();
        let known = conn
            .query_row(
                "SELECT 1 FROM mail_ember_views WHERE id = ?1",
                [ember],
                |_| Ok(()),
            )
            .is_ok();
        if !known {
            return Ok(false);
        }
        super::journal_op(&conn, actor, "mail_ember_view", ember, "delete", None)?;
        conn.execute("DELETE FROM mail_ember_views WHERE id = ?1", [ember])?;
        Ok(true)
    }

    /* ————— the queue ————— */

    /// Queue a match (once per watch and message). `true` when it is new.
    pub fn mail_match_add(&self, m: &MailMatch) -> Result<bool> {
        let id = match_id(&m.plugin, &m.watch, &m.message_id);
        let text: String = m.text.chars().take(STORED_CHARS).collect();
        let conn = self.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO mail_matches
                 (id, plugin, watch, watch_name, thread_id, message_id, sender, subject, date,
                  labels, class, why, text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                id,
                m.plugin,
                m.watch,
                m.watch_name,
                m.thread_id,
                m.message_id,
                m.sender,
                m.subject,
                m.date,
                m.labels.join(","),
                m.class,
                m.why,
                text,
                now_utc()
            ],
        )?;
        if n > 0 {
            trim_queue(&conn, &m.plugin)?;
        }
        Ok(n > 0)
    }

    /// An ember's matches after its cursor (this node's `seq`).
    pub fn mail_matches_after(
        &self,
        plugin: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<MailMatch>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MATCH_COLS} FROM mail_matches WHERE plugin = ?1 AND seq > ?2
             ORDER BY seq LIMIT ?3"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![plugin, after, limit.clamp(1, 200) as i64],
            row_to_match,
        )?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// The match an ember holds for this message, if any.
    pub fn mail_match_for(&self, plugin: &str, message_id: &str) -> Result<Option<MailMatch>> {
        let conn = self.lock();
        conn.query_row(
            &format!(
                "SELECT {MATCH_COLS} FROM mail_matches WHERE plugin = ?1 AND message_id = ?2
                 ORDER BY seq DESC LIMIT 1"
            ),
            rusqlite::params![plugin, message_id],
            row_to_match,
        )
        .optional()
        .map_err(Into::into)
    }

    /// The sync leg's page: matches past `since` (relay stamp).
    pub fn mail_matches_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {MATCH_COLS} FROM mail_matches WHERE synced_at > ?1
             ORDER BY synced_at LIMIT ?2"
        ))?;
        let items: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], row_to_match)?
            .map(|m| m.map(|m| serde_json::to_value(m).unwrap_or(Value::Null)))
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(
            &conn,
            "synced_at",
            "FROM mail_matches WHERE synced_at > ?1",
            since,
            limit,
        )?;
        Ok(json!({"items": items, "next": next}))
    }

    /// Apply a peer's page; counts only the matches that were new here.
    pub fn mail_matches_apply(&self, batch: &Value) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let mut applied = 0usize;
        let mut embers = std::collections::BTreeSet::new();
        for r in batch
            .get("items")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let s = |k: &str| r.get(k).and_then(|v| v.as_str());
            let (Some(id), Some(plugin), Some(watch), Some(message_id)) =
                (s("id"), s("plugin"), s("watch"), s("message_id"))
            else {
                continue;
            };
            let labels = r
                .get("labels")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let text: String = s("text").unwrap_or("").chars().take(STORED_CHARS).collect();
            let n = tx.execute(
                "INSERT OR IGNORE INTO mail_matches
                     (id, plugin, watch, watch_name, thread_id, message_id, sender, subject, date,
                      labels, class, why, text, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                rusqlite::params![
                    id,
                    plugin,
                    watch,
                    s("watch_name").unwrap_or(""),
                    s("thread_id"),
                    message_id,
                    s("sender").unwrap_or(""),
                    s("subject").unwrap_or(""),
                    s("date"),
                    labels,
                    s("class"),
                    s("why"),
                    text,
                    s("created_at").map(String::from).unwrap_or_else(now_utc),
                ],
            )?;
            if n > 0 {
                applied += 1;
                embers.insert(plugin.to_string());
            }
        }
        for plugin in &embers {
            trim_queue(&tx, plugin)?;
        }
        tx.commit()?;
        Ok(applied)
    }
}

fn trim_queue(conn: &rusqlite::Connection, plugin: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM mail_matches WHERE plugin = ?1 AND seq <= (
             SELECT seq FROM mail_matches WHERE plugin = ?1 ORDER BY seq DESC LIMIT 1 OFFSET ?2)",
        rusqlite::params![plugin, QUEUE_KEEP],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(&tmp.path().join("t.db")).unwrap();
        (s, tmp)
    }

    fn input<'a>(
        from: &'a str,
        addr: &'a str,
        subject: &'a str,
        text: &'a str,
        labels: &'a [String],
    ) -> WatchInput<'a> {
        WatchInput {
            from,
            from_addr: addr,
            subject,
            text,
            labels,
        }
    }

    #[test]
    fn test_a_watch_matches_every_field_it_names() {
        let labels = vec!["Inbox".to_string(), "Category Personal".to_string()];
        let m = input(
            "Dana Reyes <dana@deals.fund.example>",
            "dana@deals.fund.example",
            "Intro: seed round",
            "Happy to share the term sheet next week.",
            &labels,
        );
        let f = WatchFilter {
            domains: vec!["fund.example".into()],
            words: vec!["Term Sheet".into()],
            ..Default::default()
        };
        assert!(f.matches(&m, None));
        // a domain matches itself or a subdomain, never a lookalike
        let lookalike = WatchFilter {
            domains: vec!["und.example".into()],
            ..Default::default()
        };
        assert!(!lookalike.matches(&m, None));
        // every field must match
        let both = WatchFilter {
            from: vec!["dana".into()],
            subject: vec!["invoice".into()],
            ..Default::default()
        };
        assert!(!both.matches(&m, None));
        let label = WatchFilter {
            labels: vec!["category personal".into()],
            ..Default::default()
        };
        assert!(label.matches(&m, None));
        // a class filter waits for triage
        let class = WatchFilter {
            classes: vec!["opportunity".into()],
            ..Default::default()
        };
        assert!(class.needs_class());
        assert!(!class.matches(&m, None));
        assert!(!class.matches(&m, Some("fyi")));
        assert!(class.matches(&m, Some("opportunity")));
        assert!(WatchFilter::default().validate().is_err());
        assert!(WatchFilter {
            classes: vec!["send_money".into()],
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn test_watches_views_and_the_queue() {
        let (s, _t) = store();
        let a = Actor::new("plugin:leadfinder");
        let f = WatchFilter {
            classes: vec!["opportunity".into()],
            ..Default::default()
        };
        let w = s.mail_watch_set(&a, "leadfinder", "leads", &f).unwrap();
        assert_eq!(w.id, watch_id("leadfinder", "leads"));
        // the same watch again changes nothing
        let again = s.mail_watch_set(&a, "leadfinder", "leads", &f).unwrap();
        assert_eq!(again.updated_at, w.updated_at);
        assert_eq!(s.mail_watches(Some("leadfinder")).unwrap().len(), 1);
        assert!(s.mail_watches(Some("other")).unwrap().is_empty());

        // the default view, then one for every ember, then this ember's own
        assert_eq!(s.mail_ember_view("leadfinder").unwrap().view, "snippet");
        assert_eq!(s.mail_ember_view("leadfinder").unwrap().chars, 300);
        let owner = Actor::new("human:owner");
        s.mail_ember_view_set(&owner, ALL_EMBERS, "headers", None)
            .unwrap();
        assert_eq!(s.mail_ember_view("leadfinder").unwrap().view, "headers");
        s.mail_ember_view_set(&owner, "leadfinder", "full", None)
            .unwrap();
        assert_eq!(s.mail_ember_view("leadfinder").unwrap().view, "full");
        assert_eq!(s.mail_ember_view("other").unwrap().view, "headers");
        assert!(s
            .mail_ember_view_set(&owner, "leadfinder", "everything", None)
            .is_err());

        let m = MailMatch {
            plugin: "leadfinder".into(),
            watch: w.id.clone(),
            watch_name: "leads".into(),
            message_id: "m1@x".into(),
            sender: "Dana <dana@x.example>".into(),
            subject: "Intro".into(),
            text: "x".repeat(STORED_CHARS + 50),
            ..Default::default()
        };
        assert!(s.mail_match_add(&m).unwrap());
        assert!(!s.mail_match_add(&m).unwrap(), "once per watch and message");
        let got = s.mail_matches_after("leadfinder", 0, 50).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text.chars().count(), STORED_CHARS);
        assert!(s
            .mail_matches_after("leadfinder", got[0].seq, 50)
            .unwrap()
            .is_empty());
        assert!(s.mail_matches_after("other", 0, 50).unwrap().is_empty());

        // the sync leg carries it to another node once
        let page = s.mail_matches_since("", 100).unwrap();
        let (s2, _t2) = store();
        assert_eq!(s2.mail_matches_apply(&page).unwrap(), 1);
        assert_eq!(s2.mail_matches_apply(&page).unwrap(), 0);
        assert_eq!(
            s2.mail_matches_after("leadfinder", 0, 50).unwrap()[0].message_id,
            "m1@x"
        );

        // the ember is gone: watches, its view and its queue go
        assert_eq!(s.mail_watches_forget(&owner, "leadfinder").unwrap(), 1);
        assert!(s.mail_watches(Some("leadfinder")).unwrap().is_empty());
        assert!(s
            .mail_matches_after("leadfinder", 0, 50)
            .unwrap()
            .is_empty());
        assert_eq!(s.mail_ember_view("leadfinder").unwrap().view, "headers");
    }
}
