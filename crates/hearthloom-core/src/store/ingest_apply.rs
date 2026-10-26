//! Applying an adapter's `IngestBatch` to the store — the single write
//! path for episode material. Runs in one transaction per batch; every
//! episode/entity/edge write lands in the mutations log.

use anyhow::Result;
use rusqlite::TransactionBehavior;
use serde_json::json;

use super::episodes::{insert_episode, EpisodeRow};
use super::graph::{assert_edge, attribute_scope, peek_entity, upsert_entity};
use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::ingest::{IngestBatch, IngestReport};
use crate::paths::Home;
use crate::time::now_utc;

/// The sources that use THE SUPERSESSION PATTERN — the only ones whose
/// `#r<n>` ref suffix marks earlier revisions superseded. Their refs are
/// minted by this codebase (photo days, visit clusters, contact cards). A
/// new revisioned source must be added here; never add one whose
/// `source_ref` comes from outside (mail Message-IDs, calendar UIDs,
/// URLs, anything a third party can choose).
/// `trips` (v60): refs are minted by the daemon's own reconciliation
/// pass (`trip:<day>:<idx>#r<fingerprint>`), never chosen by a sender.
pub const REVISIONED_SOURCES: &[&str] = &["photos", "visits", "contacts", "trips"];

/// The record's own sources: the revisioned ones (a `#r<n>` ref hides the
/// real record), the consolidation passes (the digest, chapters), the
/// owner's own writing (Desk, notes and the old Codi harvest — derivation
/// `declared`), accepted proposals, and Facebook (its posts feed the
/// location index). Only the owner's keys and peers may ingest under
/// these names (http.rs `ingest`).
pub const OWNER_ONLY_SOURCES: &[&str] = &[
    "photos", "visits", "contacts", "consolidation", "desk", "notes", "codi", "memory", "facebook",
];

