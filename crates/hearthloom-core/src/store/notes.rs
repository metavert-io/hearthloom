//! Notes: plain markdown files under `~/.hearthloom/notes/` (law #1 —
//! the user owns the substrate; any editor could read them), indexed
//! here for FTS and versioning (codi's optimistic locking). The DB never
//! holds the body — only the file does.
//!
//! Notes are PRESENT-tense operational material. The record gets
//! snapshots: archiving a note mints a kind=note episode immediately,
//! and the consolidation "settle" pass snapshots any active note that
//! has sat quiet for 48h since its last snapshot — so the episode log
//! ends up holding the note's history at its resting points, append-only,
//! while the file stays freely editable.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::paths::Home;
use crate::time::now_utc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteRow {
    pub id: String,
    pub title: String,
    pub slug: String,
    pub path: String,
    pub scope: String,
    pub version: i64,
    pub status: String,
    pub linked_task: Option<String>,
    pub tags: Vec<String>,
    /// NULL = the source carried no dates (an honest unknown — never
    /// the import moment masquerading as activity). Sorts as oldest.
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub settled_at: Option<String>,
    /// Access counter (v17): the Notes dial weights labels by it.
    pub views: i64,
    /// How the note got here (v18): hearthloom | codi | future importers.
    pub origin: String,
    /// v19: note | template (a template's body is a frontmatter block
    /// + a {{placeholder}} skeleton; tasks select from them).
    pub kind: String,
    /// v26: the source-native id (Notion page-id…) — re-imports upsert
    /// by (origin, origin_ref) instead of duplicating.
    pub origin_ref: Option<String>,
    /// v29: the subject (project entity id / goal name) this note is the
    /// 1:1 primary description of, if any.
    pub primary_for: Option<String>,
}

fn note_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
    let tags: Option<String> = row.get("tags")?;
    Ok(NoteRow {
        id: row.get("id")?,
        title: row.get("title")?,
        slug: row.get("slug")?,
        path: row.get("path")?,
        scope: row.get("scope")?,
        version: row.get("version")?,
        status: row.get("status")?,
        linked_task: row.get("linked_task")?,
        tags: tags
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default(),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        settled_at: row.get("settled_at")?,
        views: row.get("views")?,
        origin: row.get("origin")?,
        kind: row.get("kind")?,
        origin_ref: row.get("origin_ref")?,
        primary_for: row.get("primary_for")?,
    })
}

const NOTE_COLUMNS: &str = "id, title, slug, path, scope, version, status, \
     linked_task, tags, created_at, updated_at, settled_at, views, origin, kind, \
     origin_ref, primary_for";

/// What an importer upsert did (the ingest report vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NoteImport {
    Created,
    Updated,
    Skipped,
}

/// The origin of a note written in the app — the owner's own words (v18).
pub const NATIVE_ORIGIN: &str = "hearthloom";
/// The origin of a note an agent wrote: an MCP commit, a scoped key's
/// POST, or an owner's note an agent has since edited.
pub const AGENT_ORIGIN: &str = "agent";
/// The kind of an Ask recipe (title = the chip's name, body = the
/// question). One click sends the body as the user's own words, so only
/// the recipe button makes one, and only on a note written in the app.
pub const RECIPE_KIND: &str = "recipe";

/// Who wrote a note, read from its `origin`. A snapshot's trust and
/// derivation follow it: only the owner's own writing lands as
/// `self`/`declared` — the tier the Ask outbound gate trusts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteWriter {
    /// Typed in the app (origin `hearthloom`).
    Owner,
    /// An agent (origin `agent`), or a topic page Hearthloom's model
    /// drafted and the owner accepted in Tend (`loom`) — an accepted
    /// proposal lands with agent trust, the proposed-memory precedent.
    Agent,
    /// Whatever an importer brought in (codi, notion, markdown…).
    Imported,
}

impl NoteWriter {
    pub fn of(origin: &str) -> Self {
        match origin {
            NATIVE_ORIGIN => NoteWriter::Owner,
            AGENT_ORIGIN | "loom" => NoteWriter::Agent,
            _ => NoteWriter::Imported,
        }
    }

    /// The trust tier a snapshot of this writer's note carries.
    pub fn trust(self) -> &'static str {
        match self {
            NoteWriter::Owner => "self",
            NoteWriter::Agent => "agent",
            NoteWriter::Imported => "imported",
        }
    }
}

/// An actor the server files as an agent (`agent:…`; a scoped key's is
/// fixed to `agent:key:<name>`). What it writes is never the owner's.
fn is_agent(actor: &Actor) -> bool {
    actor.actor.starts_with("agent:")
}

/// Pull @[Name] mention tokens out of a note body. Only the explicit
/// bracketed form links — a bare `@handle` or email stays plain text
/// (the chat-system lesson: never linkify what the user didn't pick).
/// Entity-type prefixes the typed mention form `@[Type:Name]` accepts
/// (item 13). Anything else keeps its colon as part of the name, so a
/// legacy `@[re: that thing]` never misparses.
const MENTION_TYPES: [&str; 9] = [
    "person", "org", "group", "project", "place", "artifact", "event", "topic", "device",
];

/// Split one mention token: `Person:Ada` → (Some("Person"), "Ada");
/// the legacy `Ada` → (None, "Ada"). Backward-compatible by design —
/// both forms parse forever; the typeahead just inserts the typed one.
pub fn split_typed_mention(token: &str) -> (Option<&str>, &str) {
    if let Some((prefix, rest)) = token.split_once(':') {
        let prefix = prefix.trim();
        let name = rest.trim();
        if !name.is_empty() && MENTION_TYPES.contains(&prefix.to_lowercase().as_str()) {
            return (Some(prefix), name);
        }
    }
    (None, token)
}

