//! Provenance (v61, 0.5.78): where each episode first entered one of the
//! owner's nodes — who wrote it there, when, and for which agent task —
//! and the route it took between the owner's nodes to reach this one.
//!
//! Every node records an episode's origin the moment the episode first
//! lands (`record_arrival`, inside `apply_batch`'s transaction). A local
//! write is its own origin; a synced one carries the sender's origin on
//! the wire (`Origin`, keyed by source + source_ref, sent beside the
//! drafts so no draft literal changes) and this node adds itself to the
//! route. The origin is the same on every node; the route is each node's
//! own path. No separate sync leg: it travels inside the episodes leg
//! (the written exclusion the peer-sync law asks for — see the design
//! log 2026-11-03).
//!
//! Episodes recorded before v61 have no row: `episode_origin` falls back
//! to what the episode row itself says — a local write names its writer;
//! one that arrived by sync says only which node sent it (no guessing).
//!
//! Trust follows the origin's source: text from a source that other
//! people or other software wrote never lands as the owner's own words
//! (`trust_for`; the v61 migration relabels what came before).
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{Actor, Store};
use crate::time::now_utc;

/// Sources whose text other people (or other software) wrote: message
/// threads, web page titles, screen captures, chat assistants' replies,
/// coding sessions (they hold fetched pages and tool output), calendar
/// invitations, scanned documents, media titles. Their episodes are
/// `imported`, never `self`. Git is NOT here yet: a commit is the owner's
/// or a collaborator's, which needs an author check against the owner's
/// claimed identities (a known follow-up).
pub const OTHERS_WORDS_SOURCES: &[&str] = &[
    "imessage",
    "chrome",
    "rewind",
    "openai",
    "claude-code",
    "ics",
    "ios-scan",
    "ios-media",
];

/// Source families matched by prefix (`chatlog-claude`, `chatlog-chatgpt`).
pub const OTHERS_WORDS_PREFIXES: &[&str] = &["chatlog-"];

pub fn others_words(source: &str) -> bool {
    OTHERS_WORDS_SOURCES.contains(&source)
        || OTHERS_WORDS_PREFIXES.iter().any(|p| source.starts_with(p))
}

/// The trust an arriving episode is stored with: a `self` claim from a
/// source whose text others wrote becomes `imported`. Applied at the one
/// door every episode passes (`apply_batch`), so importers, live ingest
/// and episodes synced from an older node all agree.
pub fn trust_for(source: &str, trust: &str) -> String {
    if trust == "self" && others_words(source) {
        "imported".into()
    } else {
        trust.into()
    }
}

/// The v61 relabel's WHERE clause (and the test's): `self` rows whose
/// source others wrote.
pub(crate) fn others_words_sql() -> String {
    let list = OTHERS_WORDS_SOURCES
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let prefixes = OTHERS_WORDS_PREFIXES
        .iter()
        .map(|p| format!(" OR source LIKE '{p}%'"))
        .collect::<String>();
    format!("trust = 'self' AND (source IN ({list}){prefixes})")
}

/// One node an episode passed through. `node` is a node id (names are
/// looked up in the synced node list when shown); a hop recorded before
/// v61 may carry only the name a peer was known by.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Hop {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// Where an episode came from. On the wire it is keyed by
/// `source` + `source_ref` (the episode's identity on every node).
/// `writer: None` means not known: it arrived before origins were kept.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Origin {
    pub source: String,
    pub source_ref: String,
    /// The actor that first wrote it (`adapter:imessage`,
    /// `agent:key:desk-hand`, `human:device:phone`, `human:dashboard`…).
    #[serde(default)]
    pub writer: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    /// The node it was first written on, and when.
    #[serde(default)]
    pub node: Option<String>,
    #[serde(default)]
    pub at: Option<String>,
    /// The agent task that produced it, when an agent did.
    #[serde(default)]
    pub task: Option<String>,
    /// Who let it in, when someone else wrote it (an agent's proposal
    /// accepted on the Tend page).
    #[serde(default)]
    pub accepted_by: Option<String>,
    /// Every one of the owner's nodes it passed through, first to last.
    #[serde(default)]
    pub route: Vec<Hop>,
    /// False when this node has no record of it (it predates v61) and
    /// the origin above was read off the episode row.
    #[serde(default = "yes")]
    pub recorded: bool,
}

