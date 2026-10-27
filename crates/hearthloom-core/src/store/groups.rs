//! Friend groups (v54, plan C.8/C.9): a named set of connected
//! friends, owned by THIS record. The owner is the only one who edits
//! the roster, and (v1) the only one who shares into the group.
//!
//! A group is a second way a friend can be granted something — never a
//! way to take anything back, and never a way to widen what was granted:
//! - A group carries its own sharing settings (the same descriptor a
//!   friend's override uses) and can be the target of an item share:
//!   `shared_items.friend = "group:<id>"`. A friend's access is the
//!   UNION of their direct grant and each group's grant, evaluated grant
//!   by grant (`FriendAccess`) — a class granted by one and a life area granted
//!   by another never combine into something neither granted.
//! - A group does NOT inherit the default sharing template and has no
//!   built-in floor: it grants exactly what was set on it. (Members
//!   already inherit the template directly; a group that inherited it too
//!   would quietly undo a per-friend "off".)
//! - Membership only counts for a friend whose own connection is live —
//!   `Store::access` starts from `effective_grant`, so a revoked or
//!   expired friend gets nothing through a group.
//! - 'private' stays ungrantable: the same `validate_sharing` and the
//!   same `scope != 'private'` in every read.
//!
//! Sync: group rows ride the grants/shares leg (/v1/sync/sharing): all
//! of them on every page, last-writer-wins by `updated_at` (strictly
//! increasing per group — `next_stamp`); deletion is a soft `deleted_at`
//! so it travels the same way. Item shares to a group are ordinary
//! `shared_items` rows.
//!
//! The member side: a friend's node asks `GET /v1/network/groups` and
//! keeps the answer in `peer_groups` — a per-node CACHE of what each
//! friend's node says, replaced whole on every fetch. It deliberately
//! does not sync or export: the outbound friend key that fetches it is
//! node-local, and the owner's node is the only authority.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

use super::sharing::{resolve_group, validate_sharing, EffectiveGrant, DEFAULT_FRIEND};
use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// `shared_items.friend` values that address a group start with this.
/// No connection may be named with it.
pub const GROUP_PREFIX: &str = "group:";

const MAX_NAME_CHARS: usize = 60;

#[derive(Debug, Clone, serde::Serialize)]
pub struct GroupRow {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    /// The group's sharing descriptor (classes / spheres / detail / flags).
    pub sharing: Value,
    pub created_at: String,
    pub updated_at: String,
}

impl GroupRow {
    /// The value item shares to this group carry in `shared_items.friend`.
    pub fn key(&self) -> String {
        format!("{GROUP_PREFIX}{}", self.id)
    }
}

/// Everything one friend is granted: their own connection plus each
/// group they belong to. Reads ask it questions grant by grant.
pub struct FriendAccess {
    pub friend: String,
    pub direct: EffectiveGrant,
    pub groups: Vec<(GroupRow, EffectiveGrant)>,
}

impl FriendAccess {
    fn grants(&self) -> impl Iterator<Item = &EffectiveGrant> {
        std::iter::once(&self.direct).chain(self.groups.iter().map(|(_, g)| g))
    }

    /// Does any single grant satisfy `test`?
    pub fn any(&self, test: impl Fn(&EffectiveGrant) -> bool) -> bool {
        self.grants().any(test)
    }

    /// The life areas in which some single grant satisfies `test` — the
    /// exact set a categorical read may serve. Never contains 'private'.
    pub fn spheres(&self, test: impl Fn(&EffectiveGrant) -> bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for grant in self.grants().filter(|g| test(g)) {
            for sphere in &grant.spheres {
                if sphere != "private" && !out.contains(sphere) {
                    out.push(sphere.clone());
                }
            }
        }
        out
    }

    /// SQL: is the `shared_items` row in `column` addressed to this
    /// friend — directly (bound as `param`) or through one of their
    /// groups? Group ids are generated here (`gr_` + hex) and checked
    /// again before they are written into the statement.
    pub fn share_target(&self, column: &str, param: &str) -> String {
        let keys: Vec<String> = self
            .groups
            .iter()
            .filter(|(g, _)| valid_id(&g.id))
            .map(|(g, _)| format!("'{}'", g.key()))
            .collect();
        if keys.is_empty() {
            format!("{column} = {param}")
        } else {
            format!("({column} = {param} OR {column} IN ({}))", keys.join(","))
        }
    }

