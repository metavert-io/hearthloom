//! Privacy rules: patterns that force `scope = 'private'` — retroactively
//! when a rule is created, and at ingest time for everything that arrives
//! after (the pass lives in `apply_batch`). Rules never touch content;
//! scope is correction metadata with an audit trail. The vault switch that
//! *hides* private rows lives in `mod.rs`.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::ingest::EpisodeDraft;
use crate::time::now_utc;

pub const RULE_KINDS: &[&str] = &["domain", "entity", "place", "term"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyRule {
    pub id: String,
    /// domain (URLs/refs + text), entity/place (linked entity name),
    /// term (full-text).
    pub kind: String,
    pub pattern: String,
    pub created_at: String,
    pub actor: String,
}

/// Does a draft trip any rule? Pure text/name matching — used at ingest
/// before the episode exists, so no FTS available.
pub(crate) fn draft_matches(rules: &[PrivacyRule], draft: &EpisodeDraft) -> bool {
    let haystack = format!("{}\n{}\n{}", draft.source_ref, draft.title, draft.body);
    text_matches(rules, &haystack)
        || rules.iter().any(|rule| {
            matches!(rule.kind.as_str(), "entity" | "place")
                && draft
                    .entities
                    .iter()
                    .any(|e| e.name.eq_ignore_ascii_case(&rule.pattern))
        })
}

/// Text-only rule matching (notes have no entity refs). A domain rule
/// also matches the domain's bare STEM (≥6 chars — "privateclinic"
/// without the ".example"): a note ABOUT the service is what the rule meant.
pub(crate) fn text_matches(rules: &[PrivacyRule], text: &str) -> bool {
    if rules.is_empty() {
        return false;
    }
    let haystack = text.to_lowercase();
    rules.iter().any(|rule| {
        let pattern = rule.pattern.to_lowercase();
        match rule.kind.as_str() {
            "term" => haystack.contains(&pattern),
            "domain" => {
                if haystack.contains(&pattern) {
                    return true;
                }
                let stem = pattern.split('.').next().unwrap_or("");
                stem.len() >= 6 && haystack.contains(stem)
            }
            _ => false,
        }
    })
}

/// Load all rules on an open connection/transaction (ingest runs inside
/// the batch transaction and cannot re-lock the store).
pub(crate) fn load_rules(conn: &rusqlite::Connection) -> Result<Vec<PrivacyRule>> {
    let mut stmt =
        conn.prepare("SELECT id, kind, pattern, created_at, actor FROM privacy_rules")?;
    let rows = stmt.query_map([], |row| {
        Ok(PrivacyRule {
            id: row.get(0)?,
            kind: row.get(1)?,
            pattern: row.get(2)?,
            created_at: row.get(3)?,
            actor: row.get(4)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

impl Store {
    /// Create a rule and retroactively relabel everything it matches.
    /// Returns (rule, episodes relabeled).
    pub fn add_privacy_rule(
        &self,
        actor: &Actor,
        kind: &str,
        pattern: &str,
    ) -> Result<(PrivacyRule, usize)> {
        if !RULE_KINDS.contains(&kind) {
            return Err(anyhow!("rule kind must be one of {RULE_KINDS:?}"));
        }
        let pattern = pattern.trim();
        if pattern.len() < 3 {
            return Err(anyhow!("pattern too short to be safe (min 3 chars)"));
        }
        let rule = PrivacyRule {
            id: ids::privacy_rule_id(),
            kind: kind.to_string(),
            pattern: pattern.to_string(),
            created_at: now_utc(),
            actor: actor.actor.clone(),
        };
        let conn = self.lock();
        let inserted = conn.execute(
            "INSERT INTO privacy_rules (id, kind, pattern, created_at, actor)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(kind, pattern) DO NOTHING",
            rusqlite::params![rule.id, rule.kind, rule.pattern, rule.created_at, rule.actor],
        )?;
        // Journal the new row (only when the insert actually landed — a
        // conflicting duplicate is a no-op with no new id to journal).
        if inserted > 0 {
            super::journal_row(&conn, actor, "privacy_rule", "privacy_rules", &rule.id, "create")?;
        }

        // Retroactive sweep. LIKE patterns escape their wildcards; the FTS
        // leg quotes the pattern as a phrase.
        let like = format!(
            "%{}%",
            pattern.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
        );
        let phrase = format!("\"{}\"", pattern.replace('"', ""));
        let relabeled = match kind {
            "domain" | "term" => conn.execute(
                "UPDATE episodes SET scope = 'private', scope_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                 WHERE scope != 'private'
                   AND (source_ref LIKE ?1 ESCAPE '\\'
                        OR title LIKE ?1 ESCAPE '\\'
                        OR id IN (SELECT episode_id FROM episode_fts
                                  WHERE episode_fts MATCH ?2))",
                rusqlite::params![like, phrase],
            )?,
            // entity/place: the linked entity goes private, and so does
            // every episode citing any member of its merge group.
            _ => {
                conn.execute(
                    "UPDATE entities SET scope = 'private'
                     WHERE name = ?1 COLLATE NOCASE AND scope != 'private'",
                    [pattern],
                )?;
                conn.execute(
                    "UPDATE episodes SET scope = 'private', scope_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                     WHERE scope != 'private'
                       AND id IN (SELECT ee.episode_id FROM episode_entities ee
                                  JOIN entities e ON e.id = ee.entity_id
                                  WHERE e.name = ?1 COLLATE NOCASE
                                     OR e.merged_into IN (SELECT id FROM entities
                                                          WHERE name = ?1 COLLATE NOCASE))",
                    [pattern],
                )?
            }
        };
        record_mutation(
            &conn,
            actor,
            "privacy_rule_added",
            &format!("privacy_rule_added:{}:{}", rule.kind, rule.pattern),
            &json!({"id": rule.id, "kind": rule.kind, "pattern": rule.pattern,
                    "episodes_relabeled": relabeled}),
        )?;
        Ok((rule, relabeled))
    }

    pub fn list_privacy_rules(&self) -> Result<Vec<PrivacyRule>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, pattern, created_at, actor
             FROM privacy_rules ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PrivacyRule {
                id: row.get(0)?,
                kind: row.get(1)?,
                pattern: row.get(2)?,
                created_at: row.get(3)?,
                actor: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Remove a rule. Already-applied relabels stand (they are audited
    /// corrections); future ingests simply stop matching.
    pub fn remove_privacy_rule(&self, actor: &Actor, id: &str) -> Result<bool> {
        let conn = self.lock();
        // Journal a delete-tombstone with the rule's content BEFORE the row is
        // gone, so a removed rule stays restorable.
        if super::row_as_json(&conn, "privacy_rules", id)?.is_some() {
            super::journal_row(&conn, actor, "privacy_rule", "privacy_rules", id, "delete")?;
        }
        let removed = conn.execute("DELETE FROM privacy_rules WHERE id = ?1", [id])?;
        if removed > 0 {
            record_mutation(
                &conn,
                actor,
                "privacy_rule_removed",
                &format!("privacy_rule_removed:{id}"),
                &json!({"id": id}),
            )?;
        }
        Ok(removed > 0)
    }

    /// Restore a privacy rule from its operational-journal history —
    /// re-inserts the last known post-image. Mirrors `restore_task`.
    /// (Restoring only re-registers the rule; the retroactive relabel
    /// from the original creation already stands.)
    pub fn restore_privacy_rule(&self, actor: &Actor, id: &str) -> Result<()> {
        let conn = self.lock();
        let post: Option<String> = conn
            .query_row(
                "SELECT post_image FROM op_journal
                 WHERE entity_kind='privacy_rule' AND entity_id=?1 AND post_image IS NOT NULL
                 ORDER BY ts DESC, seq DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(post) = post else {
            return Err(anyhow!("no journal history for privacy rule {id}"));
        };
        let obj: serde_json::Value = serde_json::from_str(&post)?;
        let Some(map) = obj.as_object() else {
            return Err(anyhow!("bad journal post-image for {id}"));
        };
        let cols: Vec<&String> = map.keys().collect();
        let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT OR REPLACE INTO privacy_rules ({}) VALUES ({})",
            cols.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
            placeholders.join(", ")
        );
        let params: Vec<rusqlite::types::Value> =
            cols.iter().map(|c| super::json_to_sql(&map[c.as_str()])).collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))?;
        super::journal_row(&conn, actor, "privacy_rule", "privacy_rules", id, "update")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EntityRef, IngestBatch};
    use crate::store::EpisodeFilter;
    use crate::time::EventTime;

    fn draft(source_ref: &str, title: &str, body: &str, entity: Option<&str>) -> EpisodeDraft {
        EpisodeDraft {
            source: "chrome".into(),
            source_ref: source_ref.into(),
            kind: "browsing".into(),
            title: title.into(),
            body: body.into(),
            event_time: EventTime::unknown(),
            trust: "self".into(),
            scope: None,
            entities: entity
                .map(|name| {
                    vec![EntityRef {
                        entity_type: "Place".into(),
                        name: name.into(),
                        attributes: vec![],
                        role: "visited".into(),
                    }]
                })
                .unwrap_or_default(),
            edges: vec![],
        }
    }

    #[test]
    fn test_retroactive_and_ingest_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::paths::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:chrome".into(),
                    session_id: None,
                    episodes: vec![
                        draft("https://privateclinic.example/x", "Clinic", "page visit", None),
                        draft("https://example.com/news", "News", "reading news", None),
                    ],
                },
            )
            .unwrap();

        // Retroactive: the domain rule relabels the existing episode.
        let (_, relabeled) = store
            .add_privacy_rule(&actor, "domain", "privateclinic.example")
            .unwrap();
        assert_eq!(relabeled, 1);

        // At ingest: a new matching episode arrives already private
        // (unlock to read it back — the vault is locked by default).
        store.vault_unlock(15);
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:chrome".into(),
                    session_id: None,
                    episodes: vec![draft(
                        "https://privateclinic.example/y",
                        "Clinic again",
                        "another visit",
                        None,
                    )],
                },
            )
            .unwrap();
        let episode = store
            .episode(&crate::ids::episode_id("chrome", "https://privateclinic.example/y"))
            .unwrap()
            .unwrap();
        assert_eq!(episode.scope, "private");

        // The untouched episode stays put.
        let episode = store
            .episode(&crate::ids::episode_id("chrome", "https://example.com/news"))
            .unwrap()
            .unwrap();
        assert_ne!(episode.scope, "private");

        // Bad rules rejected.
        assert!(store.add_privacy_rule(&actor, "regex", "x{3}").is_err());
        assert!(store.add_privacy_rule(&actor, "term", "ab").is_err());
    }

    #[test]
    fn test_area_filter_limits_read_surfaces() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::paths::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut health = draft(
            "https://clinic.example/1",
            "clinic visit",
            "bloodwork results at the clinic",
            Some("Clinic"),
        );
        health.scope = Some("health".into());
        let mut work = draft(
            "https://office.example/2",
            "roadmap review",
            "reviewed the roadmap at the office",
            Some("Office"),
        );
        work.scope = Some("work".into());
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:chrome".into(),
                    session_id: None,
                    episodes: vec![health, work],
                },
            )
            .unwrap();
        let health_id = crate::ids::episode_id("chrome", "https://clinic.example/1");
        let work_id = crate::ids::episode_id("chrome", "https://office.example/2");

        // No filter: both visible.
        assert_eq!(store.search("clinic", 10).unwrap().len(), 1);
        {
            let _work_only = crate::store::enter_area_filter(&["work".to_string()]);
            assert!(store.search("clinic", 10).unwrap().is_empty());
            assert_eq!(store.search("roadmap", 10).unwrap().len(), 1);
            assert!(store.episode(&health_id).unwrap().is_none());
            assert!(store.episode_body(&health_id).unwrap().is_none());
            assert!(store.episode(&work_id).unwrap().is_some());
            // An entity of another area is unknown; one of ours is not.
            assert!(store.entity_dossier("Clinic").unwrap().is_none());
            assert!(store.traverse("Clinic", None, 1, None).is_err());
            assert!(store.recall_entity("Clinic", 10).is_err());
            assert_eq!(store.entity_dossier("Office").unwrap().unwrap().episode_count, 1);
            let span = store.recall_span("0000-01-01", "9999-12-31", 50).unwrap();
            assert!(span.iter().all(|e| e.scope == "work"), "{span:?}");
            // Raw SQL knows the vault, not areas — refused outright.
            assert!(store.read_query("SELECT id FROM episodes", 10).is_err());
            {
                // Nested filters restore on drop; junk labels narrow to nothing.
                let _none = crate::store::enter_area_filter(&["private".to_string(), "x' OR 1=1 --".to_string()]);
                assert!(store.search("roadmap", 10).unwrap().is_empty());
            }
            assert_eq!(store.search("roadmap", 10).unwrap().len(), 1);
        }
        // Dropped: the thread reads everything again.
        assert_eq!(store.search("clinic", 10).unwrap().len(), 1);
        assert!(store.read_query("SELECT id FROM episodes", 10).is_ok());
    }

    #[test]
    fn test_vault_locks_every_read_surface() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::paths::Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        let mut secret = draft(
            "https://secret.example/1",
            "the secret meeting",
            "met with contact at the safehouse",
            Some("Safehouse"),
        );
        secret.scope = Some("private".into());
        let public = draft(
            "https://example.com/2",
            "regular reading",
            "read an article about rust",
            None,
        );
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:chrome".into(),
                    session_id: None,
                    episodes: vec![secret, public],
                },
            )
            .unwrap();
        let secret_id = crate::ids::episode_id("chrome", "https://secret.example/1");

        // LOCKED (default): invisible everywhere.
        assert!(store.search("safehouse", 10).unwrap().is_empty());
        assert!(store.episode(&secret_id).unwrap().is_none());
        assert!(store.episode_body(&secret_id).unwrap().is_none());
        assert!(store.entity_dossier("Safehouse").unwrap().is_none());
        assert!(store.traverse("Safehouse", None, 1, None).is_err());
        assert!(store.recall_entity("Safehouse", 10).is_err());
        assert_eq!(store.counts().unwrap().episodes, 1);
        let page = store
            .browse_episodes(&EpisodeFilter::default(), "time", "desc", 0, 50)
            .unwrap();
        assert_eq!(page.total, 1);
        // No facet leaks a private count either.
        assert!(page.facets.scope.iter().all(|f| f.value != "private"));
        // The one-scan composition matches the single-column forms and
        // leaks no private bucket.
        let comp = store.episode_composition().unwrap();
        assert_eq!(comp.total, 1);
        assert_eq!(comp.sources, store.episode_counts_by_source().unwrap());
        assert_eq!(comp.kinds, store.episode_counts_by("kind").unwrap());
        assert!(comp.scopes.iter().all(|(scope, _)| scope != "private"));
        // The raw sql handle sees shadowed tables; main. is rejected.
        let (_, rows) = store
            .read_query("SELECT id FROM episodes", 200)
            .unwrap();
        assert_eq!(rows.len(), 1);
        let (_, rows) = store
            .read_query("SELECT body FROM episode_fts WHERE body LIKE '%safehouse%'", 200)
            .unwrap();
        assert!(rows.is_empty());
        assert!(store.read_query("SELECT id FROM main.episodes", 200).is_err());
        // The location index shadows too — a private day's coordinates
        // never leave through the raw sql handle while locked.
        {
            let secret_id: String = {
                let conn = store.lock();
                conn.query_row(
                    "SELECT id FROM episodes WHERE scope = 'private'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
            };
            store
                .insert_location_points(
                    &secret_id,
                    &[(None, "2026-03-01".into(), 41.89, 12.49)],
                )
                .unwrap();
            let (_, rows) = store
                .read_query("SELECT lat, lon FROM location_points", 200)
                .unwrap();
            assert!(rows.is_empty(), "locked vault must hide private coordinates");
            assert!(store
                .atlas_points("2026-01-01", "2026-12-31", 100)
                .unwrap()
                .is_empty());
        }

        // The self-model and opportunities shadow too: a private-sphere
        // proposition, and any opportunity whose evidence or seeking is
        // private, never leave through the raw sql handle while locked.
        {
            let secret_id: String = {
                let conn = store.lock();
                conn.execute(
                    "INSERT INTO self_model (id, sphere, kind, text, origin,
                         status, created_at, updated_at)
                     VALUES ('sm_priv', 'private', 'seeking', 'secret goal',
                         'declared', 'confirmed', '2026-01-01T00:00:00Z',
                         '2026-01-01T00:00:00Z')",
                    [],
                )
                .unwrap();
                conn.query_row(
                    "SELECT id FROM episodes WHERE scope = 'private'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
            };
            {
                let conn = store.lock();
                conn.execute(
                    "INSERT INTO opportunities (id, seeking_id, episode_id,
                         score, found_at)
                     VALUES ('opp_priv', 'sm_priv', ?1, 0.9,
                         '2026-01-01T00:00:00Z')",
                    [&secret_id],
                )
                .unwrap();
            }
            let (_, rows) = store
                .read_query("SELECT text FROM self_model", 200)
                .unwrap();
            assert!(rows.is_empty(), "locked vault must hide private propositions");
            let (_, rows) = store
                .read_query("SELECT id FROM opportunities", 200)
                .unwrap();
            assert!(rows.is_empty(), "locked vault must hide private-evidence opportunities");
        }

        // Always-excluded surfaces, locked or not.
        assert!(store
            .recent_episodes(10)
            .unwrap()
            .iter()
            .all(|e| e.scope != "private"));
        assert!(store
            .top_entities(10)
            .unwrap()
            .iter()
            .all(|(e, _)| e.scope != "private"));

        // UNLOCKED: private is visible again; a write still works after
        // the shadow views were dropped.
        store.vault_unlock(15);
        assert!(!store.search("safehouse", 10).unwrap().is_empty());
        assert!(store.episode(&secret_id).unwrap().is_some());
        assert_eq!(store.counts().unwrap().episodes, 2);
        assert!(store.entity_dossier("Safehouse").unwrap().is_some());
        let (_, rows) = store.read_query("SELECT id FROM episodes", 200).unwrap();
        assert_eq!(rows.len(), 2);
        let comp = store.episode_composition().unwrap();
        assert_eq!(comp.total, 2);
        assert!(comp.scopes.iter().any(|(scope, n)| scope == "private" && *n == 1));
        // The bulk-read variant excludes private EVEN while unlocked
        // (the cloud-pass rule), and leaves no shadow views behind.
        let (_, rows) = store.read_query_public("SELECT id FROM episodes", 200).unwrap();
        assert_eq!(rows.len(), 1);
        let (_, rows) = store.read_query("SELECT id FROM episodes", 200).unwrap();
        assert_eq!(rows.len(), 2);
        // An ask begun while unlocked is sensitive: visible now.
        let sensitive_ask = store.ask_start(&actor, "what is at the safehouse?", None).unwrap();
        assert!(store.ask_get(&sensitive_ask).unwrap().is_some());

        store.vault_lock();
        assert_eq!(store.counts().unwrap().episodes, 1);
        // ...and hidden from Ask history + detail once locked again.
        assert!(store.ask_get(&sensitive_ask).unwrap().is_none());
        assert!(store
            .ask_history(0, 10, None)
            .unwrap()
            .iter()
            .all(|r| r.first.id != sensitive_ask));
        // A locked viewer can't take anything OUT of the vault: the
        // relabel passes over the private row, which stays hidden…
        assert_eq!(
            store.set_episode_scopes(&actor, std::slice::from_ref(&secret_id), "personal").unwrap(),
            0
        );
        assert!(store.episode(&secret_id).unwrap().is_none());
        // …while writes still reach the real table after the shadow views
        // (they are transient): unlocked, the same relabel lands.
        store.vault_unlock(15);
        assert_eq!(
            store.set_episode_scopes(&actor, std::slice::from_ref(&secret_id), "personal").unwrap(),
            1
        );
        store.vault_lock();
        assert!(store.episode(&secret_id).unwrap().is_some());
    }
}