fn yes() -> bool {
    true
}

/// An ember's own revision (0.5.82): a `<base>#r<n>` ref written by
/// `plugin:<name>` revises — supersedes — only episodes that same writer
/// wrote. Returns the base when the ref is a revision and the writer is a
/// plugin. (The record's own revisioned sources — photos, visits,
/// contacts, trips — keep their writer-agnostic rule in `ingest_apply`.)
pub(crate) fn plugin_revision<'a>(source_ref: &'a str, writer: Option<&str>) -> Option<&'a str> {
    let (base, rev) = source_ref.rsplit_once("#r")?;
    (!base.is_empty()
        && !rev.is_empty()
        && rev.chars().all(|c| c.is_ascii_digit())
        && writer.is_some_and(|w| w.starts_with("plugin:")))
    .then_some(base)
}

/// SQL: the `episodes` row in scope was first written by `?{n}` — its
/// recorded origin says so, or (written here before v61, so unrecorded)
/// its own actor does. A peer-sent row with no recorded origin matches
/// nothing: fail closed.
pub(crate) fn same_writer_sql(n: usize) -> String {
    format!(
        "(id IN (SELECT episode_id FROM episode_origin WHERE writer = ?{n})
          OR (id NOT IN (SELECT episode_id FROM episode_origin) AND actor = ?{n}))"
    )
}

/// SQL: the row's ref is `?{n}` itself or one of its revisions — matched
/// exactly, never with LIKE (an ember-chosen `_` or `%` must not widen it).
pub(crate) fn base_or_revision_sql(n: usize) -> String {
    format!("(source_ref = ?{n} OR substr(source_ref, 1, length(?{n}) + 2) = ?{n} || '#r')")
}

