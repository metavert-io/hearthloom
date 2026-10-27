//! The substrate store: one SQLite file, WAL mode, FTS5, migrations via
//! `PRAGMA user_version`.
//!
//! Only the daemon may construct a writable `Store` against the real
//! database file (design law #4). Every write records a row in the
//! append-only `mutations` log with actor identity and an idempotency
//! key; the change feed is a cursor over that log.

mod agents;
mod api_keys;
mod ask;
mod auth;
mod brain_fs;
mod browse;
mod cascades;
mod connections;
mod contacts;
mod device_state;
mod episodes;
mod export_all;
mod galaxy;
mod graph;
mod groups;
mod health;
mod ingest_apply;
mod ingest_log;
mod intents;
mod lint;
mod llm_usage;
mod locations;
mod movements;
mod network;
mod notes;
mod notifications;
mod observations;
mod opportunities;
mod photo;
mod plugins;
mod privacy;
mod projects;
mod quarantine;
mod recall;
mod relationships;
mod resolve;
mod retire;
mod search;
mod self_model;
mod sharing;
mod sync;
mod tasks;
mod telemetry;
mod vectors;
mod verdicts;

pub use agents::{
    decide, decide_all, AgentIn, AgentRow, AgentTaskRow, Decision, Policy, ACTION_CLASSES,
    HARNESSES, PRESETS,
};
pub use api_keys::{
    Access, ApiKeyClaims, ApiKeyRow, GrantVerdict, KeyGrants, API_KEY_SCOPES, REQUEST_WINDOW_SECS,
};
pub use ask::AskRecord;
pub use auth::SESSION_DAYS;
pub use brain_fs::{BrainFsHit, BrainFsItem};
pub use browse::{
    EdgeBrowseRow, EdgeFacets, EdgePage, EntityBrowseRow, EntityFacets, EntityPage, EpisodeFacets,
    EpisodeFilter, EpisodePage, FacetCount, IdentityBrowseRow, IdentityFacets, IdentityPage,
    SourceRow, VirtualSourceDef,
};
pub use cascades::{CascadeContext, CascadeSeed};
pub use connections::ConnectionUpsert;
pub use contacts::{ContactCardIn, PendingContact};
pub use device_state::DeviceStateIn;
pub use episodes::{EpisodeRow, PendingExtraction};
pub use export_all::{is_internal_table, TableClass, TABLE_CLASSES};
pub use graph::{
    ClaimOutcome, DossierEdge, EntityDossier, EntityRow, MergeRecord, ReviewItem, TraverseHop,
};
pub use groups::{FriendAccess, GroupRow, GROUP_PREFIX};
pub use health::{HealthSampleRow, StoredHealthSample};
pub use ingest_apply::OWNER_ONLY_SOURCES;
pub use intents::{IntentMatch, IntentRow};
pub use lint::ReviewInputs;
pub use movements::{
    MovementIn, StoredMovement, VehicleProfile, ATTRIBUTION_MODES, MOVEMENT_MODES,
    SCOPES as MOVEMENT_SCOPES,
};
pub use network::{GrantRow, NetworkHit, GRANTABLE_SPHERES};
pub use notes::{note_prefill_for_task, parse_template, render_template, NoteRow, TemplateDef};
pub use notes::{NoteImport, NoteWriter, AGENT_ORIGIN, NATIVE_ORIGIN, RECIPE_KIND};
pub use notifications::{validate_link, NotificationRow};
pub use observations::{ObservationIn, StoredObservation};
pub use opportunities::OpportunityRow;
pub use photo::{keys_hash as photo_keys_hash, PhotoPointIn, StoredPhotoPoint};
pub use plugins::PluginRow;
pub use privacy::PrivacyRule;
pub use quarantine::{outside_proposal, QuarantineEntry, PROPOSAL_KINDS};
pub use recall::DayView;
pub use relationships::{DirectoryRow, RelationshipRow, RELATIONSHIP_KINDS};
pub use retire::{
    base_ref, RetireCounts, RetirementRow, RewriteJob, RETIRED_PREFIX, SUMMARY_KINDS,
};
pub use search::SearchHit;
pub use self_model::{Proposition, PROPOSITION_KINDS, SPHERES};
pub use sharing::{EffectiveGrant, DEFAULT_FRIEND, SHAREABLE_KINDS, SHARE_CLASSES};
pub use sync::SyncPage;
pub use tasks::{CompletionOutcome, TaskRow, TaskTemplate, TASK_FREQUENCIES, TASK_KINDS};
pub use telemetry::{HealthBucket, LogRow};
pub use vectors::VectorHit;

use parking_lot::FairMutex;
use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::time::now_utc;

/// Who is writing. Mandatory on every write — blackboard attribution is a
/// query, not a migration. Convention: `human:<name>`, `agent:<id>`,
/// `adapter:<id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub actor: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

impl Actor {
    pub fn new(actor: impl Into<String>) -> Self {
        Actor {
            actor: actor.into(),
            session_id: None,
        }
    }
}

/// One row of the change feed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationRow {
    pub seq: i64,
    pub ts: String,
    pub actor: String,
    pub session_id: Option<String>,
    pub kind: String,
    pub payload: JsonValue,
}

pub struct Store {
    // FAIR mutex (2026-07-19): FIFO handoff means a grinding
    // consolidation batch queues BEHIND every waiting HTTP handler on
    // re-acquire — with the old unfair std mutex the grinder re-won the
    // lock every release and a 1-vCPU node 502'd for its whole boot
    // cycle even though every pass batched politely.
    conn: FairMutex<Connection>,
    /// The vault, PER VIEWER (0.5.71): an unlock belongs to whoever made
    /// it — one dashboard session, one API key, or the keyless loopback
    /// caller of a node with no login — and only that viewer's reads see
    /// `scope = 'private'`. Everyone else, and every read with no viewer
    /// (background passes, sync), stays LOCKED. Unlocks self-expire: a
    /// deadline per viewer, checked on read, no background task.
    vault_unlocks: Mutex<std::collections::HashMap<String, std::time::Instant>>,
    /// Sticky "the vec_chunks ANN index covers every embedded chunk" flag
    /// (vectors.rs). Once true it stays true — dual-write keeps parity and
    /// chunks are never deleted — so searches skip the coverage count.
    vec_ready: std::sync::atomic::AtomicBool,
}

/// The life areas a scoped key can be limited to. `private` is never one
/// of them — the vault is a separate, stricter gate.
pub const LIFE_AREAS: &[&str] = &["work", "personal", "family", "health"];

thread_local! {
    /// The life areas the read running on THIS thread may see (None = no
    /// limit). Set only through `enter_area_filter`.
    static AREA_FILTER: std::cell::RefCell<Option<Vec<&'static str>>> =
        const { std::cell::RefCell::new(None) };
}

/// Restores the previous area filter when dropped.
pub struct AreaFilterGuard {
    previous: Option<Vec<&'static str>>,
}

impl Drop for AreaFilterGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        AREA_FILTER.with(|f| *f.borrow_mut() = previous);
    }
}

/// Limit every guarded read on this thread to `areas` until the guard
/// drops. An unknown label is dropped, never passed through — so a
/// malformed list narrows (to nothing, at worst) and no caller string
/// ever reaches SQL. The filter is per-THREAD: work handed to another
/// thread (spawn_blocking) must enter it again there.
pub fn enter_area_filter(areas: &[String]) -> AreaFilterGuard {
    let allowed: Vec<&'static str> = LIFE_AREAS
        .iter()
        .copied()
        .filter(|a| areas.iter().any(|x| x == a))
        .collect();
    let previous = AREA_FILTER.with(|f| f.borrow_mut().replace(allowed));
    AreaFilterGuard { previous }
}

fn area_filter() -> Option<Vec<&'static str>> {
    AREA_FILTER.with(|f| f.borrow().clone())
}

thread_local! {
    /// Whose vault view the read running on THIS thread gets. None = no
    /// viewer (background work), which reads LOCKED. Set only through
    /// `enter_viewer` / `ReadScope::enter`.
    static VIEWER: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Restores the previous viewer when dropped.
pub struct ViewerGuard {
    previous: Option<String>,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        VIEWER.with(|v| *v.borrow_mut() = previous);
    }
}

/// Read as `viewer` on this thread until the guard drops: vault state is
/// looked up for that viewer alone. Per-THREAD like the area filter —
/// work handed to another thread carries it via `capture_read_scope`.
pub fn enter_viewer(viewer: &str) -> ViewerGuard {
    let previous = VIEWER.with(|v| v.borrow_mut().replace(viewer.to_string()));
    ViewerGuard { previous }
}

/// The viewer reads on this thread run as ("" when none is set).
fn current_viewer() -> String {
    VIEWER.with(|v| v.borrow().clone()).unwrap_or_default()
}

/// Everything that narrows reads on a thread — the vault viewer and the
/// area filter — so work moved to another thread or task reads exactly
/// as the request that started it (daemon `scoped.rs`). Work that loses
/// its scope reads with no viewer, i.e. LOCKED; it also loses any area
/// filter, which is why area-limited keys reach only handlers that read
/// inline or through the scoped helpers.
#[derive(Clone, Default)]
pub struct ReadScope {
    viewer: Option<String>,
    areas: Option<Vec<&'static str>>,
}

/// The read scope in force on this thread.
pub fn capture_read_scope() -> ReadScope {
    ReadScope {
        viewer: VIEWER.with(|v| v.borrow().clone()),
        areas: area_filter(),
    }
}

impl ReadScope {
    /// A request's scope: its viewer, plus the life areas an area-limited
    /// key may see (unknown labels dropped, as in `enter_area_filter`).
    pub fn new(viewer: &str, areas: Option<&[String]>) -> Self {
        let areas = areas.map(|areas| {
            LIFE_AREAS
                .iter()
                .copied()
                .filter(|a| areas.iter().any(|x| x == a))
                .collect()
        });
        ReadScope {
            viewer: Some(viewer.to_string()),
            areas,
        }
    }

    /// Make this the read scope of the current thread until the guard drops.
    pub fn enter(&self) -> ReadScopeGuard {
        let viewer = VIEWER.with(|v| std::mem::replace(&mut *v.borrow_mut(), self.viewer.clone()));
        let areas =
            AREA_FILTER.with(|f| std::mem::replace(&mut *f.borrow_mut(), self.areas.clone()));
        ReadScopeGuard {
            viewer: Some(viewer),
            areas: Some(areas),
        }
    }
}

/// Restores the previous read scope when dropped.
pub struct ReadScopeGuard {
    viewer: Option<Option<String>>,
    areas: Option<Option<Vec<&'static str>>>,
}

impl Drop for ReadScopeGuard {
    fn drop(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            VIEWER.with(|v| *v.borrow_mut() = viewer);
        }
        if let Some(areas) = self.areas.take() {
            AREA_FILTER.with(|f| *f.borrow_mut() = areas);
        }
    }
}

const SCHEMA_VERSION: i64 = 60;

