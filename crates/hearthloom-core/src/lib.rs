//! Hearthloom core: the substrate.
//!
//! An episode-sourced, bi-temporal knowledge graph in a single SQLite file,
//! with raw content kept as plain files on disk. Every index is rebuildable
//! from the episode log. See architecture.md for the architecture.
//!
//! Design laws enforced here:
//! - Episodes are append-only ground truth; the graph is derived.
//! - Every write carries actor identity and lands in the mutations log.
//! - Edges are bi-temporal (t_valid/t_invalid/t_created/t_expired);
//!   contradicted edges are invalidated, never deleted.
//! - Merges are reversible events; bias to under-merge.

pub mod brain_index;
pub mod config;
pub mod embeddings;
pub mod ids;
pub mod ingest;
pub mod machine;
pub mod paths;
pub mod provider;
pub mod sealbox;
pub mod secrets;
pub mod store;
pub mod time;

pub use ingest::{one_line, EdgeAssertion, EntityRef, EpisodeDraft, IngestBatch, IngestReport};
pub use paths::Home;
pub use store::{Actor, Store};
pub use time::{EventTime, TimeUncertainty};
