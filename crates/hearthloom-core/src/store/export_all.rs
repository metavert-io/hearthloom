//! Full-export table classification — THE ENSHRINED CONTRACT.
//!
//! Every table in the schema MUST be classified here as Exported or
//! Excluded(reason); the test below walks sqlite_master and FAILS the
//! build the moment a migration adds a table nobody classified. That is
//! how "remember to hook new data into the exporter" stops being a
//! human obligation: you cannot ship an unclassified table. The daemon
//! side (export.rs) has the same contract for on-disk directories.

use anyhow::Result;
use serde_json::{Map, Value};

use super::Store;

pub enum TableClass {
    /// Dumped into the export zip as tables/<name>.json.
    Exported,
    /// Deliberately absent, with the reason a reader deserves.
    Excluded(&'static str),
}

/// name → class. Classify EVERYTHING; the test enforces coverage.
pub const TABLE_CLASSES: &[(&str, TableClass)] = &[
    ("episodes", TableClass::Exported),
    ("entities", TableClass::Exported),
    ("entity_attributes", TableClass::Exported),
    ("episode_entities", TableClass::Exported),
    ("edges", TableClass::Exported),
    ("edge_provenance", TableClass::Exported),
    ("merges", TableClass::Exported),
    ("merge_review_queue", TableClass::Exported),
    ("mutations", TableClass::Exported),
    ("op_journal", TableClass::Exported),
    // v44: live device-streamed HealthKit samples — PRIMARY data (there
    // is no export.xml to rebuild them from), so they must travel.
    ("health_sample", TableClass::Exported),
    // v46: explicit per-item friend shares — operational primary state.
    ("shared_items", TableClass::Exported),
    // v54: friend groups — operational primary state (names, rosters,
    // what each group grants).
    ("friend_groups", TableClass::Exported),
    // v55: agents. Configuration, policy and the record of what agents
    // were asked to do and what was approved — all primary state. The
    // agent secrets are NOT here (only their names; values are in
    // .env.sealed, which never exports).
    ("agents", TableClass::Exported),
    ("governance", TableClass::Exported),
    ("agent_tasks", TableClass::Exported),
    ("agent_approvals", TableClass::Exported),
    ("nodes", TableClass::Excluded("each node republishes its own row; a restore is a new node")),
    (
        "agent_directory",
        TableClass::Excluded("an index of which node hosts which agent; each node republishes its own"),
    ),
    // v47: photo-location union store — PRIMARY device data + the
    // minted-day ledger the supersession pass reasons from.
    ("photo_point", TableClass::Exported),
    ("photo_day", TableClass::Exported),
    // v48: raw-ingest audit trail — the device-stream verification
    // surface; cheap, append-only, part of the record's history.
    ("ingest_log", TableClass::Exported),
    // v50: token-accounting ledger — spend history is part of the
    // record (tokens only; cost computed at read).
    ("llm_usage", TableClass::Exported),
    // v51 device-feed wave: the unified location layer + its minted-day
    // ledger, the address-book backbone, and the attention stream.
    ("location_observation", TableClass::Exported),
    ("visit_day", TableClass::Exported),
    ("contact_card", TableClass::Exported),
    ("device_state", TableClass::Exported),
    ("identities", TableClass::Exported),
    ("quarantine", TableClass::Exported),
    ("sources", TableClass::Exported),
    ("privacy_rules", TableClass::Exported),
    ("ask_log", TableClass::Exported),
    ("location_points", TableClass::Exported),
    ("intents", TableClass::Exported),
    ("standing_queries", TableClass::Exported),
    ("opportunities", TableClass::Exported),
    ("self_model", TableClass::Exported),
    ("consolidation_state", TableClass::Exported),
    ("tasks", TableClass::Exported),
    ("task_templates", TableClass::Exported),
    ("task_mentions", TableClass::Exported),
    ("notes_index", TableClass::Exported),
    ("note_mentions", TableClass::Exported),
    ("note_links", TableClass::Exported),
    ("note_neighbors", TableClass::Exported),
    ("lint_findings", TableClass::Exported),
    ("metric_points", TableClass::Exported),
    ("relationships", TableClass::Exported),
    ("social_connections", TableClass::Exported),
    ("semantic_pairs", TableClass::Exported),
    // D.12 retirement: the event, this node's undo record (without it a
    // restored node could not undo), and what each summary read.
    ("retirements", TableClass::Exported),
    ("retirement_rows", TableClass::Exported),
    ("summary_inputs", TableClass::Exported),
    ("entity_renames", TableClass::Exported),
    ("notifications", TableClass::Exported),
    ("plugins", TableClass::Exported),
    ("plugin_kv", TableClass::Exported),
    ("extraction_verdicts", TableClass::Exported),
    ("syslog", TableClass::Exported),
    ("goal_projects", TableClass::Exported),
    ("grants", TableClass::Exported),
    ("health_points", TableClass::Exported),
    ("intent_matches", TableClass::Exported),
    // — excluded, each with its reason —
    ("users", TableClass::Excluded("credential hashes — secrets never export")),
    (
        "peer_groups",
        TableClass::Excluded("a cache of what each friend's node says; re-fetched from them"),
    ),
    ("sessions", TableClass::Excluded("live session tokens — secrets never export")),
    ("api_keys", TableClass::Excluded("key hashes/scopes — secrets never export")),
    ("key_requests", TableClass::Excluded("pending access requests + one-shot issued secrets — never export")),
    ("chunk_embeddings", TableClass::Excluded("rebuildable vectors; hundreds of MB of floats")),
    // all-episodes: a reason string, not a query
    ("chunks", TableClass::Excluded("derived text chunks — rebuilt from episodes")),
    // v49: the sqlite-vec ANN index — derived from chunks.embedding,
    // dual-written + consolidation-backfilled; rebuilds on any node.
    ("vec_chunks", TableClass::Excluded("rebuildable ANN index over chunk embeddings")),
];

/// FTS/vec0 shadow and internal tables sqlite creates itself — never
/// classified by hand, never exported (all derived, all rebuildable).
pub fn is_internal_table(name: &str) -> bool {
    name.starts_with("sqlite_")
        || name.contains("_fts")
        // vec_chunks itself IS classified above; these are its vec0
        // shadow tables (vec_chunks_info, _chunks, _rowids, …).
        || name.starts_with("vec_chunks_")
        || name.ends_with("_data")
        || name.ends_with("_idx")
        || name.ends_with("_docsize")
        || name.ends_with("_config")
        || name.ends_with("_content")
}

impl Store {
    /// All real tables in the live schema.
    pub fn all_table_names(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names.into_iter().filter(|n| !is_internal_table(n)).collect())
    }

