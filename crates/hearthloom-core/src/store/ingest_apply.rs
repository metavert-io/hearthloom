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

pub use super::growing::GROWING_SOURCES;

/// The record's own sources: the revisioned and growing ones (a `#r<n>`
/// ref hides the real record), the consolidation passes (the digest,
/// chapters), the owner's own writing (Desk, notes and the old Codi
/// harvest — derivation `declared`), accepted proposals, and Facebook
/// (its posts feed the location index). Only the owner's keys and peers
/// may ingest under these names (http.rs `ingest`).
pub const OWNER_ONLY_SOURCES: &[&str] = &[
    "photos",
    "visits",
    "contacts",
    "chrome",
    "imessage",
    "claude-code",
    "consolidation",
    "desk",
    "notes",
    "codi",
    "memory",
    "facebook",
];

/// Sources that keep the same conversation under the same id: a ChatGPT
/// conversation imported from the full export (`openai`) and from its
/// single `conversations.json` (`chatlog-chatgpt`).
const SAME_RECORD_SOURCES: &[&[&str]] = &[&["openai", "chatlog-chatgpt"]];

/// This record, and the copies other sources keep of it under the same id
/// (`SAME_RECORD_SOURCES`), as (source, source ref).
pub fn same_record(source: &str, source_ref: &str) -> Vec<(String, String)> {
    match SAME_RECORD_SOURCES
        .iter()
        .find(|group| group.contains(&source))
    {
        Some(group) => group
            .iter()
            .map(|s| (s.to_string(), source_ref.to_string()))
            .collect(),
        None => vec![(source.to_string(), source_ref.to_string())],
    }
}

impl Store {
    /// Apply a batch of episode drafts. Raw content is written under the
    /// home's raw dir before the row lands; idempotent on re-runs.
    pub fn apply_batch(&self, home: &Home, batch: &IngestBatch) -> Result<IngestReport> {
        self.apply_batch_from(home, batch, &[])
    }

    /// The owner's import read these drafts as private (an export's own
    /// "do not remember" flag, or private asked for the whole import): a
    /// copy of the same record already stored is labelled private too —
    /// `apply_batch` leaves a stored episode as it landed, so without this a
    /// conversation imported before it was flagged (or by a reader that
    /// didn't know the flag) stays outside the vault. Only the label moves,
    /// and only into the vault; the content is untouched. The label is
    /// stamped, so the scope leg carries it to the owner's other nodes.
    /// For the owner's own import paths only — never a peer's batch (the
    /// scope leg settles labels between nodes) or another key's. Returns
    /// the records relabelled.
    /// `flagged`: (source, source ref) of records the import reads as
    /// private whether or not it made a draft of them — a ChatGPT
    /// conversation flagged "do not remember" whose kept branch holds no
    /// turns gets no draft, yet a copy an earlier reader stored of it goes
    /// into the vault all the same.
    pub fn label_private_on_reimport(
        &self,
        actor: &Actor,
        drafts: &[crate::ingest::EpisodeDraft],
        flagged: &[(String, String)],
    ) -> Result<usize> {
        let mut refs: Vec<(String, String)> = drafts
            .iter()
            .filter(|d| d.scope.as_deref() == Some("private"))
            .map(|d| (d.source.as_str(), d.source_ref.as_str()))
            .chain(flagged.iter().map(|(s, r)| (s.as_str(), r.as_str())))
            .flat_map(|(source, source_ref)| same_record(source, source_ref))
            .collect();
        refs.sort_unstable();
        refs.dedup();
        if refs.is_empty() {
            return Ok(0);
        }
        let now = now_utc();
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let mut relabeled = Vec::new();
        let mut private_ids = Vec::new();
        for (source, source_ref) in &refs {
            // a day or session of a growing source: whichever copies of it
            // are stored (the draft's own ref, read before it was revised),
            // and through them the rest of the record
            let growing = super::growing::growing_copy(source, source_ref);
            let (base, copies) = match growing {
                Some((base, _)) => (base, super::growing::growing_revision_sql(2)),
                None => (source_ref.as_str(), "0".to_string()),
            };
            let mut stmt = tx.prepare(&format!(
                "UPDATE episodes SET scope = 'private', scope_changed_at = ?3 /* all-episodes: a private label holds on every copy */
                 WHERE source = ?1 AND (source_ref = ?2 OR source_ref = ?4 OR {copies})
                   AND scope != 'private'
                 RETURNING id"
            ))?;
            let changed: Vec<String> = stmt
                .query_map(rusqlite::params![source, base, now, source_ref], |r| {
                    r.get(0)
                })?
                .collect::<std::result::Result<_, _>>()?;
            if !changed.is_empty() {
                relabeled.push(format!("{source}:{source_ref}"));
            }
            if growing.is_some() {
                private_ids.extend(changed);
                // already private: its record still follows
                private_ids.push(ids::episode_id(source, source_ref));
            }
        }
        super::growing::spread_private(&tx, actor, &private_ids)?;
        if !relabeled.is_empty() {
            record_mutation(
                &tx,
                actor,
                "scope_private_on_reimport",
                &format!("scope_private_on_reimport:{now}:{}", relabeled.len()),
                &json!({"records": relabeled, "scope": "private"}),
            )?;
        }
        tx.commit()?;
        Ok(relabeled.len())
    }