    /// The widest reading of everything granted, for the `offer`
    /// descriptor only — an advertised ceiling. NEVER enforce with this:
    /// it pairs classes and life areas that no single grant pairs.
    pub fn ceiling(&self) -> EffectiveGrant {
        let mut out = self.direct.clone();
        for (_, grant) in &self.groups {
            for class in &grant.classes {
                if !out.classes.contains(class) {
                    out.classes.push(class.clone());
                }
            }
            for sphere in &grant.spheres {
                if !out.spheres.contains(sphere) {
                    out.spheres.push(sphere.clone());
                }
            }
            if grant.detail == "full" {
                out.detail = "full".into();
            }
            out.completed_tasks |= grant.completed_tasks;
            out.evidence_relationships |= grant.evidence_relationships;
        }
        out
    }
}

fn valid_id(id: &str) -> bool {
    id.starts_with("gr_") && id.len() > 3 && id[3..].chars().all(|c| c.is_ascii_hexdigit())
}

/// The group id inside a `group:<id>` share target, if it is one.
pub fn group_id_of(target: &str) -> Option<&str> {
    target.strip_prefix(GROUP_PREFIX)
}

/// The stamp for a change to group `id`: now, or one second past the
/// row's current stamp if that is not earlier. Stamps have one-second
/// resolution and the sync leg is last-writer-wins on them, so two edits
/// inside a second must still order.
fn next_stamp(conn: &rusqlite::Connection, id: &str) -> Result<String> {
    let now = now_utc();
    let current: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM friend_groups WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    match current {
        Some(current) if current.as_str() >= now.as_str() => {
            let bumped = chrono::DateTime::parse_from_rfc3339(&current)
                .map(|t| t.with_timezone(&chrono::Utc) + chrono::Duration::seconds(1))
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            Ok(bumped.unwrap_or(now))
        }
        _ => Ok(now),
    }
}

fn clean_name(name: &str) -> Result<String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err(anyhow!("a group needs a name"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(anyhow!(
            "a group name can be at most {MAX_NAME_CHARS} characters"
        ));
    }
    Ok(name)
}