impl Store {
    /// Apply a batch of episode drafts. Raw content is written under the
    /// home's raw dir before the row lands; idempotent on re-runs.
    pub fn apply_batch(&self, home: &Home, batch: &IngestBatch) -> Result<IngestReport> {
        let actor = Actor {
            actor: batch.actor.clone(),
            session_id: batch.session_id.clone(),
        };
        let mut report = IngestReport::default();
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let privacy_rules = super::privacy::load_rules(&tx)?;
        // D.12: what live retirements say about new arrivals (a retired
        // source keeps its late episodes retired; see store/retire.rs).
        let arrivals = super::retire::ArrivalRules::load(&tx)?;

        for draft in &batch.episodes {
            let episode_id = ids::episode_id(&draft.source, &draft.source_ref);

            // Scope resolution: explicit > known Project entity's scope >
            // author/organizer identity scope > `personal`. This is how one
            // human keeps personal projects and client/company work apart.
            let mut scope = draft.scope.clone();
            if scope.is_none() {
                for entity_ref in &draft.entities {
                    if entity_ref.entity_type != "Project" {
                        continue;
                    }
                    if let Some(row) = peek_entity(
                        &tx,
                        &entity_ref.entity_type,
                        &entity_ref.name,
                        &entity_ref.attributes,
                    )? {
                        scope = Some(row.scope);
                        break;
                    }
                }
            }
            if scope.is_none() {
                for entity_ref in &draft.entities {
                    if entity_ref.role != "author" && entity_ref.role != "organizer" {
                        continue;
                    }
                    for (kind, value) in &entity_ref.attributes {
                        if let Some(identity_scope) = attribute_scope(&tx, kind, value)? {
                            scope = Some(identity_scope);
                            break;
                        }
                    }
                    if scope.is_some() {
                        break;
                    }
                }
            }
            let mut scope = scope.unwrap_or_else(|| "personal".to_string());
            // Privacy rules outrank every other inference, including an
            // adapter's explicit scope — a matching episode arrives private.
            if scope != "private" && super::privacy::draft_matches(&privacy_rules, draft) {
                scope = "private".to_string();
            }

            // Raw content file, dated by local event date (fallback: today).
            let date = draft
                .event_time
                .utc
                .as_deref()
                .map(|t| t[..10].to_string())
                .unwrap_or_else(|| now_utc()[..10].to_string());
            let content_ref = home.write_raw(&episode_id, &date, &draft.body)?;

            let row = EpisodeRow {
                id: episode_id.clone(),
                source: draft.source.clone(),
                source_ref: draft.source_ref.clone(),
                kind: draft.kind.clone(),
                title: draft.title.clone(),
                actor: actor.actor.clone(),
                event_time: draft.event_time.utc.clone(),
                tz_offset_minutes: draft.event_time.tz_offset_minutes,
                time_uncertainty: draft.event_time.uncertainty.as_str().to_string(),
                ingest_time: now_utc(),
                trust: draft.trust.clone(),
                scope: scope.clone(),
                content_ref: Some(content_ref),
                content_hash: ids::content_hash(draft.body.as_bytes()),
                derivation: super::episodes::derivation_for(&draft.source, &draft.trust).to_string(),
            };

            if !insert_episode(&tx, &row, actor.session_id.as_deref(), &draft.body)? {
                report.episodes_existing += 1;
                continue;
            }
            report.episodes_new += 1;
            // THE SUPERSESSION PATTERN (v47, CLAUDE.md): an episode whose
            // source_ref carries a revision suffix (`<base>#r<n>`) marks
            // every prior revision of the same base superseded — a pure
            // function of the ref, so BOTH peers converge with no wire
            // change when the revision syncs as an ordinary episode.
            // Superseded rows keep their content (append-only holds);
            // their location points are re-derived from the new revision.
            // ONLY for sources whose refs we mint ourselves (0.5.66): a
            // mail's ref is its Message-ID and a calendar event's is its
            // UID — both chosen by the SENDER — so honouring the suffix
            // everywhere let anyone hide a known message or event by
            // sending one whose id ended in `#r2`.
            if let Some((base, _rev)) = draft
                .source_ref
                .rsplit_once("#r")
                .filter(|(_, r)| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
                .filter(|_| REVISIONED_SOURCES.contains(&draft.source.as_str()))
            {
                tx.execute(
                    "UPDATE episodes SET superseded_by = ?1
                     WHERE source = ?2 AND superseded_by IS NULL AND id != ?1
                       AND (source_ref = ?3 OR source_ref LIKE ?3 || '#r%')",
                    rusqlite::params![episode_id, draft.source, base],
                )?;
                tx.execute(
                    "DELETE FROM location_points WHERE episode_id IN (
                         SELECT id FROM episodes WHERE superseded_by = ?1)",
                    [&episode_id],
                )?;
            }
            let arrival = arrivals.covering(&tx, &episode_id, &draft.source, &draft.source_ref)?;
            record_mutation(
                &tx,
                &actor,
                "episode_ingested",
                &format!("episode_ingested:{episode_id}"),
                &json!({
                    "id": episode_id,
                    "source": draft.source,
                    "kind": draft.kind,
                    "title": draft.title,
                }),
            )?;

            // Resolve entity references and record provenance. A
            // "Person" whose identity is a machine address (noreply,
            // bounce token, random sequence) mints as `Service` — the
            // machine-address rulebook, applied once for every adapter.
            let mut entity_ids = Vec::with_capacity(draft.entities.len());
            for entity_ref in &draft.entities {
                let entity_type = if entity_ref.entity_type == "Person"
                    && crate::machine::is_machine_person(&entity_ref.name, &entity_ref.attributes)
                {
                    "Service"
                } else {
                    entity_ref.entity_type.as_str()
                };
                let (id, created, queued) = upsert_entity(
                    &tx,
                    &actor,
                    entity_type,
                    &entity_ref.name,
                    &scope,
                    &entity_ref.attributes,
                )?;
                if created {
                    report.entities_created += 1;
                } else {
                    report.entities_resolved += 1;
                }
                if queued {
                    report.review_queued += 1;
                }
                tx.execute(
                    "INSERT OR IGNORE INTO episode_entities (episode_id, entity_id, role)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![episode_id, id, entity_ref.role],
                )?;
                // Live evidence again: an entity a retirement hid for
                // having none is visible from here on.
                if matches!(arrival, super::retire::Arrival::Live) {
                    tx.execute(
                        "UPDATE entities SET hidden_by = NULL WHERE id = ?1 AND hidden_by IS NOT NULL",
                        [&id],
                    )?;
                }
                entity_ids.push(id);
            }

            for edge in &draft.edges {
                let (Some(src), Some(dst)) =
                    (entity_ids.get(edge.src), entity_ids.get(edge.dst))
                else {
                    continue; // malformed assertion; skip rather than abort the batch
                };
                if assert_edge(
                    &tx,
                    &actor,
                    src,
                    &edge.relation,
                    dst,
                    &scope,
                    draft.event_time.utc.as_deref(),
                    &episode_id,
                )? {
                    report.edges_created += 1;
                }
            }
            match arrival {
                super::retire::Arrival::Live => {}
                super::retire::Arrival::Retire(rid) => arrivals.retire(&tx, &rid, &episode_id)?,
                super::retire::Arrival::Behind(original) => {
                    tx.execute(
                        "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
                        rusqlite::params![original, episode_id],
                    )?;
                }
            }
        }

        tx.commit()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EdgeAssertion, EntityRef, EpisodeDraft};
    use crate::time::{EventTime, TimeUncertainty};

