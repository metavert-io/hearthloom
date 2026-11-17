//! What raw SQL may read — the classification behind `Store::read_query`
//! and `Store::read_query_public` (`/v1/sql`, and Ask's `sql` and `map`
//! handles, whose results go to a model).
//!
//! Every table the database has is classified here, as the full export
//! classifies them in `export_all::TABLE_CLASSES`, and SQLite's authorizer
//! holds each raw query to its tables' classes while the query is prepared
//! and while it runs — never a reading of the SQL text:
//!
//! - `Filtered`: has a shadow view in `VAULT_SHADOW_VIEWS` (store/mod.rs).
//!   While the viewer's vault is locked, and always for
//!   `read_query_public`, it is read only through that view; with the
//!   vault open it is read in full (or through `LIVE_SHADOW_VIEWS`).
//! - `Open`: holds no private content and nothing secret; read in every
//!   state.
//! - `Vault`: holds private content, or text that can quote it, and has no
//!   exact filter; read only while the viewer's vault is open, never by
//!   `read_query_public`. Otherwise the query is refused with an error
//!   naming the table. Refused rather than empty: an empty answer would
//!   read as "there is none" to whoever asked (Ask would answer from it),
//!   and the refusal says only what this table's class is, which is the
//!   same in every brain whatever it holds.
//! - `Never`: credentials, secrets, session material; refused always.
//!
//! The full-text and vector indexes' own tables (FTS5's `_content`,
//! `_data`, `_idx`, `_docsize`, `_config`; vec0's `_chunks`, `_rowids`…)
//! are tables named `<virtual table>_<suffix>` that SQLite counts as the
//! virtual table's shadow tables (its module claims the suffix). Those
//! listed in `INDEX_TABLES` — what the modules make for the tables this
//! store declares — are `Vault` by one rule (`internal_table_of`). Any
//! other table is classified like any other: one that only shares the
//! prefix (vec0's `_vector_chunks00`, which vec0 doesn't claim), one with a
//! suffix the module claims but didn't make (a `vec_chunks_auxiliary`:
//! vec_chunks has no auxiliary columns), and a later index's tables until
//! they are listed. While a query runs, a virtual table the query reads
//! prepares statements of its own against its internal tables (FTS5 reads
//! its copy of the text that way); those reads are the index's, not the
//! query's, and are allowed.
//!
//! A new table must be classified in `RAW_SQL_CLASSES` (SQLite's own
//! tables in `SQLITE_TABLES`); `test_every_table_is_classified_for_raw_sql`
//! fails the build otherwise. Until it is classified the authorizer refuses
//! it (fail closed).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::Connection;

/// How raw SQL may read a table. See the module comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawSqlClass {
    Filtered,
    Open,
    Vault,
    Never,
}

use RawSqlClass::{Filtered, Never, Open, Vault};

/// name → class, reason. Classify EVERY table; the test enforces coverage.
/// Where it is unclear whether a table can hold private content or quote
/// it, it is `Vault`.
pub const RAW_SQL_CLASSES: &[(&str, RawSqlClass, &str)] = &[
    // — filtered: read through a shadow view while locked —
    (
        "episodes",
        Filtered,
        "the record; private and replaced memories filtered out",
    ),
    (
        "entities",
        Filtered,
        "private and hidden entities filtered out",
    ),
    (
        "entity_attributes",
        Filtered,
        "attributes of private entities, and private identities, filtered out",
    ),
    (
        "episode_entities",
        Filtered,
        "links of private memories and entities filtered out",
    ),
    ("edges", Filtered, "private edges filtered out"),
    (
        "chunks",
        Filtered,
        "chunks of private and replaced memories filtered out",
    ),
    (
        "vec_chunks",
        Filtered,
        "reads as empty: an embedding can't be tied to its memory's label",
    ),
    (
        "episode_fts",
        Filtered,
        "full text of private and replaced memories filtered out",
    ),
    (
        "location_points",
        Filtered,
        "points of private memories filtered out",
    ),
    ("tasks", Filtered, "private tasks filtered out"),
    (
        "task_mentions",
        Filtered,
        "mentions in private tasks filtered out",
    ),
    ("notes_index", Filtered, "private notes filtered out"),
    (
        "notes_fts",
        Filtered,
        "full text of private notes filtered out",
    ),
    (
        "note_mentions",
        Filtered,
        "mentions in private notes filtered out",
    ),
    (
        "mail_threads",
        Filtered,
        "private threads filtered out; none while mail is held",
    ),
    (
        "mail_drafts",
        Filtered,
        "drafts of private or unknown threads filtered out; none while mail is held",
    ),
    (
        "mail_matches",
        Filtered,
        "queued messages of private or unknown threads filtered out; none while mail is held",
    ),
    (
        "social_connections",
        Filtered,
        "network friend lists; carry no label, shown whole",
    ),
    (
        "semantic_pairs",
        Filtered,
        "pairs touching a private or replaced memory filtered out",
    ),
    (
        "metric_points",
        Filtered,
        "numbers of private memories filtered out",
    ),
    (
        "relationships",
        Filtered,
        "relationships to private entities filtered out",
    ),
    (
        "self_model",
        Filtered,
        "private-sphere propositions filtered out",
    ),
    (
        "opportunities",
        Filtered,
        "opportunities of private memories or seekings filtered out",
    ),
    (
        "intents",
        Filtered,
        "goals; private-sphere goals filtered out",
    ),
    (
        "standing_queries",
        Filtered,
        "goals' watch queries; those of private-sphere goals filtered out",
    ),
    (
        "intent_matches",
        Filtered,
        "what goals caught; private memories and private-sphere goals filtered out",
    ),
    (
        "mutations",
        Filtered,
        "reads as empty: payloads can quote private content",
    ),
    (
        "quarantine",
        Filtered,
        "reads as empty: proposals can quote private content",
    ),
    (
        "ask_log",
        Filtered,
        "conversations asked with the vault open filtered out",
    ),
    (
        "llm_usage",
        Filtered,
        "reads as empty: spend patterns can reveal vaulted activity",
    ),
    (
        "privacy_rules",
        Filtered,
        "reads as empty: the rules name what is private",
    ),
    // — open: nothing private, nothing secret —
    (
        "agent_directory",
        Open,
        "which node hosts which agent: names and harnesses",
    ),
    (
        "governance",
        Open,
        "agents' action policy: auto/ask/never per class",
    ),
    (
        "health_points",
        Open,
        "nodes' own health checks (disk, sync), not the owner's health",
    ),
    (
        "mail_ember_views",
        Open,
        "how much of a message each ember is shown",
    ),
    (
        "plugins",
        Open,
        "installed embers: manifests and granted scopes (no key material)",
    ),
    (
        "repairs",
        Open,
        "owner repair runs: kinds, counts and stamps",
    ),
    // — vault: private content or text that can quote it, no exact filter —
    (
        "agent_approvals",
        Vault,
        "approval cards hold an agent's whole tool input",
    ),
    (
        "agent_tasks",
        Vault,
        "requests, briefs and results can quote the record",
    ),
    (
        "agents",
        Vault,
        "agent configuration: working folders, key ids",
    ),
    (
        "consolidation_state",
        Vault,
        "settings and watermarks whose keys can name memories",
    ),
    (
        "contact_card",
        Vault,
        "the address book: names, addresses, phone numbers",
    ),
    (
        "device_state",
        Vault,
        "attention and presence intervals from the owner's devices",
    ),
    (
        "edge_provenance",
        Vault,
        "ties edges to memories, private ones included",
    ),
    (
        "entity_renames",
        Vault,
        "names before and after, private entities' included",
    ),
    (
        "episode_origin",
        Vault,
        "where each memory came from, private ones included",
    ),
    (
        "extraction_verdicts",
        Vault,
        "the names extracted from each memory",
    ),
    (
        "friend_groups",
        Vault,
        "who is in each friend group and what it is shown",
    ),
    (
        "goal_projects",
        Vault,
        "which projects each goal belongs to, private goals' included",
    ),
    ("grants", Vault, "what each friend is shown"),
    ("health_sample", Vault, "the owner's health samples"),
    ("ingest_log", Vault, "device batches and their references"),
    (
        "lint_findings",
        Vault,
        "findings quote notes that may since have become private",
    ),
    (
        "location_observation",
        Vault,
        "every geolocated observation",
    ),
    (
        "mail_rules",
        Vault,
        "the owner's mail rules name senders and topics",
    ),
    (
        "mail_thread_messages",
        Vault,
        "message ids and memory keys of every thread, private ones included",
    ),
    (
        "mail_watches",
        Vault,
        "embers' watch filters name senders and topics",
    ),
    (
        "merge_review_queue",
        Vault,
        "merge candidates among private entities too",
    ),
    ("merges", Vault, "merges among private entities too"),
    (
        "movement_observation",
        Vault,
        "travel spans with places and coordinates",
    ),
    ("nodes", Vault, "the owner's nodes, mailboxes and archives"),
    (
        "note_links",
        Vault,
        "links between notes, private ones included",
    ),
    (
        "note_neighbors",
        Vault,
        "similar notes, private ones included",
    ),
    (
        "notifications",
        Vault,
        "titles and bodies can quote the record",
    ),
    (
        "op_journal",
        Vault,
        "full post-images of notes, tasks and goals",
    ),
    (
        "owned_sources",
        Vault,
        "the sources the owner says are theirs",
    ),
    (
        "peer_groups",
        Vault,
        "friends' groups as their nodes describe them",
    ),
    (
        "photo_day",
        Vault,
        "photo counts per day and their memories",
    ),
    (
        "photo_point",
        Vault,
        "every photo's place, time and file name",
    ),
    (
        "photo_replaced",
        Vault,
        "photo keys: capture times and file names",
    ),
    ("plugin_kv", Vault, "embers' stored data"),
    (
        "repair_copies",
        Vault,
        "the memories each repair run made, private ones included",
    ),
    (
        "repair_revived",
        Vault,
        "memories a read put back, private ones included",
    ),
    (
        "retirement_rows",
        Vault,
        "the saved prior state of retired rows",
    ),
    ("retirements", Vault, "what was retired, with its label"),
    ("shared_items", Vault, "what is shared with which friend"),
    (
        "sources",
        Vault,
        "source settings (paths, mounts) and per-source counts",
    ),
    ("summary_inputs", Vault, "the memories each summary read"),
    (
        "syslog",
        Vault,
        "log lines can name memories and quote errors",
    ),
    (
        "task_templates",
        Vault,
        "form templates the owner wrote, with no label of their own",
    ),
    (
        "trip_attribution",
        Vault,
        "drives with their days and places",
    ),
    ("trip_day", Vault, "how much each day's movement held"),
    (
        "vec_chunks_vector_chunks00",
        Vault,
        "vec0's own store of the chunk embeddings, private memories' too; vec0 \
         doesn't claim it, so SQLite counts it as an ordinary table",
    ),
    (
        "vehicle_profile",
        Vault,
        "cars and the people who drive them",
    ),
    ("visit_day", Vault, "how much each day's places held"),
    // — never: credentials, secrets, sessions —
    ("api_keys", Never, "key hashes and scopes"),
    (
        "key_requests",
        Never,
        "holds a minted secret until it is collected",
    ),
    ("sessions", Never, "session token hashes"),
    ("users", Never, "the owner's login and password hash"),
];