const SCHEMA_V1: &str = "
    -- L2: append-only ground truth. Raw content lives on disk at
    -- content_ref; the whole graph is re-derivable from this table.
    CREATE TABLE episodes (
        id TEXT PRIMARY KEY,
        source TEXT NOT NULL,
        source_ref TEXT NOT NULL,
        kind TEXT NOT NULL,
        title TEXT NOT NULL DEFAULT '',
        actor TEXT NOT NULL,
        session_id TEXT,
        event_time TEXT,                 -- RFC 3339 UTC; NULL iff uncertainty='unknown'
        tz_offset_minutes INTEGER,       -- local offset at the event
        time_uncertainty TEXT NOT NULL,  -- exact|minute|hour|day|month|year|unknown
        ingest_time TEXT NOT NULL,
        trust TEXT NOT NULL,             -- self|imported|agent|untrusted
        scope TEXT NOT NULL,             -- work|personal|family|health|private
        content_ref TEXT,                -- path under the home root
        content_hash TEXT NOT NULL,
        UNIQUE(source, source_ref)
    );
    CREATE INDEX idx_episodes_event_time ON episodes(event_time);
    CREATE INDEX idx_episodes_source ON episodes(source, kind);

    -- L3: entities. merged_into implements reversible merges: resolution
    -- follows the chain at read time; reverting clears the pointer.
    CREATE TABLE entities (
        id TEXT PRIMARY KEY,
        type TEXT NOT NULL,
        name TEXT NOT NULL,
        scope TEXT NOT NULL,
        created_at TEXT NOT NULL,
        merged_into TEXT REFERENCES entities(id)
    );
    CREATE INDEX idx_entities_type_name ON entities(type, name);

    -- Identifying attributes drive cross-source identity: one owner per
    -- (kind, value) — the same email in git and ICS is the same Person.
    CREATE TABLE entity_attributes (
        entity_id TEXT NOT NULL REFERENCES entities(id),
        kind TEXT NOT NULL,
        value TEXT NOT NULL,
        UNIQUE(kind, value)
    );
    CREATE INDEX idx_entity_attributes_entity ON entity_attributes(entity_id);

    -- Provenance: which episodes mention/support which entities, and how.
    CREATE TABLE episode_entities (
        episode_id TEXT NOT NULL,
        entity_id TEXT NOT NULL,
        role TEXT NOT NULL,
        UNIQUE(episode_id, entity_id, role)
    );
    CREATE INDEX idx_episode_entities_entity ON episode_entities(entity_id);

    -- Bi-temporal edges (Graphiti's four timestamps). Contradicted edges
    -- are invalidated (t_invalid) or retracted (t_expired), never deleted.
    CREATE TABLE edges (
        id TEXT PRIMARY KEY,
        src TEXT NOT NULL,
        relation TEXT NOT NULL,
        dst TEXT NOT NULL,
        strength REAL NOT NULL DEFAULT 1.0,
        scope TEXT NOT NULL,
        t_valid TEXT,                    -- when the fact became true
        t_invalid TEXT,                  -- when it stopped being true (NULL = still true)
        t_created TEXT NOT NULL,         -- when we recorded it
        t_expired TEXT                   -- when we retracted the record (NULL = current)
    );
    CREATE INDEX idx_edges_src ON edges(src, relation);
    CREATE INDEX idx_edges_dst ON edges(dst, relation);

    CREATE TABLE edge_provenance (
        edge_id TEXT NOT NULL,
        episode_id TEXT NOT NULL,
        UNIQUE(edge_id, episode_id)
    );

    -- Reversible merge event log; bias to under-merge.
    CREATE TABLE merges (
        id TEXT PRIMARY KEY,
        winner_id TEXT NOT NULL,
        loser_id TEXT NOT NULL,
        confidence REAL NOT NULL,
        reason TEXT NOT NULL DEFAULT '',
        actor TEXT NOT NULL,
        merged_at TEXT NOT NULL,
        reverted_at TEXT
    );

    -- Low-confidence merge candidates await a human yes/no.
    CREATE TABLE merge_review_queue (
        id TEXT PRIMARY KEY,
        entity_a TEXT NOT NULL,
        entity_b TEXT NOT NULL,
        confidence REAL NOT NULL,
        reason TEXT NOT NULL DEFAULT '',
        created_at TEXT NOT NULL,
        resolution TEXT,                 -- NULL=pending, merged|distinct
        UNIQUE(entity_a, entity_b)
    );

    -- The append-only write event log; the change feed reads this.
    CREATE TABLE mutations (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        ts TEXT NOT NULL,
        actor TEXT NOT NULL,
        session_id TEXT,
        idempotency_key TEXT NOT NULL UNIQUE,
        kind TEXT NOT NULL,
        payload TEXT NOT NULL
    );

    -- Agent-proposed memories land here, never directly in high-trust rows.
    CREATE TABLE quarantine (
        id TEXT PRIMARY KEY,
        proposed_at TEXT NOT NULL,
        actor TEXT NOT NULL,
        session_id TEXT,
        kind TEXT NOT NULL,
        content TEXT NOT NULL,           -- JSON
        status TEXT NOT NULL DEFAULT 'pending'  -- pending|accepted|rejected
    );

    -- L1: chunks for hybrid search. embedding stays NULL until Phase 1
    -- (sqlite-vec + local ONNX); the schema is vector-ready now.
    CREATE TABLE chunks (
        id TEXT PRIMARY KEY,
        episode_id TEXT NOT NULL,
        ord INTEGER NOT NULL,
        text TEXT NOT NULL,
        embedding BLOB,
        UNIQUE(episode_id, ord)
    );

    CREATE VIRTUAL TABLE episode_fts USING fts5(
        title,
        body,
        episode_id UNINDEXED,
        tokenize = 'porter unicode61'
    );
";

/// While the vault is locked, the RLM's raw-sql handle sees these TEMP
/// VIEWS instead of the base tables (temp schema resolves first). Episode/
/// entity/edge material is scope-filtered; audit surfaces whose payloads
/// may quote private content (mutations, ask_log) and the rules themselves
/// read as empty. Created and dropped inside one connection lock.
const VAULT_SHADOW_VIEWS: &str = "
    CREATE TEMP VIEW episodes AS
        SELECT * FROM main.episodes WHERE scope != 'private' AND superseded_by IS NULL;
    CREATE TEMP VIEW entities AS
        SELECT * FROM main.entities WHERE scope != 'private' AND hidden_by IS NULL;
    CREATE TEMP VIEW entity_attributes AS
        SELECT a.* FROM main.entity_attributes a
        JOIN main.entities e ON e.id = a.entity_id
        WHERE e.scope != 'private' AND COALESCE(a.scope, '') != 'private';
    CREATE TEMP VIEW episode_entities AS
        SELECT ee.* FROM main.episode_entities ee
        JOIN main.episodes ep ON ep.id = ee.episode_id
        JOIN main.entities en ON en.id = ee.entity_id
        WHERE ep.scope != 'private' AND en.scope != 'private' AND ep.superseded_by IS NULL;
    CREATE TEMP VIEW edges AS
        SELECT * FROM main.edges WHERE scope != 'private';
    CREATE TEMP VIEW chunks AS
        SELECT c.* FROM main.chunks c
        JOIN main.episodes e ON e.id = c.episode_id
        WHERE e.scope != 'private' AND e.superseded_by IS NULL;
    CREATE TEMP VIEW vec_chunks AS
        SELECT * FROM main.vec_chunks WHERE 0;
    CREATE TEMP VIEW episode_fts AS
        SELECT f.title, f.body, f.episode_id FROM main.episode_fts f
        JOIN main.episodes e ON e.id = f.episode_id
        WHERE e.scope != 'private' AND e.superseded_by IS NULL;
    CREATE TEMP VIEW location_points AS
        SELECT p.* FROM main.location_points p
        JOIN main.episodes e ON e.id = p.episode_id
        WHERE e.scope != 'private';
    CREATE TEMP VIEW tasks AS
        SELECT * FROM main.tasks WHERE scope != 'private';
    CREATE TEMP VIEW notes_index AS
        SELECT * FROM main.notes_index WHERE scope != 'private';
    CREATE TEMP VIEW note_mentions AS
        SELECT m.* FROM main.note_mentions m
        JOIN main.notes_index n ON n.id = m.note_id
        WHERE n.scope != 'private';
    CREATE TEMP VIEW task_mentions AS
        SELECT m.* FROM main.task_mentions m
        JOIN main.tasks t ON t.id = m.task_id
        WHERE t.scope != 'private';
    CREATE TEMP VIEW social_connections AS
        SELECT * FROM main.social_connections;
    CREATE TEMP VIEW semantic_pairs AS
        SELECT p.* FROM main.semantic_pairs p
        JOIN main.episodes ea ON ea.id = p.a
        JOIN main.episodes eb ON eb.id = p.b
        WHERE ea.scope != 'private' AND eb.scope != 'private'
          AND ea.superseded_by IS NULL AND eb.superseded_by IS NULL;
    CREATE TEMP VIEW notes_fts AS
        SELECT f.title, f.body, f.note_id FROM main.notes_fts f
        JOIN main.notes_index n ON n.id = f.note_id
        WHERE n.scope != 'private';
    CREATE TEMP VIEW metric_points AS
        SELECT p.* FROM main.metric_points p
        JOIN main.episodes e ON e.id = p.episode_id
        WHERE e.scope != 'private';
    CREATE TEMP VIEW relationships AS
        SELECT r.* FROM main.relationships r
        JOIN main.entities e ON e.id = r.entity_id
        WHERE e.scope != 'private';
    CREATE TEMP VIEW self_model AS
        SELECT * FROM main.self_model WHERE sphere != 'private';
    CREATE TEMP VIEW opportunities AS
        SELECT o.* FROM main.opportunities o
        JOIN main.episodes e ON e.id = o.episode_id
        JOIN main.self_model s ON s.id = o.seeking_id
        WHERE e.scope != 'private' AND s.sphere != 'private' AND e.superseded_by IS NULL;
    CREATE TEMP VIEW mutations AS SELECT * FROM main.mutations WHERE 0;
    CREATE TEMP VIEW quarantine AS SELECT * FROM main.quarantine WHERE 0;
    CREATE TEMP VIEW ask_log AS
        SELECT * FROM main.ask_log
        WHERE COALESCE(thread, id) NOT IN
            (SELECT COALESCE(thread, id) FROM main.ask_log WHERE sensitive != 0);
    CREATE TEMP VIEW llm_usage AS SELECT * FROM main.llm_usage WHERE 0;
    CREATE TEMP VIEW privacy_rules AS SELECT * FROM main.privacy_rules WHERE 0;
";
/// With the vault OPEN the raw-sql handle still never sees retired or
/// replaced memories, nor entities a retirement hid (D.12). episode_fts
/// is left unshadowed here — MATCH can't run against a view — so a raw
/// full-text query reaches retired rows only if it never joins episodes.
/// Dropped by VAULT_SHADOW_DROP (a superset).
const LIVE_SHADOW_VIEWS: &str = "
    CREATE TEMP VIEW episodes AS
        SELECT * FROM main.episodes WHERE superseded_by IS NULL;
    CREATE TEMP VIEW entities AS
        SELECT * FROM main.entities WHERE hidden_by IS NULL;
    CREATE TEMP VIEW episode_entities AS
        SELECT ee.* FROM main.episode_entities ee
        JOIN main.episodes ep ON ep.id = ee.episode_id
        WHERE ep.superseded_by IS NULL;
    CREATE TEMP VIEW chunks AS
        SELECT c.* FROM main.chunks c
        JOIN main.episodes e ON e.id = c.episode_id
        WHERE e.superseded_by IS NULL;
    CREATE TEMP VIEW semantic_pairs AS
        SELECT p.* FROM main.semantic_pairs p
        JOIN main.episodes ea ON ea.id = p.a
        JOIN main.episodes eb ON eb.id = p.b
        WHERE ea.superseded_by IS NULL AND eb.superseded_by IS NULL;
";
const VAULT_SHADOW_DROP: &str = "
    DROP VIEW IF EXISTS temp.location_points;
    DROP VIEW IF EXISTS temp.episodes;
    DROP VIEW IF EXISTS temp.entities;
    DROP VIEW IF EXISTS temp.entity_attributes;
    DROP VIEW IF EXISTS temp.episode_entities;
    DROP VIEW IF EXISTS temp.edges;
    DROP VIEW IF EXISTS temp.chunks;
    DROP VIEW IF EXISTS temp.vec_chunks;
    DROP VIEW IF EXISTS temp.episode_fts;
    DROP VIEW IF EXISTS temp.mutations;
    DROP VIEW IF EXISTS temp.quarantine;
    DROP VIEW IF EXISTS temp.ask_log;
    DROP VIEW IF EXISTS temp.llm_usage;
    DROP VIEW IF EXISTS temp.privacy_rules;
    DROP VIEW IF EXISTS temp.self_model;
    DROP VIEW IF EXISTS temp.opportunities;
    DROP VIEW IF EXISTS temp.tasks;
    DROP VIEW IF EXISTS temp.notes_index;
    DROP VIEW IF EXISTS temp.notes_fts;
    DROP VIEW IF EXISTS temp.metric_points;
    DROP VIEW IF EXISTS temp.relationships;
    DROP VIEW IF EXISTS temp.note_mentions;
    DROP VIEW IF EXISTS temp.task_mentions;
    DROP VIEW IF EXISTS temp.social_connections;
    DROP VIEW IF EXISTS temp.semantic_pairs;
";

/// Register the sqlite-vec extension for every connection this process
/// opens — must run before the first `Connection::open` so the v49
/// `vec_chunks` vec0 table exists on migration and reopen alike.
fn register_sqlite_vec() {
    static VEC_INIT: std::sync::Once = std::sync::Once::new();
    VEC_INIT.call_once(|| unsafe {
        // sqlite-vec's init fn has the standard extension signature but is
        // typed against its own bindings — transmute to rusqlite's ffi
        // types (identical ABI) for auto_extension.
        type InitFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<*const (), InitFn>(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    });
}

impl Store {
    /// Open (or create) the store, running migrations as needed.
    pub fn open(path: &Path) -> Result<Self> {
        register_sqlite_vec();
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA_V1)?;
        }
        if version < 2 {
            // v2: identities can carry a context — the scope material
            // flowing through that identity should default to (e.g. a
            // company email is `work`, a personal one `personal`).
            conn.execute_batch("ALTER TABLE entity_attributes ADD COLUMN scope TEXT;")?;
        }
        if version < 3 {
            // v3: intents — standing queries watched by the consolidation
            // cycle; matches are the cascade seeds (expanded lazily when
            // viewed, not eagerly on match).
            conn.execute_batch(
                "CREATE TABLE intents (
                     id TEXT PRIMARY KEY,
                     name TEXT NOT NULL UNIQUE,
                     query TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     last_checked TEXT,
                     last_seq INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE intent_matches (
                     intent_id TEXT NOT NULL,
                     episode_id TEXT NOT NULL,
                     found_at TEXT NOT NULL,
                     seen INTEGER NOT NULL DEFAULT 0,
                     UNIQUE(intent_id, episode_id)
                 );",
            )?;
        }
        if version < 4 {
            // v4: idle-time LLM scope classification tracks which episodes
            // it has already judged (0 = coarse label only).
            conn.execute_batch(
                "ALTER TABLE episodes ADD COLUMN scope_classified INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if version < 5 {
            // v5: idle-time entity extraction tracks which prose episodes
            // have been mined for Person/Org/Project/Place mentions.
            conn.execute_batch(
                "ALTER TABLE episodes ADD COLUMN extracted INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if version < 6 {
            // v6: full traces of deep queries — chat history for the Ask
            // panel and the raw material for the QA/improvement loop.
            conn.execute_batch(
                "CREATE TABLE ask_log (
                     id TEXT PRIMARY KEY,
                     ts TEXT NOT NULL,
                     actor TEXT NOT NULL,
                     question TEXT NOT NULL,
                     status TEXT NOT NULL DEFAULT 'running', -- running|ok|error
                     answer TEXT,
                     error TEXT,
                     citations TEXT,            -- JSON array
                     transcript TEXT,           -- JSON array of tool rounds
                     rounds INTEGER NOT NULL DEFAULT 0,
                     duration_ms INTEGER
                 );
                 CREATE INDEX idx_ask_log_ts ON ask_log(ts DESC);",
            )?;
        }
        if version < 7 {
            // v7: the source registry — one row per data source, formalizing
            // the materialized/virtualized distinction. Materialized rows are
            // synced from the episode log; virtual rows (HearthFS mounts,
            // federated APIs, other brains) are registered explicitly and
            // surface as RLM tool handles rather than episodes.
            conn.execute_batch(
                "CREATE TABLE sources (
                     name TEXT PRIMARY KEY,
                     kind TEXT NOT NULL DEFAULT 'materialized', -- materialized|virtual
                     adapter TEXT,
                     config TEXT,                -- JSON, adapter/connection specific
                     capabilities TEXT,          -- JSON array (virtual: tool handles)
                     first_seen TEXT NOT NULL,
                     last_refresh TEXT,
                     episode_count INTEGER NOT NULL DEFAULT 0
                 );",
            )?;
        }
        if version < 8 {
            // v8: privacy rules — patterns that force scope=private, both
            // retroactively (at rule creation) and at ingest. The vault
            // switch itself is runtime state, not schema.
            conn.execute_batch(
                "CREATE TABLE privacy_rules (
                     id TEXT PRIMARY KEY,
                     kind TEXT NOT NULL,   -- domain|entity|place|term
                     pattern TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     actor TEXT NOT NULL,
                     UNIQUE(kind, pattern)
                 );",
            )?;
        }
        if version < 9 {
            // v9: vault-gate the Ask history. An ask that ran while the
            // vault was unlocked may quote private episodes in its answer or
            // transcript, so it is hidden from the Ask history/detail while
            // locked. New rows get the accurate flag at ask_start; existing
            // rows are conservatively flagged sensitive (their vault state at
            // run time is unknowable).
            conn.execute_batch(
                "ALTER TABLE ask_log ADD COLUMN sensitive INTEGER NOT NULL DEFAULT 0;
                 UPDATE ask_log SET sensitive = 1;",
            )?;
        }
        if version < 10 {
            // v10: multi-turn Ask. A follow-up question carries the ROOT
            // ask id of its conversation in `thread` (NULL = standalone or
            // thread root), so history can replay whole conversations and
            // the RLM gets prior turns as context.
            conn.execute_batch("ALTER TABLE ask_log ADD COLUMN thread TEXT;")?;
        }
        if version < 11 {
            // v11: the location index behind the Atlas. Derived from
            // episode content (photo GPS lines, workout GPX starts) at
            // consolidation — rebuildable, like every index (law #1).
            // Reads MUST join episodes for the vault scope guard.
            conn.execute_batch(
                "CREATE TABLE location_points (
                     episode_id TEXT NOT NULL,
                     ts TEXT,
                     day TEXT NOT NULL,
                     lat REAL NOT NULL,
                     lon REAL NOT NULL
                 );
                 CREATE INDEX idx_location_points_day ON location_points(day);
                 CREATE INDEX idx_location_points_episode ON location_points(episode_id);
                 ALTER TABLE episodes ADD COLUMN location_scanned INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if version < 12 {
            // v12: episodes.fts_rowid — a direct pointer into episode_fts.
            // FTS5 cannot index equality on a plain column, so every
            // `JOIN episode_fts ON f.episode_id = e.id` was a full FTS
            // scan PER ROW (quadratic; at 66k episodes the extraction
            // query froze the daemon for minutes each cycle). Rowid
            // probes are O(log n). Backfill uses ONE FTS scan via a
            // temp map — the naive correlated subquery would itself be
            // quadratic.
            conn.execute_batch(
                "ALTER TABLE episodes ADD COLUMN fts_rowid INTEGER;
                 CREATE TEMP TABLE fts_map AS SELECT rowid AS r, episode_id FROM episode_fts;
                 CREATE INDEX temp.idx_fts_map ON fts_map(episode_id);
                 UPDATE episodes SET fts_rowid =
                     (SELECT r FROM temp.fts_map WHERE episode_id = episodes.id);
                 DROP TABLE temp.fts_map;",
            )?;
        }
        if version < 13 {
            // v13: edge strength was never incremented — all 37k edges
            // sat at 1.0, so "strongest first" ordering was arbitrary
            // (rowid ties → ancient one-off correspondents topped the
            // constellation). Backfill from edge_provenance, which has
            // recorded every supporting episode all along; assert_edge
            // now increments going forward.
            conn.execute_batch(
                "UPDATE edges SET strength = MAX(1, COALESCE(
                     (SELECT COUNT(*) FROM edge_provenance ep WHERE ep.edge_id = edges.id), 1));",
            )?;
        }
        if version < 14 {
            // v14: the epistemic ledger + the reasoning layer's tables.
            // `derivation` marks HOW something entered the record:
            //   observed — absorbed from a data flow (the adapters);
            //   declared — a human stated it directly (tasks, notes,
            //              confirmed propositions);
            //   inferred — a conclusion Hearthloom reached by layering
            //              information (digests, chapters, cascades,
            //              opportunities). Inferred rows must cite the
            //              observed episodes they reasoned from, and Ask
            //              can be told to ignore them entirely.
            // Backfill is deterministic: only the consolidation source has
            // ever written inferred material. Aggregates that are plain
            // arithmetic (health_day rollups) stay `observed` — inferred
            // is reserved for semantic layering (the design log).
            conn.execute_batch(
                "ALTER TABLE episodes ADD COLUMN derivation TEXT NOT NULL DEFAULT 'observed';
                 UPDATE episodes SET derivation = 'inferred' WHERE source = 'consolidation';

                 -- Human-declared entity importance (0 = unset, 1..3). Ranking
                 -- surfaces use max(declared, observed strength) — the sky
                 -- should show your mother, not your busiest mailing list.
                 ALTER TABLE entities ADD COLUMN declared_importance INTEGER NOT NULL DEFAULT 0;

                 -- Intent decomposition: an idle LLM pass splits each goal
                 -- into a few standing queries, matched by FTS or embedding.
                 ALTER TABLE intents ADD COLUMN decomposed INTEGER NOT NULL DEFAULT 0;
                 CREATE TABLE standing_queries (
                     id TEXT PRIMARY KEY,
                     intent_id TEXT NOT NULL,
                     query TEXT NOT NULL,
                     embedding BLOB,
                     created_at TEXT NOT NULL,
                     UNIQUE(intent_id, query)
                 );

                 -- Cascades expand fresh matches once, at idle. The pre-v14
                 -- backlog is marked handled: no LLM spend on old news.
                 ALTER TABLE intent_matches ADD COLUMN cascaded INTEGER NOT NULL DEFAULT 0;
                 UPDATE intent_matches SET cascaded = 1;
                 -- timeline_notable probes matches per episode; without
                 -- this the EXISTS is a table scan per outer row.
                 CREATE INDEX idx_intent_matches_episode ON intent_matches(episode_id);

                 -- The self-model: one proposition per row, grown from
                 -- episode evidence at idle (origin=inferred, status=
                 -- proposed) or stated by the human (origin=declared).
                 -- Confirmation is recorded without laundering the origin.
                 CREATE TABLE self_model (
                     id TEXT PRIMARY KEY,
                     sphere TEXT NOT NULL,        -- scope vocabulary: work|personal|family|health|private
                     kind TEXT NOT NULL,          -- identity|offering|seeking|strength|constraint
                     text TEXT NOT NULL,
                     origin TEXT NOT NULL,        -- declared|inferred
                     status TEXT NOT NULL DEFAULT 'proposed',  -- proposed|confirmed|retired|rejected
                     evidence TEXT,               -- JSON array of episode ids
                     embedding BLOB,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     confirmed_at TEXT
                 );

                 -- Confirmed `seeking` propositions matched against new
                 -- episodes at idle; keep mints a kind=opportunity episode.
                 CREATE TABLE opportunities (
                     id TEXT PRIMARY KEY,
                     seeking_id TEXT NOT NULL REFERENCES self_model(id),
                     episode_id TEXT NOT NULL,
                     score REAL NOT NULL,
                     reason TEXT,
                     status TEXT NOT NULL DEFAULT 'pending',  -- pending|kept|dismissed
                     found_at TEXT NOT NULL,
                     resolved_at TEXT,
                     UNIQUE(seeking_id, episode_id)
                 );

                 -- Watermarks for consolidation passes that scan the
                 -- mutations feed (opportunities, merge candidates).
                 CREATE TABLE consolidation_state (
                     key TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );",
            )?;
        }
        if version < 15 {
            // v15: the productivity layer — the present is operational
            // tables (mutable rows, daemon-written, mutation-logged, the
            // intents precedent); the past stays episodes. Completing a
            // task / settling a note MINTS an episode; these tables are
            // never themselves the record.
            conn.execute_batch(
                "CREATE TABLE tasks (
                     id TEXT PRIMARY KEY,
                     kind TEXT NOT NULL DEFAULT 'plain',   -- plain|checklist|workout|form
                     description TEXT NOT NULL,
                     payload TEXT,              -- JSON: list items / exercises / form values
                     template_id TEXT,          -- form tasks: the defining template
                     frequency TEXT NOT NULL DEFAULT 'one-time', -- one-time|daily|weekly|monthly
                     scope TEXT NOT NULL DEFAULT 'personal',
                     goal_id TEXT,              -- linked intent (goals ARE intents)
                     status TEXT NOT NULL DEFAULT 'open',  -- open|done|backlog|deleted
                     remind_at TEXT,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     completed_at TEXT,
                     linked_note TEXT           -- note created at completion
                 );
                 CREATE INDEX idx_tasks_status ON tasks(status, remind_at);

                 -- User-defined structured task types (codi's hashtag
                 -- polymorphism, made explicit): fields is a JSON array of
                 -- {key, label, type: text|number|select|checkbox, options?}.
                 CREATE TABLE task_templates (
                     id TEXT PRIMARY KEY,
                     name TEXT NOT NULL UNIQUE,
                     fields TEXT NOT NULL,
                     note_on_complete INTEGER NOT NULL DEFAULT 0,
                     created_at TEXT NOT NULL
                 );

                 -- Notes are markdown FILES under ~/.hearthloom/notes/;
                 -- this is only their index (+ notes_fts). Versioned for
                 -- optimistic locking; settling snapshots an episode.
                 CREATE TABLE notes_index (
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     slug TEXT NOT NULL UNIQUE,
                     path TEXT NOT NULL,
                     scope TEXT NOT NULL DEFAULT 'personal',
                     version INTEGER NOT NULL DEFAULT 1,
                     status TEXT NOT NULL DEFAULT 'active', -- active|archived|deleted
                     linked_task TEXT,
                     tags TEXT,                 -- JSON array
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     settled_at TEXT
                 );
                 CREATE VIRTUAL TABLE notes_fts USING fts5(
                     title, body, note_id UNINDEXED, tokenize = 'porter unicode61'
                 );

                 -- Derived numeric series from structured episodes
                 -- (workout volume, task completions) — rebuildable, like
                 -- every index; Pulse and vquery-style reads live here.
                 CREATE TABLE metric_points (
                     episode_id TEXT NOT NULL,
                     day TEXT NOT NULL,
                     metric TEXT NOT NULL,
                     value REAL NOT NULL,
                     unit TEXT,
                     UNIQUE(episode_id, metric)
                 );
                 CREATE INDEX idx_metric_points_metric ON metric_points(metric, day);

                 -- Goals ARE intents, grown up: prose description, life
                 -- sphere, status, optional target date.
                 ALTER TABLE intents ADD COLUMN description TEXT;
                 ALTER TABLE intents ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
                 ALTER TABLE intents ADD COLUMN sphere TEXT;
                 ALTER TABLE intents ADD COLUMN target_date TEXT;",
            )?;
        }
        if version < 16 {
            // v16: task tags — codi's hashtag flow, kept: typing
            // "#urgent buy filters #home" tags the task and cleans the
            // description; #urgent/#important drive ordering, #daily/
            // #weekly/#monthly set frequency, #checklist/#workout set
            // the kind, and #<template-name> binds a form template
            // ("data definition requirements"). Notes already carry tags.
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN tags TEXT;")?;
        }
        if version < 17 {
            // v17: note access counter (codi's note_views, minimally) —
            // "most-accessed" is the Notes dial's label priority. A
            // telemetry counter like the intents watermark, not a
            // mutation-logged write.
            conn.execute_batch(
                "ALTER TABLE notes_index ADD COLUMN views INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if version < 18 {
            // v18: note origin — HOW a note got here ('hearthloom' =
            // written in the app, 'codi' = the one-time harvest; future
            // importers: 'notion', …). Provenance for the working set,
            // mirroring what episodes.source does for the record.
            conn.execute_batch(
                "ALTER TABLE notes_index ADD COLUMN origin TEXT NOT NULL DEFAULT 'hearthloom';",
            )?;
        }
        if version < 19 {
            // v19: templates are a KIND of note (Jon) — a markdown file
            // with a ---template frontmatter block (form fields,
            // note-on-complete) and a {{placeholder}} skeleton body.
            // The task system selects from template notes; the old
            // task_templates table is retired (never used live).
            conn.execute_batch(
                "ALTER TABLE notes_index ADD COLUMN kind TEXT NOT NULL DEFAULT 'note';",
            )?;
        }
        if version < 20 {
            // v20: relationships — how the human relates to the people
            // and orgs that matter. Kinds are PREDICTED at idle from
            // episode evidence (origin=inferred, status=proposed) and
            // confirmed by the human, or declared outright; origin is
            // never laundered (the self-model discipline). t_valid/
            // t_invalid make endings history, not deletions.
            conn.execute_batch(
                "CREATE TABLE relationships (
                     id TEXT PRIMARY KEY,
                     entity_id TEXT NOT NULL,
                     kind TEXT NOT NULL,        -- friend|family|colleague|collaborator|client|investor|advisor|acquaintance|service
                     origin TEXT NOT NULL,      -- declared|inferred
                     status TEXT NOT NULL DEFAULT 'proposed', -- proposed|confirmed|rejected|ended
                     evidence TEXT,             -- JSON episode ids (inferred)
                     note TEXT,                 -- freeform: how you met, context
                     t_valid TEXT,
                     t_invalid TEXT,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     UNIQUE(entity_id, kind)
                 );
                 CREATE INDEX idx_relationships_entity ON relationships(entity_id);",
            )?;
        }
        if version < 21 {
            // v21: note mentions — @[Name] tokens in note bodies resolve
            // to entities in a LINK TABLE, so "notes about Priya" is a
            // query, never a scan of markdown files. Derived + rebuilt
            // on every save; the file remains the truth.
            conn.execute_batch(
                "CREATE TABLE note_mentions (
                     note_id TEXT NOT NULL,
                     entity_id TEXT NOT NULL,
                     UNIQUE(note_id, entity_id)
                 );
                 CREATE INDEX idx_note_mentions_entity ON note_mentions(entity_id);",
            )?;
        }
        if version < 22 {
            // v22: task mentions — the note_mentions pattern for tasks
            // ("Send report to @[Sam]"). Derived from the description on
            // every add/edit; "tasks about Ali" is a query, never a scan.
            conn.execute_batch(
                "CREATE TABLE task_mentions (
                     task_id TEXT NOT NULL,
                     entity_id TEXT NOT NULL,
                     UNIQUE(task_id, entity_id)
                 );
                 CREATE INDEX idx_task_mentions_entity ON task_mentions(entity_id);",
            )?;
        }
        if version < 23 {
            // v23: the social-connections registry — network friend/
            // follow lists (Facebook first) WITHOUT minting entities.
            // 3k mostly-casual names as evidence-less Person rows would
            // pollute mentions/browse/merge-scan; instead they surface
            // as dossier chips + "beyond the tiers" search hits, and
            // materialize into an entity only on an explicit human
            // gesture (or by actually interacting — messages/tags).
            conn.execute_batch(
                "CREATE TABLE social_connections (
                     network TEXT NOT NULL,
                     name TEXT NOT NULL,
                     connected_at TEXT,
                     ended_at TEXT,
                     status TEXT NOT NULL DEFAULT 'active', -- active|removed
                     UNIQUE(network, name)
                 );
                 CREATE INDEX idx_social_connections_name
                     ON social_connections(name COLLATE NOCASE);",
            )?;
        }
        if version < 24 {
            // v24: the Galaxy's semantic comet pairs — nearest-neighbor
            // notable episodes ACROSS sources, recomputed at idle from
            // existing chunk embeddings (derived + rebuildable; law #3:
            // never computed on the view's query path).
            conn.execute_batch(
                "CREATE TABLE semantic_pairs (
                     a TEXT NOT NULL,
                     b TEXT NOT NULL,
                     score REAL NOT NULL,
                     computed_at TEXT NOT NULL,
                     UNIQUE(a, b)
                 );",
            )?;
        }
        if version < 25 {
            // v25: registry rows carry what the network knows about the
            // connection — LinkedIn exports company/position/email per
            // row, which is the "which Maria?" answer for 30k names
            // that will mostly never become entities.
            conn.execute_batch(
                "ALTER TABLE social_connections ADD COLUMN company TEXT;
                 ALTER TABLE social_connections ADD COLUMN position TEXT;
                 ALTER TABLE social_connections ADD COLUMN handle TEXT;",
            )?;
        }
        if version < 26 {
            // v26: notes carry their source-native id (Notion page-id,
            // future importers) — re-imports become idempotent UPSERTS:
            // changed pages update in place, unchanged skip, nothing
            // duplicates. The episode source_ref discipline, for notes.
            conn.execute_batch(
                "ALTER TABLE notes_index ADD COLUMN origin_ref TEXT;
                 CREATE INDEX idx_notes_origin_ref ON notes_index(origin, origin_ref);",
            )?;
        }
        if version < 27 {
            // v27: note dates become NULLABLE — an import whose source
            // carries no dates records NULL (honest "unknown"), never
            // the import moment masquerading as activity (Jon
            // 2026-07-10). NULLs sort as OLDEST under recency (SQLite
            // DESC puts NULLs last). Created-but-no-modified sources
            // get modified=created at the importer. Rebuild (SQLite
            // can't drop NOT NULL) + the one-time cleanup of the
            // 2026-07-10 notion import stamps.
            conn.execute_batch(
                "CREATE TABLE notes_index_v27 (
                     id TEXT PRIMARY KEY,
                     title TEXT NOT NULL,
                     slug TEXT NOT NULL UNIQUE,
                     path TEXT NOT NULL,
                     scope TEXT NOT NULL DEFAULT 'personal',
                     version INTEGER NOT NULL DEFAULT 1,
                     status TEXT NOT NULL DEFAULT 'active',
                     linked_task TEXT,
                     tags TEXT,
                     created_at TEXT,
                     updated_at TEXT,
                     settled_at TEXT,
                     views INTEGER NOT NULL DEFAULT 0,
                     origin TEXT NOT NULL DEFAULT 'hearthloom',
                     kind TEXT NOT NULL DEFAULT 'note',
                     origin_ref TEXT
                 );
                 INSERT INTO notes_index_v27 SELECT * FROM notes_index;
                 DROP TABLE notes_index;
                 ALTER TABLE notes_index_v27 RENAME TO notes_index;
                 CREATE INDEX idx_notes_origin_ref ON notes_index(origin, origin_ref);
                 UPDATE notes_index SET created_at = NULL, updated_at = NULL
                  WHERE origin = 'notion'
                    AND substr(created_at, 1, 10) = '2026-07-10'
                    AND substr(updated_at, 1, 10) = '2026-07-10';",
            )?;
        }
        if version < 28 {
            // v28: a goal may reference a PROJECT (the Project entity it
            // advances) — the goals rail shows "project: X"; projects
            // are containers, goals are aims (kept distinct, Jon).
            conn.execute_batch("ALTER TABLE intents ADD COLUMN project_id TEXT;")?;
        }
        if version < 29 {
            // v29: goals ↔ projects is MANY-TO-MANY (a goal can advance
            // several projects; a project can serve several goals) — a
            // junction replaces the single intents.project_id (its rows
            // migrate in). And a note can be the 1:1 PRIMARY (description)
            // note for a subject (a project entity id or a goal name):
            // notes_index.primary_for holds that subject.
            conn.execute_batch(
                "CREATE TABLE goal_projects (
                     goal_id TEXT NOT NULL,
                     project_id TEXT NOT NULL,
                     UNIQUE(goal_id, project_id)
                 );
                 INSERT OR IGNORE INTO goal_projects (goal_id, project_id)
                     SELECT id, project_id FROM intents WHERE project_id IS NOT NULL;
                 ALTER TABLE notes_index ADD COLUMN primary_for TEXT;
                 CREATE INDEX idx_notes_primary_for ON notes_index(primary_for);",
            )?;
        }
        if version < 30 {
            // v30: api_keys — scoped bearer tokens for third-party
            // integrations (HearthFS first). Only the SHA-256 of the
            // secret is stored; the plaintext is shown exactly once at
            // creation. Loopback dashboard access stays keyless unless
            // `api.require_keys` is set; NEW integration surfaces (the
            // discovery manifest) require a key ALWAYS.
            conn.execute_batch(
                "CREATE TABLE api_keys (
                     id TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     key_hash TEXT NOT NULL UNIQUE,
                     scopes TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     created_by TEXT NOT NULL,
                     expires_at TEXT,
                     last_used_at TEXT,
                     revoked_at TEXT
                 );",
            )?;
        }
        if version < 31 {
            // v31: the auth layer for remote postures (deployment modes
            // doctrine, 2026-07-11). One admin user for now (argon2id
            // hash), DB-backed sessions (token stored hashed, like
            // api_keys). THE POSTURE RULE: auth is required iff a user
            // row exists — bootstrap creates the admin and the door
            // locks in the same gesture; no config flag to forget.
            conn.execute_batch(
                "CREATE TABLE users (
                     id TEXT PRIMARY KEY,
                     username TEXT NOT NULL UNIQUE,
                     password_hash TEXT NOT NULL,
                     created_at TEXT NOT NULL
                 );
                 CREATE TABLE sessions (
                     token_hash TEXT PRIMARY KEY,
                     user_id TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     expires_at TEXT NOT NULL
                 );",
            )?;
        }
        if version < 32 {
            // v32: account identity is an EMAIL, not a free-form
            // username (Jon, at the bootstrap prompt: "what is that
            // supposed to be?"). Also the natural join key for
            // a provisioner's trusted-issuer logins (provisioner accounts
            // are email-keyed). No install ever created a v31 user, so this
            // is a rename, not a data migration.
            conn.execute_batch("ALTER TABLE users RENAME COLUMN username TO email;")?;
        }
        if version < 33 {
            // v33: the network layer — grants govern what a connected
            // friend's brain may read. One row per friend (keyed by the
            // name THIS node gave their key); spheres is a JSON array of
            // scope labels ('private' is NEVER grantable — enforced in
            // store/network.rs, the brain_fs discipline); detail is
            // excerpts|full; revoke = timestamp; every network access is
            // recorded in mutations (transparency both ways).
            conn.execute_batch(
                "CREATE TABLE grants (
                     friend TEXT PRIMARY KEY,
                     spheres TEXT NOT NULL,
                     detail TEXT NOT NULL DEFAULT 'excerpts',
                     created_at TEXT NOT NULL,
                     expires_at TEXT,
                     revoked_at TEXT
                 );",
            )?;
        }
        if version < 34 {
            // v34: scope changes REPLICATE between a user's own peered
            // nodes (Jon reversed private-stays-home for self-owned
            // peers, 2026-07-11: a privacy relabel on one side must take
            // effect on both; the FRIEND boundary still never carries
            // private). scope_changed_at is the LWW timestamp every
            // scope-writing path stamps; sync exchanges stamped rows.
            conn.execute_batch("ALTER TABLE episodes ADD COLUMN scope_changed_at TEXT;")?;
        }
        if version < 35 {
            // v35: observability (Jon, 2026-07-12). syslog = structured
            // system events (startup/shutdown/version/sync/auth…);
            // health_points = health-check ticks and numeric samples
            // (cpu/mem/disk/reachability). Both are NODE-TAGGED
            // ('local'|'cloud') and replicate append-only between a
            // user's own peers, so either dashboard shows both nodes.
            // Operational telemetry, never episodes; pruned on write.
            conn.execute_batch(
                "CREATE TABLE syslog (
                     id TEXT PRIMARY KEY,
                     ts TEXT NOT NULL,
                     node TEXT NOT NULL,
                     level TEXT NOT NULL,
                     event TEXT NOT NULL,
                     message TEXT NOT NULL,
                     meta TEXT
                 );
                 CREATE INDEX idx_syslog_ts ON syslog(node, ts);
                 CREATE TABLE health_points (
                     ts TEXT NOT NULL,
                     node TEXT NOT NULL,
                     check_name TEXT NOT NULL,
                     ok INTEGER,
                     value REAL,
                     unit TEXT,
                     UNIQUE(node, ts, check_name)
                 );
                 CREATE INDEX idx_health_ts ON health_points(check_name, node, ts);",
            )?;
        }
        if version < 36 {
            // v36: wiki-links — [[Note Title]] in a note body links note
            // to note (the note_mentions pattern: derived, rebuilt from
            // the body on every save; an unknown title simply doesn't
            // link). Backlinks are the reverse read of the same table.
            // note_neighbors: the RESONANT lane of a note's connections
            // panel — nearest-neighbor notes from their settled
            // snapshots' existing chunk embeddings. Derived, recomputed
            // at idle (full replace), never on the query path.
            conn.execute_batch(
                "CREATE TABLE note_links (
                     note_id TEXT NOT NULL,
                     target_note_id TEXT NOT NULL,
                     PRIMARY KEY (note_id, target_note_id)
                 );
                 CREATE INDEX idx_note_links_target ON note_links(target_note_id);
                 CREATE TABLE note_neighbors (
                     note_id TEXT NOT NULL,
                     neighbor_note_id TEXT NOT NULL,
                     score REAL NOT NULL,
                     PRIMARY KEY (note_id, neighbor_note_id)
                 );
                 CREATE TABLE lint_findings (
                     id TEXT PRIMARY KEY,
                     kind TEXT NOT NULL,
                     title TEXT NOT NULL,
                     body TEXT NOT NULL,
                     refs TEXT NOT NULL DEFAULT '[]',
                     created_at TEXT NOT NULL,
                     resolved_at TEXT,
                     resolution TEXT
                 );",
            )?;
        }
        if version < 37 {
            // v37: the rename audit trail (item 14). Append-only log of
            // entity renames so dependencies AND the peer node reconcile
            // a rename instead of seeing a new entity. Carries the TYPE
            // and both names because entity ids differ across peers —
            // replay resolves by (type, from_name), the
            // relationships_by_name precedent.
            conn.execute_batch(
                "CREATE TABLE entity_renames (
                     id TEXT PRIMARY KEY,
                     entity_id TEXT NOT NULL,
                     entity_type TEXT NOT NULL,
                     from_name TEXT NOT NULL,
                     to_name TEXT NOT NULL,
                     actor TEXT NOT NULL,
                     at TEXT NOT NULL
                 );
                 CREATE INDEX idx_entity_renames_at ON entity_renames(at);",
            )?;
        }
        if version < 38 {
            // v38: the notification queue (design locked 2026-07-16).
            // Hints, never data; UPSERT by (origin, dedupe_key) so a
            // renewal countdown updates one row instead of stacking.
            conn.execute_batch(
                "CREATE TABLE notifications (
                     id TEXT PRIMARY KEY,
                     ts TEXT NOT NULL,
                     severity TEXT NOT NULL DEFAULT 'info',
                     origin TEXT NOT NULL,
                     title TEXT NOT NULL,
                     body TEXT NOT NULL DEFAULT '',
                     link TEXT,
                     link_label TEXT,
                     dedupe_key TEXT,
                     read_at TEXT,
                     dismissed_at TEXT
                 );
                 CREATE UNIQUE INDEX idx_notifications_dedupe
                     ON notifications(origin, dedupe_key)
                     WHERE dedupe_key IS NOT NULL;
                 CREATE INDEX idx_notifications_ts ON notifications(ts);",
            )?;
        }
        if version < 39 {
            // v39: the extraction-verdict ledger (design locked
            // 2026-07-17) — the unrepeatable LLM decision per episode,
            // tier-stamped, superseded in place. Links regenerate from
            // verdicts on every node; the ledger is what replicates.
            conn.execute_batch(
                "CREATE TABLE extraction_verdicts (
                     episode_id TEXT PRIMARY KEY,
                     entities TEXT NOT NULL DEFAULT '[]',
                     tier TEXT NOT NULL,
                     model TEXT NOT NULL DEFAULT '',
                     ts TEXT NOT NULL
                 );
                 CREATE INDEX idx_extraction_verdicts_ts ON extraction_verdicts(ts);",
            )?;
        }
        if version < 40 {
            // v40: backlog partial indexes (2026-07-19) — the idle-pass
            // candidate queries (scope classification, extraction,
            // embedding backfill/coverage) full-scanned episodes/chunks
            // per call; on a freshly-synced 48k-episode tenant each call
            // read the whole 1.5GB db under the store lock and the node
            // starved. Each index's WHERE matches its query verbatim
            // (the partial-index implication rule — don't reword one
            // without the other).
            conn.execute_batch(
                "CREATE INDEX idx_episodes_scope_backlog
                     ON episodes(COALESCE(event_time, ingest_time) DESC)
                     WHERE scope_classified = 0 AND scope != 'private'
                       AND source LIKE 'chatlog-%';
                 CREATE INDEX idx_episodes_extraction_backlog
                     ON episodes(source, COALESCE(event_time, ingest_time) DESC)
                     WHERE extracted = 0 AND scope != 'private';
                 CREATE INDEX idx_chunks_unembedded ON chunks(id)
                     WHERE embedding IS NULL;
                 CREATE INDEX idx_chunks_embedded ON chunks(id)
                     WHERE embedding IS NOT NULL;",
            )?;
        }
        if version < 41 {
            // v41: the private-id set index (2026-07-19) — vault guards
            // rewritten from correlated NOT EXISTS to
            // `episode_id NOT IN (SELECT id FROM episodes WHERE scope =
            // 'private')` (top_entities, dossier count). The subquery
            // reads ONLY this covering partial index; without it, each
            // guard probed the whole episodes table per outer row and a
            // /v1/stats call held the store lock 36s+ on the tenant.
            // WHERE matches the query verbatim (the v40 rule).
            conn.execute_batch(
                "CREATE INDEX idx_episodes_private_id ON episodes(id)
                     WHERE scope = 'private';",
            )?;
        }
        if version < 42 {
            // v42: the plugin registry + plugin KV (docs/plugins.md).
            // `plugins` = installed-on-this-node list (manifest, GRANTED
            // scopes, PAT binding, enabled). `plugin_kv` = daemon-mediated
            // storage (law #4 — plugins never open the db or files
            // themselves); updated_at is the peering-sync watermark.
            conn.execute_batch(
                "CREATE TABLE plugins (
                     name TEXT PRIMARY KEY,
                     version TEXT NOT NULL,
                     manifest TEXT NOT NULL,
                     granted_scopes TEXT NOT NULL DEFAULT '[]',
                     api_key_id TEXT NOT NULL,
                     sync TEXT NOT NULL DEFAULT 'local-only',
                     enabled INTEGER NOT NULL DEFAULT 1,
                     installed_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL
                 );
                 CREATE TABLE plugin_kv (
                     plugin TEXT NOT NULL,
                     key TEXT NOT NULL,
                     value TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     PRIMARY KEY (plugin, key)
                 );
                 CREATE INDEX idx_plugin_kv_updated
                     ON plugin_kv(updated_at);",
            )?;
        }
        if version < 43 {
            // v43: the operational journal — an append-only, content-bearing
            // ledger of every create/change/delete to operational rows
            // (tasks first, then notes/goals/self-model/privacy). Unlike the
            // `mutations` breadcrumb log, `post_image` carries the WHOLE row,
            // so operational state is reconstructable and a lost/edited/
            // purged task can be restored.
            conn.execute_batch(
                "CREATE TABLE op_journal (
                     seq INTEGER PRIMARY KEY,
                     entry_id TEXT UNIQUE,
                     entity_kind TEXT NOT NULL,
                     entity_id TEXT NOT NULL,
                     op TEXT NOT NULL,            -- create | update | delete
                     post_image TEXT,            -- full row JSON (present even on delete = tombstone content)
                     actor TEXT NOT NULL,
                     ts TEXT NOT NULL
                 );
                 CREATE INDEX idx_op_journal_entity
                     ON op_journal(entity_kind, entity_id, ts);
                 CREATE INDEX idx_op_journal_seq ON op_journal(seq);",
            )?;
        }
        if version < 44 {
            // v44: durable, idempotent store for LIVE HealthKit samples
            // streamed from the iOS app (POST /v1/ingest/health). The
            // export.xml `health.vidx.sqlite` sidecar stays ephemeral +
            // rebuildable; THIS is the authoritative store for
            // device-streamed data — there is no export.xml to rebuild it
            // from — which the day-finalize pass aggregates into
            // `health_day` episodes via the SAME apple_health::DayTable
            // the export path uses. uuid PRIMARY KEY = HKSample.uuid, so
            // re-streamed batches INSERT OR IGNORE to a no-op.
            // See hearthloom-ios docs/server-implementation-spec.md.
            conn.execute_batch(
                "CREATE TABLE health_sample (
                     uuid              TEXT PRIMARY KEY,
                     metric            TEXT NOT NULL,
                     value             REAL,
                     unit              TEXT,
                     category          TEXT,
                     source            TEXT NOT NULL,
                     start_utc         TEXT NOT NULL,
                     end_utc           TEXT NOT NULL,
                     tz_offset_minutes INTEGER NOT NULL,
                     day               TEXT NOT NULL,
                     ingest_time       TEXT NOT NULL
                 );
                 CREATE INDEX idx_health_sample_day ON health_sample(day);
                 CREATE INDEX idx_health_sample_metric_day
                     ON health_sample(metric, day);",
            )?;
        }
        if version < 45 {
            // v45: index episodes.fts_rowid. The v12 pointer made
            // `episodes → probe FTS by rowid` fast, but three queries
            // (network_search, brain_fs search, cascades' FTS leg) drive
            // the join the OTHER way — `FROM episode_fts MATCH … JOIN
            // episodes ON e.fts_rowid = f.rowid` — and without this
            // index every FTS match probed episodes with a full scan:
            // quadratic on common tokens. A friend-connect probe
            // (q=hello) ground a hosted tenant into a full wedge on
            // 2026-07-28. With the index both join directions are
            // point probes.
            conn.execute_batch("CREATE INDEX idx_episodes_fts_rowid ON episodes(fts_rowid);")?;
        }
        if version < 46 {
            // v46: granular friend sharing (design log 2026-07-28).
            // grants.sharing — a JSON sharing descriptor: tri-state class
            //   map (episodes/tasks/notes/relationships/profile: "on"/"off",
            //   absent = inherit), optional spheres/detail overrides, and
            //   the completed_tasks / evidence_relationships flags. The
            //   reserved row friend='__default__' holds the TEMPLATE every
            //   connection inherits; per-friend rows override per key.
            //   Existing friend rows are FROZEN to their current behavior
            //   (episodes-only with their explicit spheres/detail) so the
            //   migration changes nothing observable.
            // shared_items — explicit per-item, per-friend shares (the
            //   union leg: an item share grants access even when the
            //   class is categorically off). created_at is the LWW clock
            //   (rides the operational mirror + journal like tasks).
            conn.execute_batch(
                "ALTER TABLE grants ADD COLUMN sharing TEXT;
                 UPDATE grants SET sharing = json_object(
                     'classes', json_object('episodes','on'),
                     'spheres', json(spheres),
                     'detail', detail)
                 WHERE sharing IS NULL;
                 INSERT OR IGNORE INTO grants
                     (friend, spheres, detail, created_at, sharing)
                 VALUES ('__default__', '[]', 'excerpts', datetime('now'),
                     json_object('classes',
                         json_object('episodes','on','profile','on'),
                         'spheres', json_array(),
                         'detail', 'excerpts'));
                 CREATE TABLE shared_items (
                     id TEXT PRIMARY KEY,
                     kind TEXT NOT NULL,          -- task | note | relationship
                     item_id TEXT NOT NULL,
                     friend TEXT NOT NULL,
                     collaborate INTEGER NOT NULL DEFAULT 0,
                     expires_at TEXT,
                     created_at TEXT NOT NULL,
                     actor TEXT NOT NULL,
                     UNIQUE(kind, item_id, friend)
                 );
                 CREATE INDEX idx_shared_items_friend ON shared_items(friend);
                 CREATE INDEX idx_shared_items_item ON shared_items(kind, item_id);",
            )?;
        }
        if version < 47 {
            // v47: photo-location union + episode SUPERSESSION
            // (design log 2026-07-28, CLAUDE.md "the supersession
            // pattern"). photo_point is the durable per-photo store both
            // the Mac Photos.sqlite scan and the iOS geotag stream feed —
            // keyed on <unix-capture-secs>|<filename>, the identity
            // visible to BOTH sources, so they UNION instead of racing.
            // photo_day records what each minted photos#DAY episode
            // contains (count + keys hash) so the finalize pass re-mints
            // ONLY when the union strictly grows. episodes.superseded_by
            // is LIFECYCLE METADATA (the scope-correction precedent —
            // content itself stays append-only): a fuller re-mint under
            // the revisioned ref photos#DAY#r<n> marks prior revisions
            // superseded; guarded read surfaces exclude them.
            conn.execute_batch(
                "CREATE TABLE photo_point (
                     key TEXT PRIMARY KEY,
                     filename TEXT NOT NULL,
                     at_unix INTEGER NOT NULL,
                     tz_offset_minutes INTEGER,
                     lat REAL,
                     lon REAL,
                     place TEXT,
                     device TEXT NOT NULL DEFAULT '',
                     day TEXT NOT NULL,
                     ingest_time TEXT NOT NULL
                 );
                 CREATE INDEX idx_photo_point_day ON photo_point(day);
                 CREATE TABLE photo_day (
                     day TEXT PRIMARY KEY,
                     count INTEGER NOT NULL,
                     keys_hash TEXT NOT NULL,
                     episode_id TEXT,
                     revision INTEGER NOT NULL DEFAULT 1,
                     minted_at TEXT NOT NULL
                 );
                 ALTER TABLE episodes ADD COLUMN superseded_by TEXT;
                 -- grants sync between a user's own peers (LWW): edits on
                 -- either node reach the other; keys stay node-local.
                 ALTER TABLE grants ADD COLUMN updated_at TEXT;
                 UPDATE grants SET updated_at = created_at WHERE updated_at IS NULL;
                 CREATE INDEX idx_episodes_superseded ON episodes(superseded_by)
                     WHERE superseded_by IS NOT NULL;",
            )?;
        }
        if version < 48 {
            // v48: ingest_log — append-only audit trail of accepted raw
            // device batches (/v1/ingest/health, /v1/ingest/photos). The
            // iOS companion's top-to-bottom verification surface: raw
            // samples land in health_sample/photo_point but don't become
            // episodes until the daily finalize, so without this there is
            // NO read path confirming the phone's stream actually lands.
            // Plain rowid PK (no AUTOINCREMENT → no sqlite_sequence to
            // classify); rows are never deleted, so seq-DESC paging is
            // stable.
            conn.execute_batch(
                "CREATE TABLE ingest_log (
                     seq INTEGER PRIMARY KEY,
                     ts TEXT NOT NULL,
                     actor TEXT NOT NULL,
                     source TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     accepted_count INTEGER NOT NULL,
                     ref TEXT
                 );",
            )?;
        }
        if version < 49 {
            // v49: vec_chunks — the sqlite-vec ANN index over chunk
            // embeddings (rowid = chunks.rowid, cosine so distance maps
            // 1:1 onto the similarity brute-force search reports).
            // Derived and rebuildable: dual-written by
            // set_chunk_embedding, backfilled by consolidation; the
            // brute-force scan over chunks.embedding stays as fallback
            // until coverage is complete (vectors.rs).
            conn.execute_batch(
                "CREATE VIRTUAL TABLE vec_chunks USING vec0(
                     embedding float[384] distance_metric=cosine
                 );",
            )?;
        }
        if version < 50 {
            // v50: llm_usage — the token-accounting ledger. One
            // append-only row per LLM API call (Ask rounds AND every
            // consolidation pass), written by the daemon's llm::call
            // wrapper. Tokens are the stored truth; dollar cost is
            // computed at read time from provider::pricing_for.
            // Node-tagged and peer-synced via the telemetry-leg
            // pattern so either dashboard shows both nodes' spend.
            conn.execute_batch(
                "CREATE TABLE llm_usage (
                     id TEXT PRIMARY KEY,
                     ts TEXT NOT NULL,
                     node TEXT NOT NULL,
                     purpose TEXT NOT NULL,
                     ref_id TEXT,
                     provider TEXT NOT NULL,
                     model TEXT NOT NULL,
                     tier TEXT NOT NULL,
                     input_tokens INTEGER NOT NULL DEFAULT 0,
                     output_tokens INTEGER NOT NULL DEFAULT 0,
                     cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
                     cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                     latency_ms INTEGER NOT NULL DEFAULT 0,
                     status TEXT NOT NULL DEFAULT 'ok'
                 );
                 CREATE INDEX idx_llm_usage_ts ON llm_usage(ts DESC);
                 CREATE INDEX idx_llm_usage_ref ON llm_usage(ref_id);",
            )?;
        }
        if version < 51 {
            // v51: the iOS device-feed wave.
            //
            // location_observation — THE unified location layer: every
            // geolocated signal from any source (photo geotags, CLVisit
            // dwells, calendar venues, scans, a Google-timeline import
            // later) lands here as one row keyed UNIQUE(source,
            // source_ref) — per-source idempotent, like health_sample.
            // The visits_finalize pass clusters ACROSS sources into
            // canonical `visit` episodes, so redundancy (photo + GPS +
            // calendar describing the same place/time) collapses at
            // cluster time, never by trusting client keys. Existing
            // photo points backfill in; photos_add_points dual-writes
            // going forward. The Atlas location_points index stays a
            // derived read-surface — primary observations live HERE.
            //
            // visit_day — the minted ledger (photo_day precedent): what
            // each day's visit episodes were clustered from (obs count +
            // keys hash), so the pass re-mints ONLY when a day's
            // observation set strictly grows (the superset guard).
            //
            // contact_card — the device address book, the identity
            // backbone: latest card per contact key, applied into Person
            // entities attribute-first (email/phone owns the Person).
            // Mutable directory rows (LWW by updated_at across peers);
            // card changes re-mint the contact episode under #r<n>.
            //
            // device_state — the generic attention/presence interval
            // stream (Jon 2026-08-16: harmonized with the hpresence
            // ember's model rather than an iOS-only focus table).
            // source='ios-focus' feeds it first; a future hpresence
            // release can post coalesced app/idle spans into the same
            // stream. Append-only, idempotent by (source, key).
            conn.execute_batch(
                "CREATE TABLE location_observation (
                     id TEXT PRIMARY KEY,
                     source TEXT NOT NULL,
                     source_ref TEXT NOT NULL,
                     lat REAL NOT NULL,
                     lon REAL NOT NULL,
                     accuracy_m REAL,
                     t_start TEXT NOT NULL,
                     t_end TEXT,
                     tz_offset_minutes INTEGER NOT NULL DEFAULT 0,
                     place TEXT,
                     day TEXT NOT NULL,
                     actor TEXT NOT NULL DEFAULT '',
                     ingest_time TEXT NOT NULL,
                     UNIQUE(source, source_ref)
                 );
                 CREATE INDEX idx_location_observation_day
                     ON location_observation(day);
                 CREATE INDEX idx_location_observation_t
                     ON location_observation(t_start);
                 CREATE TABLE visit_day (
                     day TEXT PRIMARY KEY,
                     obs_count INTEGER NOT NULL,
                     keys_hash TEXT NOT NULL,
                     revision INTEGER NOT NULL DEFAULT 1,
                     minted_at TEXT NOT NULL
                 );
                 CREATE TABLE contact_card (
                     key TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     emails TEXT NOT NULL DEFAULT '[]',
                     phones TEXT NOT NULL DEFAULT '[]',
                     org TEXT,
                     birthday TEXT,
                     postal TEXT NOT NULL DEFAULT '[]',
                     content_hash TEXT NOT NULL,
                     revision INTEGER NOT NULL DEFAULT 1,
                     applied INTEGER NOT NULL DEFAULT 0,
                     device TEXT NOT NULL DEFAULT '',
                     updated_at TEXT NOT NULL,
                     ingest_time TEXT NOT NULL
                 );
                 CREATE TABLE device_state (
                     source TEXT NOT NULL,
                     key TEXT NOT NULL,
                     state TEXT NOT NULL,
                     t_start TEXT NOT NULL,
                     t_end TEXT,
                     device TEXT NOT NULL DEFAULT '',
                     ingest_time TEXT NOT NULL,
                     PRIMARY KEY (source, key)
                 );
                 CREATE INDEX idx_device_state_t ON device_state(t_start);
                 -- Backfill: existing geotagged photo points become
                 -- observations (id = hash-free deterministic key form —
                 -- 'photos' + the photo key both nodes already share).
                 INSERT OR IGNORE INTO location_observation
                     (id, source, source_ref, lat, lon, accuracy_m,
                      t_start, t_end, tz_offset_minutes, place, day,
                      actor, ingest_time)
                 SELECT 'photos|' || key, 'photos', key, lat, lon, NULL,
                        strftime('%Y-%m-%dT%H:%M:%SZ', at_unix, 'unixepoch'),
                        NULL, COALESCE(tz_offset_minutes, 0), place, day,
                        'adapter:photos', ingest_time
                 FROM photo_point WHERE lat IS NOT NULL AND lon IS NOT NULL;",
            )?;
        }
        if version < 52 {
            // v52: SCOPED KEYS. `api_keys.grants` (JSON KeyGrants) turns a
            // key from blanket access into a default-deny route table
            // (store/api_keys.rs KeyGrants::permits); `key_requests` is
            // the ask-then-approve queue — an agent requests grants, a
            // human approves within the window, the minted secret waits
            // in `issued_secret` for ONE collection and is then erased.
            // Both are node-local like every key (secrets never sync) and
            // Excluded from the full export.
            conn.execute_batch(
                "ALTER TABLE api_keys ADD COLUMN grants TEXT;
                 CREATE TABLE key_requests (
                     id TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     grants TEXT NOT NULL,
                     ttl_secs INTEGER,
                     poll_hash TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     status TEXT NOT NULL DEFAULT 'pending',
                     issued_secret TEXT,
                     decided_at TEXT
                 );",
            )?;
        }
        if version < 53 {
            // v53: `llm_usage.items` — how many things a run processed.
            // Runs of on-machine models (the embedder) are ledger rows
            // too, but they have no tokens to count: one row per run,
            // `items` = chunks embedded, model `local:<name>`, priced $0.
            // Rides the existing /v1/sync/llm_usage leg as an additive
            // field (an older peer ignores it; a missing one reads 0).
            conn.execute_batch(
                "ALTER TABLE llm_usage ADD COLUMN items INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if version < 54 {
            // v54: FRIEND GROUPS (store/groups.rs). `friend_groups` — a
            // named set of this record's connections with its own sharing
            // descriptor; the roster is a JSON list so the row journals
            // and syncs whole (LWW by updated_at on the /v1/sync/sharing
            // leg; `deleted_at` is the tombstone). Item shares to a group
            // are `shared_items` rows whose friend is `group:<id>`.
            // `peer_groups` — what each friend's node says about the
            // groups THEY put this record in: a per-node cache, replaced
            // whole on fetch, never synced or exported.
            conn.execute_batch(
                "CREATE TABLE friend_groups (
                     id TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     members TEXT NOT NULL DEFAULT '[]',
                     sharing TEXT,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     deleted_at TEXT
                 );
                 CREATE TABLE peer_groups (
                     friend TEXT NOT NULL,
                     group_id TEXT NOT NULL,
                     name TEXT NOT NULL,
                     members TEXT NOT NULL DEFAULT '[]',
                     updated_at TEXT,
                     fetched_at TEXT NOT NULL,
                     PRIMARY KEY (friend, group_id)
                 );",
            )?;
        }
        if version < 55 {
            // v55: AGENTS (store/agents.rs; the design log 2026-10-17 — Ask
            // may now ACT through an agent harness, under governance).
            // `ask_log.tainted` — why a turn read content the user did not
            // write (the outbound guard carries it across a conversation).
            // `agents` — configured bridges to harnesses; node-local (their
            // addresses and secrets belong to one machine; the secret
            // itself is in .env.sealed, only its NAME is here).
            // `governance` — the autonomy policy, one row 'global' plus one
            // per agent; synced (one posture across a user's nodes).
            // `agent_tasks` / `agent_approvals` — the work Ask hands off and
            // every decision on it; synced so either node can approve.
            conn.execute_batch(
                "ALTER TABLE ask_log ADD COLUMN tainted TEXT;
                 CREATE TABLE agents (
                     id TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     harness TEXT NOT NULL,
                     config TEXT NOT NULL DEFAULT '{}',
                     secret_name TEXT,
                     key_id TEXT,
                     enabled INTEGER NOT NULL DEFAULT 1,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL
                 );
                 CREATE TABLE governance (
                     id TEXT PRIMARY KEY,
                     policy TEXT NOT NULL,
                     updated_at TEXT NOT NULL
                 );
                 CREATE TABLE agent_tasks (
                     id TEXT PRIMARY KEY,
                     ask_id TEXT,
                     agent_id TEXT,
                     request TEXT NOT NULL,
                     brief TEXT NOT NULL,
                     classes TEXT NOT NULL DEFAULT '[]',
                     tainted TEXT,
                     sensitive INTEGER NOT NULL DEFAULT 0,
                     status TEXT NOT NULL,
                     result TEXT,
                     error TEXT,
                     node TEXT,
                     actions INTEGER NOT NULL DEFAULT 0,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL,
                     started_at TEXT,
                     finished_at TEXT
                 );
                 CREATE INDEX idx_agent_tasks_status ON agent_tasks(status, updated_at);
                 CREATE TABLE agent_approvals (
                     id TEXT PRIMARY KEY,
                     task_id TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     class TEXT,
                     summary TEXT NOT NULL,
                     detail TEXT,
                     status TEXT NOT NULL DEFAULT 'pending',
                     decided_by TEXT,
                     decided_at TEXT,
                     created_at TEXT NOT NULL,
                     updated_at TEXT NOT NULL
                 );
                 CREATE INDEX idx_agent_approvals_task ON agent_approvals(task_id);
                 CREATE INDEX idx_agent_approvals_status ON agent_approvals(status);",
            )?;
        }
        if version < 56 {
            // v56: the AGENT DIRECTORY (multi-node M2). Every node lists
            // the agents it hosts here — name, harness, which node, on/off;
            // never configuration or secrets — and the list syncs, so any
            // node's Ask can hand work to an agent on another machine and
            // its card can say where that agent runs. A node only writes
            // rows for its OWN agents; `removed` is the tombstone (an agent
            // deleted or moved away).
            conn.execute_batch(
                "CREATE TABLE agent_directory (
                     agent_id TEXT PRIMARY KEY,
                     node_id TEXT NOT NULL,
                     node_name TEXT NOT NULL,
                     name TEXT NOT NULL,
                     harness TEXT NOT NULL,
                     enabled INTEGER NOT NULL DEFAULT 1,
                     removed INTEGER NOT NULL DEFAULT 0,
                     updated_at TEXT NOT NULL
                 );",
            )?;
        }
        if version < 57 {
            // v57: RELAY STAMPS (multi-node M4). Timestamp-cursor sync legs
            // exported by the SENDER's stamps, so a row relayed through a
            // hub kept an older stamp than a third node's cursor and was
            // never forwarded. Each such table gets `synced_at` — when THIS
            // node last wrote or received the row, millisecond UTC — kept
            // by triggers so local writes and peer applies both restamp it.
            // Exports page by it; last-writer-wins still uses the true
            // stamps. Backfilled from the old cursor column so the cursors
            // peers already hold stay valid.
            for (table, stamp) in RELAY_STAMPED {
                conn.execute_batch(&relay_stamp_sql(table, "synced_at", stamp, ""))?;
            }
            // episodes: only scope changes travel on the scope leg
            conn.execute_batch(&relay_stamp_sql(
                "episodes",
                "scope_synced_at",
                "scope_changed_at",
                "OF scope",
            ))?;
        }
        if version < 58 {
            // v58: NODES (multi-node M6/M7). Every node publishes one row
            // about itself — name, kind, roles, version, last seen — and the
            // list syncs, so the Peering page shows all of a person's
            // machines and the "thinking" lease can see who is alive.
            conn.execute_batch(
                "CREATE TABLE nodes (
                     node_id TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     roles TEXT NOT NULL DEFAULT '{}',
                     version TEXT,
                     last_seen TEXT NOT NULL,
                     updated_at TEXT NOT NULL
                 );",
            )?;
            conn.execute_batch(&relay_stamp_sql("nodes", "synced_at", "updated_at", ""))?;
        }
        if version < 59 {
            // v59: REVERSIBLE RETIREMENT (D.12). A retirement takes a memory
            // — or a whole source — out of the record without deleting it:
            // the episode is marked `superseded_by = 'retired:<id>'` (so
            // every reader that honours supersession honours retirement),
            // and what it fed is switched off with the prior state saved.
            //   retirements      — the event (synced, LWW by updated_at):
            //                      what was retired, which summaries to
            //                      rewrite, and when it was undone.
            //   retirement_rows  — NODE-LOCAL undo record: every row this
            //                      node touched and its prior state. Local
            //                      because entity and edge ids are minted
            //                      per node; each node applies the event
            //                      itself and records what IT changed.
            //   summary_inputs   — what each digest / weekly review /
            //                      chapter read, recorded from now on so a
            //                      later retirement knows exactly which
            //                      summaries it touches (synced, union).
            //   entities.hidden_by, sources.retired_by — the retirement
            //                      that hid an entity that existed only
            //                      because of the retired memory / that
            //                      retired a whole source.
            // idx_edge_provenance_episode: retiring walks provenance by
            // episode (the UNIQUE index leads with edge_id).
            conn.execute_batch(
                "CREATE TABLE retirements (
                     id TEXT PRIMARY KEY,
                     kind TEXT NOT NULL,
                     target TEXT NOT NULL,
                     label TEXT NOT NULL DEFAULT '',
                     private INTEGER NOT NULL DEFAULT 0,
                     regenerate TEXT NOT NULL DEFAULT '[]',
                     reason TEXT NOT NULL DEFAULT '',
                     actor TEXT NOT NULL,
                     created_at TEXT NOT NULL,
                     undone_at TEXT,
                     updated_at TEXT NOT NULL
                 );
                 CREATE TABLE retirement_rows (
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,
                     retirement_id TEXT NOT NULL,
                     tbl TEXT NOT NULL,
                     op TEXT NOT NULL,
                     row_key TEXT NOT NULL,
                     prior TEXT NOT NULL
                 );
                 CREATE INDEX idx_retirement_rows_rid ON retirement_rows(retirement_id, seq);
                 CREATE INDEX idx_retirement_rows_key ON retirement_rows(tbl, row_key);
                 CREATE TABLE summary_inputs (
                     summary_id TEXT NOT NULL,
                     episode_id TEXT NOT NULL,
                     ts TEXT NOT NULL,
                     UNIQUE(summary_id, episode_id)
                 );
                 CREATE INDEX idx_summary_inputs_episode ON summary_inputs(episode_id);
                 ALTER TABLE entities ADD COLUMN hidden_by TEXT;
                 ALTER TABLE sources ADD COLUMN retired_by TEXT;
                 CREATE INDEX idx_edge_provenance_episode ON edge_provenance(episode_id);",
            )?;
            conn.execute_batch(&relay_stamp_sql(
                "retirements",
                "synced_at",
                "updated_at",
                "",
            ))?;
            conn.execute_batch(&relay_stamp_sql("summary_inputs", "synced_at", "ts", ""))?;
        }
        if version < 60 {
            // v60: THE MOVEMENT LAYER (Tesla ember, design log 2026-10-23).
            //   movement_observation — every source's spans of travel, with
            //     `subject`: 'me' (the phone) or 'vehicle:<ember>:<id>' (a
            //     car's own drive record). Append-only, UNIQUE(source,
            //     source_ref); synced on the telemetry-leg pattern.
            //   vehicle_profile — per-car attribution settings (synced LWW).
            //   trip_attribution — the Tend "were you in the car?" answers
            //     (synced LWW).
            //   trip_day — the reconciliation ledger: input fingerprint +
            //     minted ids + a dirty flag (node-local — each node
            //     reconciles the synced inputs itself).
            conn.execute_batch(
                "CREATE TABLE movement_observation (
                     id TEXT PRIMARY KEY,
                     source TEXT NOT NULL,
                     source_ref TEXT NOT NULL,
                     subject TEXT NOT NULL,
                     mode TEXT NOT NULL,
                     t_start TEXT NOT NULL,
                     t_end TEXT NOT NULL,
                     tz_offset_minutes INTEGER NOT NULL DEFAULT 0,
                     start_lat REAL, start_lon REAL, end_lat REAL, end_lon REAL,
                     start_place TEXT, end_place TEXT,
                     distance_m REAL,
                     attrs TEXT NOT NULL DEFAULT '{}',
                     day TEXT NOT NULL,
                     actor TEXT NOT NULL DEFAULT '',
                     ingest_time TEXT NOT NULL,
                     UNIQUE(source, source_ref)
                 );
                 CREATE INDEX idx_movement_observation_day ON movement_observation(day);
                 CREATE INDEX idx_movement_observation_subject ON movement_observation(subject, t_start);
                 CREATE TABLE vehicle_profile (
                     vehicle TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     descriptor TEXT NOT NULL DEFAULT '{}',
                     attribution TEXT NOT NULL DEFAULT 'phone',
                     aboard_driver TEXT NOT NULL DEFAULT 'me',
                     household TEXT NOT NULL DEFAULT '[]',
                     others_scope TEXT NOT NULL DEFAULT 'family',
                     ask INTEGER NOT NULL DEFAULT 1,
                     updated_at TEXT NOT NULL
                 );
                 CREATE TABLE trip_attribution (
                     key TEXT PRIMARY KEY,
                     vehicle TEXT NOT NULL,
                     day TEXT NOT NULL,
                     summary TEXT NOT NULL DEFAULT '',
                     status TEXT NOT NULL DEFAULT 'pending',
                     answer TEXT,
                     asked_at TEXT,
                     answered_at TEXT,
                     updated_at TEXT NOT NULL
                 );
                 CREATE INDEX idx_trip_attribution_status ON trip_attribution(status);
                 CREATE TABLE trip_day (
                     day TEXT PRIMARY KEY,
                     obs_count INTEGER NOT NULL,
                     fingerprint TEXT NOT NULL,
                     minted TEXT NOT NULL DEFAULT '[]',
                     dirty INTEGER NOT NULL DEFAULT 0,
                     minted_at TEXT NOT NULL
                 );",
            )?;
            conn.execute_batch(&relay_stamp_sql(
                "movement_observation",
                "synced_at",
                "ingest_time",
                "",
            ))?;
            conn.execute_batch(&relay_stamp_sql(
                "vehicle_profile",
                "synced_at",
                "updated_at",
                "",
            ))?;
            conn.execute_batch(&relay_stamp_sql(
                "trip_attribution",
                "synced_at",
                "updated_at",
                "",
            ))?;
        }
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Store {
            conn: FairMutex::new(conn),
            vault_unlocks: Mutex::new(std::collections::HashMap::new()),
            vec_ready: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /* ————— the vault ————— */

    /// Unlock private material for `minutes` (auto-relocks by deadline),
    /// for the CURRENT viewer only. With no viewer set (tests, tools) the
    /// viewer-less view unlocks; production reads never run as it.
    pub fn vault_unlock(&self, minutes: u64) {
        let now = std::time::Instant::now();
        let mut unlocks = self.vault_unlocks.lock().unwrap();
        unlocks.retain(|_, until| *until > now);
        unlocks.insert(
            current_viewer(),
            now + std::time::Duration::from_secs(minutes * 60),
        );
    }

    /// Lock the vault for the current viewer.
    pub fn vault_lock(&self) {
        self.vault_unlocks.lock().unwrap().remove(&current_viewer());
    }

    /// End a viewer's unlock from outside its requests (sign-out).
    pub fn vault_forget(&self, viewer: &str) {
        self.vault_unlocks.lock().unwrap().remove(viewer);
    }

    /// Consistent point-in-time copy of the live database (the export's
    /// restore artifact) — safe under WAL while the daemon keeps running.
    pub fn vacuum_into(&self, dest: &Path) -> Result<()> {
        let conn = self.lock();
        conn.execute("VACUUM INTO ?1", [dest.to_string_lossy().as_ref()])?;
        Ok(())
    }

    /// (locked, seconds_remaining) for the CURRENT viewer. Locked mode is
    /// the default state.
    pub fn vault_status(&self) -> (bool, u64) {
        let until = self
            .vault_unlocks
            .lock()
            .unwrap()
            .get(&current_viewer())
            .copied();
        match until {
            Some(deadline) => {
                let now = std::time::Instant::now();
                if deadline > now {
                    (false, (deadline - now).as_secs())
                } else {
                    (true, 0)
                }
            }
            None => (true, 0),
        }
    }

    /// May reads currently see `scope = 'private'` rows?
    pub(crate) fn private_visible(&self) -> bool {
        !self.vault_status().0
    }

    /// SQL tail for a relabel: a viewer whose vault is locked can't see a
    /// private row, so it can't take one out of the vault either — the
    /// row keeps its label (moving rows INTO private is always allowed).
    pub(crate) fn relabel_keeps_private(&self) -> &'static str {
        if self.private_visible() {
            ""
        } else {
            " AND scope != 'private'"
        }
    }

    /// SQL fragment excluding private rows while locked. `column` is the
    /// fully qualified scope column (e.g. "e.scope"); composes after an
    /// existing WHERE as an AND-clause. Under an area filter (an
    /// area-limited scoped key's request — `enter_area_filter`) it also
    /// narrows to the allowed life areas.
    pub(crate) fn scope_guard(&self, column: &str) -> String {
        let mut clause = if self.private_visible() {
            String::new()
        } else {
            format!(" AND {column} != 'private'")
        };
        if let Some(areas) = area_filter() {
            if areas.is_empty() {
                clause.push_str(" AND 0");
            } else {
                let list = areas
                    .iter()
                    .map(|a| format!("'{a}'"))
                    .collect::<Vec<_>>()
                    .join(",");
                clause.push_str(&format!(" AND {column} IN ({list})"));
            }
        }
        clause
    }

    /// Is a row with this scope label hidden from the current read? The
    /// row-at-a-time twin of `scope_guard`, for checks made in Rust.
    pub(crate) fn scope_hidden(&self, scope: &str) -> bool {
        self.scope_hider()(scope)
    }

    /// `scope_hidden` resolved once, for loops over many rows.
    pub(crate) fn scope_hider(&self) -> impl Fn(&str) -> bool {
        let hide_private = !self.private_visible();
        let areas = area_filter();
        move |scope: &str| {
            (hide_private && scope == "private")
                || areas
                    .as_ref()
                    .is_some_and(|allowed| !allowed.contains(&scope))
        }
    }

    /// Is an area filter narrowing reads on this thread?
    pub(crate) fn area_limited(&self) -> bool {
        area_filter().is_some()
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::open(Path::new(":memory:"))
    }

    pub(crate) fn lock(&self) -> parking_lot::FairMutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// Change feed: mutations after `cursor`, oldest first.
    pub fn mutations_since(&self, cursor: i64, limit: usize) -> Result<Vec<MutationRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, actor, session_id, kind, payload
             FROM mutations WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![cursor, limit as i64], |row| {
            Ok(MutationRow {
                seq: row.get(0)?,
                ts: row.get(1)?,
                actor: row.get(2)?,
                session_id: row.get(3)?,
                kind: row.get(4)?,
                payload: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or(JsonValue::Null),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Execute a read-only SQL query (the RLM engine's `sql` handle).
    /// The prepared statement must be verifiably read-only; rows are
    /// capped. Returns (column names, rows as JSON values).
    pub fn read_query(
        &self,
        sql: &str,
        max_rows: usize,
    ) -> Result<(Vec<String>, Vec<Vec<JsonValue>>)> {
        self.read_query_guarded(sql, max_rows, false)
    }

    /// `read_query` with the private exclusion applied ALWAYS — vault
    /// open or not. For reads whose rows fan out to a model in bulk (the
    /// Ask engine's `map` handle): the cloud-pass rule, not the vault
    /// state, decides what leaves the machine there.
    pub fn read_query_public(
        &self,
        sql: &str,
        max_rows: usize,
    ) -> Result<(Vec<String>, Vec<Vec<JsonValue>>)> {
        self.read_query_guarded(sql, max_rows, true)
    }

    fn read_query_guarded(
        &self,
        sql: &str,
        max_rows: usize,
        always_exclude_private: bool,
    ) -> Result<(Vec<String>, Vec<Vec<JsonValue>>)> {
        let trimmed = sql.trim().trim_end_matches(';');
        if trimmed.contains(';') {
            return Err(anyhow::anyhow!("one statement at a time"));
        }
        // Vault enforcement for raw SQL: while locked, TEMP VIEWS shadow the
        // table names (SQLite resolves the temp schema first), filtering
        // private rows; `main.`-qualified names would bypass the shadowing,
        // so they are rejected outright. Views are dropped before the lock
        // guard releases — writers on this connection never see them.
        if trimmed.to_ascii_lowercase().contains("main.") {
            return Err(anyhow::anyhow!(
                "schema-qualified table names are not allowed"
            ));
        }
        // The shadow views know the vault, not life areas — raw SQL is
        // never served under an area filter.
        if area_filter().is_some() {
            return Err(anyhow::anyhow!(
                "raw SQL is not available to an area-limited key"
            ));
        }
        let conn = self.lock();
        let shadowed = always_exclude_private || !self.private_visible();
        conn.execute_batch(if shadowed {
            VAULT_SHADOW_VIEWS
        } else {
            LIVE_SHADOW_VIEWS
        })?;
        let result = Self::run_read_query(&conn, trimmed, max_rows);
        conn.execute_batch(VAULT_SHADOW_DROP)?;
        result
    }

    fn run_read_query(
        conn: &Connection,
        trimmed: &str,
        max_rows: usize,
    ) -> Result<(Vec<String>, Vec<Vec<JsonValue>>)> {
        let mut stmt = conn.prepare(trimmed)?;
        if !stmt.readonly() {
            return Err(anyhow::anyhow!("only read-only queries are allowed"));
        }
        let columns: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
        let column_count = columns.len();
        let mut rows_out = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            if rows_out.len() >= max_rows {
                break;
            }
            let mut out = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let value = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => JsonValue::Null,
                    rusqlite::types::ValueRef::Integer(v) => serde_json::json!(v),
                    rusqlite::types::ValueRef::Real(v) => serde_json::json!(v),
                    rusqlite::types::ValueRef::Text(t) => {
                        serde_json::json!(String::from_utf8_lossy(t))
                    }
                    rusqlite::types::ValueRef::Blob(b) => {
                        serde_json::json!(format!("<blob {} bytes>", b.len()))
                    }
                };
                out.push(value);
            }
            rows_out.push(out);
        }
        Ok((columns, rows_out))
    }

    /// Timestamp of the newest mutation of a kind (refresh gating etc.).
    pub fn latest_mutation_ts(&self, kind: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT ts FROM mutations WHERE kind = ?1 ORDER BY seq DESC LIMIT 1",
            [kind],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Append an arbitrary audit event to the mutations log (used for
    /// user-invoked actions like deep queries).
    pub fn record_event(&self, actor: &Actor, kind: &str, payload: &JsonValue) -> Result<()> {
        let conn = self.lock();
        record_mutation(
            &conn,
            actor,
            kind,
            &format!("{kind}:{}", now_utc()),
            payload,
        )?;
        Ok(())
    }

    /// Latest `<kind>_ingest_done` event per kind — the Integrations
    /// cards' "last import" line. Returns (kind, ts, payload).
    pub fn latest_ingest_events(&self) -> Result<Vec<(String, String, JsonValue)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT kind, ts, payload, MAX(seq) FROM mutations
             WHERE kind LIKE '%_ingest_done' GROUP BY kind",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|r| {
            r.map(|(kind, ts, payload)| {
                let payload = serde_json::from_str(&payload).unwrap_or(JsonValue::Null);
                (kind, ts, payload)
            })
            .map_err(Into::into)
        })
        .collect()
    }

    /// Stamp a completed local-source refresh in the mutations log.
    pub fn record_refresh(&self, actor: &Actor, ts: &str, summary: &JsonValue) -> Result<()> {
        let conn = self.lock();
        record_mutation(
            &conn,
            actor,
            "sources_refreshed",
            &format!("sources_refreshed:{ts}"),
            summary,
        )?;
        Ok(())
    }

    /// Latest mutation cursor (0 if empty).
    pub fn latest_cursor(&self) -> Result<i64> {
        let conn = self.lock();
        let seq: i64 =
            conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM mutations", [], |row| {
                row.get(0)
            })?;
        Ok(seq)
    }

    /// Row counts for status displays and the brain index.
    pub fn counts(&self) -> Result<StoreCounts> {
        let conn = self.lock();
        let count = |table: &str| -> rusqlite::Result<i64> {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
        };
        // Locked counts exclude private rows entirely — no "N hidden".
        let guard = self.scope_guard("scope");
        Ok(StoreCounts {
            // Index-only on both sides: the bare count walks the smallest
            // index and the private count is exactly the partial index
            // `idx_episodes_private_id` — `WHERE 1=1 AND scope != 'private'`
            // instead reads every row of a multi-GB table.
            episodes: if guard.is_empty() {
                count("episodes")?
            } else {
                count("episodes")? - count("episodes WHERE scope = 'private'")?
            },
            entities: count(&format!("entities WHERE merged_into IS NULL{guard}"))?,
            edges: count(&format!("edges WHERE t_expired IS NULL{guard}"))?,
            mutations: count("mutations")?,
            quarantine_pending: count("quarantine WHERE status = 'pending'")?,
            review_pending: count("merge_review_queue WHERE resolution IS NULL")?,
            // Proposed self-model rows + fresh opportunities await the
            // same Tend gate; private spheres/evidence stay uncounted
            // while locked (no "N hidden").
            propositions_pending: count(&format!(
                "self_model WHERE status = 'proposed'{}",
                self.scope_guard("sphere")
            ))?,
            opportunities_pending: count(&format!(
                "opportunities o JOIN episodes e ON e.id = o.episode_id
                     JOIN self_model s ON s.id = o.seeking_id
                 WHERE o.status = 'pending' AND e.superseded_by IS NULL{}{}",
                self.scope_guard("e.scope"),
                self.scope_guard("s.sphere")
            ))?,
            // Lint inputs exclude private at the source (cloud-pass
            // rule), so the count needs no guard.
            lint_pending: count("lint_findings WHERE resolution IS NULL")?,
            // v60: "were you in the car?" yes/no questions
            trips_pending: count("trip_attribution WHERE status = 'pending'")?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreCounts {
    pub episodes: i64,
    pub entities: i64,
    pub edges: i64,
    pub mutations: i64,
    pub quarantine_pending: i64,
    pub review_pending: i64,
    pub propositions_pending: i64,
    pub opportunities_pending: i64,
    pub lint_pending: i64,
    #[serde(default)]
    pub trips_pending: i64,
}

/// Append a mutation inside an existing connection/transaction. Returns
/// false if the idempotency key was already present (retried write).
pub(crate) fn record_mutation(
    conn: &Connection,
    actor: &Actor,
    kind: &str,
    idempotency_key: &str,
    payload: &JsonValue,
) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO mutations (ts, actor, session_id, idempotency_key, kind, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            now_utc(),
            actor.actor,
            actor.session_id,
            idempotency_key,
            kind,
            payload.to_string()
        ],
    )?;
    Ok(changed > 0)
}

/// Read one row as a JSON object of all its columns — the post-image the
/// operational journal stores. None if the row is absent.
pub(crate) fn row_as_json(conn: &Connection, table: &str, id: &str) -> Result<Option<JsonValue>> {
    let mut stmt = conn.prepare(&format!("SELECT * FROM {table} WHERE id = ?1"))?;
    let cols: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let mut rows = stmt.query([id])?;
    match rows.next()? {
        Some(row) => {
            let mut obj = serde_json::Map::new();
            for (i, c) in cols.iter().enumerate() {
                let v: rusqlite::types::Value = row.get(i)?;
                obj.insert(
                    c.clone(),
                    match v {
                        rusqlite::types::Value::Null => JsonValue::Null,
                        rusqlite::types::Value::Integer(n) => JsonValue::from(n),
                        rusqlite::types::Value::Real(f) => JsonValue::from(f),
                        rusqlite::types::Value::Text(t) => JsonValue::from(t),
                        rusqlite::types::Value::Blob(b) => {
                            JsonValue::from(String::from_utf8_lossy(&b).into_owned())
                        }
                    },
                );
            }
            Ok(Some(JsonValue::Object(obj)))
        }
        None => Ok(None),
    }
}

/// Convert a JSON value from a journal post-image back to a SQLite value.
pub(crate) fn json_to_sql(v: &JsonValue) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        JsonValue::Null => Value::Null,
        JsonValue::Number(n) if n.is_i64() => Value::Integer(n.as_i64().unwrap()),
        JsonValue::Number(n) => Value::Real(n.as_f64().unwrap_or(0.0)),
        JsonValue::Bool(b) => Value::Integer(*b as i64),
        JsonValue::String(t) => Value::Text(t.clone()),
        other => Value::Text(other.to_string()),
    }
}