/// Typed extraction: (entity_type, name) per mention, deduped by name.
/// A typed mention resolves against its type; a legacy one against any.
pub fn extract_mentions_typed(body: &str) -> Vec<(Option<String>, String)> {
    let mut out: Vec<(Option<String>, String)> = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("@[") {
        let after = &rest[at + 2..];
        if let Some(end) = after.find(']') {
            let token = after[..end].trim();
            if !token.is_empty() && token.len() <= 96 {
                let (ty, name) = split_typed_mention(token);
                if !name.is_empty() && name.len() <= 80 && !out.iter().any(|(_, n)| n == name) {
                    out.push((ty.map(str::to_string), name.to_string()));
                }
            }
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// Pull [[Note Title]] wiki-link tokens out of a note body. Same
/// discipline as @[Name]: only the explicit bracketed form links.
pub fn extract_wiki_links(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("[[") {
        let after = &rest[at + 2..];
        if let Some(end) = after.find("]]") {
            let title = after[..end].trim();
            if !title.is_empty() && title.len() <= 200 && !out.iter().any(|t| t == title) {
                out.push(title.to_string());
            }
            rest = &after[end + 2..];
        } else {
            break;
        }
    }
    out
}

/// Journal a note's CURRENT state with its markdown BODY folded in.
///
/// `journal_row` alone serializes only the `notes_index` metadata row; a
/// note's substance lives in the file. So we read the row, splice the body
/// under the synthetic `__body__` key, and journal that as the post-image —
/// making the note fully recoverable from the operational journal (the file
/// is the substrate, but the file can be edited or lost between snapshots).
/// `restore_note` strips `__body__` before the SQL insert and rewrites the
/// markdown file from it.
fn journal_note_with_body(
    conn: &rusqlite::Connection,
    home: &Home,
    actor: &Actor,
    id: &str,
    op: &str,
) -> Result<()> {
    let post = super::row_as_json(conn, "notes_index", id)?;
    let post = post.map(|mut v| {
        if let Some(map) = v.as_object_mut() {
            let path = map.get("path").and_then(|p| p.as_str());
            let body = path
                .map(|p| home.read_note(p).unwrap_or_default())
                .unwrap_or_default();
            map.insert("__body__".into(), Value::String(body));
        }
        v
    });
    super::journal_op(conn, actor, "note", id, op, post.as_ref())
}

fn slugify(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(60).collect();
    if slug.is_empty() {
        "note".into()
    } else {
        slug
    }
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn create_note(
        &self,
        actor: &Actor,
        home: &Home,
        title: &str,
        body: &str,
        tags: &[String],
        scope: &str,
        linked_task: Option<&str>,
    ) -> Result<String> {
        self.create_note_from(
            actor,
            home,
            title,
            body,
            tags,
            scope,
            linked_task,
            NATIVE_ORIGIN,
            None,
        )
    }

    /// The importer path: same note, explicit ORIGIN ('codi', future
    /// 'notion'…) and original timestamps. Imported notes arrive
    /// pre-settled — their history already lives in the record via the
    /// import's episode harvest; the first EDIT re-dirties them and the
    /// settle pass snapshots normally from there.
    #[allow(clippy::too_many_arguments)]
    pub fn create_note_from(
        &self,
        actor: &Actor,
        home: &Home,
        title: &str,
        body: &str,
        tags: &[String],
        scope: &str,
        linked_task: Option<&str>,
        origin: &str,
        timestamps: Option<(&str, &str)>, // (created_at, updated_at)
    ) -> Result<String> {
        self.create_note_kind(
            actor,
            home,
            title,
            body,
            tags,
            scope,
            linked_task,
            origin,
            timestamps,
            "note",
        )
    }

    /// Full-control creation: also sets the note KIND (note | template).
    #[allow(clippy::too_many_arguments)]
    pub fn create_note_kind(
        &self,
        actor: &Actor,
        home: &Home,
        title: &str,
        body: &str,
        tags: &[String],
        scope: &str,
        linked_task: Option<&str>,
        origin: &str,
        timestamps: Option<(&str, &str)>,
        kind: &str,
    ) -> Result<String> {
        if !matches!(kind, "note" | "template" | RECIPE_KIND) {
            return Err(anyhow!("note kind must be note|template|recipe"));
        }
        let title = title.trim();
        if title.is_empty() {
            return Err(anyhow!("note title is required"));
        }
        // Who wrote it is the server's record, not the caller's claim: an
        // agent asking for the native origin is filed as `agent`, so its
        // text never settles as the owner's words or shows as a recipe.
        let origin = if origin == NATIVE_ORIGIN && is_agent(actor) {
            AGENT_ORIGIN
        } else {
            origin
        };
        if kind == RECIPE_KIND && origin != NATIVE_ORIGIN {
            return Err(anyhow!(
                "only a note written in the app can be an Ask recipe"
            ));
        }
        let conn = self.lock();
        // Unique slug: base, base-2, base-3…
        let base = slugify(title);
        let mut slug = base.clone();
        let mut n = 1;
        loop {
            let taken: i64 = conn.query_row(
                "SELECT COUNT(*) FROM notes_index WHERE slug = ?1",
                [&slug],
                |row| row.get(0),
            )?;
            if taken == 0 {
                break;
            }
            n += 1;
            slug = format!("{base}-{n}");
        }
        // Privacy rules outrank everything here too — a note about a
        // vaulted matter arrives private, importer or keyboard alike.
        let rules = super::privacy::load_rules(&conn)?;
        let scope = if scope != "private"
            && super::privacy::text_matches(&rules, &format!("{title}\n{body}"))
        {
            "private"
        } else {
            scope
        };
        let path = home.write_note(&slug, body)?;
        let id = ids::note_id();
        let now = now_utc();
        // Notes written live (in the app, or by an agent) are born now and
        // settle on the normal path; imports carry their SOURCE dates —
        // and when the source has none, NULL (unknown ≠ today).
        let live = origin == NATIVE_ORIGIN || origin == AGENT_ORIGIN;
        let (created_at, updated_at): (Option<&str>, Option<&str>) = match timestamps {
            Some((c, u)) => (Some(c), Some(u)),
            None if live => (Some(&now), Some(&now)),
            None => (None, None),
        };
        let settled_at: Option<&str> = if live { None } else { Some(&now) };
        conn.execute(
            "INSERT INTO notes_index (id, title, slug, path, scope, linked_task,
                 tags, created_at, updated_at, origin, settled_at, kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                id,
                title,
                slug,
                path,
                scope,
                linked_task,
                serde_json::to_string(tags)?,
                created_at,
                updated_at,
                origin,
                settled_at,
                kind,
            ],
        )?;
        conn.execute(
            "INSERT INTO notes_fts (title, body, note_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![title, body, id],
        )?;
        // mentions come from the TITLE and the body (Jon: @ in titles)
        index_mentions(&conn, &id, &format!("{title}\n{body}"))?;
        index_wiki_links(&conn, &id, body)?;
        record_mutation(
            &conn,
            actor,
            "note_created",
            &format!("note_created:{id}"),
            &json!({"id": id, "title": title, "slug": slug, "origin": origin}),
        )?;
        journal_note_with_body(&conn, home, actor, &id, "create")?;
        Ok(id)
    }

    /// Idempotent importer upsert keyed by (origin, origin_ref) — the
    /// episode source_ref discipline for notes. New page → created;
    /// changed content → updated in place (version bump, fts+mentions
    /// reindex, file rewritten); identical → skipped. Imported writes
    /// stay pre-settled (their history lives at the origin); the first
    /// HUMAN edit re-dirties them into the normal settle flow.
    #[allow(clippy::too_many_arguments)]
    pub fn import_note(
        &self,
        actor: &Actor,
        home: &Home,
        title: &str,
        body: &str,
        tags: &[String],
        scope: &str,
        origin: &str,
        origin_ref: &str,
        timestamps: Option<(&str, &str)>, // source-native (created, updated)
    ) -> Result<NoteImport> {
        let existing: Option<(String, String, i64)> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT id, slug, version FROM notes_index
                 WHERE origin = ?1 AND origin_ref = ?2",
                [origin, origin_ref],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
        };
        let Some((id, slug, version)) = existing else {
            let id = self.create_note_from(
                actor, home, title, body, tags, scope, None, origin, timestamps,
            )?;
            let conn = self.lock();
            conn.execute(
                "UPDATE notes_index SET origin_ref = ?1 WHERE id = ?2",
                [origin_ref, &id],
            )?;
            return Ok(NoteImport::Created);
        };
        let current = home
            .read_note(&format!("notes/{slug}.md"))
            .unwrap_or_default();
        let unchanged = {
            let conn = self.lock();
            let (cur_title, cur_tags): (String, Option<String>) = conn.query_row(
                "SELECT title, tags FROM notes_index WHERE id = ?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let cur_tags: Vec<String> = cur_tags
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            cur_title == title && cur_tags == tags && current == body
        };
        if unchanged {
            return Ok(NoteImport::Skipped);
        }
        self.update_note_as(
            actor,
            home,
            &id,
            Some(title),
            Some(body),
            Some(tags),
            None,
            version,
            false,
        )?;
        // an importer's update is not a human edit — keep it settled,
        // and keep the SOURCE's dates (update_note stamped "now"):
        // real dates when the source has them, NULL when it doesn't.
        let conn = self.lock();
        let (created, updated) = match timestamps {
            Some((c, u)) => (Some(c), Some(u)),
            None => (None, None),
        };
        conn.execute(
            "UPDATE notes_index SET created_at = ?1, updated_at = ?2,
                 settled_at = COALESCE(?2, settled_at) WHERE id = ?3",
            rusqlite::params![created, updated, id],
        )?;
        // update_note already journaled the body; re-journal the corrected
        // source timestamps so the latest post-image is date-accurate.
        journal_note_with_body(&conn, home, actor, &id, "update")?;
        Ok(NoteImport::Updated)
    }

    /// The scope of the note owning a media directory (by origin_ref) —
    /// the /notes-media serving guard. Deliberately UNGUARDED itself:
    /// the caller decides what a private scope means (404 while locked).
    pub fn media_owner_scope(&self, origin_ref: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT scope FROM notes_index
             WHERE origin_ref = ?1 AND status != 'deleted' LIMIT 1",
            [origin_ref],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Origin census for the Privacy page: (origin, total, private).
    /// Back-fill a real date onto consolidator-created (`loom`) notes that
    /// were imported dateless (NULL) — those sort OLDEST and hid at the
    /// bottom of Notes. Idempotent; only touches NULL-dated loom notes.
    pub fn date_undated_loom_notes(&self) -> Result<usize> {
        let conn = self.lock();
        let now = now_utc();
        let n = conn.execute(
            "UPDATE notes_index
                SET updated_at = ?1, created_at = COALESCE(created_at, ?1)
              WHERE origin = 'loom' AND updated_at IS NULL",
            [&now],
        )?;
        Ok(n)
    }

    pub fn note_origins(&self) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT origin, COUNT(*), SUM(scope = 'private')
             FROM notes_index WHERE status != 'deleted'
             GROUP BY origin ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Bulk scope for a whole origin ("vault all Notion content") —
    /// covers the live notes AND their settled snapshot episodes
    /// (source='notes', ref 'note:{id}:v…'). Unvaulting re-runs the
    /// privacy RULES per note: a rule-matched note stays private (rules
    /// outrank bulk gestures, same as everywhere else).
    pub fn set_notes_scope_by_origin(
        &self,
        actor: &Actor,
        home: &Home,
        origin: &str,
        scope: &str,
    ) -> Result<usize> {
        if !super::self_model::SPHERES.contains(&scope) {
            return Err(anyhow!("unknown scope {scope:?}"));
        }
        // A viewer whose vault is locked can't see a vaulted note, so a
        // bulk unvault passes over those (they stay private).
        let hide_private = !self.private_visible();
        let notes: Vec<(String, String, String, String)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT id, title, path, scope FROM notes_index
                 WHERE origin = ?1 AND status != 'deleted'",
            )?;
            let rows = stmt.query_map([origin], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut changed = 0usize;
        for chunk in notes.chunks(200) {
            {
                let conn = self.lock();
                let rules = super::privacy::load_rules(&conn)?;
                for (id, title, path, current) in chunk {
                    if hide_private && current == "private" && scope != "private" {
                        continue;
                    }
                    let scope = if scope != "private" {
                        let body = home.read_note(path).unwrap_or_default();
                        if super::privacy::text_matches(&rules, &format!("{title}\n{body}")) {
                            "private" // rules outrank the bulk unvault
                        } else {
                            scope
                        }
                    } else {
                        scope
                    };
                    conn.execute(
                        "UPDATE notes_index SET scope = ?1 WHERE id = ?2",
                        rusqlite::params![scope, id],
                    )?;
                    conn.execute(
                        "UPDATE episodes SET scope = ?1, scope_changed_at = strftime('%Y-%m-%dT%H:%M:%SZ','now')
                         WHERE source = 'notes' AND source_ref LIKE 'note:' || ?2 || ':%'",
                        rusqlite::params![scope, id],
                    )?;
                    journal_note_with_body(&conn, home, actor, id, "update")?;
                    changed += 1;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        record_mutation(
            &self.lock(),
            actor,
            "notes_scope_by_origin",
            &format!("notes_scope:{origin}:{scope}:{}", now_utc()),
            &json!({"origin": origin, "scope": scope, "notes": changed}),
        )?;
        Ok(changed)
    }

    /// Vault-guarded index listing; `q` searches title+body via FTS,
    /// `tag` filters the JSON tag array exactly, `offset` pages for
    /// infinite scroll. Most-recently-modified first; unknown dates
    /// (NULL) sort oldest.
    pub fn list_notes_filtered(
        &self,
        q: Option<&str>,
        tag: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<NoteRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let q = q.map(str::trim).filter(|s| !s.is_empty());
        let mut clauses = String::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(q) = q {
            let fts: String = q
                .split_whitespace()
                .map(|t| format!("\"{}\"", t.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" ");
            clauses.push_str(" AND id IN (SELECT note_id FROM notes_fts WHERE notes_fts MATCH ?)");
            params.push(Box::new(fts));
        }
        if let Some(tag) = tag.map(str::trim).filter(|t| !t.is_empty()) {
            // tags is a JSON array — exact-element match via json_each
            clauses.push_str(
                " AND EXISTS (SELECT 1 FROM json_each(COALESCE(tags, '[]'))
                              WHERE json_each.value = ?)",
            );
            params.push(Box::new(tag.trim_start_matches('#').to_lowercase()));
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {NOTE_COLUMNS} FROM notes_index
             WHERE status != 'deleted'{guard}{clauses}
             ORDER BY updated_at DESC LIMIT {limit} OFFSET {offset}"
        ))?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
                note_from_row,
            )
            .map(|r| r.filter_map(|x| x.ok()).collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(rows)
    }

    /// Back-compat listing (q + limit only).
    pub fn list_notes(&self, q: Option<&str>, limit: usize) -> Result<Vec<NoteRow>> {
        self.list_notes_filtered(q, None, limit, 0)
    }

    /// Designate a note as the 1:1 PRIMARY (description) note for a
    /// subject (a project entity id or a goal name). Clears any other
    /// primary for that subject — the convention is exactly one.
    /// subject=None clears this note's primary role.
    pub fn set_primary_note(
        &self,
        actor: &Actor,
        note_id: &str,
        subject: Option<&str>,
    ) -> Result<()> {
        let conn = self.lock();
        if let Some(subject) = subject {
            conn.execute(
                "UPDATE notes_index SET primary_for = NULL WHERE primary_for = ?1",
                [subject],
            )?;
            conn.execute(
                "UPDATE notes_index SET primary_for = ?1 WHERE id = ?2",
                [subject, note_id],
            )?;
        } else {
            conn.execute(
                "UPDATE notes_index SET primary_for = NULL WHERE id = ?1",
                [note_id],
            )?;
        }
        record_mutation(
            &conn,
            actor,
            "note_primary_set",
            &format!("note_primary:{note_id}:{}", now_utc()),
            &json!({"note": note_id, "subject": subject}),
        )?;
        // Metadata-only change (no Home handle in scope): plain journal_row,
        // no body captured — the body is unchanged and recoverable from the
        // note's most recent body-bearing journal entry.
        super::journal_row(&conn, actor, "note", "notes_index", note_id, "update")?;
        Ok(())
    }

    /// The primary/description note for a subject (id, title, excerpt),
    /// vault-guarded. Excerpt is the first ~240 chars of the file body.
    pub fn primary_note_meta(
        &self,
        home: &Home,
        subject: &str,
    ) -> Result<Option<(String, String, String)>> {
        let guard = self.scope_guard("scope");
        let hit: Option<(String, String, String)> = {
            let conn = self.lock();
            conn.query_row(
                &format!(
                    "SELECT id, title, path FROM notes_index
                     WHERE primary_for = ?1 AND status != 'deleted'{guard} LIMIT 1"
                ),
                [subject],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
        };
        Ok(hit.map(|(id, title, path)| {
            let body = home.read_note(&path).unwrap_or_default();
            let excerpt: String = body.chars().take(240).collect();
            (id, title, excerpt)
        }))
    }

    /// Tag census for the quick tabs + # typeahead: (tag, note count),
    /// most common first. Vault-guarded.
    pub fn note_tags(&self, limit: usize) -> Result<Vec<(String, i64)>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT json_each.value, COUNT(*) AS n
             FROM notes_index, json_each(COALESCE(tags, '[]'))
             WHERE status != 'deleted'{guard}
             GROUP BY json_each.value
             ORDER BY n DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Index row + body (from the file). Vault-guarded.
    pub fn note(&self, home: &Home, id: &str) -> Result<Option<(NoteRow, String)>> {
        let guard = self.scope_guard("scope");
        let row = {
            let conn = self.lock();
            conn.query_row(
                &format!("SELECT {NOTE_COLUMNS} FROM notes_index WHERE id = ?1{guard}"),
                [id],
                note_from_row,
            )
            .optional()?
        };
        match row {
            Some(row) => {
                // access telemetry for the Notes dial (counter, not a
                // mutation — the intents-watermark precedent)
                let conn = self.lock();
                conn.execute(
                    "UPDATE notes_index SET views = views + 1 WHERE id = ?1",
                    [id],
                )?;
                drop(conn);
                let body = home.read_note(&row.path).unwrap_or_default();
                Ok(Some((row, body)))
            }
            None => Ok(None),
        }
    }

    /// Version history from the op_journal (v43): every note write
    /// journaled a full post-image, markdown body included (`__body__`).
    /// Gated by the note's CURRENT visibility — the same scope guard as
    /// `note()`, so a vaulted note has no history surface while locked.
    /// Returns None when the note is absent or invisible.
    pub fn note_versions(&self, id: &str) -> Result<Option<Vec<serde_json::Value>>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let visible: Option<String> = conn
            .query_row(
                &format!("SELECT id FROM notes_index WHERE id = ?1{guard}"),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if visible.is_none() {
            return Ok(None);
        }
        let mut stmt = conn.prepare(
            "SELECT entry_id, op, actor, ts, post_image FROM op_journal
             WHERE entity_kind = 'note' AND entity_id = ?1
             ORDER BY seq DESC LIMIT 100",
        )?;
        let rows: Vec<serde_json::Value> = stmt
            .query_map([id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(entry_id, op, actor, ts, post_image)| {
                let image: serde_json::Value = post_image
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "entry_id": entry_id,
                    "op": op,
                    "actor": actor,
                    "ts": ts,
                    "title": image.get("title").and_then(|v| v.as_str()),
                    "body_chars": image.get("__body__").and_then(|v| v.as_str()).map(|b| b.len()),
                })
            })
            .collect();
        Ok(Some(rows))
    }

    /// One journaled version in full (title + markdown body). Same
    /// visibility gate as `note_versions`.
    pub fn note_version(&self, id: &str, entry_id: &str) -> Result<Option<serde_json::Value>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let visible: Option<String> = conn
            .query_row(
                &format!("SELECT id FROM notes_index WHERE id = ?1{guard}"),
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if visible.is_none() {
            return Ok(None);
        }
        let row: Option<(String, String, String, Option<String>)> = conn
            .query_row(
                "SELECT op, actor, ts, post_image FROM op_journal
                 WHERE entity_kind = 'note' AND entity_id = ?1 AND entry_id = ?2",
                [id, entry_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row.map(|(op, actor, ts, post_image)| {
            let image: serde_json::Value = post_image
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "entry_id": entry_id,
                "op": op,
                "actor": actor,
                "ts": ts,
                "title": image.get("title").and_then(|v| v.as_str()),
                "body": image.get("__body__").and_then(|v| v.as_str()).unwrap_or(""),
            })
        }))
    }

    /// Optimistic-locked update: `expected_version` must match or the
    /// caller gets a conflict error (codi's Version pattern). A vaulted
    /// note can be changed only by a viewer whose vault is open.
    #[allow(clippy::too_many_arguments)]
    pub fn update_note(
        &self,
        actor: &Actor,
        home: &Home,
        id: &str,
        title: Option<&str>,
        body: Option<&str>,
        tags: Option<&[String]>,
        scope: Option<&str>,
        expected_version: i64,
    ) -> Result<i64> {
        self.update_note_as(
            actor,
            home,
            id,
            title,
            body,
            tags,
            scope,
            expected_version,
            true,
        )
    }

    /// `update_note`, with the vault check optional: an importer's refresh
    /// of a note from its source changes content, never the label, so it
    /// runs whether or not anyone's vault is open.
    #[allow(clippy::too_many_arguments)]
    fn update_note_as(
        &self,
        actor: &Actor,
        home: &Home,
        id: &str,
        title: Option<&str>,
        body: Option<&str>,
        tags: Option<&[String]>,
        scope: Option<&str>,
        expected_version: i64,
        vault_checked: bool,
    ) -> Result<i64> {
        if let Some(scope) = scope {
            if !super::self_model::SPHERES.contains(&scope) {
                return Err(anyhow!("unknown scope {scope:?}"));
            }
        }
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!("SELECT {NOTE_COLUMNS} FROM notes_index WHERE id = ?1"),
                [id],
                note_from_row,
            )
            .optional()?
            .ok_or_else(|| anyhow!("no such note {id}"))?;
        if row.status == "deleted" {
            return Err(anyhow!("note {id} is deleted"));
        }
        // A vaulted note is invisible to a viewer whose vault is locked,
        // and so is beyond its edits — including taking it out of the vault.
        if vault_checked && row.scope == "private" && !self.private_visible() {
            return Err(anyhow!(
                "note {id} is in the vault — unlock it to change the note"
            ));
        }
        if row.version != expected_version {
            return Err(anyhow!(
                "version conflict: note is at v{}, you edited v{expected_version}",
                row.version
            ));
        }
        // An agent's edit makes the text partly the agent's: an owner note
        // it touches is filed as `agent` from here on (its snapshots stop
        // landing as the owner's words, a recipe stops being one), and the
        // owner's later edits don't launder it back.
        let (origin, kind) = if is_agent(actor) && row.origin == NATIVE_ORIGIN {
            (
                AGENT_ORIGIN,
                if row.kind == RECIPE_KIND {
                    "note"
                } else {
                    row.kind.as_str()
                },
            )
        } else {
            (row.origin.as_str(), row.kind.as_str())
        };
        let new_title = title
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or(&row.title);
        let new_body = match body {
            Some(body) => {
                home.write_note(&row.slug, body)?;
                body.to_string()
            }
            None => home.read_note(&row.path).unwrap_or_default(),
        };
        let now = now_utc();
        // Rules re-check on every edit; a match forces private and an
        // explicit downgrade attempt loses to the rule.
        let rules = super::privacy::load_rules(&conn)?;
        let scope = if super::privacy::text_matches(&rules, &format!("{new_title}\n{new_body}")) {
            Some("private")
        } else {
            scope
        };
        conn.execute(
            "UPDATE notes_index SET title = ?1, version = version + 1,
                 tags = COALESCE(?2, tags), scope = COALESCE(?3, scope),
                 updated_at = ?4, origin = ?6, kind = ?7
             WHERE id = ?5",
            rusqlite::params![
                new_title,
                tags.map(|t| serde_json::to_string(t).unwrap_or_default()),
                scope,
                now,
                id,
                origin,
                kind
            ],
        )?;
        conn.execute("DELETE FROM notes_fts WHERE note_id = ?1", [id])?;
        conn.execute(
            "INSERT INTO notes_fts (title, body, note_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![new_title, new_body, id],
        )?;
        index_mentions(&conn, id, &format!("{new_title}\n{new_body}"))?;
        index_wiki_links(&conn, id, &new_body)?;
        record_mutation(
            &conn,
            actor,
            "note_updated",
            &format!("note_updated:{id}:v{}", row.version + 1),
            &json!({"id": id, "version": row.version + 1, "origin": origin}),
        )?;
        journal_note_with_body(&conn, home, actor, id, "update")?;
        Ok(row.version + 1)
    }

    /// Archive (snapshot an episode now) or soft-delete (file survives —
    /// raw content is never discarded).
    pub fn set_note_status(
        &self,
        actor: &Actor,
        home: &Home,
        id: &str,
        status: &str,
    ) -> Result<Option<String>> {
        if !matches!(status, "active" | "archived" | "deleted") {
            return Err(anyhow!("status must be active|archived|deleted"));
        }
        {
            let conn = self.lock();
            let changed = conn.execute(
                "UPDATE notes_index SET status = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![status, now_utc(), id],
            )?;
            if changed == 0 {
                return Err(anyhow!("no such note {id}"));
            }
            record_mutation(
                &conn,
                actor,
                "note_status",
                &format!("note_status:{id}:{status}:{}", now_utc()),
                &json!({"id": id, "status": status}),
            )?;
            // Notes are soft-deleted (status='deleted'); the file survives
            // and there is no hard row deletion anywhere. So the soft-delete
            // IS the tombstone — journaled as an update carrying the body.
            journal_note_with_body(&conn, home, actor, id, "update")?;
        }
        if status == "archived" {
            return self.snapshot_note(actor, home, id).map(Some);
        }
        Ok(None)
    }

    /// Mint the note's current state as a kind=note episode. Its trust
    /// follows who wrote the note (`NoteWriter`): the owner's own note is
    /// `self` (source "notes" → derivation=declared); an agent's is
    /// `agent` and an import's `imported`, both derivation=observed — the
    /// record saw them arrive, the owner never declared them.
    /// Idempotent per (note, version).
    fn snapshot_note(&self, actor: &Actor, home: &Home, id: &str) -> Result<String> {
        let (row, body, entities) = {
            let conn = self.lock();
            let row = conn
                .query_row(
                    &format!("SELECT {NOTE_COLUMNS} FROM notes_index WHERE id = ?1"),
                    [id],
                    note_from_row,
                )
                .optional()?
                .ok_or_else(|| anyhow!("no such note {id}"))?;
            let body = home.read_note(&row.path).unwrap_or_default();
            let entities = snapshot_mentions(
                &conn,
                NoteWriter::of(&row.origin),
                &format!("{}\n{}", row.title, body),
            )?;
            (row, body, entities)
        };
        let source_ref = format!("note:{id}:v{}", row.version);
        let draft = crate::ingest::EpisodeDraft {
            source: "notes".into(),
            source_ref: source_ref.clone(),
            kind: "note".into(),
            title: row.title.clone(),
            body,
            event_time: crate::time::EventTime {
                utc: row.updated_at.clone(),
                tz_offset_minutes: None,
                uncertainty: if row.updated_at.is_some() {
                    crate::time::TimeUncertainty::Minute
                } else {
                    crate::time::TimeUncertainty::Unknown
                },
            },
            trust: NoteWriter::of(&row.origin).trust().into(),
            scope: Some(row.scope.clone()),
            entities,
            edges: vec![],
        };
        self.apply_batch(
            home,
            &crate::ingest::IngestBatch {
                actor: actor.actor.clone(),
                session_id: actor.session_id.clone(),
                episodes: vec![draft],
            },
        )?;
        let episode_id = ids::episode_id("notes", &source_ref);
        let conn = self.lock();
        conn.execute(
            "UPDATE notes_index SET settled_at = ?1 WHERE id = ?2",
            rusqlite::params![now_utc(), id],
        )?;
        Ok(episode_id)
    }

    /// ANY-term note search (OR semantics, best match first) — the
    /// RLM's search leg: engine queries carry extra words ("membership",
    /// "account") that AND semantics would demand of a three-word note.
    /// Vault-guarded.
    pub fn search_notes_any(&self, query: &str, limit: usize) -> Result<Vec<NoteRow>> {
        let fts: String = query
            .split_whitespace()
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect::<Vec<_>>()
            .join(" OR ");
        if fts.is_empty() {
            return Ok(vec![]);
        }
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {NOTE_COLUMNS} FROM notes_index
             WHERE status != 'deleted'{guard}
               AND id IN (SELECT note_id FROM notes_fts
                          WHERE notes_fts MATCH ?1 ORDER BY rank LIMIT {limit})
             LIMIT {limit}"
        ))?;
        let rows = stmt
            .query_map([fts], note_from_row)
            .map(|r| r.filter_map(|x| x.ok()).collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(rows)
    }

    /// Notes mentioning an entity, newest first (the query the link
    /// table exists for). Merge-group aware, vault-guarded.
    pub fn notes_mentioning(&self, entity_id: &str, limit: usize) -> Result<Vec<NoteRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        let winner = super::graph::resolve_merged(&conn, entity_id)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {NOTE_COLUMNS} FROM notes_index
             WHERE status != 'deleted'{guard}
               AND id IN (SELECT note_id FROM note_mentions WHERE entity_id = ?1)
             ORDER BY updated_at DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([&winner], note_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Outbound wiki-links of a note: (id, title) of each linked note.
    /// Vault-guarded on the TARGET (a locked vault must not reveal a
    /// private note's title through someone else's backlink panel).
    pub fn note_links(&self, note_id: &str) -> Result<Vec<(String, String)>> {
        let guard = self.scope_guard("n.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT n.id, n.title FROM note_links l
             JOIN notes_index n ON n.id = l.target_note_id
             WHERE l.note_id = ?1 AND n.status != 'deleted'{guard}
             ORDER BY n.title"
        ))?;
        let rows = stmt.query_map([note_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Backlinks: notes whose body wiki-links TO this note. Same guard,
    /// applied to the linking note.
    pub fn note_backlinks(&self, note_id: &str) -> Result<Vec<(String, String)>> {
        let guard = self.scope_guard("n.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT n.id, n.title FROM note_links l
             JOIN notes_index n ON n.id = l.note_id
             WHERE l.target_note_id = ?1 AND n.status != 'deleted'{guard}
             ORDER BY n.updated_at DESC"
        ))?;
        let rows = stmt.query_map([note_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Idle pass: recompute the RESONANT lane — nearest-neighbor notes
    /// from the mean chunk embedding of each note's latest settled
    /// snapshot (existing vectors only; a note that has never settled
    /// simply doesn't resonate yet — it settles within 48h). Local,
    /// no LLM, full replace; reads are vault-guarded separately.
    pub fn refresh_note_neighbors(&self, per_note: usize) -> Result<usize> {
        use crate::embeddings::{bytes_to_floats, cosine_similarity};
        let vectors: Vec<(String, Vec<f32>)> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(
                "SELECT n.id, c.embedding FROM notes_index n
                 JOIN chunks c ON c.episode_id = (
                     SELECT e.id FROM episodes e
                     WHERE e.source = 'notes' AND e.superseded_by IS NULL
                       AND e.source_ref LIKE 'note:' || n.id || ':v%'
                     ORDER BY e.rowid DESC LIMIT 1)
                   AND c.embedding IS NOT NULL
                 WHERE n.status = 'active' AND n.kind = 'note'
                 ORDER BY n.id",
            )?;
            let rows: Vec<(String, Vec<u8>)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            // mean the chunk vectors per note
            let mut out: Vec<(String, Vec<f32>, usize)> = Vec::new();
            for (id, blob) in rows {
                let v = bytes_to_floats(&blob);
                match out.last_mut() {
                    Some((last, sum, n)) if *last == id => {
                        for (a, b) in sum.iter_mut().zip(&v) {
                            *a += b;
                        }
                        *n += 1;
                    }
                    _ => out.push((id, v, 1)),
                }
            }
            out.into_iter()
                .map(|(id, mut sum, n)| {
                    for a in sum.iter_mut() {
                        *a /= n as f32;
                    }
                    (id, sum)
                })
                .collect()
        }; // lock released before the O(n²) math
        let mut rows: Vec<(String, String, f32)> = Vec::new();
        for i in 0..vectors.len() {
            let mut best: Vec<(usize, f32)> = Vec::new();
            for j in 0..vectors.len() {
                if i == j {
                    continue;
                }
                let s = cosine_similarity(&vectors[i].1, &vectors[j].1);
                // near-duplicates (a template copy, a re-import) rhyme
                // trivially — the semantic-pairs discipline
                if (0.5..=0.97).contains(&s) {
                    best.push((j, s));
                }
            }
            best.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
            best.truncate(per_note);
            for (j, s) in best {
                rows.push((vectors[i].0.clone(), vectors[j].0.clone(), s));
            }
        }
        let conn = self.lock();
        conn.execute("DELETE FROM note_neighbors", [])?;
        for (a, b, s) in &rows {
            conn.execute(
                "INSERT OR IGNORE INTO note_neighbors (note_id, neighbor_note_id, score)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![a, b, *s as f64],
            )?;
        }
        Ok(rows.len())
    }

    /// The resonant lane for one note: (id, title, score), best first.
    /// Vault-guarded on the neighbor.
    pub fn note_resonant(&self, note_id: &str, limit: usize) -> Result<Vec<(String, String, f64)>> {
        let guard = self.scope_guard("n.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT n.id, n.title, r.score FROM note_neighbors r
             JOIN notes_index n ON n.id = r.neighbor_note_id
             WHERE r.note_id = ?1 AND n.status != 'deleted'{guard}
             ORDER BY r.score DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([note_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Count of vaulted (private) notes — shown on the Privacy page
    /// even while LOCKED (a count reveals nothing; titles do). Ignores
    /// the vault guard on purpose, so the number is always honest.
    pub fn vaulted_note_count(&self) -> Result<i64> {
        let conn = self.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM notes_index WHERE scope = 'private' AND status != 'deleted'",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Vaulted notes (titles) — vault-guarded: returns EMPTY while
    /// locked (revealing a title is revealing content), the list when
    /// unlocked. The Privacy page pairs it with the always-visible count.
    pub fn vaulted_notes(&self, limit: usize) -> Result<Vec<NoteRow>> {
        if !self.private_visible() {
            return Ok(vec![]);
        }
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {NOTE_COLUMNS} FROM notes_index
             WHERE scope = 'private' AND status != 'deleted'
             ORDER BY updated_at DESC LIMIT {limit}"
        ))?;
        let rows = stmt.query_map([], note_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The entities a note @-mentions (id, name, type) — the note
    /// header's clickable association chips. Merge-aware, vault-guarded.
    pub fn note_mention_entities(&self, note_id: &str) -> Result<Vec<(String, String, String)>> {
        let guard = self.scope_guard("en.scope");
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT en.id, en.name, en.type FROM note_mentions m
             JOIN entities en ON en.id = m.entity_id AND en.merged_into IS NULL
             WHERE m.note_id = ?1{guard}
             ORDER BY CASE en.type WHEN 'Project' THEN 0 WHEN 'Org' THEN 1
                                   WHEN 'Person' THEN 2 ELSE 3 END, en.name"
        ))?;
        let rows = stmt.query_map([note_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Mention counts per entity (the relationship-row chip), one query.
    pub fn mention_counts(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT m.entity_id, COUNT(DISTINCT m.note_id)
             FROM note_mentions m
             JOIN notes_index n ON n.id = m.note_id
             WHERE n.status != 'deleted'
             GROUP BY m.entity_id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The consolidation settle pass: any active note quiet for
    /// `quiet_hours` whose latest state isn't snapshotted yet gets one.
    /// Returns snapshot count. (Editing later re-dirties it: the next
    /// quiet period snapshots the new version — history at rest points.)
    pub fn settle_notes(&self, actor: &Actor, home: &Home, quiet_hours: i64) -> Result<usize> {
        let ids: Vec<String> = {
            let conn = self.lock();
            // updated_at is RFC 3339 ("…T…Z"); julianday() parses it,
            // where a lexical compare against datetime('now') would not.
            let mut stmt = conn.prepare(
                "SELECT id FROM notes_index
                 WHERE status = 'active' AND kind = 'note'
                   AND julianday(updated_at) <= julianday('now') - (?1 / 24.0)
                   AND (settled_at IS NULL OR settled_at < updated_at)
                 LIMIT 50",
            )?;
            let rows = stmt.query_map([quiet_hours as f64], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        let mut settled = 0usize;
        for id in ids {
            self.snapshot_note(actor, home, &id)?;
            settled += 1;
        }
        Ok(settled)
    }

    /// Restore a note from its operational-journal history — re-inserts the
    /// last known `notes_index` post-image AND rewrites the markdown file
    /// from the captured `__body__`. Mirrors `restore_task`/`restore_intent`;
    /// the note-specific twist is the file: notes are soft-deleted and the
    /// file normally survives, but a lost/edited body is recoverable here
    /// because create/update journaling folds the body into the post-image.
    pub fn restore_note(&self, actor: &Actor, home: &Home, id: &str) -> Result<()> {
        let conn = self.lock();
        let post: Option<String> = conn
            .query_row(
                "SELECT post_image FROM op_journal
                 WHERE entity_kind='note' AND entity_id=?1 AND post_image IS NOT NULL
                 ORDER BY ts DESC, seq DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(post) = post else {
            return Err(anyhow!("no journal history for note {id}"));
        };
        let obj: Value = serde_json::from_str(&post)?;
        let Some(map) = obj.as_object() else {
            return Err(anyhow!("bad journal post-image for {id}"));
        };
        // Split the synthetic body off the metadata row before the insert.
        let body = map
            .get("__body__")
            .and_then(|v| v.as_str())
            .map(String::from);
        let cols: Vec<&String> = map.keys().filter(|k| k.as_str() != "__body__").collect();
        let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT OR REPLACE INTO notes_index ({}) VALUES ({})",
            cols.iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            placeholders.join(", ")
        );
        let params: Vec<rusqlite::types::Value> = cols
            .iter()
            .map(|c| super::json_to_sql(&map[c.as_str()]))
            .collect();
        conn.execute(&sql, rusqlite::params_from_iter(params))?;
        // Rewrite the markdown file from the captured body, then reindex the
        // FTS + mention/wiki-link derivations from the restored content.
        let slug = map.get("slug").and_then(|v| v.as_str());
        let title = map
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if let (Some(slug), Some(body)) = (slug, body.as_deref()) {
            home.write_note(slug, body)?;
            conn.execute("DELETE FROM notes_fts WHERE note_id = ?1", [id])?;
            conn.execute(
                "INSERT INTO notes_fts (title, body, note_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![title, body, id],
            )?;
            index_mentions(&conn, id, &format!("{title}\n{body}"))?;
            index_wiki_links(&conn, id, body)?;
        }
        journal_note_with_body(&conn, home, actor, id, "update")?;
        Ok(())
    }
}

/// Rebuild a note's wiki-links from its current body (derived, like
/// note_mentions; an unknown title stays plain text — it may resolve
/// on a later save once the target note exists).
fn index_wiki_links(conn: &rusqlite::Connection, note_id: &str, body: &str) -> Result<()> {
    conn.execute("DELETE FROM note_links WHERE note_id = ?1", [note_id])?;
    for title in extract_wiki_links(body) {
        let hit: Option<String> = conn
            .query_row(
                "SELECT id FROM notes_index
                 WHERE status != 'deleted' AND lower(title) = lower(?1)
                 ORDER BY (title = ?1) DESC, created_at LIMIT 1",
                [&title],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(target) = hit {
            if target != note_id {
                conn.execute(
                    "INSERT OR IGNORE INTO note_links (note_id, target_note_id)
                     VALUES (?1, ?2)",
                    [note_id, &target],
                )?;
            }
        }
    }
    Ok(())
}

/// Rebuild a note's mention links from its current body (derived data;
/// resolution is name-based and merge-group aware — an unknown name
/// simply doesn't link, it stays text).
fn index_mentions(conn: &rusqlite::Connection, note_id: &str, body: &str) -> Result<()> {
    conn.execute("DELETE FROM note_mentions WHERE note_id = ?1", [note_id])?;
    for (ty, name) in extract_mentions_typed(body) {
        if let Some(entity_id) = resolve_mention(conn, ty.as_deref(), &name)? {
            conn.execute(
                "INSERT OR IGNORE INTO note_mentions (note_id, entity_id) VALUES (?1, ?2)",
                [note_id, &entity_id],
            )?;
        }
    }
    Ok(())
}

/// The entities a snapshot cites: its @[Name] mentions (title + body),
/// role `mentioned` — a note about someone strengthens their graph. Only
/// the owner's own note may MINT one that way (a typed mention carries its
/// type; the legacy form keeps the historical Person default). A note an
/// agent or an importer wrote links only entities the record already has
/// — the live note_mentions rule — so a pasted page or an agent's text
/// can't add people to the graph as if the owner had named them.
fn snapshot_mentions(
    conn: &rusqlite::Connection,
    writer: NoteWriter,
    text: &str,
) -> Result<Vec<crate::ingest::EntityRef>> {
    let mut out = Vec::new();
    for (ty, name) in extract_mentions_typed(text) {
        let (entity_type, name) = if writer == NoteWriter::Owner {
            (ty.unwrap_or_else(|| "Person".into()), name)
        } else {
            // The existing entity's exact type and name, so ingest
            // resolves to it instead of creating one.
            let known = resolve_mention(conn, ty.as_deref(), &name)?
                .map(|id| super::graph::entity_row(conn, &id))
                .transpose()?
                .flatten();
            match known {
                Some(entity) => (entity.entity_type, entity.name),
                None => continue,
            }
        };
        out.push(crate::ingest::EntityRef {
            entity_type,
            name,
            attributes: vec![],
            role: "mentioned".into(),
        });
    }
    Ok(out)
}

/// Resolve a mention to an entity id. A typed mention (`@[Org:Apex]`)
/// only matches its type — the disambiguation the legacy form can't
/// express when a Person and an Org share a name.
pub(crate) fn resolve_mention(
    conn: &rusqlite::Connection,
    entity_type: Option<&str>,
    name: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT id FROM entities
             WHERE merged_into IS NULL AND lower(name) = lower(?1)
               AND (?2 IS NULL OR lower(type) = lower(?2))
             ORDER BY (name = ?1) DESC, created_at LIMIT 1",
            rusqlite::params![name, entity_type],
            |row| row.get(0),
        )
        .optional()?)
}

/// A template note, parsed: the ---template frontmatter block defines
/// the task form; the remainder is the note SKELETON with
/// {{field-key}}, {{task}} and {{date}} placeholders.
///
/// ```text
/// ---template
/// field: source | Source | text
/// field: rating | Rating | select | 1,2,3,4,5
/// note-on-complete: true
/// ---
/// ## What I learned from {{source}}
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct TemplateDef {
    pub id: String,
    pub name: String,
    pub fields: Value,
    pub note_on_complete: bool,
    pub skeleton: String,
}

pub fn parse_template(note_id: &str, title: &str, body: &str) -> TemplateDef {
    let mut fields = Vec::new();
    let mut note_on_complete = false;
    let mut skeleton = body.to_string();
    if let Some(rest) = body.trim_start().strip_prefix("---template") {
        if let Some(end) = rest.find("---") {
            for line in rest[..end].lines() {
                let line = line.trim();
                if let Some(spec) = line.strip_prefix("field:") {
                    let parts: Vec<&str> = spec.split('|').map(str::trim).collect();
                    if parts.is_empty() || parts[0].is_empty() {
                        continue;
                    }
                    let mut field = serde_json::json!({
                        "key": parts[0],
                        "label": parts.get(1).copied().unwrap_or(parts[0]),
                        "type": parts.get(2).copied().unwrap_or("text"),
                    });
                    if let Some(options) = parts.get(3) {
                        field["options"] = Value::Array(
                            options
                                .split(',')
                                .map(|o| Value::String(o.trim().into()))
                                .collect(),
                        );
                    }
                    fields.push(field);
                } else if let Some(v) = line.strip_prefix("note-on-complete:") {
                    note_on_complete = v.trim() == "true";
                }
            }
            skeleton = rest[end + 3..].trim_start_matches('\n').to_string();
        }
    }
    TemplateDef {
        id: note_id.to_string(),
        name: title.to_string(),
        fields: Value::Array(fields),
        note_on_complete,
        skeleton,
    }
}

impl Store {
    /// Every active template note, parsed and ready for the task system.
    /// Vault-guarded like all note reads.
    pub fn list_templates(&self, home: &Home) -> Result<Vec<TemplateDef>> {
        let guard = self.scope_guard("scope");
        let rows: Vec<NoteRow> = {
            let conn = self.lock();
            let mut stmt = conn.prepare(&format!(
                "SELECT {NOTE_COLUMNS} FROM notes_index
                 WHERE kind = 'template' AND status = 'active'{guard}
                 ORDER BY title"
            ))?;
            let rows = stmt.query_map([], note_from_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        Ok(rows
            .into_iter()
            .map(|n| {
                let body = home.read_note(&n.path).unwrap_or_default();
                parse_template(&n.id, &n.title, &body)
            })
            .collect())
    }

    /// One template by note id (the tasks system's binding).
    pub fn template(&self, home: &Home, id: &str) -> Result<Option<TemplateDef>> {
        Ok(self.note(home, id)?.and_then(|(row, body)| {
            (row.kind == "template").then(|| parse_template(&row.id, &row.title, &body))
        }))
    }

    /// Template lookup by slugified title — the #template-name hashtag.
    pub fn template_by_tag(&self, home: &Home, tag: &str) -> Result<Option<TemplateDef>> {
        let id: Option<String> = {
            let conn = self.lock();
            conn.query_row(
                "SELECT id FROM notes_index
                 WHERE kind = 'template' AND status = 'active'
                   AND lower(replace(title, ' ', '-')) = ?1",
                [tag],
                |row| row.get(0),
            )
            .optional()?
        };
        match id {
            Some(id) => self.template(home, &id),
            None => Ok(None),
        }
    }
}

/// Render a template's skeleton with the task's form values —
/// {{field-key}}, {{task}}, {{date}} — for the offered note.
pub fn render_template(template: &TemplateDef, task: &super::tasks::TaskRow) -> (String, String) {
    let values = task
        .payload
        .as_ref()
        .and_then(|p| p.get("values"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let subst = |text: &str| -> String {
        let mut out = text.to_string();
        out = out.replace("{{task}}", &task.description);
        out = out.replace("{{date}}", &crate::time::now_utc()[..10]);
        for (key, value) in &values {
            let rendered = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out = out.replace(&format!("{{{{{key}}}}}"), &rendered);
        }
        out
    };
    let title = subst(&format!("{}: {{{{task}}}}", template.name));
    (title, subst(&template.skeleton))
}

/// Prefill body for the task→note loop: the completed task's structured
/// payload rendered as a starting point for what was learned. Tasks
/// bound to a template render its skeleton instead (render_template).
pub fn note_prefill_for_task(task: &super::tasks::TaskRow) -> (String, String) {
    let title = format!("Notes: {}", task.description);
    let mut body = format!("_from task: {}_\n\n", task.description);
    if let Some(Value::Object(payload)) = &task.payload {
        if let Some(Value::Object(values)) = payload.get("values") {
            for (key, value) in values {
                body.push_str(&format!("- **{key}**: {value}\n"));
            }
            body.push('\n');
        }
    }
    body.push_str("## What I learned\n\n");
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> Actor {
        Actor::new("human:test")
    }

    #[test]
    fn test_a_locked_viewer_cannot_unvault_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let open = store
            .create_note(&actor(), &home, "Groceries", "milk", &[], "personal", None)
            .unwrap();
        let vaulted = store
            .create_note(
                &actor(),
                &home,
                "Diagnosis",
                "private",
                &[],
                "private",
                None,
            )
            .unwrap();
        // Read the label directly: `note()` itself hides a vaulted note while locked.
        let scope_of = |id: &str| -> String {
            store
                .lock()
                .query_row("SELECT scope FROM notes_index WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };

        // Locked (no viewer here): the vaulted note can't be edited or unvaulted…
        assert!(store
            .update_note(
                &actor(),
                &home,
                &vaulted,
                None,
                None,
                None,
                Some("personal"),
                1
            )
            .is_err());
        // …and a bulk "unvault this origin" passes over it.
        store
            .set_notes_scope_by_origin(&actor(), &home, "hearthloom", "work")
            .unwrap();
        assert_eq!(
            (scope_of(&open), scope_of(&vaulted)),
            ("work".to_string(), "private".to_string())
        );

        // With the vault open the owner can do both.
        store.vault_unlock(15);
        store
            .update_note(
                &actor(),
                &home,
                &vaulted,
                None,
                None,
                None,
                Some("health"),
                1,
            )
            .unwrap();
        assert_eq!(scope_of(&vaulted), "health");
    }

    #[test]
    fn test_note_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        let id = store
            .create_note(
                &actor(),
                &home,
                "Reading: attention is all you need",
                "# Transformers\n\nqueries, keys, values",
                &["ml".into()],
                "work",
                None,
            )
            .unwrap();
        // The file exists on disk, human-readable.
        let (row, body) = store.note(&home, &id).unwrap().unwrap();
        assert!(home.root().join(&row.path).exists());
        assert!(body.contains("Transformers"));
        assert_eq!(row.version, 1);

        // Same title → deduped slug.
        let id2 = store
            .create_note(
                &actor(),
                &home,
                "Reading: attention is all you need",
                "second",
                &[],
                "work",
                None,
            )
            .unwrap();
        let (row2, _) = store.note(&home, &id2).unwrap().unwrap();
        assert_ne!(row.slug, row2.slug);

        // FTS search finds it; optimistic locking guards edits.
        assert_eq!(store.list_notes(Some("transformers"), 10).unwrap().len(), 1);
        assert!(store
            .update_note(&actor(), &home, &id, None, Some("edited"), None, None, 99)
            .is_err());
        let v2 = store
            .update_note(
                &actor(),
                &home,
                &id,
                None,
                Some("# Edited\nnew text"),
                None,
                None,
                1,
            )
            .unwrap();
        assert_eq!(v2, 2);
        assert!(store
            .list_notes(Some("transformers"), 10)
            .unwrap()
            .is_empty());
        assert_eq!(store.list_notes(Some("edited"), 10).unwrap().len(), 1);

        // Archive → snapshot episode, derivation declared.
        let episode_id = store
            .set_note_status(&actor(), &home, &id, "archived")
            .unwrap()
            .unwrap();
        let episode = store.episode(&episode_id).unwrap().unwrap();
        assert_eq!(episode.kind, "note");
        assert_eq!(episode.derivation, "declared");
        assert!(store
            .episode_body(&episode_id)
            .unwrap()
            .unwrap()
            .contains("new text"));

        // Delete is soft; the file survives.
        store
            .set_note_status(&actor(), &home, &id2, "deleted")
            .unwrap();
        assert!(home.root().join(&row2.path).exists());
        assert_eq!(store.list_notes(None, 10).unwrap().len(), 1);
    }

    #[test]
    fn test_mentions_extract_index_and_snapshot() {
        // Only the explicit @[Name] form links; bare @text never does.
        let names = |s: &str| -> Vec<String> {
            extract_mentions_typed(s)
                .into_iter()
                .map(|(_, n)| n)
                .collect()
        };
        assert_eq!(
            names("met @[Ada Lovelace] and @[Bob], cc @everyone at a@b.com"),
            vec!["Ada Lovelace".to_string(), "Bob".to_string()]
        );
        // Typed form (item 13): the type splits off; an unknown prefix
        // (or a colon mid-name) stays part of the name — legacy-safe.
        assert_eq!(
            extract_mentions_typed("@[Org:Apex Corp] @[person: Ada] @[re: that thing]"),
            vec![
                (Some("Org".to_string()), "Apex Corp".to_string()),
                (Some("person".to_string()), "Ada".to_string()),
                (None, "re: that thing".to_string()),
            ]
        );
        // The names-only view strips types (settle snapshots, callers).
        assert_eq!(
            names("@[Project:Hearthloom] and @[Bob]"),
            vec!["Hearthloom".to_string(), "Bob".to_string()]
        );

        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        // A known entity to link against.
        {
            let conn = store.lock();
            crate::store::graph::upsert_entity(&conn, &actor(), "Person", "Ada", "work", &[])
                .unwrap();
        }
        let ada = store.entity_by_name("Ada").unwrap().unwrap();
        let id = store
            .create_note(
                &actor(),
                &home,
                "sync",
                "talked to @[Ada] about @[Nobody Known]",
                &[],
                "work",
                None,
            )
            .unwrap();
        // Known name links; unknown stays text (no row).
        let notes = store.notes_mentioning(&ada.id, 10).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, id);
        assert_eq!(store.mention_counts().unwrap(), vec![(ada.id.clone(), 1)]);
        // Editing away the mention unlinks it.
        store
            .update_note(
                &actor(),
                &home,
                &id,
                None,
                Some("no more mentions"),
                None,
                None,
                1,
            )
            .unwrap();
        assert!(store.notes_mentioning(&ada.id, 10).unwrap().is_empty());
        // Re-add and settle: the snapshot carries the mention into the graph.
        store
            .update_note(
                &actor(),
                &home,
                &id,
                None,
                Some("again @[Ada]"),
                None,
                None,
                2,
            )
            .unwrap();
        assert_eq!(store.settle_notes(&actor(), &home, 0).unwrap(), 1);
        let dossier = store.entity_dossier("Ada").unwrap().unwrap();
        assert!(dossier.episode_count >= 1);
    }

    #[test]
    fn test_typed_mention_resolves_by_type() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        // The collision the typed form exists for: a Person and an Org
        // sharing one name.
        {
            let conn = store.lock();
            crate::store::graph::upsert_entity(&conn, &actor(), "Person", "Apex", "work", &[])
                .unwrap();
            crate::store::graph::upsert_entity(&conn, &actor(), "Org", "Apex", "work", &[])
                .unwrap();
        }
        let (person_id, org_id) = {
            let conn = store.lock();
            let pick = |ty: &str| -> String {
                conn.query_row(
                    "SELECT id FROM entities WHERE name='Apex' AND type=?1",
                    [ty],
                    |row| row.get(0),
                )
                .unwrap()
            };
            (pick("Person"), pick("Org"))
        };
        let id = store
            .create_note(
                &actor(),
                &home,
                "deal",
                "call with @[Org:Apex]",
                &[],
                "work",
                None,
            )
            .unwrap();
        let org_notes = store.notes_mentioning(&org_id, 10).unwrap();
        assert_eq!(org_notes.len(), 1);
        assert_eq!(org_notes[0].id, id);
        assert!(store.notes_mentioning(&person_id, 10).unwrap().is_empty());
    }

    #[test]
    fn test_wiki_links_extract_index_and_backlinks() {
        // Only the explicit [[Title]] form links.
        assert_eq!(
            extract_wiki_links("see [[Project Notes]] and [[Ideas]], not [single] or [[]]"),
            vec!["Project Notes".to_string(), "Ideas".to_string()]
        );

        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let target = store
            .create_note(
                &actor(),
                &home,
                "Project Notes",
                "the hub",
                &[],
                "work",
                None,
            )
            .unwrap();
        let linker = store
            .create_note(
                &actor(),
                &home,
                "sync",
                "details in [[Project Notes]] and [[Nowhere Yet]]",
                &[],
                "work",
                None,
            )
            .unwrap();
        // Known title links (case-insensitive resolve); unknown stays text.
        assert_eq!(
            store.note_links(&linker).unwrap(),
            vec![(target.clone(), "Project Notes".to_string())]
        );
        assert_eq!(
            store.note_backlinks(&target).unwrap(),
            vec![(linker.clone(), "sync".to_string())]
        );
        // Editing away the link unlinks it.
        store
            .update_note(
                &actor(),
                &home,
                &linker,
                None,
                Some("no links now"),
                None,
                None,
                1,
            )
            .unwrap();
        assert!(store.note_links(&linker).unwrap().is_empty());
        assert!(store.note_backlinks(&target).unwrap().is_empty());
        // A private target hides from the links read while locked…
        store
            .update_note(
                &actor(),
                &home,
                &linker,
                None,
                Some("back to [[Project Notes]]"),
                None,
                None,
                2,
            )
            .unwrap();
        store
            .update_note(
                &actor(),
                &home,
                &target,
                None,
                None,
                None,
                Some("private"),
                1,
            )
            .unwrap();
        assert!(store.note_links(&linker).unwrap().is_empty());
        // …and reappears when the vault opens.
        store.vault_unlock(5);
        assert_eq!(store.note_links(&linker).unwrap().len(), 1);
        store.vault_lock();
    }

    #[test]
    fn test_template_notes_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        // A template IS a note: frontmatter form + skeleton body.
        let body = "---template\nfield: source | Source | text\n\
            field: rating | Rating | select | 1,2,3\nnote-on-complete: true\n---\n\
            _from {{task}} on {{date}}_\n\n**Source:** {{source}} ({{rating}}/3)\n\n## What I learned\n";
        let template_id = store
            .create_note_kind(
                &actor(),
                &home,
                "Research Reading",
                body,
                &[],
                "personal",
                None,
                "hearthloom",
                None,
                "template",
            )
            .unwrap();
        let templates = store.list_templates(&home).unwrap();
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].fields.as_array().unwrap().len(), 2);
        assert!(templates[0].note_on_complete);
        assert!(templates[0].skeleton.contains("{{source}}"));
        // #research-reading resolves to it
        assert!(store
            .template_by_tag(&home, "research-reading")
            .unwrap()
            .is_some());
        // templates never settle into the record
        assert_eq!(store.settle_notes(&actor(), &home, 0).unwrap(), 0);

        // The workflow: a #research-reading task completes → the offered
        // note is the rendered skeleton.
        let task_id = store
            .add_task(
                &actor(),
                "plain",
                "read the attention paper #research-reading",
                Some(&serde_json::json!({"values": {"source": "arxiv", "rating": "3"}})),
                None,
                "one-time",
                "work",
                None,
                None,
                &[],
            )
            .unwrap();
        let task = store.task(&task_id).unwrap().unwrap();
        assert_eq!(task.kind, "form");
        assert_eq!(task.template_id.as_deref(), Some(template_id.as_str()));
        let outcome = store
            .complete_task(&actor(), &home, &task_id, None)
            .unwrap();
        assert!(outcome.offer_note);
        let task = store.task(&task_id).unwrap().unwrap();
        let template = store.template(&home, &template_id).unwrap().unwrap();
        let (title, rendered) = render_template(&template, &task);
        assert!(title.contains("read the attention paper"));
        assert!(rendered.contains("**Source:** arxiv (3/3)"));
        assert!(!rendered.contains("---template"));
    }

    #[test]
    fn test_privacy_rules_apply_to_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .add_privacy_rule(&actor(), "domain", "secretplace.com")
            .unwrap();

        // A note ABOUT the vaulted domain arrives private — even via an
        // importer, even without the ".com" (the stem matches).
        let id = store
            .create_note_from(
                &actor(),
                &home,
                "Secretplace plans",
                "meet at secretplace",
                &[],
                "personal",
                None,
                "codi",
                Some(("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z")),
            )
            .unwrap();
        // Locked vault: invisible; unlocked: origin + timestamps kept.
        assert!(store.list_notes(None, 10).unwrap().is_empty());
        store.vault_unlock(5);
        let notes = store.list_notes(None, 10).unwrap();
        assert_eq!(notes[0].scope, "private");
        assert_eq!(notes[0].origin, "codi");
        assert_eq!(notes[0].created_at.as_deref(), Some("2026-01-01T00:00:00Z"));
        // Imported notes arrive pre-settled: no duplicate snapshot.
        assert!(notes[0].settled_at.is_some());
        store.vault_lock();

        // An innocent note that EDITS into rule territory flips private.
        let ok = store
            .create_note(
                &actor(),
                &home,
                "harmless",
                "nothing here",
                &[],
                "personal",
                None,
            )
            .unwrap();
        store
            .update_note(
                &actor(),
                &home,
                &ok,
                None,
                Some("now about secretplace.com"),
                None,
                None,
                1,
            )
            .unwrap();
        assert!(store
            .list_notes(None, 10)
            .unwrap()
            .iter()
            .all(|n| n.id != ok));
        let _ = id;
    }

    #[test]
    fn test_settle_pass_snapshots_quiet_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        let id = store
            .create_note(
                &actor(),
                &home,
                "quiet thought",
                "still true",
                &[],
                "personal",
                None,
            )
            .unwrap();
        // Not quiet long enough with 48h threshold; 0h settles it now.
        assert_eq!(store.settle_notes(&actor(), &home, 48).unwrap(), 0);
        assert_eq!(store.settle_notes(&actor(), &home, 0).unwrap(), 1);
        // Already settled at this version — nothing new.
        assert_eq!(store.settle_notes(&actor(), &home, 0).unwrap(), 0);
        // Snapshot is idempotent per version (same source_ref).
        let (row, _) = store.note(&home, &id).unwrap().unwrap();
        assert_eq!(row.version, 1);
        assert_eq!(store.counts().unwrap().episodes, 1);
    }

    /// Who wrote a note decides how its snapshot lands: the owner's own
    /// words are self/declared and may mint the people they name; an
    /// agent's or an import's land as agent/imported + observed and only
    /// link entities the record already has.
    #[test]
    fn test_snapshot_trust_follows_the_writer() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        {
            let conn = store.lock();
            crate::store::graph::upsert_entity(&conn, &actor(), "Person", "Ada", "work", &[])
                .unwrap();
            crate::store::graph::upsert_entity(&conn, &actor(), "Org", "Apex", "work", &[])
                .unwrap();
        }
        let ada = store.entity_by_name("Ada").unwrap().unwrap();
        let apex = store.entity_by_name("Apex").unwrap().unwrap();
        let cites = |episode_id: &str, entity_id: &str| -> bool {
            let conn = store.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM episode_entities WHERE episode_id = ?1 AND entity_id = ?2",
                [episode_id, entity_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
                > 0
        };
        let entity_count = || -> i64 {
            let conn = store.lock();
            conn.query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))
                .unwrap()
        };
        let tier = |episode_id: &str| -> (String, String) {
            let episode = store.episode(episode_id).unwrap().unwrap();
            (episode.trust, episode.derivation)
        };

        // The owner's note: their own words; naming someone new mints them.
        let mine = store
            .create_note(
                &actor(),
                &home,
                "lunch",
                "with @[Ada] and @[Grace Hopper]",
                &[],
                "work",
                None,
            )
            .unwrap();
        let ep = store
            .set_note_status(&actor(), &home, &mine, "archived")
            .unwrap()
            .unwrap();
        assert_eq!(tier(&ep), ("self".into(), "declared".into()));
        assert!(cites(&ep, &ada.id));
        assert!(store.entity_by_name("Grace Hopper").unwrap().is_some());
        let before = entity_count();

        // An agent's note (a scoped key asking for the native origin):
        // known names link — a typed one to that type — new ones mint nothing.
        let agent = Actor::new("agent:key:helper");
        let theirs = store
            .create_note(
                &agent,
                &home,
                "plan",
                "@[Ada], @[org:apex] and @[Mallory Unknown]",
                &[],
                "work",
                None,
            )
            .unwrap();
        assert_eq!(
            store.note(&home, &theirs).unwrap().unwrap().0.origin,
            AGENT_ORIGIN
        );
        let ep = store
            .set_note_status(&actor(), &home, &theirs, "archived")
            .unwrap()
            .unwrap();
        assert_eq!(tier(&ep), ("agent".into(), "observed".into()));
        assert!(cites(&ep, &ada.id));
        assert!(cites(&ep, &apex.id));

        // An import: imported trust, observed, and no minting either.
        store
            .import_note(
                &Actor::new("adapter:notion"),
                &home,
                "page",
                "met @[Ada] and @[Eve Stranger]",
                &[],
                "work",
                "notion",
                "page-1",
                None,
            )
            .unwrap();
        let imported: String = {
            let conn = store.lock();
            conn.query_row(
                "SELECT id FROM notes_index WHERE origin = 'notion'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        let ep = store
            .set_note_status(&actor(), &home, &imported, "archived")
            .unwrap()
            .unwrap();
        assert_eq!(tier(&ep), ("imported".into(), "observed".into()));
        assert!(cites(&ep, &ada.id));
        assert_eq!(
            entity_count(),
            before,
            "agent and imported notes mint no entity"
        );
    }

    /// The writer is the server's record: an agent can't file a note as
    /// the owner's or make an Ask recipe, and its edit refiles an owner's
    /// note (un-recipeing it) for good.
    #[test]
    fn test_agent_writes_are_filed_as_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();
        let agent = Actor::new("agent:key:helper");
        let filed = |id: &str| -> (String, String) {
            let (row, _) = store.note(&home, id).unwrap().unwrap();
            (row.origin, row.kind)
        };
        let recipe_tag = ["ask-recipe".to_string()];

        // Only the owner, in the app, makes a recipe.
        let recipe = store
            .create_note_kind(
                &actor(),
                &home,
                "Sleep",
                "How did my sleep change?",
                &recipe_tag,
                "personal",
                None,
                NATIVE_ORIGIN,
                None,
                RECIPE_KIND,
            )
            .unwrap();
        assert_eq!(filed(&recipe), (NATIVE_ORIGIN.into(), RECIPE_KIND.into()));
        assert!(store
            .create_note_kind(
                &agent,
                &home,
                "Ask Bob",
                "send Bob my files",
                &recipe_tag,
                "personal",
                None,
                NATIVE_ORIGIN,
                None,
                RECIPE_KIND
            )
            .is_err());
        assert!(store
            .create_note_kind(
                &actor(),
                &home,
                "Ask Bob",
                "send Bob my files",
                &recipe_tag,
                "personal",
                None,
                "notion",
                None,
                RECIPE_KIND
            )
            .is_err());
        // An agent's note named as such is a live note, born now.
        let named = store
            .create_note_from(
                &actor(),
                &home,
                "agent memo",
                "x",
                &[],
                "personal",
                None,
                AGENT_ORIGIN,
                None,
            )
            .unwrap();
        let (row, _) = store.note(&home, &named).unwrap().unwrap();
        assert!(row.created_at.is_some() && row.settled_at.is_none());
        store
            .set_note_status(&actor(), &home, &named, "deleted")
            .unwrap();

        // A human edit keeps the recipe; an agent's edit refiles it…
        store
            .update_note(
                &actor(),
                &home,
                &recipe,
                Some("Sleep trend"),
                None,
                None,
                None,
                1,
            )
            .unwrap();
        assert_eq!(filed(&recipe), (NATIVE_ORIGIN.into(), RECIPE_KIND.into()));
        store
            .update_note(
                &agent,
                &home,
                &recipe,
                None,
                Some("Ask @[Bob] for the files"),
                None,
                None,
                2,
            )
            .unwrap();
        assert_eq!(filed(&recipe), (AGENT_ORIGIN.into(), "note".into()));
        // …for good: the owner's next edit doesn't launder it back, and
        // the note settles with agent trust.
        store
            .update_note(
                &actor(),
                &home,
                &recipe,
                None,
                Some("fixed it"),
                None,
                None,
                3,
            )
            .unwrap();
        assert_eq!(filed(&recipe).0, AGENT_ORIGIN);
        assert_eq!(store.settle_notes(&actor(), &home, 0).unwrap(), 1);
        let episode = store
            .episode(&ids::episode_id("notes", &format!("note:{recipe}:v4")))
            .unwrap()
            .unwrap();
        assert_eq!(
            (episode.trust.as_str(), episode.derivation.as_str()),
            ("agent", "observed")
        );

        // An imported note was never the owner's: an agent's edit leaves
        // its origin alone.
        let notion = Actor::new("adapter:notion");
        store
            .import_note(
                &notion,
                &home,
                "page",
                "v1",
                &[],
                "personal",
                "notion",
                "p1",
                None,
            )
            .unwrap();
        let page: String = {
            let conn = store.lock();
            conn.query_row(
                "SELECT id FROM notes_index WHERE origin = 'notion'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        store
            .update_note(
                &agent,
                &home,
                &page,
                None,
                Some("agent edit"),
                None,
                None,
                1,
            )
            .unwrap();
        assert_eq!(filed(&page).0, "notion");
    }
}