/// SQLite's own tables. `sqlite_sequence` exists in every store; the
/// others appear only when SQLite makes them (ANALYZE), so they are not
/// expected to exist.
pub const SQLITE_TABLES: &[(&str, RawSqlClass, &str)] = &[
    // `sqlite_schema` is read under this name
    (
        "sqlite_master",
        Open,
        "the schema: table, index and view definitions",
    ),
    (
        "sqlite_temp_master",
        Open,
        "the temp schema: this query's own views",
    ),
    (
        "sqlite_sequence",
        Open,
        "the highest id of each AUTOINCREMENT table",
    ),
    ("sqlite_stat1", Open, "row counts per index"),
    (
        "sqlite_stat4",
        Vault,
        "sampled index keys: values copied out of indexed columns",
    ),
];

/// The class of an internal table of a virtual table (see the module
/// comment).
pub const INTERNAL_TABLE_CLASS: RawSqlClass = Vault;

/// The virtual tables' internal tables: what their modules make for the
/// tables this store declares. A table is internal only when it is named
/// here AND SQLite counts it as a virtual table's shadow table. The label
/// alone isn't enough: SQLite gives it to any table named `<virtual
/// table>_<suffix>` whose suffix the module claims, whether or not the
/// module made that table (vec0 claims `_auxiliary`, and vec_chunks has no
/// auxiliary columns). A new index's tables are added here with it;
/// `test_every_table_is_classified_for_raw_sql` reports them until then,
/// and the authorizer refuses them.
pub const INDEX_TABLES: &[&str] = &[
    "episode_fts_config",
    "episode_fts_content",
    "episode_fts_data",
    "episode_fts_docsize",
    "episode_fts_idx",
    "notes_fts_config",
    "notes_fts_content",
    "notes_fts_data",
    "notes_fts_docsize",
    "notes_fts_idx",
    "vec_chunks_chunks",
    "vec_chunks_info",
    "vec_chunks_rowids",
];

/// The SQL functions a raw read may call. Every function the connection
/// has is listed here or in `FUNCTIONS_REFUSED`
/// (`test_every_sql_function_is_classified`); any other is refused.
pub const FUNCTIONS: &[&str] = &[
    // operators SQLite runs as functions
    "->",
    "->>",
    "glob",
    "like",
    "match",
    // core scalars
    "abs",
    "char",
    "coalesce",
    "concat",
    "concat_ws",
    "format",
    "hex",
    "ifnull",
    "iif",
    "if", // newer SQLite's name for iif
    "instr",
    "length",
    "likelihood",
    "likely",
    "lower",
    "ltrim",
    "max",
    "min",
    "nullif",
    "octet_length",
    "printf",
    "quote",
    "random",
    "randomblob",
    "replace",
    "round",
    "rtrim",
    "sign",
    "soundex",
    "sqlite_compileoption_get",
    "sqlite_compileoption_used",
    "sqlite_source_id",
    "sqlite_version",
    "substr",
    "substring",
    "subtype",
    "trim",
    "typeof",
    "unhex",
    "unicode",
    "unistr",
    "unistr_quote",
    "unlikely",
    "upper",
    "zeroblob",
    // dates and times
    "current_date",
    "current_time",
    "current_timestamp",
    "date",
    "datetime",
    "julianday",
    "strftime",
    "time",
    "timediff",
    "unixepoch",
    // aggregates and windows
    "avg",
    "count",
    "group_concat",
    "string_agg",
    "sum",
    "total",
    "cume_dist",
    "dense_rank",
    "first_value",
    "lag",
    "last_value",
    "lead",
    "nth_value",
    "ntile",
    "percent_rank",
    "rank",
    "row_number",
    // JSON
    "json",
    "json_array",
    "json_array_insert",
    "json_array_length",
    "json_error_position",
    "json_extract",
    "json_group_array",
    "json_group_object",
    "json_insert",
    "json_object",
    "json_patch",
    "json_pretty",
    "json_quote",
    "json_remove",
    "json_replace",
    "json_set",
    "json_type",
    "json_valid",
    "jsonb",
    "jsonb_array",
    "jsonb_extract",
    "jsonb_group_array",
    "jsonb_group_object",
    "jsonb_insert",
    "jsonb_array_insert",
    "jsonb_object",
    "jsonb_patch",
    "jsonb_remove",
    "jsonb_replace",
    "jsonb_set",
    // FTS5's ranking and highlighting
    "bm25",
    "highlight",
    "fts5_get_locale",
    "fts5_insttoken",
    "fts5_locale",
    "snippet",
    "fts5_source_id",
    // sqlite-vec's vector arithmetic
    "vec_add",
    "vec_bit",
    "vec_debug",
    "vec_distance_cosine",
    "vec_distance_hamming",
    "vec_distance_l1",
    "vec_distance_l2",
    "vec_f32",
    "vec_int8",
    "vec_length",
    "vec_normalize",
    "vec_quantize_binary",
    "vec_quantize_int8",
    "vec_slice",
    "vec_sub",
    "vec_to_json",
    "vec_type",
    "vec_version",
];