    /// `apply_batch` with the sender's provenance for synced drafts
    /// (`Origin`s keyed by source + source_ref; see store/provenance.rs).
    /// Every newly landed episode gets its origin recorded in the same
    /// transaction, and its trust passes `trust_for`.
    pub fn apply_batch_from(
        &self,
        home: &Home,
        batch: &IngestBatch,
        origins: &[super::provenance::Origin],
    ) -> Result<IngestReport> {
        let origins: std::collections::HashMap<(&str, &str), &super::provenance::Origin> = origins
            .iter()
            .map(|o| ((o.source.as_str(), o.source_ref.as_str()), o))
            .collect();
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
        // Per retirement: the episodes it took as they arrived here, with
        // the copies of their records — what they seeded goes too.
        let mut retired_here: std::collections::HashMap<String, std::collections::HashSet<String>> =
            std::collections::HashMap::new();

        for draft in &batch.episodes {
            let episode_id = ids::episode_id(&draft.source, &draft.source_ref);

            // An episode already here keeps the text it landed with. Its
            // raw file is what peer sync, export and friend reads serve,
            // and its row, hash and index hold the first text — so a draft
            // sent again (or a second one in this batch) never touches it.
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM episodes /* all-episodes: an existence check */
                 WHERE id = ?1 OR (source = ?2 AND source_ref = ?3))",
                rusqlite::params![episode_id, draft.source, draft.source_ref],
                |r| r.get(0),
            )?;
            if exists {
                report.episodes_existing += 1;
                continue;
            }

            // A copy of a growing source's day or session: measured against
            // the copies already here (see store/growing.rs).
            let growing = super::growing::Arriving::read(
                &tx,
                home,
                &draft.source,
                &draft.source_ref,
                &draft.body,
            )?;

            // Scope resolution: explicit > the label of the record this is
            // a copy of > known Project entity's scope > author/organizer
            // identity scope > `personal`. This is how one human keeps
            // personal projects and client/company work apart.
            let mut scope = draft.scope.clone().or_else(|| {
                growing
                    .as_ref()
                    .and_then(|g| g.inherited_scope())
                    .map(String::from)
            });
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
            // A copy of a private record is private, whatever its sender
            // labelled it. A copy that lands private — for that reason, or
            // its draft's, a rule's, the label it inherited — is stamped
            // (when the record's label was given, else now), so the label
            // travels on the scope leg to a node that minted this copy, or
            // holds another copy of the record, under another label.
            let mut private_since: Option<String> = None;
            if let Some(g) = &growing {
                let record = g.private();
                if record.is_some() {
                    scope = "private".to_string();
                }
                if scope == "private" {
                    private_since =
                        Some(record.flatten().map(String::from).unwrap_or_else(now_utc));
                }
            }