/// Append an operational-journal entry (append-only, content-bearing). The
/// entry_id folds a hash of the post-image so identical retries dedup while
/// distinct edits in the same instant stay separate. See `journal_row`.
/// The tables whose sync legs page by a relay stamp (v57), with the
/// column their old cursor used (for the backfill).
pub(crate) const RELAY_STAMPED: &[(&str, &str)] = &[
    ("contact_card", "updated_at"),
    ("device_state", "ingest_time"),
    ("location_observation", "ingest_time"),
    ("llm_usage", "ts"),
    (
        "notifications",
        "MAX(ts, COALESCE(read_at, ''), COALESCE(dismissed_at, ''))",
    ),
    ("plugin_kv", "updated_at"),
    ("syslog", "ts"),
    ("health_points", "ts"),
    ("extraction_verdicts", "ts"),
    ("grants", "COALESCE(updated_at, created_at)"),
    ("shared_items", "created_at"),
    ("governance", "updated_at"),
    ("agent_tasks", "updated_at"),
    ("agent_approvals", "updated_at"),
    ("agent_directory", "updated_at"),
];

/// Millisecond UTC — the relay stamp's resolution (seconds lost rows
/// written in the same second as a page was cut).
pub(crate) const RELAY_NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// DDL for one relay-stamped table: the column, its backfill (seconds
/// stamps padded to .000Z so they order correctly against new ones), an
/// index, and the two triggers. `on_update` narrows the update trigger
/// (e.g. "OF scope").
pub(crate) fn relay_stamp_sql(
    table: &str,
    column: &str,
    backfill: &str,
    on_update: &str,
) -> String {
    format!(
        "ALTER TABLE {table} ADD COLUMN {column} TEXT;
         UPDATE {table} SET {column} = CASE
             WHEN length(b) = 20 AND substr(b, 20, 1) = 'Z' THEN substr(b, 1, 19) || '.000Z'
             ELSE b END
           FROM (SELECT rowid AS r, {backfill} AS b FROM {table}) AS src
           WHERE {table}.rowid = src.r AND src.b IS NOT NULL;
         CREATE INDEX idx_{table}_{column} ON {table}({column});
         CREATE TRIGGER {table}_{column}_ins AFTER INSERT ON {table} BEGIN
             UPDATE {table} SET {column} = {RELAY_NOW} WHERE rowid = NEW.rowid;
         END;
         CREATE TRIGGER {table}_{column}_upd AFTER UPDATE {on_update} ON {table}
             WHEN NEW.{column} IS OLD.{column} BEGIN
             UPDATE {table} SET {column} = {RELAY_NOW} WHERE rowid = NEW.rowid;
         END;"
    )
}