/// Functions the connection has that a raw read may not call, and why.
pub const FUNCTIONS_REFUSED: &[(&str, &str)] = &[
    ("load_extension", "loads code into the daemon"),
    (
        "fts3_tokenizer",
        "reads or replaces an FTS3 tokenizer by its address",
    ),
    ("fts5", "hands out FTS5's programming interface"),
    ("optimize", "rewrites an FTS3/4 index"),
    (
        "matchinfo",
        "FTS3/4 only; the record's full-text tables are FTS5",
    ),
    (
        "offsets",
        "FTS3/4 only; the record's full-text tables are FTS5",
    ),
    (
        "rtreecheck",
        "runs statements of its own over R*Tree storage; the record has none",
    ),
    ("rtreedepth", "R*Tree tool; the record has no R*Tree table"),
    ("rtreenode", "R*Tree tool; the record has no R*Tree table"),
    ("sqlite_log", "writes to the process's log"),
    ("changes", "describes the daemon's own last write"),
    ("total_changes", "describes the daemon's own writes"),
    ("last_insert_rowid", "describes the daemon's own last write"),
];

/// The pragmas a raw read may run, as a statement or a `pragma_<name>`
/// table: each reports only the schema or the database's version counters
/// (FTS5 reads `data_version` while a query runs). `true`: it takes an
/// argument (a table or index name); one that doesn't is refused with a
/// value, which would set it.
pub const PRAGMAS: &[(&str, bool)] = &[
    ("table_info", true),
    ("table_xinfo", true),
    ("table_list", true),
    ("index_list", true),
    ("index_info", true),
    ("index_xinfo", true),
    ("foreign_key_list", true),
    ("collation_list", false),
    ("function_list", false),
    ("module_list", false),
    ("pragma_list", false),
    ("compile_options", false),
    ("data_version", false),
    ("schema_version", false),
    ("user_version", false),
];

/// Table-valued functions (eponymous virtual tables) a raw read may use,
/// besides `pragma_<name>` for each of `PRAGMAS`. Any other (`dbstat`,
/// which reads the database's pages) is refused.
pub const TABLE_FUNCTIONS: &[&str] = &["json_each", "json_tree", "vec_each"];

fn class_of(name: &str) -> Option<RawSqlClass> {
    static CLASSES: OnceLock<HashMap<&'static str, RawSqlClass>> = OnceLock::new();
    CLASSES
        .get_or_init(|| {
            RAW_SQL_CLASSES
                .iter()
                .chain(SQLITE_TABLES)
                .map(|(name, class, _)| (*name, *class))
                .collect()
        })
        .get(name)
        .copied()
}

/// The virtual table whose own storage `name` is: one listed in
/// `INDEX_TABLES` that SQLite itself counts as a virtual table's own
/// (`shadow_tables`), named `<virtual table>_<suffix>`. Any other table —
/// one that only shares the prefix, or carries a suffix the module claims
/// without having made it — is an ordinary table and needs a class of its
/// own. Names are compared lowercase.
pub(crate) fn internal_table_of<'a>(
    name: &str,
    virtual_tables: &'a [String],
    shadow_tables: &HashSet<String>,
) -> Option<&'a str> {
    if !INDEX_TABLES.contains(&name) || !shadow_tables.contains(name) {
        return None;
    }
    virtual_tables
        .iter()
        .filter(|v| {
            name.len() > v.len() + 1
                && name.starts_with(v.as_str())
                && name.as_bytes()[v.len()] == b'_'
        })
        .max_by_key(|v| v.len())
        .map(String::as_str)
}

/// The main schema's shadow tables (lowercase): those SQLite counts as a
/// virtual table's own, `shadow` in its table list. The PRAGMA, not
/// `pragma_table_list`, which a table of that name would stand in for;
/// `main.`, so the temp schema's views aren't prepared to list them.
fn shadow_tables(conn: &Connection) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("PRAGMA main.table_list")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>("name")?, r.get::<_, String>("type")?))
    })?;
    let mut shadow = HashSet::new();
    for row in rows {
        let (name, kind) = row?;
        if kind == "shadow" {
            shadow.insert(name.to_ascii_lowercase());
        }
    }
    Ok(shadow)
}

/// The main schema's tables and views, lowercase, with whether each is a
/// virtual table.
fn main_objects(conn: &Connection) -> Result<Vec<(String, bool)>> {
    let mut stmt = conn.prepare(
        "SELECT lower(name), type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%'
         FROM main.sqlite_master WHERE type IN ('table', 'view')",
    )?;
    let objects = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(objects)
}

/// The main schema's tables that are a virtual table's own, lowercase.
#[cfg(test)]
pub(crate) fn internal_tables(conn: &Connection) -> HashSet<String> {
    let virtual_tables: Vec<String> = main_objects(conn)
        .unwrap()
        .into_iter()
        .filter(|(_, is_virtual)| *is_virtual)
        .map(|(name, _)| name)
        .collect();
    let shadow = shadow_tables(conn).unwrap();
    shadow
        .iter()
        .filter(|n| internal_table_of(n, &virtual_tables, &shadow).is_some())
        .cloned()
        .collect()
}

fn table_function_allowed(name: &str) -> bool {
    TABLE_FUNCTIONS.contains(&name)
        || name
            .strip_prefix("pragma_")
            .is_some_and(|p| PRAGMAS.iter().any(|(allowed, _)| *allowed == p))
}

/// Names a query can't use even without reading a column of them: the
/// table-valued functions not allowed above, and SQLite's own objects.
fn table_function_refused(name: &str) -> bool {
    name == "dbstat" || name.starts_with("pragma_") || name.starts_with("sqlite_")
}

/// One raw query's shadow views: the SQL that makes them, and what each
/// reads.
///
/// Each `CREATE TEMP VIEW <name> AS <body>` of the batches is made as
/// `CREATE TEMP VIEW <name> AS WITH "<cte>" AS (<body>) SELECT * FROM
/// "<cte>"`, where `<cte>` carries a tag drawn afresh for every query. The
/// authorizer is told the innermost view or CTE behind each read: a read
/// behind one of these CTEs is the view's own reading of the table it
/// stands in for. The query's SQL was written before the tag existed, so
/// nothing in it — a CTE named like a view, say — can carry that name.
pub(crate) struct ShadowViews {
    pub(crate) sql: String,
    /// the views (lowercase) — the names of the tables they stand in for
    views: HashSet<String>,
    /// the CTE in each view (lowercase) → the main tables its body reads
    bodies: HashMap<String, HashSet<String>>,
}

