//! Projects: a `Project` is a first-class ENTITY that CONTAINS work —
//! git repos (sometimes 1:1, sometimes many), and by extension the
//! tasks, notes and commits that cite it. Composition is bi-temporal
//! `contains` edges (project → repo), so "one repo or many" needs no
//! special case. Declared projects are human-made; the auto-minted repo
//! Projects (one per git remote) can be composed under a declared one,
//! or merged into it for the 1:1 case. Distinct from GOALS: a goal is
//! an aim with a watch-query; a project is a container of work.

use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;

use super::graph::{resolve_merged, upsert_entity};
use super::{record_mutation, Actor, Store};
use crate::ids;
use crate::time::now_utc;

#[derive(Debug, Clone, Serialize)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub declared_importance: i64,
    /// Repos composed under it (id, name).
    pub repos: Vec<(String, String)>,
    /// Recent evidence count (episodes citing the project or its repos).
    pub evidence: i64,
    pub last_activity: Option<String>,
}

impl Store {
    /// Declare a project by name (idempotent — an existing Project of
    /// that name is reused). Returns the entity id.
    pub fn declare_project(&self, actor: &Actor, name: &str) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("project name is required"));
        }
        let conn = self.lock();
        let (id, _created, _queued) = upsert_entity(&conn, actor, "Project", name, "work", &[])?;
        // stamp declared_importance ≥1 so it outranks auto-minted repos
        // in the galaxy/rankings (a named project is a deliberate thing)
        conn.execute(
            "UPDATE entities SET declared_importance = MAX(declared_importance, 1)
             WHERE id = ?1",
            [&id],
        )?;
        // Declaring a project named like an existing repo ("Hearthloom"
        // vs the auto-minted "hearthloom") is deliberate, not a
        // duplicate — a project CONTAINS its repo. Retire any pending
        // case-name merge review pairing this project with a same-named
        // entity so the sweep never eats it and Tend stays clean.
        conn.execute(
            "UPDATE merge_review_queue SET resolution = 'distinct'
             WHERE resolution IS NULL
               AND (entity_a = ?1 OR entity_b = ?1)
               AND lower((SELECT name FROM entities WHERE id = entity_a))
                 = lower((SELECT name FROM entities WHERE id = entity_b))",
            [&id],
        )?;
        record_mutation(
            &conn,
            actor,
            "project_declared",
            &format!("project_declared:{id}"),
            &json!({"id": id, "name": name}),
        )?;
        Ok(id)
    }

    /// Compose one entity (a repo Project, usually) under a project via
    /// a `contains` edge. Both are resolved through merge groups.
    pub fn attach_to_project(&self, actor: &Actor, project_id: &str, child_id: &str) -> Result<()> {
        let conn = self.lock();
        let project = resolve_merged(&conn, project_id)?;
        let child = resolve_merged(&conn, child_id)?;
        if project == child {
            return Err(anyhow!("a project cannot contain itself"));
        }
        let now = now_utc();
        conn.execute(
            "INSERT INTO edges (id, src, relation, dst, strength, scope, t_valid, t_created)
             SELECT ?1, ?2, 'contains', ?3, 1.0, 'work', ?4, ?4
             WHERE NOT EXISTS (
                 SELECT 1 FROM edges WHERE src = ?2 AND relation = 'contains'
                     AND dst = ?3 AND t_expired IS NULL)",
            rusqlite::params![ids::edge_id(), project, child, now],
        )?;
        record_mutation(
            &conn,
            actor,
            "project_attach",
            &format!("project_attach:{project}:{child}"),
            &json!({"project": project, "child": child}),
        )?;
        Ok(())
    }

    /// Remove a repo from a project (retract the edge — bi-temporal, not
    /// deleted).
    pub fn detach_from_project(
        &self,
        actor: &Actor,
        project_id: &str,
        child_id: &str,
    ) -> Result<()> {
        let conn = self.lock();
        let project = resolve_merged(&conn, project_id)?;
        let child = resolve_merged(&conn, child_id)?;
        let now = now_utc();
        conn.execute(
            "UPDATE edges SET t_expired = ?3
             WHERE src = ?1 AND relation = 'contains' AND dst = ?2 AND t_expired IS NULL",
            rusqlite::params![project, child, now],
        )?;
        record_mutation(
            &conn,
            actor,
            "project_detach",
            &format!("project_detach:{project}:{child}:{now}"),
            &json!({"project": project, "child": child}),
        )?;
        Ok(())
    }

    /// All declared projects with their composed repos + activity, for
    /// the Desk rail. Vault-guarded.
    pub fn list_projects(&self) -> Result<Vec<ProjectRow>> {
        let guard = self.scope_guard("scope");
        let conn = self.lock();
        // declared projects = Project entities with declared_importance>0
        let mut stmt = conn.prepare(&format!(
            "SELECT id, name, declared_importance FROM entities
             WHERE type = 'Project' AND merged_into IS NULL
               AND declared_importance > 0{guard}
             ORDER BY declared_importance DESC, name"
        ))?;
        let base: Vec<(String, String, i64)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = Vec::new();
        for (id, name, importance) in base {
            let repos = self.project_repos(&conn, &id)?;
            // evidence: episodes citing the project OR any composed repo
            let mut ids = vec![id.clone()];
            ids.extend(repos.iter().map(|(rid, _)| rid.clone()));
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let params = rusqlite::params_from_iter(ids.iter());
            let (evidence, last): (i64, Option<String>) = conn.query_row(
                &format!(
                    "SELECT COUNT(*), MAX(substr(COALESCE(ep.event_time, ep.ingest_time),1,10))
                     FROM episode_entities ee JOIN episodes ep ON ep.id = ee.episode_id
                     WHERE ee.entity_id IN ({placeholders}) AND ep.superseded_by IS NULL"
                ),
                params,
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            out.push(ProjectRow {
                id,
                name,
                declared_importance: importance,
                repos,
                evidence,
                last_activity: last,
            });
        }
        Ok(out)
    }

    fn project_repos(
        &self,
        conn: &rusqlite::Connection,
        project_id: &str,
    ) -> Result<Vec<(String, String)>> {
        // Only repo-like children count as repos: a git repo is a
        // Project entity. Filtering by type keeps the file Artifacts a
        // repo itself contains from ever showing up as "repos" (the
        // bug where a repo dossier listed lib.rs, DESIGN.md, … as repos).
        let mut stmt = conn.prepare(
            "SELECT en.id, en.name FROM edges ed
             JOIN entities en ON en.id = ed.dst AND en.merged_into IS NULL
             WHERE ed.src = ?1 AND ed.relation = 'contains' AND ed.t_expired IS NULL
               AND en.type = 'Project'
             ORDER BY en.name",
        )?;
        let rows = stmt.query_map([project_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The composed repos for one project (dossier + detach UI).
    pub fn project_detail(&self, entity_id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.lock();
        let winner = resolve_merged(&conn, entity_id)?;
        self.project_repos(&conn, &winner)
    }
}