/// The cursor after one page of a relay-stamped export: the largest
/// stamp among the first `limit` rows past `since`, with the page's own
/// filter (`from_where` must use ?1 for `since`) and order.
pub(crate) fn page_cursor(
    conn: &Connection,
    column: &str,
    from_where: &str,
    since: &str,
    limit: usize,
) -> Result<String> {
    let max: Option<String> = conn.query_row(
        &format!(
            "SELECT MAX(s) FROM (SELECT {column} AS s {from_where} ORDER BY {column} LIMIT ?2)"
        ),
        rusqlite::params![since, limit as i64],
        |row| row.get(0),
    )?;
    Ok(max.unwrap_or_else(|| since.to_string()))
}

pub(crate) fn journal_op(
    conn: &Connection,
    actor: &Actor,
    entity_kind: &str,
    entity_id: &str,
    op: &str,
    post_image: Option<&JsonValue>,
) -> Result<()> {
    use std::hash::{Hash, Hasher};
    let ts = now_utc();
    let body = post_image.map(|p| p.to_string()).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    let entry_id = format!("{entity_kind}:{entity_id}:{ts}:{op}:{:x}", hasher.finish());
    conn.execute(
        "INSERT OR IGNORE INTO op_journal
             (entry_id, entity_kind, entity_id, op, post_image, actor, ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            entry_id,
            entity_kind,
            entity_id,
            op,
            post_image.map(|p| p.to_string()),
            actor.actor,
            ts
        ],
    )?;
    Ok(())
}