            // Raw content file, dated by local event date (fallback: today,
            // also for an event time that doesn't open with a real date).
            let date = draft
                .event_time
                .utc
                .as_deref()
                .and_then(|t| t.get(..10))
                .filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok())
                .map(str::to_string)
                .unwrap_or_else(|| now_utc()[..10].to_string());
            let content_ref = home.write_raw(&episode_id, &date, &draft.body)?;

            // text others wrote never lands as the owner's own (v61)
            let trust = super::provenance::trust_for(&draft.source, &draft.trust);
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
                trust: trust.clone(),
                scope: scope.clone(),
                content_ref: Some(content_ref),
                content_hash: ids::content_hash(draft.body.as_bytes()),
                derivation: super::episodes::derivation_for(&draft.source, &trust).to_string(),
            };

            if !insert_episode(&tx, &row, actor.session_id.as_deref(), &draft.body)? {
                report.episodes_existing += 1;
                continue;
            }
            report.episodes_new += 1;
            super::provenance::record_arrival(
                &tx,
                &episode_id,
                &actor,
                origins
                    .get(&(draft.source.as_str(), draft.source_ref.as_str()))
                    .copied(),
            )?;
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
            // Who wrote this draft: the batch's actor here, or — synced —
            // the writer its sender recorded (provenance, v61).
            let writer: Option<String> = if actor.actor.starts_with("peer:") {
                origins
                    .get(&(draft.source.as_str(), draft.source_ref.as_str()))
                    .and_then(|o| o.writer.clone())
            } else {
                Some(actor.actor.clone())
            };
            // An ember revises what IT wrote, never anyone else's (0.5.82):
            // a `<base>#r<n>` from `plugin:<name>` supersedes the base and
            // lower revisions only where their recorded writer is the same.
            // Derived from the ref and the recorded origin, so every node
            // converges when the revision arrives by sync.
            if let Some(base) = (!REVISIONED_SOURCES.contains(&draft.source.as_str())
                && !GROWING_SOURCES.contains(&draft.source.as_str()))
            .then(|| super::provenance::plugin_revision(&draft.source_ref, writer.as_deref()))
            .flatten()
            {
                tx.execute(
                    &format!(
                        "UPDATE episodes SET superseded_by = ?1
                         WHERE source = ?2 AND superseded_by IS NULL AND id != ?1
                           AND {} AND {}",
                        super::provenance::base_or_revision_sql(3),
                        super::provenance::same_writer_sql(4)
                    ),
                    rusqlite::params![episode_id, draft.source, base, writer],
                )?;
                tx.execute(
                    "DELETE FROM location_points WHERE episode_id IN (
                         SELECT id FROM episodes WHERE superseded_by = ?1)",
                    [&episode_id],
                )?;
            }
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
            if let Some(since) = &private_since {
                tx.execute(
                    "UPDATE episodes SET scope_changed_at = ?1 WHERE id = ?2",
                    rusqlite::params![since, episode_id],
                )?;
            }
            // A growing source keeps the copy holding the most live, and a
            // copy that lands private makes the copies stored before private.
            if let Some(g) = &growing {
                g.settle(&tx, &episode_id)?;
                if scope == "private" {
                    super::growing::spread_private(&tx, &actor, std::slice::from_ref(&episode_id))?;
                }
            }
            let mut arrival = arrivals.covering(
                &tx,
                &episode_id,
                &draft.source,
                &draft.source_ref,
                writer.as_deref(),
            )?;
            // ...and a copy of a retired day or session lands retired: the
            // person retired the record, not one copy of it.
            if matches!(arrival, super::retire::Arrival::Live) {
                if let Some(rid) = growing
                    .as_ref()
                    .and_then(|g| g.retired_marks().find_map(|m| arrivals.live_retirement(m)))
                {
                    arrival = super::retire::Arrival::Retire(rid);
                }
            }
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
                let (Some(src), Some(dst)) = (entity_ids.get(edge.src), entity_ids.get(edge.dst))
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
                super::retire::Arrival::Retire(rid) => {
                    // the record is retired, not one copy of it: the
                    // copies here that aren't retired yet go with it
                    // (gathered first — retiring a copy replaces the mark
                    // that says which copy it stands behind)
                    let copies = match &growing {
                        Some(_) => super::growing::other_copies(&tx, &episode_id)?,
                        None => Vec::new(),
                    };
                    arrivals.retire(&tx, &rid, &episode_id)?;
                    for copy in &copies {
                        arrivals.retire(&tx, &rid, copy)?;
                    }
                    retired_here
                        .entry(rid)
                        .or_default()
                        .extend(std::iter::once(episode_id.clone()).chain(copies));
                }
                super::retire::Arrival::Behind(original) => {
                    tx.execute(
                        "UPDATE episodes SET superseded_by = ?1 WHERE id = ?2",
                        rusqlite::params![original, episode_id],
                    )?;
                }
            }
        }
        // What those episodes seeded here before they arrived — a cascade of
        // a copy held already, one that came ahead of its seed — is retired
        // with them (a cascade arriving after its seed already lands retired).
        for (rid, seeds) in &retired_here {
            arrivals.retire_derived(&tx, rid, seeds)?;
        }

        tx.commit()?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::super::growing::growing_copy;
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

    fn ember(source_ref: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: "minidash".into(),
            kind: "minidash_day".into(),
            title: format!("day {source_ref}"),
            trust: "imported".into(),
            entities: vec![],
            edges: vec![],
            ..draft(source_ref)
        }
    }

    fn apply_as(store: &Store, home: &Home, actor: &str, drafts: Vec<EpisodeDraft>) {
        store
            .apply_batch(
                home,
                &IngestBatch {
                    actor: actor.into(),
                    session_id: None,
                    episodes: drafts,
                },
            )
            .unwrap();
    }

    fn superseded_ref(store: &Store, source_ref: &str) -> Option<String> {
        store
            .lock()
            .query_row(
                "SELECT superseded_by FROM episodes WHERE source = 'minidash' AND source_ref = ?1",
                [source_ref],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A growing source keeps exactly one copy of a base live — the one
    /// holding the most — whatever order the copies arrive in, and a
    /// revision touches only its own base.
    #[test]
    fn test_a_growing_source_keeps_its_largest_copy_in_any_order() {
        let day = |source_ref: &str| EpisodeDraft {
            source: "imessage".into(),
            kind: "messages".into(),
            title: format!("day {source_ref}"),
            body: format!("messages of {source_ref}"),
            trust: "self".into(),
            entities: vec![],
            edges: vec![],
            ..draft(source_ref)
        };
        let live = |store: &Store| -> Vec<String> {
            let conn = store.lock();
            let mut stmt = conn
                .prepare(
                    "SELECT source_ref FROM episodes WHERE superseded_by IS NULL ORDER BY source_ref",
                )
                .unwrap();
            let rows = stmt.query_map([], |r| r.get(0)).unwrap();
            rows.collect::<std::result::Result<_, _>>().unwrap()
        };
        let base = "+15551234567#2026-03-15";
        let copies = [
            base.to_string(),
            format!("{base}#r3"),
            format!("{base}#r12"),
        ];
        let orders: [[usize; 3]; 4] = [[0, 1, 2], [2, 1, 0], [1, 2, 0], [0, 2, 1]];
        for order in orders {
            let tmp = tempfile::tempdir().unwrap();
            let home = Home::at(tmp.path());
            home.init().unwrap();
            let store = Store::open_in_memory().unwrap();
            // a neighbour whose name extends this base, and one in a
            // pattern's wildcard position, are other records
            apply_as(
                &store,
                &home,
                "adapter:imessage",
                vec![
                    day("+15551234567#2026-03-15x"),
                    day("+1555123456_#2026-03-15"),
                ],
            );
            for i in order {
                apply_as(&store, &home, "adapter:imessage", vec![day(&copies[i])]);
            }
            let mut expected = vec![
                "+1555123456_#2026-03-15".to_string(),
                format!("{base}#r12"),
                "+15551234567#2026-03-15x".to_string(),
            ];
            expected.sort();
            assert_eq!(live(&store), expected, "{order:?}");
            let latest = store
                .growing_latest(&home, "imessage", base)
                .unwrap()
                .unwrap();
            assert_eq!(latest.source_ref, format!("{base}#r12"));
            assert_eq!(latest.items, 12);
            assert_eq!(
                latest.body(&home).as_deref(),
                Some(&*format!("messages of {base}#r12"))
            );
        }
        // the same suffix on a source that doesn't grow names another record
        assert_eq!(growing_copy("mbox", "a@b#r2"), None);
        assert_eq!(
            growing_copy("chrome", "Work#2026-03-15"),
            Some(("Work#2026-03-15", 0))
        );
        assert_eq!(
            growing_copy("chrome", "Work#2026-03-15#r7"),
            Some(("Work#2026-03-15", 7))
        );
        assert_eq!(
            growing_copy("chrome", "Work#2026-03-15#rx"),
            Some(("Work#2026-03-15#rx", 0))
        );
        // only the owner and peers write under a name whose refs supersede
        for source in GROWING_SOURCES {
            assert!(OWNER_ONLY_SOURCES.contains(source), "{source}");
        }
    }

    /// A chat-day of `n` messages, as the iMessage adapter writes it.
    fn chat_day(source_ref: &str, n: usize) -> EpisodeDraft {
        let mut body = format!("messages: {n}\nchat: +15551234567\ndate: 2026-03-15\n\n");
        for i in 0..n {
            body.push_str(&format!("[10:{:02}] +15551234567: message {i}\n", i % 60));
        }
        EpisodeDraft {
            source: "imessage".into(),
            kind: "messages".into(),
            title: "Messages with +15551234567".into(),
            body,
            trust: "self".into(),
            scope: None,
            entities: vec![],
            edges: vec![],
            ..draft(source_ref)
        }
    }

    fn live_refs(store: &Store, source: &str) -> Vec<String> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare(
                "SELECT source_ref FROM episodes
                 WHERE source = ?1 AND superseded_by IS NULL ORDER BY source_ref",
            )
            .unwrap();
        let rows = stmt.query_map([source], |r| r.get(0)).unwrap();
        rows.collect::<std::result::Result<_, _>>().unwrap()
    }

    /// The unversioned copy counts what its text holds: a smaller copy
    /// another node read (its own base held less) never hides a fuller one
    /// stored here, and a fuller one still replaces it.
    #[test]
    fn test_a_smaller_copy_from_a_peer_never_hides_a_fuller_base() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let base = "+15551234567#2026-03-15";
        // this Mac read 100 messages of the day; the other Mac held 2 and
        // then read 3
        apply_as(&store, &home, "adapter:imessage", vec![chat_day(base, 100)]);
        apply_as(
            &store,
            &home,
            "peer:laptop",
            vec![chat_day(&format!("{base}#r3"), 3)],
        );
        assert_eq!(live_refs(&store, "imessage"), [base]);
        let latest = store
            .growing_latest(&home, "imessage", base)
            .unwrap()
            .unwrap();
        assert_eq!((latest.source_ref.as_str(), latest.items), (base, 100));
        // a copy holding more than this one's 100 replaces it
        apply_as(
            &store,
            &home,
            "peer:laptop",
            vec![chat_day(&format!("{base}#r101"), 101)],
        );
        assert_eq!(live_refs(&store, "imessage"), [format!("{base}#r101")]);
    }

    /// A fuller copy of a day the owner made private lands private — on
    /// this node and, through the scope leg, on a node that holds it under
    /// another label — and a day the owner later took out of the vault
    /// stays out.
    #[test]
    fn test_a_fuller_copy_of_a_private_day_stays_private() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let base = "+15551234567#2026-03-15";
        let scope_of = |source_ref: &str| -> (String, Option<String>) {
            store
                .lock()
                .query_row(
                    "SELECT scope, scope_changed_at FROM episodes WHERE source_ref = ?1",
                    [source_ref],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        let actor = Actor::new("human:test");
        apply_as(&store, &home, "adapter:imessage", vec![chat_day(base, 2)]);
        let first = ids::episode_id("imessage", base);
        store
            .set_episode_scopes(&actor, std::slice::from_ref(&first), "private")
            .unwrap();
        // read again, fuller, with no label of its own
        apply_as(
            &store,
            &home,
            "adapter:imessage",
            vec![chat_day(&format!("{base}#r5"), 5)],
        );
        assert_eq!(scope_of(&format!("{base}#r5")).0, "private");
        // a copy a peer sent labelled personal is private here too, and
        // stamped so the scope leg tells the peer
        let mut sent = chat_day(&format!("{base}#r9"), 9);
        sent.scope = Some("personal".into());
        apply_as(&store, &home, "peer:laptop", vec![sent]);
        let (scope, stamp) = scope_of(&format!("{base}#r9"));
        assert_eq!(scope, "private");
        assert!(stamp.is_some());
        let r9 = ids::episode_id("imessage", &format!("{base}#r9"));
        let changes = store.scope_changes_since("").unwrap();
        assert!(changes
            .iter()
            .any(|(id, scope, _)| *id == r9 && scope == "private"));

        // taken out of the vault since: the next copy follows
        std::thread::sleep(std::time::Duration::from_millis(1100));
        store.vault_unlock(10);
        store
            .set_episode_scopes(&actor, std::slice::from_ref(&r9), "work")
            .unwrap();
        apply_as(
            &store,
            &home,
            "adapter:imessage",
            vec![chat_day(&format!("{base}#r12"), 12)],
        );
        assert_eq!(scope_of(&format!("{base}#r12")).0, "work");
    }

    /// Scope changes one node sends, applied on another (the scope leg).
    fn exchange_scopes(from: &Store, to: &Store) -> usize {
        let changes = from.scope_changes_since("").unwrap();
        to.apply_scope_changes(&Actor::new("peer:other"), &changes)
            .unwrap()
    }

    fn scope_at(store: &Store, source_ref: &str) -> String {
        store
            .lock()
            .query_row(
                "SELECT scope FROM episodes WHERE source = 'imessage' AND source_ref = ?1",
                [source_ref],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Two nodes that each read the same fuller copy of a day — one holding
    /// the day private, the other not — end with every copy private on both
    /// once they exchange scope changes, also when the private label was
    /// stored without a stamp (an earlier version's private import).
    #[test]
    fn test_copies_read_on_two_nodes_of_a_private_day_end_private_on_both() {
        let tmp = tempfile::tempdir().unwrap();
        let (a_home, b_home) = (
            Home::at(tmp.path().join("a")),
            Home::at(tmp.path().join("b")),
        );
        a_home.init().unwrap();
        b_home.init().unwrap();
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        let base = "+15551234567#2026-03-15";
        let fuller = format!("{base}#r5");
        let mut first = chat_day(base, 2);
        first.scope = Some("private".into());
        apply_as(&a, &a_home, "adapter:imessage", vec![first]);
        a.lock()
            .execute("UPDATE episodes SET scope_changed_at = NULL", [])
            .unwrap();
        apply_as(&b, &b_home, "adapter:imessage", vec![chat_day(base, 2)]);
        // each node reads the day again, fuller, itself
        apply_as(&a, &a_home, "adapter:imessage", vec![chat_day(&fuller, 5)]);
        apply_as(&b, &b_home, "adapter:imessage", vec![chat_day(&fuller, 5)]);
        assert_eq!(scope_at(&a, &fuller), "private");
        assert_eq!(scope_at(&b, &fuller), "personal");

        exchange_scopes(&a, &b);
        exchange_scopes(&b, &a);
        for (store, node) in [(&a, "a"), (&b, "b")] {
            for source_ref in [base, fuller.as_str()] {
                assert_eq!(
                    scope_at(store, source_ref),
                    "private",
                    "{node} {source_ref}"
                );
            }
        }
    }

    /// The latest label given to a day holds on every copy: a private label
    /// a peer sends with a newer stamp, for a copy private here already,
    /// reaches the copy taken out of the vault in between; and a copy
    /// private here under an older stamp takes the newer one, so an older
    /// label sent for that copy alone leaves it in the vault.
    #[test]
    fn test_a_newer_private_label_holds_on_every_copy_already_private_or_not() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let base = "+15551234567#2026-03-15";
        let (r2, r5) = (format!("{base}#r2"), format!("{base}#r5"));
        apply_as(
            &store,
            &home,
            "adapter:imessage",
            vec![chat_day(&r2, 2), chat_day(&r5, 5)],
        );
        let label = |source_ref: &str, scope: &str, at: &str| {
            store
                .lock()
                .execute(
                    "UPDATE episodes SET scope = ?2, scope_changed_at = ?3 WHERE source_ref = ?1",
                    [source_ref, scope, at],
                )
                .unwrap();
        };
        let sent = |source_ref: &str, scope: &str, at: &str| {
            store
                .apply_scope_changes(
                    &Actor::new("peer:laptop"),
                    &[(
                        ids::episode_id("imessage", source_ref),
                        scope.to_string(),
                        at.to_string(),
                    )],
                )
                .unwrap()
        };

        // r2 vaulted at 10:00, r5 taken out at 11:00; r2 vaulted at 12:00
        // on the peer
        label(&r2, "private", "2026-03-16T10:00:00Z");
        label(&r5, "personal", "2026-03-16T11:00:00Z");
        sent(&r2, "private", "2026-03-16T12:00:00Z");
        assert_eq!(scope_at(&store, &r5), "private");

        // both private, r5 under the older stamp: the record's 12:00 holds
        // against r5's 11:00 label
        label(&r2, "private", "2026-03-16T10:00:00Z");
        label(&r5, "private", "2026-03-16T09:00:00Z");
        sent(&r2, "private", "2026-03-16T12:00:00Z");
        sent(&r5, "personal", "2026-03-16T11:00:00Z");
        assert_eq!(scope_at(&store, &r5), "private");
        // sent again, nothing changes
        assert_eq!(sent(&r2, "private", "2026-03-16T12:00:00Z"), 0);
    }

    /// Vaulting a day on a node whose fullest copy is its first read
    /// reaches the node that first read less: the later copy it shows, and
    /// one only it holds, become private once the nodes exchange scope
    /// changes — and exchanging again changes nothing.
    #[test]
    fn test_vaulting_a_day_reaches_the_copy_another_node_shows() {
        let tmp = tempfile::tempdir().unwrap();
        let (a_home, b_home) = (
            Home::at(tmp.path().join("a")),
            Home::at(tmp.path().join("b")),
        );
        a_home.init().unwrap();
        b_home.init().unwrap();
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        let base = "+15551234567#2026-03-15";
        let (r5, r7) = (format!("{base}#r5"), format!("{base}#r7"));
        // A read 100 messages first; B read 2, then 5, then 7
        apply_as(&a, &a_home, "adapter:imessage", vec![chat_day(base, 100)]);
        apply_as(&b, &b_home, "adapter:imessage", vec![chat_day(base, 2)]);
        apply_as(&b, &b_home, "adapter:imessage", vec![chat_day(&r5, 5)]);
        apply_as(&a, &a_home, "peer:b", vec![chat_day(&r5, 5)]);
        apply_as(&b, &b_home, "adapter:imessage", vec![chat_day(&r7, 7)]);
        assert_eq!(live_refs(&a, "imessage"), [base]);
        assert_eq!(live_refs(&b, "imessage"), std::slice::from_ref(&r7));

        a.set_episode_scopes(
            &Actor::new("human:test"),
            &[ids::episode_id("imessage", base)],
            "private",
        )
        .unwrap();
        assert_eq!(scope_at(&a, &r5), "private");
        exchange_scopes(&a, &b);
        for source_ref in [base, r5.as_str(), r7.as_str()] {
            assert_eq!(scope_at(&b, source_ref), "private", "{source_ref}");
        }
        exchange_scopes(&b, &a);
        assert_eq!(exchange_scopes(&a, &b), 0);
        assert_eq!(exchange_scopes(&b, &a), 0);
    }

    #[test]
    fn test_an_ember_revises_only_what_it_wrote_itself_and_peers_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let mini_home = Home::at(tmp.path().join("mini"));
        let cloud_home = Home::at(tmp.path().join("cloud"));
        mini_home.init().unwrap();
        cloud_home.init().unwrap();
        let mini = Store::open(&mini_home.db_path()).unwrap();
        let cloud = Store::open(&cloud_home.db_path()).unwrap();
        let sync = |after: i64| -> i64 {
            let page = mini
                .sync_export_for(&mini_home, after, 100, Some("cloud"))
                .unwrap();
            cloud
                .apply_batch_from(
                    &cloud_home,
                    &IngestBatch {
                        actor: "peer:mini".into(),
                        session_id: None,
                        episodes: page.episodes,
                    },
                    &page.origins,
                )
                .unwrap();
            page.next_cursor
        };

        // the ember's own days, and others' writing under the same source
        apply_as(
            &mini,
            &mini_home,
            "plugin:minidash",
            vec![ember("h:day:1"), ember("h:day:2"), ember("x_y")],
        );
        apply_as(&mini, &mini_home, "plugin:other", vec![ember("h:other:1")]);
        apply_as(
            &mini,
            &mini_home,
            "human:dashboard",
            vec![ember("h:mine:1")],
        );
        apply_as(&mini, &mini_home, "plugin:minidash", vec![ember("xzy")]);
        let cursor = sync(0);

        // its revision of its own day; its "revisions" of others' writing;
        // another ember's revision of its day; a LIKE-shaped base
        apply_as(
            &mini,
            &mini_home,
            "plugin:minidash",
            vec![ember("h:day:1#r2")],
        );
        apply_as(
            &mini,
            &mini_home,
            "plugin:minidash",
            vec![ember("h:other:1#r2"), ember("h:mine:1#r2")],
        );
        apply_as(&mini, &mini_home, "plugin:other", vec![ember("h:day:2#r2")]);
        apply_as(&mini, &mini_home, "plugin:minidash", vec![ember("x_y#r2")]);
        sync(cursor);

        for (store, node) in [
            (&mini, "the ingesting node"),
            (&cloud, "the peer, after sync"),
        ] {
            let revision = crate::ids::episode_id("minidash", "h:day:1#r2");
            assert_eq!(superseded_ref(store, "h:day:1"), Some(revision), "{node}");
            assert_eq!(superseded_ref(store, "h:day:1#r2"), None, "{node}");
            assert_eq!(
                superseded_ref(store, "h:other:1"),
                None,
                "{node}: another ember's"
            );
            assert_eq!(
                superseded_ref(store, "h:mine:1"),
                None,
                "{node}: the owner's"
            );
            assert_eq!(
                superseded_ref(store, "h:day:2"),
                None,
                "{node}: a different ember's revision"
            );
            assert!(superseded_ref(store, "x_y").is_some(), "{node}");
            assert_eq!(
                superseded_ref(store, "xzy"),
                None,
                "{node}: matched exactly, never LIKE"
            );
        }

        // a peer that doesn't say who wrote a revision supersedes nothing
        apply_as(&cloud, &cloud_home, "peer:mini", vec![ember("xzy#r2")]);
        assert_eq!(
            superseded_ref(&cloud, "xzy"),
            None,
            "an unknown writer fails closed"
        );
    }

    #[test]
    fn test_an_embers_revision_of_a_retired_memory_stays_retired() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        apply_as(&store, &home, "plugin:minidash", vec![ember("h:day:1")]);
        let first = crate::ids::episode_id("minidash", "h:day:1");
        let r = store
            .retire(&Actor::new("human:test"), "memory", &first, &[], "")
            .unwrap();
        store.retirement_apply(&r.id).unwrap();
        apply_as(&store, &home, "plugin:minidash", vec![ember("h:day:1#r2")]);
        let retired = Some(format!("retired:{}", r.id));
        assert_eq!(
            superseded_ref(&store, "h:day:1"),
            retired,
            "the retired copy keeps its mark"
        );
        assert_eq!(
            superseded_ref(&store, "h:day:1#r2"),
            retired,
            "the revision lands retired"
        );
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
                .apply_batch(
                    &home,
                    &IngestBatch {
                        actor: "adapter:test".into(),
                        session_id: None,
                        episodes: drafts,
                    },
                )
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
        apply(vec![
            from("mbox", "invoice-1234@bank.example"),
            from("ics", "meeting-77@corp.example"),
        ]);
        apply(vec![
            from("mbox", "invoice-1234@bank.example#r2"),
            from("ics", "meeting-77@corp.example#r9"),
        ]);
        assert!(!superseded("mbox", "invoice-1234@bank.example"));
        assert!(!superseded("ics", "meeting-77@corp.example"));
        // Nor may an ember: its own-revision rule never reaches what an
        // importer wrote, under any source name (0.5.82).
        apply_as(
            &store,
            &home,
            "plugin:minidash",
            vec![
                from("mbox", "invoice-1234@bank.example#r3"),
                from("ics", "meeting-77@corp.example#r4"),
            ],
        );
        assert!(!superseded("mbox", "invoice-1234@bank.example"));
        assert!(!superseded("mbox", "invoice-1234@bank.example#r2"));
        assert!(!superseded("ics", "meeting-77@corp.example"));
        // Our own revisioned sources still supersede.
        for (source, base) in [
            ("photos", "photos#2026-07-20"),
            ("visits", "visit:2026-07-20:0"),
            ("contacts", "ABC-DEF"),
        ] {
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
            .claim_identity(
                &actor,
                "Jon Hale",
                "email",
                "jon@harbor.example",
                Some("work"),
            )
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

    /// An episode keeps the text it landed with: a later draft under the
    /// same source and ref — in another batch or later in the same one —
    /// leaves its raw file byte-identical.
    #[test]
    fn test_a_draft_sent_again_leaves_the_stored_text_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let with_body = |body: &str| EpisodeDraft {
            body: body.into(),
            ..draft("repo:abc123")
        };
        apply_as(&store, &home, "adapter:git", vec![with_body("first text")]);
        let id = ids::episode_id("git", "repo:abc123");
        let raw = tmp.path().join(format!("raw/2026/03/15/{id}.txt"));
        assert_eq!(std::fs::read_to_string(&raw).unwrap(), "first text");

        let report = store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:git".into(),
                    session_id: None,
                    episodes: vec![with_body("second text"), with_body("third text")],
                },
            )
            .unwrap();
        assert_eq!((report.episodes_new, report.episodes_existing), (0, 2));
        assert_eq!(std::fs::read_to_string(&raw).unwrap(), "first text");

        // two drafts for one new episode in one batch: the first one lands
        let report = store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:git".into(),
                    session_id: None,
                    episodes: vec![
                        EpisodeDraft {
                            body: "kept".into(),
                            ..draft("repo:def456")
                        },
                        EpisodeDraft {
                            body: "dropped".into(),
                            ..draft("repo:def456")
                        },
                    ],
                },
            )
            .unwrap();
        assert_eq!((report.episodes_new, report.episodes_existing), (1, 1));
        let id = ids::episode_id("git", "repo:def456");
        let raw = tmp.path().join(format!("raw/2026/03/15/{id}.txt"));
        assert_eq!(std::fs::read_to_string(&raw).unwrap(), "kept");
    }

    /// An event time that doesn't open with a date files its raw text
    /// under today instead of failing the batch.
    #[test]
    fn test_an_event_time_without_a_date_files_under_today() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&home.db_path()).unwrap();
        let odd = |source_ref: &str, utc: &str| EpisodeDraft {
            event_time: EventTime {
                utc: Some(utc.into()),
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Exact,
            },
            ..draft(source_ref)
        };
        let day = || now_utc()[..10].replace('-', "/");
        let before = day();
        let report = store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:git".into(),
                    session_id: None,
                    episodes: vec![
                        odd("repo:short", "2026"),
                        odd("repo:wide", "2026-03-1é5T00:00:00Z"),
                        odd("repo:nodate", "yesterday afternoon"),
                    ],
                },
            )
            .unwrap();
        assert_eq!(report.episodes_new, 3);
        let after = day();
        for source_ref in ["repo:short", "repo:wide", "repo:nodate"] {
            let id = ids::episode_id("git", source_ref);
            assert!(
                [&before, &after]
                    .iter()
                    .any(|d| tmp.path().join(format!("raw/{d}/{id}.txt")).is_file()),
                "{source_ref}"
            );
        }
    }
}
