//! Identifier and hashing helpers.
//!
//! Episode ids are deterministic (derived from source + source_ref) so
//! re-ingesting the same material always addresses the same row. Entity,
//! edge, and merge ids are random: their identity lives in the graph, not
//! in their source material.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Hex SHA-256 of arbitrary content.
pub fn content_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Deterministic episode id from the source-native identity.
pub fn episode_id(source: &str, source_ref: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(source_ref.as_bytes());
    format!("ep_{}", &format!("{:x}", hasher.finalize())[..16])
}

pub fn entity_id() -> String {
    format!("en_{}", Uuid::new_v4().simple())
}

pub fn edge_id() -> String {
    format!("ed_{}", Uuid::new_v4().simple())
}

pub fn merge_id() -> String {
    format!("mg_{}", Uuid::new_v4().simple())
}

pub fn review_id() -> String {
    format!("rv_{}", Uuid::new_v4().simple())
}

pub fn quarantine_id() -> String {
    format!("qr_{}", Uuid::new_v4().simple())
}

pub fn intent_id() -> String {
    format!("in_{}", Uuid::new_v4().simple())
}

pub fn privacy_rule_id() -> String {
    format!("pv_{}", Uuid::new_v4().simple())
}

pub fn proposition_id() -> String {
    format!("sm_{}", Uuid::new_v4().simple())
}

pub fn standing_query_id() -> String {
    format!("sq_{}", Uuid::new_v4().simple())
}

pub fn opportunity_id() -> String {
    format!("op_{}", Uuid::new_v4().simple())
}

pub fn task_id() -> String {
    format!("tk_{}", Uuid::new_v4().simple())
}

pub fn template_id() -> String {
    format!("tt_{}", Uuid::new_v4().simple())
}

pub fn note_id() -> String {
    format!("nt_{}", Uuid::new_v4().simple())
}

pub fn relationship_id() -> String {
    format!("rl_{}", Uuid::new_v4().simple())
}

pub fn api_key_id() -> String {
    format!("ak_{}", Uuid::new_v4().simple())
}

pub fn rename_id() -> String {
    format!("rn_{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_episode_id_deterministic() {
        assert_eq!(episode_id("git", "repo:abc"), episode_id("git", "repo:abc"));
        assert_ne!(episode_id("git", "repo:abc"), episode_id("git", "repo:def"));
        assert_ne!(episode_id("git", "a:b"), episode_id("git:a", "b"));
    }

    #[test]
    fn test_entity_ids_unique() {
        assert_ne!(entity_id(), entity_id());
    }
}