    pub fn table_row_count(&self, table: &str) -> Result<i64> {
        // table names come from TABLE_CLASSES / sqlite_master, never users
        let conn = self.lock();
        Ok(conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| r.get(0))?)
    }

    /// One page of a table as generic JSON rows (column-name keyed;
    /// blobs surface as byte lengths, not payloads).
    pub fn dump_table_page(&self, table: &str, offset: i64, limit: i64) -> Result<Vec<Value>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare(&format!("SELECT * FROM \"{table}\" LIMIT ?1 OFFSET ?2"))?;
        let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let rows = stmt.query_map(rusqlite::params![limit, offset], |row| {
            let mut obj = Map::new();
            for (i, col) in cols.iter().enumerate() {
                let v = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => Value::from(n),
                    rusqlite::types::ValueRef::Real(f) => Value::from(f),
                    rusqlite::types::ValueRef::Text(t) => {
                        Value::from(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => {
                        Value::from(format!("<blob {} bytes>", b.len()))
                    }
                };
                obj.insert(col.clone(), v);
            }
            Ok(Value::Object(obj))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE contract test: an unclassified table fails the build. Adding
    /// a migration? Classify its table above (Exported or Excluded with
    /// a reason) — there is no third option.
    #[test]
    fn test_every_table_is_classified() {
        let store = Store::open_in_memory().unwrap();
        let classified: Vec<&str> = TABLE_CLASSES.iter().map(|(n, _)| *n).collect();
        for table in store.all_table_names().unwrap() {
            assert!(
                classified.contains(&table.as_str()),
                "table {table:?} is not classified in export_all::TABLE_CLASSES — \
                 add it as Exported or Excluded(reason); the full-export contract \
                 (CLAUDE.md \"Full export\") forbids unclassified data"
            );
        }
    }

    #[test]
    fn test_dump_pages_and_counts() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.table_row_count("episodes").unwrap(), 0);
        let rows = store.dump_table_page("entities", 0, 10).unwrap();
        assert!(rows.is_empty());
    }
}