/// This node's id — the one the node list knows it by. Created on first
/// use (the daemon's `agents::node_id` reads it through here).
pub(crate) fn node_id_on(conn: &Connection) -> Result<String> {
    if let Some(id) = conn
        .query_row(
            "SELECT value FROM consolidation_state WHERE key = 'node.id'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .filter(|id| !id.is_empty())
    {
        return Ok(id);
    }
    let id = format!("nd_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
    conn.execute(
        "INSERT OR REPLACE INTO consolidation_state (key, value) VALUES ('node.id', ?1)",
        [&id],
    )?;
    Ok(id)
}

/// Record where a newly landed episode came from (first write wins: an
/// episode's origin is never rewritten by a later arrival).
pub(crate) fn record_arrival(
    conn: &Connection,
    episode_id: &str,
    actor: &Actor,
    wire: Option<&Origin>,
) -> Result<()> {
    let me = node_id_on(conn)?;
    let now = now_utc();
    let here = Hop {
        node: Some(me.clone()),
        name: None,
        at: Some(now.clone()),
    };
    let origin = match actor.actor.strip_prefix("peer:") {
        Some(peer) => match wire {
            Some(o) => Origin {
                route: o.route.iter().cloned().chain([here]).collect(),
                recorded: true,
                ..o.clone()
            },
            // a sender too old to say where it came from
            None => Origin {
                route: vec![
                    Hop {
                        node: None,
                        name: Some(peer.to_string()),
                        at: None,
                    },
                    here,
                ],
                ..Default::default()
            },
        },
        None => Origin {
            writer: Some(actor.actor.clone()),
            session: actor.session_id.clone(),
            node: Some(me),
            at: Some(now.clone()),
            route: vec![here],
            ..Default::default()
        },
    };
    conn.execute(
        "INSERT OR IGNORE INTO episode_origin
             (episode_id, writer, session, node, at, task, accepted_by, route, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            episode_id,
            origin.writer,
            origin.session,
            origin.node,
            origin.at,
            origin.task,
            origin.accepted_by,
            serde_json::to_string(&origin.route)?,
            now,
        ],
    )?;
    Ok(())
}

/// The origin of one episode on this node, from its record, or — for an
/// episode that predates v61 — read off the episode row: a local write
/// names its writer here; a synced one only the node that sent it.
pub(crate) fn origin_on(conn: &Connection, episode_id: &str) -> Result<Option<Origin>> {
    let row = conn
        .query_row(
            "SELECT source, source_ref, actor, session_id, ingest_time
             FROM episodes /* all-episodes: provenance answers for any episode, retired included */
             WHERE id = ?1",
            [episode_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((source, source_ref, actor, session, ingest_time)) = row else {
        return Ok(None);
    };
    let stored = conn
        .query_row(
            "SELECT writer, session, node, at, task, accepted_by, route
             FROM episode_origin WHERE episode_id = ?1",
            [episode_id],
            |r| {
                Ok(Origin {
                    source: source.clone(),
                    source_ref: source_ref.clone(),
                    writer: r.get(0)?,
                    session: r.get(1)?,
                    node: r.get(2)?,
                    at: r.get(3)?,
                    task: r.get(4)?,
                    accepted_by: r.get(5)?,
                    route: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                    recorded: true,
                })
            },
        )
        .optional()?;
    if stored.is_some() {
        return Ok(stored);
    }
    let me = node_id_on(conn)?;
    let here = Hop {
        node: Some(me.clone()),
        name: None,
        at: Some(ingest_time.clone()),
    };
    Ok(Some(match actor.strip_prefix("peer:") {
        Some(peer) => Origin {
            source,
            source_ref,
            route: vec![
                Hop {
                    node: None,
                    name: Some(peer.to_string()),
                    at: None,
                },
                here,
            ],
            recorded: false,
            ..Default::default()
        },
        None => Origin {
            source,
            source_ref,
            writer: Some(actor),
            session,
            node: Some(me),
            at: Some(ingest_time),
            route: vec![here],
            recorded: false,
            ..Default::default()
        },
    }))
}

impl Store {
    /// This node's id (see `node_id_on`).
    pub fn this_node_id(&self) -> Result<String> {
        node_id_on(&self.lock())
    }

    /// Where an episode came from and how it reached this node.
    pub fn episode_origin(&self, episode_id: &str) -> Result<Option<Origin>> {
        origin_on(&self.lock(), episode_id)
    }

    /// Fill in who really wrote an episode the batch's actor only let in
    /// (an agent's proposal, accepted by the owner) and the task that
    /// produced it. Only on this node's own record of a local arrival.
    pub fn set_origin_writer(
        &self,
        episode_id: &str,
        writer: &str,
        accepted_by: Option<&str>,
        task: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE episode_origin SET writer = ?2, accepted_by = COALESCE(?3, accepted_by),
                 task = COALESCE(?4, task)
             WHERE episode_id = ?1",
            rusqlite::params![episode_id, writer, accepted_by, task],
        )?;
        Ok(())
    }

    /// Name the agent task that produced an episode.
    pub fn set_origin_task(&self, episode_id: &str, task: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE episode_origin SET task = ?2 WHERE episode_id = ?1",
            [episode_id, task],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::time::{EventTime, TimeUncertainty};

    fn draft(source: &str, source_ref: &str, trust: &str) -> EpisodeDraft {
        EpisodeDraft {
            source: source.into(),
            source_ref: source_ref.into(),
            kind: "note".into(),
            title: source_ref.into(),
            body: "words".into(),
            event_time: EventTime::parse_rfc3339("2026-11-01T00:00:00Z", TimeUncertainty::Exact)
                .unwrap(),
            trust: trust.into(),
            scope: Some("personal".into()),
            entities: vec![],
            edges: vec![],
        }
    }

    #[test]
    fn test_others_words_never_land_as_the_owners() {
        assert_eq!(trust_for("imessage", "self"), "imported");
        assert_eq!(trust_for("chatlog-claude", "self"), "imported");
        assert_eq!(trust_for("ics", "self"), "imported");
        assert_eq!(trust_for("desk", "self"), "self");
        assert_eq!(trust_for("substack", "self"), "self");
        assert_eq!(
            trust_for("imessage", "agent"),
            "agent",
            "only a self claim moves"
        );
        assert!(others_words_sql().contains("source LIKE 'chatlog-%'"));
    }

    #[test]
    fn test_an_origin_follows_an_episode_across_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        let laptop_home = crate::paths::Home::at(tmp.path().join("laptop"));
        let hub_home = crate::paths::Home::at(tmp.path().join("hub"));
        let spoke_home = crate::paths::Home::at(tmp.path().join("spoke"));
        for h in [&laptop_home, &hub_home, &spoke_home] {
            h.init().unwrap();
        }
        let laptop = Store::open(&laptop_home.db_path()).unwrap();
        let hub = Store::open(&hub_home.db_path()).unwrap();
        let spoke = Store::open(&spoke_home.db_path()).unwrap();

        // written on the laptop by the iMessage import, claiming self
        laptop
            .apply_batch(
                &laptop_home,
                &IngestBatch {
                    actor: "adapter:imessage".into(),
                    session_id: Some("run-1".into()),
                    episodes: vec![draft("imessage", "chat:7", "self")],
                },
            )
            .unwrap();
        let id = crate::ids::episode_id("imessage", "chat:7");
        let at_laptop = laptop.episode_origin(&id).unwrap().unwrap();
        assert_eq!(at_laptop.writer.as_deref(), Some("adapter:imessage"));
        assert_eq!(at_laptop.node, Some(laptop.this_node_id().unwrap()));
        assert_eq!(at_laptop.route.len(), 1);
        assert_eq!(laptop.episode(&id).unwrap().unwrap().trust, "imported");

        // laptop → hub → spoke, each sending the origin beside the draft
        let send = |from: &Store,
                    from_home: &crate::paths::Home,
                    to: &Store,
                    to_home: &crate::paths::Home,
                    peer: &str| {
            // a pull, as the receiver asks for it (the hub relays a
            // spoke's rows to the other spokes, never back to the sender)
            let page = from
                .sync_export_for(from_home, 0, 50, Some(&format!("asker-of-{peer}")))
                .unwrap();
            to.apply_batch_from(
                to_home,
                &IngestBatch {
                    actor: format!("peer:{peer}"),
                    session_id: None,
                    episodes: page.episodes,
                },
                &page.origins,
            )
            .unwrap();
        };
        send(&laptop, &laptop_home, &hub, &hub_home, "laptop");
        send(&hub, &hub_home, &spoke, &spoke_home, "hub");
        let at_spoke = spoke.episode_origin(&id).unwrap().unwrap();
        assert_eq!(at_spoke.writer.as_deref(), Some("adapter:imessage"));
        assert_eq!(
            at_spoke.node, at_laptop.node,
            "the origin is the same everywhere"
        );
        let route: Vec<_> = at_spoke
            .route
            .iter()
            .map(|h| h.node.clone().unwrap())
            .collect();
        assert_eq!(
            route,
            [
                laptop.this_node_id().unwrap(),
                hub.this_node_id().unwrap(),
                spoke.this_node_id().unwrap()
            ]
        );

        // a sender too old to send origins: only the hop is known
        spoke
            .apply_batch(
                &spoke_home,
                &IngestBatch {
                    actor: "peer:hub".into(),
                    session_id: None,
                    episodes: vec![draft("desk", "task:1", "self")],
                },
            )
            .unwrap();
        let old = spoke
            .episode_origin(&crate::ids::episode_id("desk", "task:1"))
            .unwrap()
            .unwrap();
        assert!(old.writer.is_none());
        assert_eq!(old.route[0].name.as_deref(), Some("hub"));
    }
}