impl ShadowViews {
    /// `batches` run in order; a later one may drop a view and make it
    /// again (`MAIL_HELD_SHADOW_VIEWS`).
    pub(crate) fn new(batches: &[&str]) -> ShadowViews {
        let tag = format!("hl{}", uuid::Uuid::new_v4().simple());
        let mut sql = String::new();
        let mut views: HashMap<String, String> = HashMap::new();
        let mut bodies = HashMap::new();
        // comment lines out first: a comment's own `;` ends no statement
        let uncommented: Vec<String> = batches
            .iter()
            .map(|b| {
                b.lines()
                    .filter(|line| !line.trim_start().starts_with("--"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .collect();
        for statement in uncommented.iter().flat_map(|b| b.split(';')) {
            let statement = statement.trim();
            if statement.is_empty() {
                continue;
            }
            if let Some((name, body)) = parse_view(statement) {
                let cte = format!("{tag}_{}", name.to_ascii_lowercase());
                sql.push_str(&format!(
                    "CREATE TEMP VIEW {name} AS WITH \"{cte}\" AS ({body}) SELECT * FROM \"{cte}\";\n"
                ));
                bodies.insert(cte.clone(), main_tables_read(body));
                if let Some(earlier) = views.insert(name.to_ascii_lowercase(), cte) {
                    bodies.remove(&earlier);
                }
                continue;
            }
            if let Some(name) = statement.strip_prefix("DROP VIEW IF EXISTS temp.") {
                if let Some(cte) = views.remove(&name.trim().to_ascii_lowercase()) {
                    bodies.remove(&cte);
                }
            }
            sql.push_str(statement);
            sql.push_str(";\n");
        }
        ShadowViews {
            sql,
            views: views.into_keys().collect(),
            bodies,
        }
    }

    /// The views' names (lowercase).
    #[cfg(test)]
    pub(crate) fn names(&self) -> &HashSet<String> {
        &self.views
    }

    /// The main tables each view reads, by view name.
    #[cfg(test)]
    pub(crate) fn reads_by_view(&self) -> HashMap<String, HashSet<String>> {
        self.bodies
            .iter()
            .map(|(cte, reads)| {
                let view = cte.split_once('_').map(|(_, v)| v).unwrap_or(cte);
                (view.to_string(), reads.clone())
            })
            .collect()
    }
}

/// `CREATE TEMP VIEW <name> AS <body>` → (name, body).
fn parse_view(statement: &str) -> Option<(&str, &str)> {
    let rest = statement.strip_prefix("CREATE TEMP VIEW ")?;
    let (name, rest) = rest.trim_start().split_once(char::is_whitespace)?;
    let body = rest.trim_start().strip_prefix("AS")?;
    Some((name, body.trim()))
}

/// The tables a view body names as `main.<table>` (lowercase) — the views
/// name every table they read so.
fn main_tables_read(body: &str) -> HashSet<String> {
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let bytes = body.as_bytes();
    let mut out = HashSet::new();
    let mut from = 0;
    while let Some(at) = body[from..].find("main.") {
        let start = from + at;
        let end = start + "main.".len();
        let mut stop = end;
        while stop < bytes.len() && ident(bytes[stop]) {
            stop += 1;
        }
        if (start == 0 || !ident(bytes[start - 1])) && stop > end {
            out.insert(body[end..stop].to_ascii_lowercase());
        }
        from = end;
    }
    out
}

/// Who the raw query reads for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reader {
    /// `read_query` with the viewer's vault open
    Open,
    /// `read_query` with the viewer's vault locked
    Locked,
    /// `read_query_public`: its rows go to a model in bulk
    Public,
}

struct Rules {
    reader: Reader,
    views: HashSet<String>,
    bodies: HashMap<String, HashSet<String>>,
    /// every table and view of the main schema (lowercase)
    main_objects: HashSet<String>,
    virtual_tables: Vec<String>,
    shadow_tables: HashSet<String>,
}

/// What the authorizer learns while one query is prepared and run.
#[derive(Default)]
struct Progress {
    /// prepared: what is asked from here on comes from the statements
    /// virtual tables prepare while the query runs
    running: AtomicBool,
    /// the first refusal, worded for the caller
    refused: Mutex<Option<String>>,
    /// the virtual tables the query reads
    read_virtual: Mutex<HashSet<String>>,
}

/// Holds the connection while the authorizer is installed (rusqlite installs
/// and removes it through `&mut`); the query reads the connection through
/// it. `off` removes the authorizer and reports a failure; dropping it
/// without `off` (a panic in the query) removes it too, as best it can.
struct AuthorizerOff<'a> {
    conn: &'a mut Connection,
    on: bool,
}

impl AuthorizerOff<'_> {
    fn off(mut self) -> Result<()> {
        self.on = false;
        self.conn
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .map_err(|e| anyhow!("removing the raw SQL authorizer: {e}"))
    }
}

impl std::ops::Deref for AuthorizerOff<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
    }
}

impl Drop for AuthorizerOff<'_> {
    fn drop(&mut self) {
        if self.on {
            let _ = self
                .conn
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        }
    }
}

/// Run one raw query under the authorizer. `run` prepares and runs it on
/// the connection it is given, calling the function it is given once the
/// statement is prepared.
pub(crate) fn run_guarded<T>(
    conn: &mut Connection,
    reader: Reader,
    shadow: &ShadowViews,
    run: impl FnOnce(&Connection, &dyn Fn()) -> Result<T>,
) -> Result<T> {
    let objects = main_objects(conn)?;
    let virtual_tables: Vec<String> = objects
        .iter()
        .filter(|(_, is_virtual)| *is_virtual)
        .map(|(name, _)| name.clone())
        .collect();
    // Connect each virtual table first: FTS5 reads its own tables when it
    // connects, and those reads are the index's, not the query's. (A
    // virtual table that can't connect fails the query that reads it.)
    for table in &virtual_tables {
        let _ = conn.prepare(&format!(
            "SELECT * FROM main.\"{}\" WHERE 0",
            table.replace('"', "\"\"")
        ));
    }
    let rules = Rules {
        reader,
        views: shadow.views.clone(),
        bodies: shadow.bodies.clone(),
        main_objects: objects.into_iter().map(|(name, _)| name).collect(),
        virtual_tables,
        // after connecting them: listing would otherwise prepare each one
        shadow_tables: shadow_tables(conn)?,
    };
    let progress = Arc::new(Progress::default());
    let seen = Arc::clone(&progress);
    conn.authorizer(Some(move |ctx: AuthContext<'_>| {
        match rules.judge(&seen, &ctx) {
            Ok(()) => Authorization::Allow,
            Err(why) => {
                seen.refused.lock().get_or_insert(why);
                Authorization::Deny
            }
        }
    }))
    .map_err(|e| anyhow!("installing the raw SQL authorizer: {e}"))?;
    let off = AuthorizerOff { conn, on: true };
    let result = run(&off, &|| progress.running.store(true, Ordering::SeqCst));
    off.off()?;
    // a refusal fails the query even where the statement that met it (a
    // virtual table's own) swallowed the error
    if let Some(why) = progress.refused.lock().take() {
        return Err(anyhow!(why));
    }
    result
}

impl Rules {
    fn judge(&self, progress: &Progress, ctx: &AuthContext<'_>) -> std::result::Result<(), String> {
        match ctx.action {
            AuthAction::Select | AuthAction::Recursive => Ok(()),
            AuthAction::Read {
                table_name,
                column_name,
            } => {
                let name = table_name.to_ascii_lowercase();
                let schema = ctx.database_name.map(str::to_ascii_lowercase);
                let accessor = ctx.accessor.map(str::to_ascii_lowercase);
                let decided = self.read(
                    progress,
                    &name,
                    column_name,
                    schema.as_deref(),
                    accessor.as_deref(),
                );
                if decided.is_ok()
                    && !progress.running.load(Ordering::SeqCst)
                    && self.virtual_tables.contains(&name)
                {
                    progress.read_virtual.lock().insert(name);
                }
                decided
            }
            AuthAction::Function { function_name } => {
                if FUNCTIONS.contains(&function_name.to_ascii_lowercase().as_str()) {
                    Ok(())
                } else {
                    Err(format!("raw SQL can't call {function_name}()"))
                }
            }
            AuthAction::Pragma {
                pragma_name,
                pragma_value,
            } => {
                let name = pragma_name.to_ascii_lowercase();
                match PRAGMAS.iter().find(|(p, _)| *p == name) {
                    Some((_, true)) => Ok(()),
                    Some((_, false)) if pragma_value.is_none() => Ok(()),
                    _ => Err(format!("raw SQL can't run PRAGMA {name}")),
                }
            }
            AuthAction::Attach { .. } | AuthAction::Detach { .. } => {
                Err("raw SQL can't attach or detach a database".into())
            }
            AuthAction::Transaction { .. } | AuthAction::Savepoint { .. } => {
                Err("raw SQL can't begin or end a transaction".into())
            }
            _ => Err("raw SQL only reads".into()),
        }
    }

