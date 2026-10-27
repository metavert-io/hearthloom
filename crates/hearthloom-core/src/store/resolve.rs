//! Citation resolution: turn the symbolic ids the RLM cites
//! (`nt_…` notes, `en_…` entities, `ep_…` episodes) into human-readable
//! references with the RIGHT click target — a note opens in the slider,
//! a person opens their card, an event lands on its day. Everything
//! vault-guarded: an id the guard can't see resolves to nothing and the
//! chip is dropped (zero leakage, like every read surface).

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::Store;

/// One resolved reference. `route` tells the dashboard how to open it:
///   note   → the note slider (arg = note id)
///   person → the person/relationship modal (arg = entity name)
///   day    → Day view (arg = YYYY-MM-DD)
///   search → Memories search fallback (arg = query text)
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedRef {
    pub id: String,
    /// note | person | org | place | event | episode
    pub kind: String,
    pub label: String,
    /// A date or context line (may be empty).
    pub sub: String,
    pub route: String,
    pub arg: String,
}

impl Store {
    /// Resolve a batch of cited ids (order preserved, unresolvable
    /// dropped). Small N — the RLM cites a handful.
    pub fn resolve_citations(&self, ids: &[String]) -> Result<Vec<ResolvedRef>> {
        let mut out = Vec::new();
        for id in ids.iter().take(24) {
            if let Some(r) = self.resolve_one(id)? {
                out.push(r);
            }
        }
        Ok(out)
    }

    fn resolve_one(&self, id: &str) -> Result<Option<ResolvedRef>> {
        if id.starts_with("nt_") {
            return self.resolve_note(id);
        }
        if id.starts_with("en_") {
            return self.resolve_entity(id);
        }
        if id.starts_with("ep_") {
            return self.resolve_episode(id);
        }
        if id.starts_with("tk_") {
            return self.resolve_task(id);
        }
        if id.starts_with("in_") {
            return self.resolve_intent(id);
        }
        Ok(None)
    }

    fn resolve_task(&self, id: &str) -> Result<Option<ResolvedRef>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let hit: Option<(String, String)> = conn
            .query_row(
                &format!("SELECT description, status FROM tasks WHERE id = ?1{guard}"),
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(hit.map(|(description, status)| ResolvedRef {
            id: id.into(),
            kind: "task".into(),
            label: description,
            sub: if status == "open" {
                String::new()
            } else {
                status
            },
            route: "task".into(),
            arg: id.into(),
        }))
    }

    fn resolve_intent(&self, id: &str) -> Result<Option<ResolvedRef>> {
        // intents scope by `sphere`, private goals hide while locked
        let guard = self.scope_guard("sphere");
        let conn = self.lock();
        let hit: Option<(String, String)> = conn
            .query_row(
                &format!("SELECT name, status FROM intents WHERE id = ?1{guard}"),
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(hit.map(|(name, status)| ResolvedRef {
            id: id.into(),
            kind: "goal".into(),
            label: name,
            sub: if status == "active" {
                String::new()
            } else {
                status
            },
            route: "goal".into(),
            arg: id.into(),
        }))
    }

    fn resolve_note(&self, id: &str) -> Result<Option<ResolvedRef>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let hit: Option<(String, Option<String>)> = conn
            .query_row(
                &format!(
                    "SELECT title, updated_at FROM notes_index
                     WHERE id = ?1 AND status != 'deleted'{guard}"
                ),
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(hit.map(|(title, updated)| ResolvedRef {
            id: id.into(),
            kind: "note".into(),
            label: title,
            sub: updated
                .map(|d| d[..10.min(d.len())].to_string())
                .unwrap_or_default(),
            route: "note".into(),
            arg: id.into(),
        }))
    }

    fn resolve_entity(&self, id: &str) -> Result<Option<ResolvedRef>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let winner = super::graph::resolve_merged(&conn, id)?;
        let hit: Option<(String, String)> = conn
            .query_row(
                &format!("SELECT name, type FROM entities WHERE id = ?1{guard}"),
                [&winner],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((name, entity_type)) = hit else {
            return Ok(None);
        };
        // Person/Org/Group open the person card; Place → Atlas-ish
        // Memories on the name; anything else → Memories search.
        let (kind, route, arg) = match entity_type.as_str() {
            "Person" | "Org" | "Group" | "Project" => ("person", "person", name.clone()),
            "Place" => ("place", "search", name.clone()),
            _ => ("entity", "person", name.clone()),
        };
        Ok(Some(ResolvedRef {
            id: id.into(),
            kind: kind.into(),
            label: name,
            sub: entity_type.to_lowercase(),
            route: route.into(),
            arg,
        }))
    }

    fn resolve_episode(&self, id: &str) -> Result<Option<ResolvedRef>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let hit: Option<(String, String, String, Option<String>, String)> = conn
            .query_row(
                &format!(
                    "SELECT source, kind, title, event_time, source_ref
                     FROM episodes WHERE id = ?1 AND superseded_by IS NULL{guard}"
                ),
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((source, kind, title, event_time, source_ref)) = hit else {
            return Ok(None);
        };
        let day = event_time
            .as_deref()
            .map(|t| t[..10.min(t.len())].to_string())
            .unwrap_or_default();

        // A note SNAPSHOT episode (source='notes', ref 'note:{nt}:v…')
        // resolves back to the live note — open the slider, not a day.
        if source == "notes" {
            if let Some(rest) = source_ref.strip_prefix("note:") {
                if let Some((note_id, _)) = rest.split_once(':') {
                    // only if the live note still exists + is visible
                    let note_guard = self.scope_guard("scope");
                    let exists: Option<String> = conn
                        .query_row(
                            &format!(
                                "SELECT title FROM notes_index
                                 WHERE id = ?1 AND status != 'deleted'{note_guard}"
                            ),
                            [note_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if let Some(note_title) = exists {
                        return Ok(Some(ResolvedRef {
                            id: id.into(),
                            kind: "note".into(),
                            label: note_title,
                            sub: day,
                            route: "note".into(),
                            arg: note_id.into(),
                        }));
                    }
                }
            }
        }

        // Otherwise: an event in time — label it, land on its day
        // (Memories search as the fallback when it has no date).
        let label = format!("{source}/{kind}: {title}");
        let (route, arg) = if day.is_empty() {
            ("search".into(), title.clone())
        } else {
            ("day".into(), day.clone())
        };
        Ok(Some(ResolvedRef {
            id: id.into(),
            kind: "episode".into(),
            label,
            sub: day,
            route,
            arg,
        }))
    }
}
