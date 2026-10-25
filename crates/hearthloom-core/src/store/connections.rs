//! The social-connections registry (v23, enriched v25): who a network
//! SAYS you're connected to, kept apart from the entity graph. A friend
//! list is a claim, not evidence — evidence (messages, tags) mints
//! entities via ingest; everyone else waits here, surfaced as dossier
//! chips and search hits, materializing only on an explicit human
//! gesture. v25 adds what the network knows (company/position/handle —
//! LinkedIn exports them per row): disambiguation for names that will
//! mostly never become entities.

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

/// One row headed for the registry (adapter-shaped, network-agnostic).
#[derive(Debug, Clone, Default)]
pub struct ConnectionUpsert {
    pub name: String,
    pub connected_at: Option<String>,
    pub ended_at: Option<String>,
    pub status: String, // active | removed
    pub company: Option<String>,
    pub position: Option<String>,
    /// Email or profile URL — whatever identifies them best.
    pub handle: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SocialConnection {
    pub network: String,
    pub name: String,
    pub connected_at: Option<String>,
    pub ended_at: Option<String>,
    pub status: String,
    pub company: Option<String>,
    pub position: Option<String>,
    pub handle: Option<String>,
    /// The entity carrying this exact name, if one already exists —
    /// the UI shows "in the record" vs "registry only".
    pub entity_id: Option<String>,
}

impl Store {
    /// Bulk upsert one network's connection list. Idempotent per
    /// (network, name); re-imports refresh dates/status/details.
    pub fn upsert_social_connections(
        &self,
        actor: &Actor,
        network: &str,
        rows: &[ConnectionUpsert],
    ) -> Result<usize> {
        let conn = self.lock();
        let mut n = 0usize;
        for r in rows {
            conn.execute(
                "INSERT INTO social_connections
                     (network, name, connected_at, ended_at, status, company, position, handle)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(network, name) DO UPDATE SET
                     connected_at = COALESCE(excluded.connected_at, connected_at),
                     ended_at = COALESCE(excluded.ended_at, ended_at),
                     status = excluded.status,
                     company = COALESCE(excluded.company, company),
                     position = COALESCE(excluded.position, position),
                     handle = COALESCE(excluded.handle, handle)",
                rusqlite::params![
                    network,
                    r.name,
                    r.connected_at,
                    r.ended_at,
                    r.status,
                    r.company,
                    r.position,
                    r.handle
                ],
            )?;
            n += 1;
        }
        record_mutation(
            &conn,
            actor,
            "social_connections_upserted",
            &format!("social_connections:{network}:{}", now_utc()),
            &json!({"network": network, "rows": n}),
        )?;
        Ok(n)
    }

    /// Search the registry (q matches name/company/position) or look up
    /// one exact name.
    pub fn social_connections(
        &self,
        q: Option<&str>,
        name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SocialConnection>> {
        let conn = self.lock();
        let (where_sql, param): (&str, String) = match (name, q) {
            (Some(n), _) => ("WHERE name = ?1 COLLATE NOCASE", n.to_string()),
            (None, Some(q)) if !q.trim().is_empty() => (
                "WHERE (name LIKE ?1 COLLATE NOCASE
                     OR company LIKE ?1 COLLATE NOCASE
                     OR position LIKE ?1 COLLATE NOCASE)",
                format!("%{}%", q.trim()),
            ),
            // full dumps aren't a surface; the registry is always filtered
            _ => return Ok(vec![]),
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT network, name, connected_at, ended_at, status, company, position, handle
             FROM social_connections {where_sql}
             ORDER BY status = 'active' DESC, connected_at DESC
             LIMIT {limit}"
        ))?;
        type Row = (
            String, String, Option<String>, Option<String>, String,
            Option<String>, Option<String>, Option<String>,
        );
        let base: Vec<Row> = stmt
            .query_map([&param], |row| {
                Ok((
                    row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                    row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        base.into_iter()
            .map(|(network, name, connected_at, ended_at, status, company, position, handle)| {
                let entity_id: Option<String> = conn
                    .query_row(
                        "SELECT id FROM entities
                         WHERE merged_into IS NULL AND lower(name) = lower(?1) LIMIT 1",
                        [&name],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(SocialConnection {
                    network,
                    name,
                    connected_at,
                    ended_at,
                    status,
                    company,
                    position,
                    handle,
                    entity_id,
                })
            })
            .collect()
    }

    /// The explicit human gesture: turn a registry row into a Person
    /// entity (attribute-stamped with its network handle when known).
    /// Returns the entity id — existing entities are reused, never
    /// duplicated.
    pub fn materialize_connection(
        &self,
        actor: &Actor,
        network: &str,
        name: &str,
    ) -> Result<String> {
        let conn = self.lock();
        let known: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT connected_at, handle FROM social_connections
                 WHERE network = ?1 AND name = ?2 COLLATE NOCASE",
                [network, name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((connected_at, handle)) = known else {
            anyhow::bail!("no {network} connection named {name:?} in the registry");
        };
        let mut attrs = Vec::new();
        if let Some(handle) = handle.filter(|h| !h.is_empty()) {
            let kind = if handle.contains('@') && !handle.starts_with("http") {
                "email".to_string()
            } else {
                format!("{network}-url")
            };
            attrs.push((kind, handle.to_lowercase()));
        }
        attrs.push((
            format!("{network}-friend"),
            format!("{}:{}", name.to_lowercase(), connected_at.as_deref().unwrap_or("?")),
        ));
        let (id, _created, _queued) =
            super::graph::upsert_entity(&conn, actor, "Person", name, "personal", &attrs)?;
        Ok(id)
    }
}