/// Journal the CURRENT state of `table`/`id` (read now, stored as the
/// post-image). Call BEFORE a hard delete so the tombstone still carries the
/// last content. `op` is create | update | delete.
pub(crate) fn journal_row(
    conn: &Connection,
    actor: &Actor,
    entity_kind: &str,
    table: &str,
    id: &str,
    op: &str,
) -> Result<()> {
    let post = row_as_json(conn, table, id)?;
    journal_op(conn, actor, entity_kind, id, op, post.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_vault_unlock_belongs_to_its_viewer() {
        let store = Store::open_in_memory().unwrap();
        {
            let _a = enter_viewer("session:a");
            store.vault_unlock(15);
            assert!(
                !store.vault_status().0,
                "the viewer who unlocked sees private"
            );
            assert_eq!(store.scope_guard("e.scope"), "");
        }
        {
            let _b = enter_viewer("key:b");
            assert!(store.vault_status().0, "another viewer stays locked");
            assert_eq!(store.scope_guard("e.scope"), " AND e.scope != 'private'");
        }
        assert!(store.vault_status().0, "a read with no viewer stays locked");

        // A captured scope carries the viewer (and its areas) to another thread.
        let scope = {
            let _a = enter_viewer("session:a");
            capture_read_scope()
        };
        std::thread::scope(|s| {
            s.spawn(|| {
                let _g = scope.enter();
                assert!(!store.vault_status().0);
            });
            s.spawn(|| assert!(store.vault_status().0, "an uncarried thread reads locked"));
        });
        let areas = ReadScope::new("key:c", Some(&["work".to_string(), "bogus".to_string()]));
        {
            let _g = areas.enter();
            assert_eq!(
                store.scope_guard("e.scope"),
                " AND e.scope != 'private' AND e.scope IN ('work')"
            );
        }
        assert_eq!(
            store.scope_guard("e.scope"),
            " AND e.scope != 'private'",
            "the guard restores"
        );

        // Locking, or the session ending, closes only that viewer's view.
        store.vault_forget("session:a");
        let _a = enter_viewer("session:a");
        assert!(store.vault_status().0);
    }

    #[test]
    fn test_open_and_migrate() {
        let store = Store::open_in_memory().unwrap();
        let counts = store.counts().unwrap();
        assert_eq!(counts.episodes, 0);
        assert_eq!(counts.entities, 0);
    }

    #[test]
    fn test_migration_idempotent_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.db");
        drop(Store::open(&path).unwrap());
        // Re-open: user_version is set, schema not re-created.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.counts().unwrap().episodes, 0);
    }

    #[test]
    fn test_read_query_guard() {
        let store = Store::open_in_memory().unwrap();
        // Reads work and are capped.
        let (columns, rows) = store
            .read_query("SELECT 1 AS one UNION SELECT 2 UNION SELECT 3", 2)
            .unwrap();
        assert_eq!(columns, vec!["one"]);
        assert_eq!(rows.len(), 2);
        // Writes are rejected before execution.
        assert!(store
            .read_query("UPDATE episodes SET scope='x'", 10)
            .is_err());
        assert!(store
            .read_query("SELECT 1; DROP TABLE episodes", 10)
            .is_err());
        assert_eq!(store.counts().unwrap().episodes, 0);
    }

    #[test]
    fn test_mutation_log_and_feed() {
        let store = Store::open_in_memory().unwrap();
        let actor = Actor::new("human:test");
        {
            let conn = store.lock();
            assert!(record_mutation(&conn, &actor, "test", "key1", &json!({"a": 1})).unwrap());
            // Idempotent retry is a no-op.
            assert!(!record_mutation(&conn, &actor, "test", "key1", &json!({"a": 1})).unwrap());
            assert!(record_mutation(&conn, &actor, "test", "key2", &json!({"b": 2})).unwrap());
        }
        let feed = store.mutations_since(0, 10).unwrap();
        assert_eq!(feed.len(), 2);
        assert_eq!(feed[0].kind, "test");
        assert_eq!(feed[0].payload, json!({"a": 1}));

        let feed = store.mutations_since(feed[0].seq, 10).unwrap();
        assert_eq!(feed.len(), 1);
        // Seq numbers may have gaps (ignored idempotent retries still burn
        // AUTOINCREMENT ids); the cursor contract is only monotonicity.
        assert_eq!(store.latest_cursor().unwrap(), feed[0].seq);
    }

    /// Multi-node M4: a row written on A, relayed through a hub, must
    /// reach C even though C's cursor on the hub is already past the
    /// row's own (older) timestamp — the case sender-stamp cursors lost.
    #[test]
    fn test_relay_through_a_hub_reaches_a_third_node() {
        use crate::provider::Usage;
        let (a, hub, c) = (
            Store::open_in_memory().unwrap(),
            Store::open_in_memory().unwrap(),
            Store::open_in_memory().unwrap(),
        );
        let me = Actor::new("human:test");
        // A writes first (older stamps)…
        a.record_llm_usage(
            "local",
            "ask",
            None,
            "anthropic",
            "claude-opus-4-8",
            "high",
            &Usage {
                input_tokens: 7,
                ..Default::default()
            },
            1,
            "ok",
        )
        .unwrap();
        a.notify(&me, "test:a", "info", "from A", "", None, Some("a1"))
            .unwrap();
        let task = a
            .create_agent_task(
                &me,
                None,
                None,
                "from A",
                "brief",
                &["read".into()],
                None,
                false,
            )
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // …then the hub has something newer of its own, and C syncs it,
        // so C's cursors sit past A's stamps.
        hub.notify(&me, "test:hub", "info", "from hub", "", None, Some("h1"))
            .unwrap();
        hub.record_llm_usage(
            "cloud",
            "ask",
            None,
            "anthropic",
            "claude-opus-4-8",
            "high",
            &Usage::default(),
            1,
            "ok",
        )
        .unwrap();
        let c_notif = {
            let p = hub.notifications_since("", 500).unwrap();
            c.apply_notifications(&p).unwrap();
            p["next"].as_str().unwrap().to_string()
        };
        let c_usage = {
            let p = hub.llm_usage_since("", 500).unwrap();
            c.apply_llm_usage(&p).unwrap();
            p["next"].as_str().unwrap().to_string()
        };
        let c_agents = {
            let p = hub.agents_sync_export("").unwrap();
            c.agents_sync_apply(&p).unwrap();
            p["next"].as_str().unwrap().to_string()
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        // The hub now receives A's older rows.
        hub.apply_notifications(&a.notifications_since("", 500).unwrap())
            .unwrap();
        hub.apply_llm_usage(&a.llm_usage_since("", 500).unwrap())
            .unwrap();
        hub.agents_sync_apply(&a.agents_sync_export("").unwrap())
            .unwrap();
        // C pulls from the hub with its existing cursors and gets them.
        c.apply_notifications(&hub.notifications_since(&c_notif, 500).unwrap())
            .unwrap();
        c.apply_llm_usage(&hub.llm_usage_since(&c_usage, 500).unwrap())
            .unwrap();
        c.agents_sync_apply(&hub.agents_sync_export(&c_agents).unwrap())
            .unwrap();
        let titles: Vec<String> = c
            .notifications(true, 50)
            .unwrap()
            .into_iter()
            .map(|n| n.title)
            .collect();
        assert!(titles.contains(&"from A".to_string()), "{titles:?}");
        assert_eq!(
            c.llm_usage_since("", 500).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(
            c.agent_task(&task.id).unwrap().is_some(),
            "an agent task relays too"
        );
        // and a page from an idle exporter doesn't move the cursor backwards
        let idle = hub.notifications_since("9999", 500).unwrap();
        assert_eq!(idle["next"], "9999");
    }

    /// Multi-node M5: a hub relays journal entries from one spoke to
    /// another, and never echoes a spoke's own entries back to it.
    #[test]
    fn test_hub_relays_journal_entries_between_spokes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::paths::Home::at(tmp.path());
        home.init().unwrap();
        let a = Store::open(&home.db_path()).unwrap();
        let hub = Store::open_in_memory().unwrap();
        let id = a
            .add_task(
                &Actor::new("human:a"),
                "plain",
                "from the laptop",
                None,
                None,
                "one-time",
                "personal",
                None,
                None,
                &[],
            )
            .unwrap();
        let pushed = a.journal_export(0, 500).unwrap();
        hub.apply_journal(&Actor::new("peer:laptop"), &pushed)
            .unwrap();
        // the Mac mini (key "mini") gets the laptop's entry through the hub…
        let for_mini = hub.journal_export_for(0, 500, Some("mini")).unwrap();
        assert!(for_mini["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["entity_id"] == serde_json::json!(id)));
        // …and the laptop doesn't get its own entry echoed back
        let for_laptop = hub.journal_export_for(0, 500, Some("laptop")).unwrap();
        assert!(for_laptop["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["entity_id"] != serde_json::json!(id)));
        // a spoke pushing to its hub still sends only its own entries
        assert!(hub.journal_export(0, 500).unwrap()["entries"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}