    /// A read of `name` (lowercase), from `schema` (None: a FROM item the
    /// query names and reads no column of, under the name it gave it), by
    /// the innermost view or CTE `accessor`.
    fn read(
        &self,
        progress: &Progress,
        name: &str,
        column: &str,
        schema: Option<&str>,
        accessor: Option<&str>,
    ) -> std::result::Result<(), String> {
        // one of the guard's views reading a table it stands on
        if schema == Some("main")
            && accessor
                .and_then(|a| self.bodies.get(a))
                .is_some_and(|reads| reads.contains(name))
        {
            return Ok(());
        }
        match schema {
            Some("temp") => {
                return if self.views.contains(name) || name == "sqlite_temp_master" {
                    Ok(())
                } else {
                    Err(format!("raw SQL can't read temp.{name}"))
                };
            }
            // the name resolved to one of the guard's views, or to the CTE
            // in one (a query's own CTE of that name reads only what its
            // body reads, and each of those reads is judged)
            None if self.views.contains(name) || self.bodies.contains_key(name) => {
                return Ok(());
            }
            None | Some("main") => {}
            Some(other) => return Err(format!("raw SQL reads only this database, not {other}")),
        }
        // a table one of the guard's views stands in for is read only
        // through it. One exception: when SQLite flattens a view into the
        // query, a FROM item of the view's none of whose columns is read
        // (a view filtering on no column, under `count(*)`) is reported
        // without the view's name. Such a read takes no value, and a query
        // can't name the table behind a view itself (names_another_schema).
        if self.views.contains(name) && column.is_empty() {
            return Ok(());
        }
        if self.views.contains(name) {
            return Err(format!("raw SQL reads {name} only through its view"));
        }
        let class = match class_of(name) {
            Some(class) => class,
            None => match internal_table_of(name, &self.virtual_tables, &self.shadow_tables) {
                Some(table) => {
                    // the statements a virtual table the query reads
                    // prepares of its own while the query runs
                    if progress.running.load(Ordering::SeqCst)
                        && accessor.is_none()
                        && progress.read_virtual.lock().contains(table)
                    {
                        return Ok(());
                    }
                    INTERNAL_TABLE_CLASS
                }
                // a table function, unless a table of the database's own
                // has its name (SQLite then reads the table)
                None if table_function_allowed(name) && !self.main_objects.contains(name) => {
                    return Ok(())
                }
                // a CTE of the query's own, no column of which is read: a
                // name that is no table or view here and no refused
                // table-valued function
                None if schema.is_none()
                    && column.is_empty()
                    && !self.main_objects.contains(name)
                    && !table_function_refused(name) =>
                {
                    return Ok(())
                }
                None => return Err(format!("raw SQL can't read {name}")),
            },
        };
        match (class, self.reader) {
            (Open, _) | (Filtered | Vault, Reader::Open) => Ok(()),
            (Never, _) => Err(format!("raw SQL never reads {name}")),
            (Filtered, _) => Err(format!("raw SQL reads {name} only through its view")),
            (Vault, Reader::Locked) => {
                Err(format!("raw SQL reads {name} only while the vault is open"))
            }
            (Vault, Reader::Public) => Err(format!(
                "raw SQL whose rows go to a model in bulk never reads {name}"
            )),
        }
    }
}