fn row_to_group(row: &rusqlite::Row) -> rusqlite::Result<GroupRow> {
    let members: String = row.get(2)?;
    let sharing: Option<String> = row.get(3)?;
    Ok(GroupRow {
        id: row.get(0)?,
        name: row.get(1)?,
        members: serde_json::from_str(&members).unwrap_or_default(),
        sharing: sharing
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({})),
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

const GROUP_COLS: &str = "id, name, members, sharing, created_at, updated_at";

impl Store {
    /// Every live group, by name.
    pub fn groups(&self) -> Result<Vec<GroupRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {GROUP_COLS} FROM friend_groups WHERE deleted_at IS NULL
             ORDER BY name COLLATE NOCASE"
        ))?;
        let rows = stmt.query_map([], row_to_group)?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    pub fn group(&self, id: &str) -> Result<Option<GroupRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {GROUP_COLS} FROM friend_groups WHERE id = ?1 AND deleted_at IS NULL"),
            [id],
            row_to_group,
        )
        .optional()
        .map_err(Into::into)
    }

    fn live_group(&self, id: &str) -> Result<GroupRow> {
        self.group(id)?.ok_or_else(|| anyhow!("no such group"))
    }

    /// The live groups a friend belongs to.
    pub fn groups_of(&self, friend: &str) -> Result<Vec<GroupRow>> {
        Ok(self
            .groups()?
            .into_iter()
            .filter(|g| g.members.iter().any(|m| m == friend))
            .collect())
    }

    /// Everything a friend is granted, or a refusal if their own
    /// connection is not live. THE entry point for friend-surface reads.
    pub fn access(&self, friend: &str) -> Result<FriendAccess> {
        let direct = self.effective_grant(friend)?; // liveness gate
        let groups = self
            .groups_of(friend)?
            .into_iter()
            .map(|group| {
                let grant = resolve_group(&group.key(), &group.sharing);
                (group, grant)
            })
            .collect();
        Ok(FriendAccess {
            friend: friend.to_string(),
            direct,
            groups,
        })
    }

    fn name_taken(&self, name: &str, except: Option<&str>) -> Result<bool> {
        Ok(self
            .groups()?
            .iter()
            .any(|g| g.name.eq_ignore_ascii_case(name) && Some(g.id.as_str()) != except))
    }

    fn write_group(&self, actor: &Actor, id: &str, verb: &str, detail: &Value) -> Result<()> {
        let conn = self.lock();
        super::journal_row(&conn, actor, "friend_group", "friend_groups", id, "update")?;
        record_mutation(
            &conn,
            actor,
            verb,
            &format!("{verb}:{id}:{}", now_utc()),
            detail,
        )?;
        Ok(())
    }

    /// Create an empty group that grants nothing yet.
    pub fn create_group(&self, actor: &Actor, name: &str) -> Result<GroupRow> {
        let name = clean_name(name)?;
        if self.name_taken(&name, None)? {
            return Err(anyhow!("you already have a group called {name:?}"));
        }
        let id = format!("gr_{}", uuid::Uuid::new_v4().simple());
        let now = now_utc();
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO friend_groups (id, name, members, sharing, created_at, updated_at)
                 VALUES (?1, ?2, '[]', '{}', ?3, ?3)",
                rusqlite::params![id, name, now],
            )?;
        }
        self.write_group(
            actor,
            &id,
            "group_created",
            &json!({"id": id, "name": name}),
        )?;
        self.live_group(&id)
    }

    pub fn rename_group(&self, actor: &Actor, id: &str, name: &str) -> Result<GroupRow> {
        self.live_group(id)?;
        let name = clean_name(name)?;
        if self.name_taken(&name, Some(id))? {
            return Err(anyhow!("you already have a group called {name:?}"));
        }
        {
            let conn = self.lock();
            conn.execute(
                "UPDATE friend_groups SET name = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, name, next_stamp(&conn, id)?],
            )?;
        }
        self.write_group(actor, id, "group_renamed", &json!({"id": id, "name": name}))?;
        self.live_group(id)
    }

    /// Replace the roster. Every member must be a connection of this
    /// record (a grant row by that name); the list is de-duplicated.
    pub fn set_group_members(
        &self,
        actor: &Actor,
        id: &str,
        members: &[String],
    ) -> Result<GroupRow> {
        self.live_group(id)?;
        let mut roster: Vec<String> = Vec::new();
        for member in members {
            if member == DEFAULT_FRIEND || member.starts_with(GROUP_PREFIX) {
                return Err(anyhow!("{member:?} is not a connection"));
            }
            if self.grant(member)?.is_none() {
                return Err(anyhow!("{member:?} is not one of your connections"));
            }
            if !roster.contains(member) {
                roster.push(member.clone());
            }
        }
        {
            let conn = self.lock();
            conn.execute(
                "UPDATE friend_groups SET members = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, json!(roster).to_string(), next_stamp(&conn, id)?],
            )?;
        }
        self.write_group(
            actor,
            id,
            "group_members_set",
            &json!({"id": id, "members": roster}),
        )?;
        self.live_group(id)
    }

    /// Set what the group grants its members (the friend-override
    /// descriptor; absent keys grant nothing — a group inherits nothing).
    pub fn set_group_sharing(&self, actor: &Actor, id: &str, sharing: &Value) -> Result<GroupRow> {
        self.live_group(id)?;
        let clean = validate_sharing(sharing)?;
        {
            let conn = self.lock();
            conn.execute(
                "UPDATE friend_groups SET sharing = ?2, updated_at = ?3 WHERE id = ?1",
                rusqlite::params![id, clean.to_string(), next_stamp(&conn, id)?],
            )?;
        }
        self.write_group(
            actor,
            id,
            "group_sharing_set",
            &json!({"id": id, "sharing": clean}),
        )?;
        self.live_group(id)
    }

    /// Delete a group: its grant ends on the members' next request and
    /// every item shared to it is un-shared. The row stays as a
    /// tombstone (`deleted_at`) so the deletion reaches the other node.
    pub fn delete_group(&self, actor: &Actor, id: &str) -> Result<bool> {
        let Some(group) = self.group(id)? else {
            return Ok(false);
        };
        let key = group.key();
        let conn = self.lock();
        let share_ids: Vec<String> = {
            let mut stmt = conn.prepare("SELECT id FROM shared_items WHERE friend = ?1")?;
            let rows = stmt.query_map([&key], |row| row.get(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for share_id in &share_ids {
            // tombstone BEFORE the row is gone (deletes travel by journal)
            super::journal_row(
                &conn,
                actor,
                "shared_item",
                "shared_items",
                share_id,
                "delete",
            )?;
            conn.execute("DELETE FROM shared_items WHERE id = ?1", [share_id])?;
        }
        let now = next_stamp(&conn, id)?;
        conn.execute(
            "UPDATE friend_groups SET deleted_at = ?2, updated_at = ?2, members = '[]', sharing = '{}'
             WHERE id = ?1",
            rusqlite::params![id, now],
        )?;
        super::journal_row(&conn, actor, "friend_group", "friend_groups", id, "delete")?;
        record_mutation(
            &conn,
            actor,
            "group_deleted",
            &format!("group_deleted:{id}:{now}"),
            &json!({"id": id, "name": group.name, "unshared": share_ids.len()}),
        )?;
        Ok(true)
    }

    /// Friend surface: the groups this caller belongs to — name and
    /// roster, as this record names its connections. Liveness-gated.
    pub fn network_groups(&self, friend: &str) -> Result<Vec<Value>> {
        self.effective_grant(friend)?;
        let out: Vec<Value> = self
            .groups_of(friend)?
            .into_iter()
            .map(|g| json!({"id": g.id, "name": g.name, "members": g.members, "updated_at": g.updated_at}))
            .collect();
        self.log_network_access_pub(friend, "groups", &json!({"n": out.len()}))?;
        Ok(out)
    }

    /* ————— the member side: what a friend's node says ————— */

    /// Replace everything known about the groups `friend`'s record has
    /// put this record in. The friend's node is the only authority, so a
    /// group missing from its answer is gone (deleted, or we were removed).
    pub fn set_peer_groups(&self, friend: &str, groups: &[Value]) -> Result<usize> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM peer_groups WHERE friend = ?1", [friend])?;
        let now = now_utc();
        let mut kept = 0usize;
        for group in groups {
            let (Some(id), Some(name)) = (
                group.get("id").and_then(|v| v.as_str()),
                group.get("name").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let members: Vec<String> = group
                .get("members")
                .and_then(|m| m.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .take(500)
                        .collect()
                })
                .unwrap_or_default();
            kept += tx.execute(
                "INSERT OR REPLACE INTO peer_groups (friend, group_id, name, members, updated_at, fetched_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    friend,
                    id.chars().take(80).collect::<String>(),
                    name.chars().take(MAX_NAME_CHARS).collect::<String>(),
                    json!(members).to_string(),
                    group.get("updated_at").and_then(|v| v.as_str()),
                    now,
                ],
            )?;
        }
        tx.commit()?;
        Ok(kept)
    }

    /// Groups other people's records have put this record in.
    pub fn peer_groups(&self) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT friend, group_id, name, members, fetched_at FROM peer_groups
             ORDER BY friend COLLATE NOCASE, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "owner": row.get::<_, String>(0)?,
                "id": row.get::<_, String>(1)?,
                "name": row.get::<_, String>(2)?,
                "members": serde_json::from_str::<Value>(&row.get::<_, String>(3)?).unwrap_or_else(|_| json!([])),
                "fetched_at": row.get::<_, String>(4)?,
            }))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Forget a friend's groups (the connection was removed).
    pub fn clear_peer_groups(&self, friend: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM peer_groups WHERE friend = ?1", [friend])?;
        Ok(())
    }

    /* ————— peer sync (rides /v1/sync/sharing) ————— */

    /// Every group row, tombstones included, on every page: there are
    /// few, the apply is idempotent, and `next_stamp` can run a second
    /// or two ahead of the clock — so group stamps must stay out of the
    /// leg's shared `since` cursor (a cursor in the future would skip
    /// grants and shares written in the meantime).
    pub(crate) fn groups_sync_export(conn: &rusqlite::Connection) -> Result<Vec<Value>> {
        let mut stmt = conn.prepare(
            "SELECT id, name, members, sharing, created_at, updated_at, deleted_at
             FROM friend_groups ORDER BY updated_at DESC LIMIT 500",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "members": row.get::<_, String>(2)?,
                "sharing": row.get::<_, Option<String>>(3)?,
                "created_at": row.get::<_, String>(4)?,
                "updated_at": row.get::<_, String>(5)?,
                "deleted_at": row.get::<_, Option<String>>(6)?,
            }))
        })?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(Into::into)
    }

    /// Last-writer-wins by `updated_at`, per group.
    pub(crate) fn groups_sync_apply(tx: &rusqlite::Transaction, batch: &Value) -> Result<usize> {
        let mut applied = 0usize;
        for g in batch
            .get("groups")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(name), Some(stamp)) = (
                g.get("id").and_then(|v| v.as_str()),
                g.get("name").and_then(|v| v.as_str()),
                g.get("updated_at").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            if !valid_id(id) {
                continue;
            }
            let sharing = g
                .get("sharing")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .and_then(|v| validate_sharing(&v).ok())
                .unwrap_or_else(|| json!({}));
            applied += tx.execute(
                "INSERT INTO friend_groups (id, name, members, sharing, created_at, updated_at, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, members = excluded.members,
                     sharing = excluded.sharing, updated_at = excluded.updated_at,
                     deleted_at = excluded.deleted_at
                 WHERE excluded.updated_at > friend_groups.updated_at",
                rusqlite::params![
                    id,
                    name,
                    g.get("members").and_then(|v| v.as_str()).unwrap_or("[]"),
                    sharing.to_string(),
                    g.get("created_at").and_then(|v| v.as_str()).unwrap_or(stamp),
                    stamp,
                    g.get("deleted_at").and_then(|v| v.as_str()),
                ],
            )?;
        }
        Ok(applied)
    }
}
