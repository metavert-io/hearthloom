//! Browse: faceted, paged reads over the raw tables — the data explorer's
//! workbench queries — plus the source registry (materialized vs virtual).
//!
//! Facet counts follow the standard rule: each dimension's counts are
//! computed with every filter applied *except its own*, so chips within a
//! dimension stay clickable after one is selected.

use anyhow::Result;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::episodes::{episode_from_row, EpisodeRow, EPISODE_COLUMNS};
use super::Store;
use crate::time::now_utc;

/// Common filter set for the episode grid. Empty vecs mean "no filter".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EpisodeFilter {
    pub sources: Vec<String>,
    pub kinds: Vec<String>,
    pub scopes: Vec<String>,
    pub trusts: Vec<String>,
    /// Inclusive YYYY-MM-DD bounds on COALESCE(event_time, ingest_time).
    pub from: Option<String>,
    pub to: Option<String>,
    /// Case-insensitive substring over the title.
    pub q: Option<String>,
    /// Substring over cited Place entity names (the Atlas's tabular twin).
    pub place: Option<String>,
    /// Also list retired and replaced memories (labelled in `lifecycle`).
    pub include_retired: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FacetCount {
    pub value: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EpisodePage {
    pub total: i64,
    pub items: Vec<EpisodeRow>,
    pub facets: EpisodeFacets,
    /// id → `superseded_by` for the page's retired (`retired:<id>`) and
    /// replaced rows; only filled when `include_retired`.
    pub lifecycle: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EpisodeFacets {
    pub source: Vec<FacetCount>,
    pub kind: Vec<FacetCount>,
    pub scope: Vec<FacetCount>,
    pub trust: Vec<FacetCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityBrowseRow {
    pub id: String,
    pub entity_type: String,
    pub name: String,
    pub scope: String,
    pub created_at: String,
    pub episode_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityPage {
    pub total: i64,
    pub items: Vec<EntityBrowseRow>,
    pub facets: EntityFacets,
}

#[derive(Debug, Clone, Serialize)]
pub struct EntityFacets {
    pub entity_type: Vec<FacetCount>,
    pub scope: Vec<FacetCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityPage {
    pub total: i64,
    pub items: Vec<IdentityBrowseRow>,
    pub facets: IdentityFacets,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityFacets {
    pub kind: Vec<FacetCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdentityBrowseRow {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: String,
    pub kind: String,
    pub value: String,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgePage {
    pub total: i64,
    pub items: Vec<EdgeBrowseRow>,
    pub facets: EdgeFacets,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeFacets {
    pub relation: Vec<FacetCount>,
    pub status: Vec<FacetCount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EdgeBrowseRow {
    pub id: String,
    pub src_name: String,
    pub relation: String,
    pub dst_name: String,
    pub scope: String,
    pub status: String, // current|invalidated|expired
    pub t_valid: Option<String>,
    pub t_invalid: Option<String>,
    pub t_created: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceRow {
    pub name: String,
    pub kind: String,
    pub adapter: Option<String>,
    pub capabilities: Option<String>,
    pub first_seen: String,
    pub last_refresh: Option<String>,
    pub episode_count: i64,
    /// The retirement that retired this whole source (D.12), if any.
    pub retired_by: Option<String>,
}

/// A virtual source's full definition, including the `config` blob that
/// `list_sources`/`SourceRow` intentionally omit (it can carry a mount
/// path, endpoint, credentials-ref, trust tier). Used by the daemon to
/// query the origin at need and to promote fetched results.
#[derive(Debug, Clone, Serialize)]
pub struct VirtualSourceDef {
    pub name: String,
    pub adapter: String,
    pub config: serde_json::Value,
    pub capabilities: Vec<String>,
}

/// One WHERE fragment + its positional params.
struct Clause {
    /// Which facet dimension this clause filters (facets skip their own).
    dim: &'static str,
    sql: String,
    params: Vec<SqlValue>,
}

fn in_clause(dim: &'static str, column: &str, values: &[String]) -> Option<Clause> {
    if values.is_empty() {
        return None;
    }
    let marks = vec!["?"; values.len()].join(",");
    Some(Clause {
        dim,
        sql: format!("{column} IN ({marks})"),
        params: values.iter().map(|v| SqlValue::from(v.clone())).collect(),
    })
}

fn assemble(clauses: &[Clause], skip_dim: &str) -> (String, Vec<SqlValue>) {
    let mut fragments = Vec::new();
    let mut params = Vec::new();
    for clause in clauses {
        if clause.dim == skip_dim {
            continue;
        }
        fragments.push(clause.sql.clone());
        params.extend(clause.params.iter().cloned());
    }
    let where_sql = if fragments.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", fragments.join(" AND "))
    };
    (where_sql, params)
}

fn facet(
    conn: &Connection,
    table_sql: &str,
    column: &str,
    clauses: &[Clause],
    dim: &str,
) -> Result<Vec<FacetCount>> {
    let (where_sql, params) = assemble(clauses, dim);
    let mut stmt = conn.prepare(&format!(
        "SELECT {column}, COUNT(*) FROM {table_sql} {where_sql}
         GROUP BY {column} ORDER BY 2 DESC, 1"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok(FacetCount {
            value: row.get(0)?,
            count: row.get(1)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn episode_clauses(filter: &EpisodeFilter) -> Vec<Clause> {
    let mut clauses = Vec::new();
    clauses.extend(in_clause("source", "source", &filter.sources));
    clauses.extend(in_clause("kind", "kind", &filter.kinds));
    clauses.extend(in_clause("scope", "scope", &filter.scopes));
    clauses.extend(in_clause("trust", "trust", &filter.trusts));
    if let Some(from) = filter.from.as_deref().filter(|s| !s.is_empty()) {
        clauses.push(Clause {
            dim: "time",
            sql: "substr(COALESCE(event_time, ingest_time), 1, 10) >= ?".into(),
            params: vec![SqlValue::from(from.to_string())],
        });
    }
    if let Some(to) = filter.to.as_deref().filter(|s| !s.is_empty()) {
        clauses.push(Clause {
            dim: "time",
            sql: "substr(COALESCE(event_time, ingest_time), 1, 10) <= ?".into(),
            params: vec![SqlValue::from(to.to_string())],
        });
    }
    if let Some(q) = filter.q.as_deref().filter(|s| !s.trim().is_empty()) {
        clauses.push(Clause {
            dim: "q",
            sql: "title LIKE ? ESCAPE '\\'".into(),
            params: vec![SqlValue::from(format!(
                "%{}%",
                q.trim()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            ))],
        });
    }
    if let Some(place) = filter.place.as_deref().filter(|s| !s.trim().is_empty()) {
        clauses.push(Clause {
            dim: "place",
            sql: "id IN (SELECT ee.episode_id FROM episode_entities ee                   JOIN entities en ON en.id = ee.entity_id                   WHERE en.type = 'Place' AND en.name LIKE ? ESCAPE '\\') /* all-episodes: a filter inside a query guarded where it is used */"
                .into(),
            params: vec![SqlValue::from(format!(
                "%{}%",
                place.trim().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
            ))],
        });
    }
    clauses
}

/// Whitelisted sort keys → SQL expressions (never interpolate user input).
fn episode_sort(sort: &str, dir: &str) -> String {
    let expr = match sort {
        "ingest" => "ingest_time",
        "source" => "source",
        "kind" => "kind",
        "title" => "title",
        "scope" => "scope",
        _ => "COALESCE(event_time, ingest_time)",
    };
    let dir = if dir.eq_ignore_ascii_case("asc") {
        "ASC"
    } else {
        "DESC"
    };
    format!("{expr} {dir}, id {dir}")
}

impl Store {
    pub fn browse_episodes(
        &self,
        filter: &EpisodeFilter,
        sort: &str,
        dir: &str,
        offset: usize,
        limit: usize,
    ) -> Result<EpisodePage> {
        let hide_private = !self.private_visible();
        let conn = self.lock();
        let mut clauses = episode_clauses(filter);
        if !filter.include_retired {
            clauses.push(Clause {
                dim: "lifecycle",
                sql: "superseded_by IS NULL".into(),
                params: vec![],
            });
        }
        if hide_private {
            // dim "vault" matches no facet dimension, so the guard applies
            // to every facet count too — locked mode leaks nothing.
            clauses.push(Clause {
                dim: "vault",
                sql: "scope != 'private'".into(),
                params: vec![],
            });
        }
        let (where_sql, params) = assemble(&clauses, "");

        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM episodes /* all-episodes: the forensic workbench — its lifecycle clause hides retired and replaced rows unless asked */ {where_sql}"),
            rusqlite::params_from_iter(params.clone()),
            |row| row.get(0),
        )?;

        let order = episode_sort(sort, dir);
        let mut stmt = conn.prepare(&format!(
            "SELECT {EPISODE_COLUMNS} FROM episodes /* all-episodes: the forensic workbench — its lifecycle clause hides retired and replaced rows unless asked */ {where_sql}
             ORDER BY {order} LIMIT {} OFFSET {}",
            limit.min(200),
            offset
        ))?;
        let items = stmt
            .query_map(rusqlite::params_from_iter(params), episode_from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let facets = EpisodeFacets {
            source: facet(&conn, "episodes", "source", &clauses, "source")?,
            kind: facet(&conn, "episodes", "kind", &clauses, "kind")?,
            scope: facet(&conn, "episodes", "scope", &clauses, "scope")?,
            trust: facet(&conn, "episodes", "trust", &clauses, "trust")?,
        };
        let mut lifecycle = std::collections::HashMap::new();
        if filter.include_retired && !items.is_empty() {
            let marks = vec!["?"; items.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, superseded_by FROM episodes
                 WHERE id IN ({marks}) AND superseded_by IS NOT NULL"
            ))?;
            let rows = stmt.query_map(
                rusqlite::params_from_iter(items.iter().map(|e| e.id.as_str())),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?;
            for row in rows {
                let (id, state) = row?;
                lifecycle.insert(id, state);
            }
        }
        Ok(EpisodePage {
            total,
            items,
            facets,
            lifecycle,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn browse_entities(
        &self,
        types: &[String],
        scopes: &[String],
        q: Option<&str>,
        sort: &str,
        dir: &str,
        offset: usize,
        limit: usize,
    ) -> Result<EntityPage> {
        let hide_private = !self.private_visible();
        let conn = self.lock();
        let mut clauses = Vec::new();
        clauses.push(Clause {
            dim: "merged",
            sql: "merged_into IS NULL".into(),
            params: vec![],
        });
        // An entity a retirement hid (it existed only because of the
        // retired memory) stays out of every list until the undo.
        clauses.push(Clause {
            dim: "hidden",
            sql: "hidden_by IS NULL".into(),
            params: vec![],
        });
        if hide_private {
            clauses.push(Clause {
                dim: "vault",
                sql: "scope != 'private'".into(),
                params: vec![],
            });
        }
        clauses.extend(in_clause("entity_type", "type", types));
        clauses.extend(in_clause("scope", "scope", scopes));
        if let Some(q) = q.filter(|s| !s.trim().is_empty()) {
            clauses.push(Clause {
                dim: "q",
                sql: "name LIKE ? ESCAPE '\\'".into(),
                params: vec![SqlValue::from(format!(
                    "%{}%",
                    q.trim()
                        .replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                ))],
            });
        }
        let (where_sql, params) = assemble(&clauses, "");

        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM entities {where_sql}"),
            rusqlite::params_from_iter(params.clone()),
            |row| row.get(0),
        )?;

        let order = match sort {
            "name" => format!(
                "name COLLATE NOCASE {}",
                if dir.eq_ignore_ascii_case("desc") {
                    "DESC"
                } else {
                    "ASC"
                }
            ),
            "created" => format!(
                "created_at {}",
                if dir.eq_ignore_ascii_case("asc") {
                    "ASC"
                } else {
                    "DESC"
                }
            ),
            _ => format!(
                "episode_count {}",
                if dir.eq_ignore_ascii_case("asc") {
                    "ASC"
                } else {
                    "DESC"
                }
            ),
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT id, type, name, scope, created_at,
                    (SELECT COUNT(DISTINCT episode_id) FROM episode_entities
                     WHERE entity_id = entities.id
                       AND episode_id NOT IN (SELECT id FROM episodes WHERE superseded_by IS NOT NULL)) AS episode_count
             FROM entities {where_sql}
             ORDER BY {order}, id LIMIT {} OFFSET {}",
            limit.min(200),
            offset
        ))?;
        let items = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                Ok(EntityBrowseRow {
                    id: row.get(0)?,
                    entity_type: row.get(1)?,
                    name: row.get(2)?,
                    scope: row.get(3)?,
                    created_at: row.get(4)?,
                    episode_count: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let facets = EntityFacets {
            entity_type: facet(&conn, "entities", "type", &clauses, "entity_type")?,
            scope: facet(&conn, "entities", "scope", &clauses, "scope")?,
        };
        Ok(EntityPage {
            total,
            items,
            facets,
        })
    }

    /// Identities grid: every claimed/observed identifying attribute.
    pub fn browse_identities(
        &self,
        kinds: &[String],
        q: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<IdentityPage> {
        let hide_private = !self.private_visible();
        let conn = self.lock();
        let mut clauses = Vec::new();
        if hide_private {
            clauses.push(Clause {
                dim: "vault",
                sql: "(e.scope != 'private' AND COALESCE(a.scope, '') != 'private')".into(),
                params: vec![],
            });
        }
        clauses.extend(in_clause("kind", "a.kind", kinds));
        if let Some(q) = q.filter(|s| !s.trim().is_empty()) {
            let like = format!(
                "%{}%",
                q.trim()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            clauses.push(Clause {
                dim: "q",
                sql: "(a.value LIKE ?1 ESCAPE '\\' OR e.name LIKE ?1 ESCAPE '\\')".into(),
                params: vec![SqlValue::from(like)],
            });
        }
        let table = "entity_attributes a JOIN entities e ON e.id = a.entity_id";
        let (where_sql, params) = assemble(&clauses, "");
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} {where_sql}"),
            rusqlite::params_from_iter(params.clone()),
            |row| row.get(0),
        )?;
        let mut stmt = conn.prepare(&format!(
            "SELECT a.entity_id, e.name, e.type, a.kind, a.value, a.scope
             FROM {table} {where_sql}
             ORDER BY a.kind, a.value LIMIT {} OFFSET {}",
            limit.min(200),
            offset
        ))?;
        let items = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                Ok(IdentityBrowseRow {
                    entity_id: row.get(0)?,
                    entity_name: row.get(1)?,
                    entity_type: row.get(2)?,
                    kind: row.get(3)?,
                    value: row.get(4)?,
                    scope: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let kinds_facet = facet(&conn, table, "a.kind", &clauses, "kind")?;
        Ok(IdentityPage {
            total,
            items,
            facets: IdentityFacets { kind: kinds_facet },
        })
    }

    /// Edges grid with bi-temporal status.
    pub fn browse_edges(
        &self,
        relations: &[String],
        statuses: &[String],
        offset: usize,
        limit: usize,
    ) -> Result<EdgePage> {
        let hide_private = !self.private_visible();
        let conn = self.lock();
        let status_expr = "CASE WHEN t_expired IS NOT NULL THEN 'expired'
                                WHEN t_invalid IS NOT NULL THEN 'invalidated'
                                ELSE 'current' END";
        let mut clauses = Vec::new();
        if hide_private {
            clauses.push(Clause {
                dim: "vault",
                sql: "scope != 'private'".into(),
                params: vec![],
            });
        }
        clauses.extend(in_clause("relation", "relation", relations));
        clauses.extend(in_clause("status", &format!("({status_expr})"), statuses));
        let (where_sql, params) = assemble(&clauses, "");
        let total: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM edges {where_sql}"),
            rusqlite::params_from_iter(params.clone()),
            |row| row.get(0),
        )?;
        let mut stmt = conn.prepare(&format!(
            "SELECT g.id,
                    COALESCE((SELECT name FROM entities WHERE id = g.src), g.src),
                    g.relation,
                    COALESCE((SELECT name FROM entities WHERE id = g.dst), g.dst),
                    g.scope,
                    CASE WHEN g.t_expired IS NOT NULL THEN 'expired'
                         WHEN g.t_invalid IS NOT NULL THEN 'invalidated'
                         ELSE 'current' END,
                    g.t_valid, g.t_invalid, g.t_created
             FROM (SELECT * FROM edges {where_sql}) g
             ORDER BY g.t_created DESC, g.id LIMIT {} OFFSET {}",
            limit.min(200),
            offset
        ))?;
        let items = stmt
            .query_map(rusqlite::params_from_iter(params), |row| {
                Ok(EdgeBrowseRow {
                    id: row.get(0)?,
                    src_name: row.get(1)?,
                    relation: row.get(2)?,
                    dst_name: row.get(3)?,
                    scope: row.get(4)?,
                    status: row.get(5)?,
                    t_valid: row.get(6)?,
                    t_invalid: row.get(7)?,
                    t_created: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let relation_facet = facet(&conn, "edges", "relation", &clauses, "relation")?;
        let status_facet = facet(&conn, "edges", status_expr, &clauses, "status")?;
        Ok(EdgePage {
            total,
            items,
            facets: EdgeFacets {
                relation: relation_facet,
                status: status_facet,
            },
        })
    }

    /// Sync the source registry from the episode log: one materialized row
    /// per distinct episode source. Virtual rows are untouched (they have
    /// no episodes until promotion). Cheap; called lazily before listing.
    pub fn sync_sources(&self) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sources (name, kind, first_seen, last_refresh, episode_count)
             SELECT source, 'materialized', MIN(ingest_time), MAX(ingest_time), COUNT(*)
             FROM episodes /* all-episodes: the registry tracks every source that ever had episodes */
             GROUP BY source
             ON CONFLICT(name) DO UPDATE SET
                 last_refresh = excluded.last_refresh,
                 episode_count = excluded.episode_count",
            [],
        )?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRow>> {
        // Locked, the displayed counts exclude private rows (computed live;
        // the stored sync count stays the registry's bookkeeping value).
        // Live counts: retired and replaced memories don't count.
        let count_expr = if self.private_visible() {
            "(SELECT COUNT(*) FROM episodes e
              WHERE e.source = sources.name AND e.superseded_by IS NULL)"
        } else {
            "(SELECT COUNT(*) FROM episodes e
              WHERE e.source = sources.name AND e.scope != 'private' AND e.superseded_by IS NULL)"
        };
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT name, kind, adapter, capabilities, first_seen, last_refresh, {count_expr}, retired_by
             FROM sources ORDER BY 7 DESC, name"
        ))?;
        let rows = stmt.query_map([], |row| {
            Ok(SourceRow {
                name: row.get(0)?,
                kind: row.get(1)?,
                adapter: row.get(2)?,
                capabilities: row.get(3)?,
                first_seen: row.get(4)?,
                last_refresh: row.get(5)?,
                episode_count: row.get(6)?,
                retired_by: row.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Register (or update) a virtual source — data that stays at origin
    /// and is queried at need. Surfaces as an RLM tool handle, never as
    /// substrate the system depends on.
    pub fn register_virtual_source(
        &self,
        actor: &super::Actor,
        name: &str,
        adapter: &str,
        config: &serde_json::Value,
        capabilities: &[String],
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sources (name, kind, adapter, config, capabilities, first_seen)
             VALUES (?1, 'virtual', ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
                 adapter = excluded.adapter,
                 config = excluded.config,
                 capabilities = excluded.capabilities",
            rusqlite::params![
                name,
                adapter,
                config.to_string(),
                serde_json::json!(capabilities).to_string(),
                now_utc(),
            ],
        )?;
        super::record_mutation(
            &conn,
            actor,
            "source_registered",
            &format!("source_registered:{name}"),
            &serde_json::json!({"name": name, "kind": "virtual", "adapter": adapter}),
        )?;
        Ok(())
    }

    /// Read one virtual source's full definition (adapter + config +
    /// capabilities). Returns None if no virtual source by that name is
    /// registered. Not vault-guarded: a source *definition* holds no
    /// private episode content (the mount/endpoint is bookkeeping).
    pub fn virtual_source(&self, name: &str) -> Result<Option<VirtualSourceDef>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT name, adapter, config, capabilities
             FROM sources WHERE name = ?1 AND kind = 'virtual'",
        )?;
        let mut rows = stmt.query_map([name], row_to_virtual_source)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// All registered virtual sources — the RLM surfaces these as extra
    /// read handles, and Browse lists them alongside materialized sources.
    pub fn list_virtual_sources(&self) -> Result<Vec<VirtualSourceDef>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT name, adapter, config, capabilities
             FROM sources WHERE kind = 'virtual' ORDER BY name",
        )?;
        let rows = stmt.query_map([], row_to_virtual_source)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

/// Map a `(name, adapter, config, capabilities)` row into a definition,
/// tolerating malformed/empty JSON blobs (treated as `{}` / `[]`).
fn row_to_virtual_source(row: &rusqlite::Row) -> rusqlite::Result<VirtualSourceDef> {
    let name: String = row.get(0)?;
    let adapter: Option<String> = row.get(1)?;
    let config: Option<String> = row.get(2)?;
    let capabilities: Option<String> = row.get(3)?;
    Ok(VirtualSourceDef {
        name,
        adapter: adapter.unwrap_or_default(),
        config: config
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| serde_json::json!({})),
        capabilities: capabilities
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::{EpisodeDraft, EventTime, IngestBatch};

    fn seeded() -> (Store, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.db")).unwrap();
        let home = crate::paths::Home::at(tmp.path().to_path_buf());
        let episodes = vec![
            ("git", "c1", "commit", "fix the flux capacitor", "work"),
            ("git", "c2", "commit", "add browse grid", "work"),
            ("imessage", "m1", "messages", "dinner plans", "personal"),
            (
                "chrome",
                "b1",
                "browsing",
                "rust faceted search",
                "personal",
            ),
        ]
        .into_iter()
        .map(|(source, r, kind, title, scope)| EpisodeDraft {
            source: source.into(),
            source_ref: r.into(),
            kind: kind.into(),
            title: title.into(),
            body: title.into(),
            event_time: EventTime::unknown(),
            trust: "self".into(),
            scope: Some(scope.into()),
            entities: vec![],
            edges: vec![],
        })
        .collect();
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "human:test".into(),
                    session_id: None,
                    episodes,
                },
            )
            .unwrap();
        (store, tmp)
    }

    #[test]
    fn test_browse_episodes_filters_and_facets() {
        let (store, _tmp) = seeded();
        let page = store
            .browse_episodes(&EpisodeFilter::default(), "time", "desc", 0, 50)
            .unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(page.items.len(), 4);
        assert_eq!(page.facets.source.len(), 3);

        let filtered = store
            .browse_episodes(
                &EpisodeFilter {
                    sources: vec!["git".into()],
                    ..Default::default()
                },
                "time",
                "desc",
                0,
                50,
            )
            .unwrap();
        assert_eq!(filtered.total, 2);
        // Own dimension keeps the unfiltered counts (chips stay clickable)…
        assert_eq!(filtered.facets.source.len(), 3);
        // …other dimensions reflect the source filter.
        assert!(filtered.facets.scope.iter().all(|f| f.value == "work"));

        let searched = store
            .browse_episodes(
                &EpisodeFilter {
                    q: Some("browse".into()),
                    ..Default::default()
                },
                "title",
                "asc",
                0,
                50,
            )
            .unwrap();
        assert_eq!(searched.total, 1);
        assert_eq!(searched.items[0].source_ref, "c2");
    }

    #[test]
    fn test_source_registry_sync() {
        let (store, _tmp) = seeded();
        store.sync_sources().unwrap();
        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 3);
        assert!(sources.iter().all(|s| s.kind == "materialized"));
        let git = sources.iter().find(|s| s.name == "git").unwrap();
        assert_eq!(git.episode_count, 2);

        // Virtual registration survives a materialized sync.
        store
            .register_virtual_source(
                &crate::store::Actor::new("human:test"),
                "hearthfs",
                "hearthfs",
                &serde_json::json!({"mount": "/tmp/hfs"}),
                &["read".into(), "search".into()],
            )
            .unwrap();
        store.sync_sources().unwrap();
        let sources = store.list_sources().unwrap();
        let hfs = sources.iter().find(|s| s.name == "hearthfs").unwrap();
        assert_eq!(hfs.kind, "virtual");
        assert_eq!(hfs.episode_count, 0);
    }

    #[test]
    fn test_browse_entities_and_identities() {
        let (store, _tmp) = seeded();
        let actor = crate::store::Actor::new("human:test");
        store
            .claim_identity(&actor, "Jon", "email", "jon@example.com", Some("personal"))
            .unwrap();
        let page = store
            .browse_entities(&[], &[], None, "episodes", "desc", 0, 50)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].name, "Jon");

        let ids = store.browse_identities(&[], Some("jon@"), 0, 50).unwrap();
        assert_eq!(ids.total, 1);
        assert_eq!(ids.items[0].value, "jon@example.com");
        assert_eq!(ids.facets.kind[0].value, "email");
    }
}