    fn draft(source_ref: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "git".into(),
            source_ref: source_ref.into(),
            kind: "commit".into(),
            title: format!("commit {source_ref}"),
            body: "fix: something\n\n src/main.rs".into(),
            event_time: EventTime::parse_rfc3339(
                "2026-03-15T10:00:00-04:00",
                TimeUncertainty::Exact,
            )
            .unwrap(),
            trust: "self".into(),
            scope: Some("work".into()),
            entities: vec![
                EntityRef {
                    entity_type: "Person".into(),
                    name: "Jon Hale".into(),
                    attributes: vec![("email".into(), "jon@harbor.example".into())],
                    role: "author".into(),
                },
                EntityRef {
                    entity_type: "Project".into(),
                    name: "orrery".into(),
                    attributes: vec![("repo-path".into(), "/Users/you/code/garden".into())],
                    role: "repo".into(),
                },
            ],
            edges: vec![EdgeAssertion {
                src: 0,
                relation: "contributes_to".into(),
                dst: 1,
            }],
        }
    }

    #[test]
    fn test_revision_suffix_only_supersedes_for_our_own_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let from = |source: &str, source_ref: &str| EpisodeDraft {
            source: source.into(),
            entities: vec![],
            edges: vec![],
            ..draft(source_ref)
        };
        let apply = |drafts: Vec<EpisodeDraft>| {
            store
                .apply_batch(&home, &IngestBatch { actor: "adapter:test".into(), session_id: None, episodes: drafts })
                .unwrap();
        };
        let superseded = |source: &str, source_ref: &str| -> bool {
            let conn = store.lock();
            conn.query_row(
                "SELECT superseded_by IS NOT NULL FROM episodes WHERE source = ?1 AND source_ref = ?2",
                [source, source_ref],
                |row| row.get(0),
            )
            .unwrap()
        };
        // A mail whose Message-ID ends in `#r2` — the sender chose it —
        // must not hide the real message it names. Same for a calendar UID.
        apply(vec![from("mbox", "invoice-1234@bank.example"), from("ics", "meeting-77@corp.example")]);
        apply(vec![from("mbox", "invoice-1234@bank.example#r2"), from("ics", "meeting-77@corp.example#r9")]);
        assert!(!superseded("mbox", "invoice-1234@bank.example"));
        assert!(!superseded("ics", "meeting-77@corp.example"));
        // Our own revisioned sources still supersede.
        for (source, base) in [("photos", "photos#2026-07-20"), ("visits", "visit:2026-07-20:0"), ("contacts", "ABC-DEF")] {
            assert!(REVISIONED_SOURCES.contains(&source));
            apply(vec![from(source, base)]);
            apply(vec![from(source, &format!("{base}#r2"))]);
            assert!(superseded(source, base), "{source}");
            assert!(!superseded(source, &format!("{base}#r2")), "{source}");
        }
    }

    #[test]
    fn test_scope_inference() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");

        // A work-labeled identity: material authored through it is work.
        {
            let conn = store.lock();
            crate::store::graph::upsert_entity(
                &conn,
                &actor,
                "Person",
                "Jon Hale",
                "personal",
                &[("email".into(), "jon@harbor.example".into())],
            )
            .unwrap();
        }
        store
            .claim_identity(&actor, "Jon Hale", "email", "jon@harbor.example", Some("work"))
            .unwrap();

        // Draft with no scope, authored via the work identity.
        let mut d = draft("repo:s1");
        d.scope = None;
        d.entities[0].attributes = vec![("email".into(), "jon@harbor.example".into())];
        d.entities[1].name = "clientwork".into();
        d.entities[1].attributes = vec![("repo-path".into(), "/tmp/clientwork".into())];
        let batch = IngestBatch {
            actor: "adapter:git".into(),
            session_id: None,
            episodes: vec![d],
        };
        store.apply_batch(&home, &batch).unwrap();
        let episode = store
            .episode(&ids::episode_id("git", "repo:s1"))
            .unwrap()
            .unwrap();
        assert_eq!(episode.scope, "work");

        // Correct the project to personal; later scope-less ingests of the
        // same project inherit it — the project wins over the identity.
        store
            .set_entity_scope(&actor, "clientwork", "personal", true)
            .unwrap();
        let mut d2 = draft("repo:s2");
        d2.scope = None;
        d2.entities[0].attributes = vec![("email".into(), "jon@harbor.example".into())];
        d2.entities[1].name = "clientwork".into();
        d2.entities[1].attributes = vec![("repo-path".into(), "/tmp/clientwork".into())];
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:git".into(),
                    session_id: None,
                    episodes: vec![d2],
                },
            )
            .unwrap();
        let episode = store
            .episode(&ids::episode_id("git", "repo:s2"))
            .unwrap()
            .unwrap();
        assert_eq!(episode.scope, "personal");

        // No identity, no known project → falls back to personal.
        let mut d3 = draft("repo:s3");
        d3.scope = None;
        d3.entities[0].attributes = vec![("email".into(), "stranger@example.com".into())];
        d3.entities[0].name = "Stranger".into();
        d3.entities[1].name = "mystery".into();
        d3.entities[1].attributes = vec![("repo-path".into(), "/tmp/mystery".into())];
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:git".into(),
                    session_id: None,
                    episodes: vec![d3],
                },
            )
            .unwrap();
        let episode = store
            .episode(&ids::episode_id("git", "repo:s3"))
            .unwrap()
            .unwrap();
        assert_eq!(episode.scope, "personal");
    }

    #[test]
    fn test_apply_batch_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        let batch = IngestBatch {
            actor: "adapter:git".into(),
            session_id: None,
            episodes: vec![draft("repo:abc123"), draft("repo:def456")],
        };
        let report = store.apply_batch(&home, &batch).unwrap();
        assert_eq!(report.episodes_new, 2);
        assert_eq!(report.entities_created, 2); // person + project, once
        assert_eq!(report.edges_created, 1); // one contributes_to edge

        // Re-run: everything already there.
        let report = store.apply_batch(&home, &batch).unwrap();
        assert_eq!(report.episodes_new, 0);
        assert_eq!(report.episodes_existing, 2);
        assert_eq!(store.counts().unwrap().episodes, 2);

        // Raw content landed under the local event date.
        let episode_id = ids::episode_id("git", "repo:abc123");
        // Event is 2026-03-15 10:00 -04:00 → UTC 14:00, date 2026-03-15.
        assert!(tmp
            .path()
            .join(format!("raw/2026/03/15/{episode_id}.txt"))
            .exists());

        // Day view sees the commits with cited entities.
        let day = store.recall_day("2026-03-15").unwrap();
        assert_eq!(day.episodes.len(), 2);
        assert!(day.episodes[0]
            .entities
            .iter()
            .any(|(name, _, role)| name == "Jon Hale" && role == "author"));
        assert_eq!(day.valid_edges.len(), 1);

        // Search finds them; dossier cites them.
        assert!(!store.search("fix something", 10).unwrap().is_empty());
        let dossier = store.entity_dossier("Jon Hale").unwrap().unwrap();
        assert_eq!(dossier.episode_count, 2);
    }
}