/// Tables of the live schema no class covers: neither listed nor an
/// internal table of a virtual table.
#[cfg(test)]
pub(crate) fn unclassified_tables(conn: &Connection) -> Vec<String> {
    let internal = internal_tables(conn);
    let mut stmt = conn
        .prepare("SELECT name FROM main.sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    names
        .into_iter()
        .filter(|n| {
            let n = n.to_ascii_lowercase();
            class_of(&n).is_none() && !internal.contains(&n)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{EpisodeDraft, IngestBatch};
    use crate::paths::Home;
    use crate::store::{enter_viewer, Actor, Store};
    use crate::store::{LIVE_SHADOW_VIEWS, MAIL_HELD_SHADOW_VIEWS, VAULT_SHADOW_VIEWS};
    use crate::time::EventTime;

    fn table_names(store: &Store) -> Vec<String> {
        let conn = store.lock();
        let mut stmt = conn
            .prepare("SELECT name FROM main.sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        let names = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        names
    }

    fn quoted(name: &str) -> String {
        format!("\"{}\"", name.replace('"', "\"\""))
    }

    /// THE contract test: a table nobody classified for raw SQL fails the
    /// build, and so does a class naming a table that doesn't exist.
    #[test]
    fn test_every_table_is_classified_for_raw_sql() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.db")).unwrap();
        let unclassified = unclassified_tables(&store.lock());
        assert!(
            unclassified.is_empty(),
            "tables {unclassified:?} are not classified for raw SQL — add each to \
             store/raw_sql.rs RAW_SQL_CLASSES as Filtered (with a shadow view in \
             VAULT_SHADOW_VIEWS), Open, Vault or Never, with a reason; SQLite's own \
             tables go in SQLITE_TABLES (CLAUDE.md \"Raw SQL\")"
        );
        let names = table_names(&store);
        let mut seen = HashSet::new();
        for (name, _, reason) in RAW_SQL_CLASSES {
            assert!(seen.insert(*name), "{name} is classified twice");
            assert!(!reason.is_empty(), "{name} needs a reason");
            assert!(
                names.iter().any(|n| n == name),
                "{name} is classified for raw SQL but no such table exists"
            );
        }
        // an explicit class never also matches the internal-table rule
        let internal = internal_tables(&store.lock());
        for (name, _, _) in RAW_SQL_CLASSES.iter().chain(SQLITE_TABLES) {
            assert!(
                !internal.contains(*name),
                "{name} reads as a virtual table's own table"
            );
        }
        // the indexes' own tables are covered by the rule, and each one
        // listed is a virtual table's own table here
        let listed: HashSet<String> = INDEX_TABLES.iter().map(|n| n.to_string()).collect();
        assert_eq!(
            listed.len(),
            INDEX_TABLES.len(),
            "INDEX_TABLES lists one twice"
        );
        assert_eq!(internal, listed);
        // Filtered is exactly the tables VAULT_SHADOW_VIEWS shadows, and
        // each view reads the table it stands in for, and only tables that
        // exist
        let shadow = ShadowViews::new(&[VAULT_SHADOW_VIEWS]);
        let filtered: HashSet<String> = RAW_SQL_CLASSES
            .iter()
            .filter(|(_, c, _)| *c == Filtered)
            .map(|(n, _, _)| n.to_string())
            .collect();
        assert_eq!(shadow.names(), &filtered);
        for batch in [
            VAULT_SHADOW_VIEWS,
            LIVE_SHADOW_VIEWS,
            MAIL_HELD_SHADOW_VIEWS,
        ] {
            let shadow = ShadowViews::new(&[batch]);
            assert_eq!(
                shadow.names().len(),
                batch.matches("CREATE TEMP VIEW ").count(),
                "every view is read"
            );
            for (view, reads) in shadow.reads_by_view() {
                assert!(reads.contains(&view), "{view} reads its own table");
                for table in reads {
                    assert!(names.contains(&table), "{view} reads {table}");
                }
            }
        }
    }

    /// Every function the connection has is allowed or refused by name;
    /// every pragma and table-valued function allowed exists.
    #[test]
    fn test_every_sql_function_is_classified() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.lock();
        let list = |sql: &str| -> HashSet<String> {
            let mut stmt = conn.prepare(sql).unwrap();
            let names = stmt
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            names
        };
        let functions = list("SELECT DISTINCT lower(name) FROM pragma_function_list");
        let refused: HashSet<&str> = FUNCTIONS_REFUSED.iter().map(|(n, _)| *n).collect();
        let mut unclassified: Vec<&String> = functions
            .iter()
            .filter(|name| !FUNCTIONS.contains(&name.as_str()) && !refused.contains(name.as_str()))
            .collect();
        unclassified.sort();
        assert!(
            unclassified.is_empty(),
            "SQL functions not classified: {unclassified:?} — add each to store/raw_sql.rs \
             FUNCTIONS (a raw read may call it) or FUNCTIONS_REFUSED (with why not)"
        );
        let mut gone: Vec<&&str> = FUNCTIONS
            .iter()
            .chain(refused.iter())
            .filter(|name| !functions.contains(**name))
            .collect();
        gone.sort();
        assert!(
            gone.is_empty(),
            "classified SQL functions that don't exist: {gone:?}"
        );
        for name in FUNCTIONS.iter().chain(refused.iter()) {
            assert!(
                !(FUNCTIONS.contains(name) && refused.contains(name)),
                "{name}() is both allowed and refused"
            );
        }
        let pragmas = list("SELECT name FROM pragma_pragma_list");
        for (name, _) in PRAGMAS {
            assert!(pragmas.contains(*name), "PRAGMA {name} doesn't exist");
        }
        // newer SQLite leaves its built-in table functions (json_each) out
        // of pragma_module_list, and an extension's (vec_each) won't prepare
        // without its argument: either one says it exists
        let modules = list("SELECT name FROM pragma_module_list");
        for name in TABLE_FUNCTIONS {
            assert!(
                modules.contains(*name)
                    || conn
                        .prepare(&format!("SELECT * FROM {name} LIMIT 0"))
                        .is_ok(),
                "{name} doesn't exist"
            );
        }
    }

    /// A store holding one ordinary and one private memory, a private note
    /// (and so its journal entry), a location observation and a health
    /// sample.
    fn store_with_private_material() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open(&tmp.path().join("t.db")).unwrap();
        let draft = |source_ref: &str, title: &str, body: &str, scope: Option<&str>| EpisodeDraft {
            source: "chrome".into(),
            source_ref: source_ref.into(),
            kind: "browsing".into(),
            title: title.into(),
            body: body.into(),
            event_time: EventTime::unknown(),
            trust: "self".into(),
            scope: scope.map(String::from),
            entities: vec![],
            edges: vec![],
        };
        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:chrome".into(),
                    session_id: None,
                    episodes: vec![
                        draft(
                            "https://a.example/1",
                            "lanternfish appointment",
                            "the lanternfish appointment is on tuesday",
                            Some("private"),
                        ),
                        draft(
                            "https://a.example/2",
                            "harbor walk",
                            "a walk along the harbor",
                            None,
                        ),
                    ],
                },
            )
            .unwrap();
        let actor = Actor::new("human:owner");
        store
            .create_note(
                &actor,
                &home,
                "quillwort plan",
                "the quillwort plan stays between us",
                &[],
                "private",
                None,
            )
            .unwrap();
        store
            .create_note(
                &actor,
                &home,
                "groceries",
                "bread and milk",
                &[],
                "personal",
                None,
            )
            .unwrap();
        {
            let conn = store.lock();
            conn.execute(
                "INSERT INTO location_observation
                     (id, source, source_ref, lat, lon, t_start, day, actor, ingest_time)
                 VALUES ('lo1', 'ios-visit', 'v1', 47.123456, 8.654321,
                         '2026-03-01T10:00:00Z', '2026-03-01', 'human:device:phone',
                         '2026-03-01T10:00:00Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO health_sample
                     (uuid, metric, value, unit, source, start_utc, end_utc,
                      tz_offset_minutes, day, ingest_time)
                 VALUES ('hs1', 'heart_rate', 61.5, 'count/min', 'watch',
                         '2026-03-01T10:00:00Z', '2026-03-01T10:01:00Z', 0,
                         '2026-03-01', '2026-03-01T10:02:00Z')",
                [],
            )
            .unwrap();
        }
        (tmp, store)
    }

    const PRIVATE_WORDS: &[&str] = &["lanternfish", "quillwort", "47.123456", "61.5"];

    fn holds_private(rows: &[Vec<serde_json::Value>]) -> bool {
        rows.iter().flatten().any(|cell| {
            let text = cell.to_string();
            PRIVATE_WORDS.iter().any(|w| text.contains(w))
        })
    }

    /// Locked, neither reader gets anything private out of ANY table — each
    /// table is refused or answers without it — and the indexes' own
    /// tables, the journal, the location layer and health samples are
    /// refused by name.
    #[test]
    fn test_locked_raw_sql_reads_nothing_private_from_any_table() {
        let (_tmp, store) = store_with_private_material();
        for name in table_names(&store) {
            for sql in [
                format!("SELECT * FROM {}", quoted(&name)),
                format!("SELECT count(*) FROM {}", quoted(&name)),
            ] {
                for (reader, result) in [
                    ("read_query", store.read_query(&sql, 1000)),
                    ("read_query_public", store.read_query_public(&sql, 1000)),
                ] {
                    if let Ok((_, rows)) = result {
                        assert!(!holds_private(&rows), "{reader}: {sql} → {rows:?}");
                    }
                }
            }
        }
        let internal = internal_tables(&store.lock());
        let refused: Vec<String> = table_names(&store)
            .into_iter()
            .filter(|n| internal.contains(n))
            .chain(
                [
                    "op_journal",
                    "location_observation",
                    "health_sample",
                    "vec_chunks_vector_chunks00",
                ]
                .into_iter()
                .map(String::from),
            )
            .collect();
        for name in &refused {
            for sql in [
                format!("SELECT * FROM {name}"),
                format!("SELECT count(*) FROM {name}"),
            ] {
                for result in [
                    store.read_query(&sql, 10),
                    store.read_query_public(&sql, 10),
                ] {
                    let err = result.expect_err(&sql).to_string();
                    assert!(err.contains(name.as_str()), "{sql}: {err}");
                    assert!(!PRIVATE_WORDS.iter().any(|w| err.contains(w)), "{err}");
                }
            }
        }
        let err = store
            .read_query("SELECT c0, c1 FROM episode_fts_content", 10)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "raw SQL reads episode_fts_content only while the vault is open"
        );
        // the full-text tables still answer through their views, for what
        // isn't private (FTS5 reads its own tables while the query runs)
        for (sql, expected) in [
            (
                "SELECT title FROM episode_fts WHERE body LIKE '%harbor%'",
                1,
            ),
            (
                "SELECT title FROM episode_fts WHERE body LIKE '%lanternfish%'",
                0,
            ),
            ("SELECT note_id FROM notes_fts WHERE body LIKE '%bread%'", 1),
            (
                "SELECT note_id FROM notes_fts WHERE body LIKE '%quillwort%'",
                0,
            ),
        ] {
            for result in [store.read_query(sql, 10), store.read_query_public(sql, 10)] {
                assert_eq!(result.unwrap().1.len(), expected, "{sql}");
            }
        }
    }

    /// With the vault open `read_query` reads the vault tables and the
    /// indexes' own tables; `read_query_public` still reads neither.
    #[test]
    fn test_open_vault_reads_vault_tables_but_never_for_a_model() {
        let (_tmp, store) = store_with_private_material();
        let _owner = enter_viewer("session:owner");
        store.vault_unlock(15);
        for (sql, word) in [
            ("SELECT c0, c1 FROM episode_fts_content", "lanternfish"),
            ("SELECT c1 FROM notes_fts_content", "quillwort"),
            ("SELECT post_image FROM op_journal", "quillwort"),
            ("SELECT lat FROM location_observation", "47.123456"),
            ("SELECT value FROM health_sample", "61.5"),
        ] {
            let (_, rows) = store.read_query(sql, 100).unwrap();
            assert!(
                rows.iter().flatten().any(|c| c.to_string().contains(word)),
                "{sql}"
            );
            let err = store.read_query_public(sql, 100).unwrap_err().to_string();
            assert!(err.contains("never reads"), "{sql}: {err}");
        }
        // the full-text index answers MATCH with the vault open
        let (_, rows) = store
            .read_query(
                "SELECT highlight(episode_fts, 0, '[', ']'), bm25(episode_fts)
                 FROM episode_fts WHERE episode_fts MATCH 'lanternfish'",
                10,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        // the public reader filters, as with the vault locked
        let (_, rows) = store
            .read_query_public("SELECT title FROM episodes", 10)
            .unwrap();
        assert!(!holds_private(&rows));
        store.vault_lock();
    }

    #[test]
    fn test_credentials_are_never_read() {
        let (_tmp, store) = store_with_private_material();
        let check = |store: &Store| {
            for table in ["sessions", "api_keys", "users", "key_requests"] {
                for sql in [
                    format!("SELECT * FROM {table}"),
                    format!("SELECT count(*) FROM {table}"),
                    format!("SELECT * FROM \"{}\"", table.to_ascii_uppercase()),
                    format!("WITH c AS (SELECT * FROM {table}) SELECT * FROM c"),
                    format!("SELECT (SELECT count(*) FROM {table})"),
                    format!("SELECT id FROM episodes UNION ALL SELECT rowid FROM {table}"),
                ] {
                    for result in [
                        store.read_query(&sql, 10),
                        store.read_query_public(&sql, 10),
                    ] {
                        let err = result.expect_err(&sql).to_string();
                        assert_eq!(err, format!("raw SQL never reads {table}"), "{sql}");
                    }
                }
            }
        };
        check(&store);
        let _owner = enter_viewer("session:owner");
        store.vault_unlock(15);
        check(&store);
        store.vault_lock();
    }

    /// A table named only inside a CTE, a subquery, a view or a UNION is
    /// judged as if named directly — including a CTE named like one of the
    /// guard's views, and any spelling of the name.
    #[test]
    fn test_a_table_named_only_inside_a_cte_subquery_view_or_union_is_judged_the_same() {
        let (_tmp, store) = store_with_private_material();
        store
            .lock()
            .execute_batch(
                "CREATE VIEW journal_view AS SELECT * FROM op_journal;
                 CREATE VIEW content_view AS SELECT * FROM episode_fts_content;",
            )
            .unwrap();
        let forms = |t: &str| -> Vec<String> {
            vec![
                format!("SELECT * FROM {t}"),
                format!("WITH x AS (SELECT * FROM {t}) SELECT * FROM x"),
                format!("WITH x AS (SELECT 1 FROM {t}) SELECT count(*) FROM x"),
                format!("WITH episodes AS (SELECT * FROM {t}) SELECT * FROM episodes"),
                format!("WITH mail_drafts AS (SELECT * FROM {t}) SELECT * FROM mail_drafts"),
                format!("SELECT * FROM (SELECT * FROM {t})"),
                format!("SELECT (SELECT count(*) FROM {t})"),
                format!("SELECT id FROM episodes WHERE EXISTS (SELECT 1 FROM {t})"),
                format!("SELECT id FROM episodes UNION SELECT rowid FROM {t}"),
                format!("SELECT 1 UNION ALL SELECT count(*) FROM {t}"),
                format!("SELECT count(*) FROM {}", t.to_ascii_uppercase()),
            ]
        };
        let refused_locked = [
            "op_journal",
            "episode_fts_content",
            "notes_fts_data",
            "mail_thread_messages",
            "location_observation",
        ];
        for table in refused_locked {
            for sql in forms(table) {
                for result in [
                    store.read_query(&sql, 10),
                    store.read_query_public(&sql, 10),
                ] {
                    let err = result.expect_err(&sql).to_string();
                    assert!(err.contains(table), "{sql}: {err}");
                }
            }
        }
        // a view of the database's own is no way around a table's class
        for sql in forms("journal_view")
            .into_iter()
            .chain(forms("content_view"))
        {
            assert!(store.read_query(&sql, 10).is_err(), "{sql}");
            assert!(store.read_query_public(&sql, 10).is_err(), "{sql}");
        }
        // a CTE named like one of the guard's views reads no more than its
        // body may: the drafts view's own reading of the thread index isn't
        // the query's
        let sql = "WITH mail_drafts AS (SELECT message_id FROM mail_thread_messages)
                   SELECT * FROM mail_drafts";
        assert!(store.read_query(sql, 10).is_err());
        assert!(store.read_query("SELECT id FROM mail_drafts", 10).is_ok());
        // with the vault open the same forms read the vault tables
        let _owner = enter_viewer("session:owner");
        store.vault_unlock(15);
        for table in refused_locked {
            for sql in forms(table) {
                store.read_query(&sql, 10).expect(&sql);
                assert!(store.read_query_public(&sql, 10).is_err(), "{sql}");
            }
        }
        store.vault_lock();
    }

    /// The queries Ask and the tests run keep working in every state.
    #[test]
    fn test_ordinary_raw_queries_still_work() {
        let (_tmp, store) = store_with_private_material();
        let everywhere = [
            "SELECT 1 AS one UNION SELECT 2",
            "SELECT id, title FROM episodes ORDER BY event_time DESC LIMIT 5",
            "SELECT source, COUNT(*) AS n FROM episodes GROUP BY source ORDER BY n DESC",
            "SELECT COUNT(*), MIN(ingest_time), MAX(ingest_time), group_concat(source, ',')
             FROM episodes",
            "SELECT id, row_number() OVER (ORDER BY ingest_time) FROM episodes",
            "SELECT e.name, COUNT(*) FROM entities e
             JOIN episode_entities ee ON ee.entity_id = e.id GROUP BY e.id",
            "SELECT a.kind, a.value FROM entity_attributes a JOIN entities e ON e.id = a.entity_id",
            "SELECT * FROM edges",
            "SELECT id, description, tags FROM tasks WHERE status = 'open'",
            "SELECT t.id, j.value FROM tasks t, json_each(t.tags) j",
            "SELECT value FROM json_each('[1,2,3]')",
            "SELECT key FROM json_tree('{\"a\":{\"b\":1}}')",
            "SELECT json_extract('{\"a\":1}', '$.a'), '{\"a\":1}' ->> '$.a'",
            "SELECT id, title, tags FROM notes_index WHERE status != 'deleted'",
            "SELECT origin, COUNT(*) AS n FROM notes_index GROUP BY origin",
            "SELECT note_id FROM notes_fts WHERE body LIKE '%bread%'",
            "SELECT title FROM episode_fts WHERE body LIKE '%harbor%'",
            "SELECT e.id, e.title, f.body FROM episodes e
             JOIN episode_fts f ON f.episode_id = e.id WHERE f.body LIKE '%walk%'",
            "WITH m AS (SELECT substr(ingest_time, 1, 7) AS month, COUNT(*) AS n
                        FROM episodes GROUP BY month)
             SELECT * FROM m ORDER BY month",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5)
             SELECT group_concat(i) FROM n",
            "SELECT * FROM (SELECT id FROM episodes) WHERE id IN (SELECT episode_id FROM chunks)",
            "SELECT lat, lon FROM location_points",
            "SELECT * FROM social_connections",
            "SELECT * FROM semantic_pairs",
            "SELECT * FROM metric_points",
            "SELECT * FROM relationships",
            "SELECT * FROM self_model WHERE status = 'confirmed'",
            "SELECT * FROM opportunities",
            "SELECT id, name, query, description, status, sphere, target_date FROM intents",
            "SELECT * FROM standing_queries",
            "SELECT * FROM intent_matches",
            "SELECT * FROM mail_threads",
            "SELECT * FROM mail_drafts",
            "SELECT * FROM mail_matches",
            "SELECT id, ts, question, answer FROM ask_log",
            "SELECT count(*) FROM mutations",
            "SELECT * FROM vec_chunks",
            "SELECT * FROM note_mentions",
            "SELECT * FROM task_mentions",
            "SELECT * FROM governance",
            "SELECT * FROM plugins",
            "SELECT name FROM sqlite_master WHERE type = 'table'",
            "SELECT name FROM sqlite_schema",
            "SELECT name FROM temp.sqlite_master",
            "SELECT * FROM sqlite_sequence",
            "SELECT name FROM pragma_table_info('episodes')",
            "PRAGMA table_info(episodes)",
            "PRAGMA index_list('episodes')",
            "SELECT strftime('%Y', 'now'), date('now'), julianday('now'), datetime('now')",
            "SELECT lower('A'), upper('a'), length('abc'), instr('abc', 'b'), printf('%d', 3)",
            "SELECT count(*) FROM EPISODES",
            "SELECT title FROM temp.episodes",
            "SELECT count(*) FROM episode_fts",
            "SELECT count(*) FROM notes_fts",
            "SELECT count(*) FROM vec_chunks",
        ];
        // MATCH can't run against a view, and the journal is a vault table
        let open_only = [
            "SELECT title FROM episode_fts WHERE episode_fts MATCH 'harbor'",
            "SELECT snippet(episode_fts, 1, '[', ']', '…', 5) FROM episode_fts
             WHERE episode_fts MATCH 'harbor' ORDER BY bm25(episode_fts)",
            "SELECT note_id FROM notes_fts WHERE notes_fts MATCH 'bread'",
            "SELECT * FROM op_journal",
        ];
        let run_all = |store: &Store, open: bool| {
            for sql in everywhere {
                store.read_query(sql, 100).expect(sql);
                store.read_query_public(sql, 100).expect(sql);
            }
            for sql in open_only {
                assert_eq!(store.read_query(sql, 100).is_ok(), open, "{sql}");
            }
        };
        run_all(&store, false);
        {
            let _owner = enter_viewer("session:owner");
            store.vault_unlock(15);
            run_all(&store, true);
            store.vault_lock();
        }
        // while mail is held
        store.set_mail_held(true);
        run_all(&store, false);
        store.set_mail_held(false);
    }

    /// Raw SQL only reads: transactions, settings, attaching, loading code
    /// and reading the database's pages are refused, and the connection is
    /// left as it was — no authorizer, no transaction, no views.
    #[test]
    fn test_raw_sql_runs_only_reads() {
        let (_tmp, store) = store_with_private_material();
        let _owner = enter_viewer("session:owner");
        store.vault_unlock(15);
        for sql in [
            "BEGIN",
            "BEGIN IMMEDIATE",
            "SAVEPOINT a",
            "COMMIT",
            "PRAGMA foreign_keys = OFF",
            "PRAGMA query_only = 1",
            "PRAGMA journal_mode",
            "PRAGMA database_list",
            "PRAGMA user_version = 3",
            "PRAGMA integrity_check",
            "SELECT * FROM pragma_database_list",
            "SELECT load_extension('x')",
            "SELECT last_insert_rowid()",
            "SELECT * FROM dbstat",
            "SELECT count(*) FROM dbstat",
            "SELECT count(*) FROM sqlite_dbpage",
            "ATTACH ':memory:' AS y",
            "SELECT fts3_tokenizer('simple')",
        ] {
            assert!(store.read_query(sql, 10).is_err(), "{sql}");
            assert!(store.read_query_public(sql, 10).is_err(), "{sql}");
        }
        // refused by the authorizer, which says what was refused
        for (sql, why) in [
            ("BEGIN", "raw SQL can't begin or end a transaction"),
            (
                "PRAGMA foreign_keys = OFF",
                "raw SQL can't run PRAGMA foreign_keys",
            ),
            (
                "PRAGMA user_version = 3",
                "raw SQL can't run PRAGMA user_version",
            ),
            (
                "SELECT load_extension('x')",
                "raw SQL can't call load_extension()",
            ),
            ("SELECT count(*) FROM dbstat", "raw SQL can't read dbstat"),
        ] {
            assert_eq!(store.read_query(sql, 10).unwrap_err().to_string(), why);
        }
        store.vault_lock();
        let conn = store.lock();
        assert!(conn.is_autocommit(), "no transaction was left open");
        let views: i64 = conn
            .query_row("SELECT count(*) FROM temp.sqlite_master", [], |r| r.get(0))
            .unwrap();
        assert_eq!(views, 0, "no view was left behind");
        // the authorizer is gone: the store reads and writes every table
        let sessions: i64 = conn
            .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 0);
        conn.execute(
            "INSERT INTO consolidation_state (key, value) VALUES ('raw_sql.test', '1')",
            [],
        )
        .unwrap();
    }

    /// The authorizer holds without the SQL-text check: a read of a table
    /// behind one of the views, under any name a query can give it (a CTE
    /// named like the view included), is refused, while the views read it.
    #[test]
    fn test_the_authorizer_refuses_the_tables_behind_the_views_on_its_own() {
        use crate::store::VAULT_SHADOW_DROP;
        let (_tmp, store) = store_with_private_material();
        let guarded = |reader: Reader, sql: &str| -> Result<usize> {
            let mut conn = store.lock();
            let shadow = ShadowViews::new(&[VAULT_SHADOW_VIEWS]);
            conn.execute_batch(&shadow.sql).unwrap();
            let result = run_guarded(&mut conn, reader, &shadow, |conn, prepared| {
                Store::run_read_query(conn, sql, 100, prepared)
            })
            .map(|(_, rows)| rows.len());
            conn.execute_batch(VAULT_SHADOW_DROP).unwrap();
            result
        };
        for reader in [Reader::Locked, Reader::Public] {
            for sql in [
                "SELECT title FROM main.episodes",
                "SELECT title FROM \"main\".\"EPISODES\"",
                "SELECT body FROM main.episode_fts",
                "SELECT c1 FROM main.episode_fts_content",
                "WITH episode_fts AS (SELECT title FROM main.episodes) SELECT * FROM episode_fts",
                "WITH mail_drafts AS (SELECT message_id FROM main.mail_thread_messages)
                 SELECT * FROM mail_drafts",
                "SELECT * FROM main.op_journal",
                "SELECT x.title FROM episodes x JOIN main.notes_index n ON n.title = x.title",
            ] {
                assert!(guarded(reader, sql).is_err(), "{reader:?}: {sql}");
            }
            assert_eq!(guarded(reader, "SELECT title FROM episodes").unwrap(), 1);
            assert_eq!(guarded(reader, "SELECT id FROM mail_drafts").unwrap(), 0);
        }
    }

    /// A table added later without a class — an index's own tables too,
    /// until they are listed in INDEX_TABLES — is reported and never read.
    #[test]
    fn test_tables_added_later_are_refused_until_classified() {
        let (_tmp, store) = store_with_private_material();
        store
            .lock()
            .execute_batch(
                "CREATE VIRTUAL TABLE later_fts USING fts5(body);
                 INSERT INTO later_fts VALUES ('marigold');
                 CREATE TABLE later_table (x TEXT);
                 INSERT INTO later_table VALUES ('marigold');",
            )
            .unwrap();
        let later = [
            "later_fts",
            "later_fts_config",
            "later_fts_content",
            "later_fts_data",
            "later_fts_docsize",
            "later_fts_idx",
            "later_table",
        ];
        assert_eq!(unclassified_tables(&store.lock()), later);
        assert_refused_as_unclassified(&store, &later);
    }

    /// `tables` (each holding the word `marigold`) are refused as
    /// unclassified by both readers, with the vault locked and open.
    fn assert_refused_as_unclassified(store: &Store, tables: &[&str]) {
        let check = || {
            for table in tables {
                for sql in [
                    format!("SELECT * FROM {table}"),
                    format!("SELECT count(*) FROM {table}"),
                ] {
                    for result in [
                        store.read_query(&sql, 10),
                        store.read_query_public(&sql, 10),
                    ] {
                        let err = result.expect_err(&sql).to_string();
                        assert_eq!(err, format!("raw SQL can't read {table}"), "{sql}");
                    }
                }
            }
        };
        check();
        let _owner = enter_viewer("session:owner");
        store.vault_unlock(15);
        check();
        store.vault_lock();
    }

    /// A table of the database's own named like an allowed table-valued
    /// function is a table, judged by its class, never the function.
    #[test]
    fn test_a_table_named_like_a_table_function_is_judged_as_a_table() {
        let (_tmp, store) = store_with_private_material();
        store
            .lock()
            .execute_batch(
                "CREATE TABLE json_each (secret TEXT);
                 INSERT INTO json_each VALUES ('marigold');
                 CREATE TABLE pragma_table_info (secret TEXT);
                 INSERT INTO pragma_table_info VALUES ('marigold');",
            )
            .unwrap();
        assert_eq!(
            unclassified_tables(&store.lock()),
            ["json_each", "pragma_table_info"]
        );
        assert_refused_as_unclassified(&store, &["json_each", "pragma_table_info"]);
    }

    /// A table that only shares a virtual table's prefix is no internal
    /// table of it: it needs a class of its own, and is refused until it
    /// has one.
    #[test]
    fn test_a_table_sharing_an_index_prefix_needs_its_own_class() {
        let (_tmp, store) = store_with_private_material();
        store
            .lock()
            .execute_batch(
                "CREATE TABLE episode_fts_credentials (secret TEXT);
                 INSERT INTO episode_fts_credentials VALUES ('marigold');
                 CREATE TABLE vec_chunks_chunks_copy (secret TEXT);
                 INSERT INTO vec_chunks_chunks_copy VALUES ('marigold');",
            )
            .unwrap();
        assert_eq!(
            unclassified_tables(&store.lock()),
            ["episode_fts_credentials", "vec_chunks_chunks_copy"]
        );
        assert_refused_as_unclassified(
            &store,
            &["episode_fts_credentials", "vec_chunks_chunks_copy"],
        );
        // the indexes still read their own tables while a query runs
        let vector = format!("[{}]", vec!["0.5"; 384].join(","));
        store
            .lock()
            .execute(
                "INSERT INTO vec_chunks (rowid, embedding) VALUES (1, ?1)",
                [&vector],
            )
            .unwrap();
        let _owner = enter_viewer("session:owner");
        store.vault_unlock(15);
        let sql =
            format!("SELECT rowid FROM vec_chunks WHERE embedding MATCH '{vector}' AND k = 1");
        assert_eq!(store.read_query(&sql, 10).unwrap().1.len(), 1);
        let sql = "SELECT title FROM episode_fts WHERE episode_fts MATCH 'harbor'";
        assert_eq!(store.read_query(sql, 10).unwrap().1.len(), 1);
        store.vault_lock();
    }

    /// A table SQLite labels a virtual table's shadow table, because the
    /// module claims its suffix, that the module didn't make (vec_chunks
    /// has no auxiliary columns) is no internal table either: it needs a
    /// class of its own, and is refused until it has one.
    #[test]
    fn test_a_claimed_suffix_the_index_did_not_make_needs_its_own_class() {
        let (_tmp, store) = store_with_private_material();
        store
            .lock()
            .execute_batch(
                "CREATE TABLE vec_chunks_auxiliary (secret TEXT);
                 INSERT INTO vec_chunks_auxiliary VALUES ('marigold');",
            )
            .unwrap();
        assert!(
            shadow_tables(&store.lock())
                .unwrap()
                .contains("vec_chunks_auxiliary"),
            "SQLite labels it a shadow table"
        );
        assert_eq!(unclassified_tables(&store.lock()), ["vec_chunks_auxiliary"]);
        assert_refused_as_unclassified(&store, &["vec_chunks_auxiliary"]);
    }
}
