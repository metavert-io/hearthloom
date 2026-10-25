//! The self-model: who the user is, one proposition per row — identity,
//! offerings, seeking, strengths, constraints, grouped by life sphere
//! (the scope vocabulary). Grown from episode EVIDENCE at idle
//! (origin=inferred, lands as `proposed` for a Tend yes/no) or stated
//! directly by the human (origin=declared, confirmed immediately).
//! Confirmation never launders origin: "you confirmed the inference"
//! stays distinguishable from "you said it yourself".
//!
//! Confirmed `seeking` rows are the substrate for opportunity detection;
//! confirmed rows of every kind render into BRAIN.md (private sphere
//! excluded ALWAYS there, vault or no vault — the persisted-artifact rule).

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{record_mutation, Actor, Store};
use crate::embeddings::{bytes_to_floats, floats_to_bytes};
use crate::ids;
use crate::time::now_utc;

/// (proposition id, text, embedding) of a confirmed seeking row.
pub type SeekingRow = (String, String, Option<Vec<f32>>);
/// Grower evidence set: (episode id, title/body-excerpt) pairs.
pub type GrowerInputs = (Vec<(String, String)>, Vec<(String, String)>);

pub const SPHERES: &[&str] = &["work", "personal", "family", "health", "private"];
pub const PROPOSITION_KINDS: &[&str] =
    &["identity", "offering", "seeking", "strength", "constraint"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposition {
    pub id: String,
    pub sphere: String,
    pub kind: String,
    pub text: String,
    pub origin: String,
    pub status: String,
    pub evidence: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub confirmed_at: Option<String>,
}

fn proposition_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Proposition> {
    let evidence: Option<String> = row.get("evidence")?;
    Ok(Proposition {
        id: row.get("id")?,
        sphere: row.get("sphere")?,
        kind: row.get("kind")?,
        text: row.get("text")?,
        origin: row.get("origin")?,
        status: row.get("status")?,
        evidence: evidence
            .and_then(|e| serde_json::from_str(&e).ok())
            .unwrap_or_default(),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        confirmed_at: row.get("confirmed_at")?,
    })
}

impl Store {
    /// Add a proposition. Declared propositions are confirmed on arrival
    /// (the human said so); inferred ones land as `proposed` for Tend.
    /// Re-proposing identical text for the same (sphere, kind) is a no-op
    /// returning the existing id — the grower must not flood Tend.
    #[allow(clippy::too_many_arguments)]
    pub fn add_proposition(
        &self,
        actor: &Actor,
        sphere: &str,
        kind: &str,
        text: &str,
        origin: &str,
        evidence: &[String],
        embedding: Option<&[f32]>,
    ) -> Result<String> {
        if !SPHERES.contains(&sphere) {
            return Err(anyhow!("unknown sphere {sphere:?}"));
        }
        if !PROPOSITION_KINDS.contains(&kind) {
            return Err(anyhow!("unknown proposition kind {kind:?}"));
        }
        if !matches!(origin, "declared" | "inferred") {
            return Err(anyhow!("origin must be declared|inferred, got {origin:?}"));
        }
        let text = text.trim();
        if text.len() < 8 {
            return Err(anyhow!("proposition text too short"));
        }
        let conn = self.lock();
        if let Some(existing) = conn
            .query_row(
                "SELECT id FROM self_model WHERE sphere = ?1 AND kind = ?2 AND text = ?3",
                rusqlite::params![sphere, kind, text],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(existing);
        }
        // Inferred propositions must point at the episodes they came from.
        if origin == "inferred" && evidence.is_empty() {
            return Err(anyhow!("inferred propositions require evidence"));
        }
        let id = ids::proposition_id();
        let now = now_utc();
        let (status, confirmed_at) = if origin == "declared" {
            ("confirmed", Some(now.clone()))
        } else {
            ("proposed", None)
        };
        conn.execute(
            "INSERT INTO self_model (id, sphere, kind, text, origin, status,
                 evidence, embedding, created_at, updated_at, confirmed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
            rusqlite::params![
                id,
                sphere,
                kind,
                text,
                origin,
                status,
                serde_json::to_string(evidence)?,
                embedding.map(floats_to_bytes),
                now,
                confirmed_at,
            ],
        )?;
        record_mutation(
            &conn,
            actor,
            "proposition_added",
            &format!("proposition_added:{id}"),
            &json!({"id": id, "sphere": sphere, "kind": kind, "origin": origin}),
        )?;
        super::journal_row(&conn, actor, "self_model", "self_model", &id, "create")?;
        Ok(id)
    }

    /// List propositions, optionally by status. Private-sphere rows are
    /// vault-guarded like every read surface.
    pub fn list_propositions(&self, status: Option<&str>) -> Result<Vec<Proposition>> {
        let guard = self.scope_guard("sphere");
        let conn = self.lock();
        let mut sql = format!(
            "SELECT id, sphere, kind, text, origin, status, evidence,
                    created_at, updated_at, confirmed_at
             FROM self_model WHERE 1=1{guard}"
        );
        if status.is_some() {
            sql.push_str(" AND status = ?1");
        }
        sql.push_str(
            " ORDER BY CASE status WHEN 'proposed' THEN 0 WHEN 'confirmed' THEN 1 ELSE 2 END,
              sphere, kind, created_at DESC",
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = match status {
            Some(s) => stmt.query_map([s], proposition_from_row)?,
            None => stmt.query_map([], proposition_from_row)?,
        };
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Confirm / reject / retire / remove a proposition, or move it into
    /// the vault; `confirm` may carry an edited text (the human's wording
    /// wins, origin unchanged).
    pub fn resolve_proposition(
        &self,
        actor: &Actor,
        id: &str,
        action: &str,
        edited_text: Option<&str>,
    ) -> Result<()> {
        let now = now_utc();
        let conn = self.lock();
        let changed = match action {
            "confirm" => {
                if let Some(text) = edited_text.map(str::trim).filter(|t| t.len() >= 8) {
                    conn.execute(
                        "UPDATE self_model SET status = 'confirmed', text = ?1,
                             confirmed_at = ?2, updated_at = ?2
                         WHERE id = ?3 AND status = 'proposed'",
                        rusqlite::params![text, now, id],
                    )?
                } else {
                    conn.execute(
                        "UPDATE self_model SET status = 'confirmed',
                             confirmed_at = ?1, updated_at = ?1
                         WHERE id = ?2 AND status = 'proposed'",
                        rusqlite::params![now, id],
                    )?
                }
            }
            "reject" => conn.execute(
                "UPDATE self_model SET status = 'rejected', updated_at = ?1
                 WHERE id = ?2 AND status = 'proposed'",
                rusqlite::params![now, id],
            )?,
            "retire" => conn.execute(
                "UPDATE self_model SET status = 'retired', updated_at = ?1
                 WHERE id = ?2 AND status = 'confirmed'",
                rusqlite::params![now, id],
            )?,
            // "This was never right": off the page and out of the
            // briefing for good. The row stays as `rejected`, so the
            // grower's dedupe never proposes the same thing again.
            "remove" => conn.execute(
                "UPDATE self_model SET status = 'rejected', updated_at = ?1
                 WHERE id = ?2 AND status IN ('confirmed', 'retired')",
                rusqlite::params![now, id],
            )?,
            // Move a belief into the vault, whatever its status.
            "make_private" => conn.execute(
                "UPDATE self_model SET sphere = 'private', updated_at = ?1
                 WHERE id = ?2 AND sphere != 'private'",
                rusqlite::params![now, id],
            )?,
            other => return Err(anyhow!("unknown action {other:?}")),
        };
        if changed == 0 {
            return Err(anyhow!("no proposition {id} eligible for {action}"));
        }
        record_mutation(
            &conn,
            actor,
            "proposition_resolved",
            &format!("proposition_resolved:{id}:{action}"),
            &json!({"id": id, "action": action}),
        )?;
        super::journal_row(&conn, actor, "self_model", "self_model", id, "update")?;
        Ok(())
    }

    /// Pending (proposed) count for the Tend badge — vault-guarded so a
    /// locked dashboard never even hints at private-sphere proposals.
    pub fn pending_propositions(&self) -> Result<i64> {
        let guard = self.scope_guard("sphere");
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT COUNT(*) FROM self_model WHERE status = 'proposed'{guard}"),
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    /// Confirmed `seeking` propositions with embeddings, for the idle
    /// opportunity scanner. Matching is local and server-side; DISPLAY of
    /// any resulting opportunity is vault-guarded, not the matching.
    pub fn confirmed_seeking(&self) -> Result<Vec<SeekingRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, text, embedding FROM self_model
             WHERE kind = 'seeking' AND status = 'confirmed'",
        )?;
        let rows = stmt.query_map([], |row| {
            let blob: Option<Vec<u8>> = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                blob.map(|b| bytes_to_floats(&b)),
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Confirmed propositions for BRAIN.md — private sphere excluded
    /// ALWAYS (persisted artifact), regardless of vault state.
    pub fn brain_propositions(&self) -> Result<Vec<Proposition>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, sphere, kind, text, origin, status, evidence,
                    created_at, updated_at, confirmed_at
             FROM self_model
             WHERE status = 'confirmed' AND sphere != 'private'
             ORDER BY sphere, kind, created_at",
        )?;
        let rows = stmt.query_map([], proposition_from_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn set_proposition_embedding(&self, id: &str, embedding: &[f32]) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE self_model SET embedding = ?1 WHERE id = ?2",
            rusqlite::params![floats_to_bytes(embedding), id],
        )?;
        Ok(())
    }

    /// Existing proposition texts (any status) — fed to the grower so it
    /// never re-proposes what was already suggested, confirmed or refused.
    pub fn known_proposition_texts(&self) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT text FROM self_model ORDER BY created_at")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Grower inputs: recent consolidation digests (id, title) and chapter
    /// headings (id, title). Digest/chapter material is non-private by
    /// construction, but the cloud-pass rule gets an explicit filter
    /// anyway — nothing scope=private may reach an LLM, ever.
    pub fn self_model_grower_inputs(
        &self,
        digests: usize,
        chapters: usize,
    ) -> Result<GrowerInputs> {
        let conn = self.lock();
        let pull = |kind: &str, limit: usize| -> Result<Vec<(String, String)>> {
            let mut stmt = conn.prepare(
                "SELECT id, title FROM episodes
                 WHERE source = 'consolidation' AND kind = ?1 AND scope != 'private'
                   AND superseded_by IS NULL
                 ORDER BY event_time DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![kind, limit as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(Into::into)
        };
        Ok((pull("summary", digests)?, pull("chapter", chapters)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> Actor {
        Actor::new("human:test")
    }

    #[test]
    fn test_remove_and_make_private() {
        let store = Store::open_in_memory().unwrap();
        let add = |text: &str| {
            store.add_proposition(&actor(), "work", "strength", text, "declared", &[], None).unwrap()
        };
        let wrong = add("a strong background in marine biology");
        let quiet = add("a habit of working very late nights");
        // Remove: gone from every list, but remembered so it is never
        // proposed again; a second remove has nothing to act on.
        store.resolve_proposition(&actor(), &wrong, "remove", None).unwrap();
        assert!(store.list_propositions(Some("confirmed")).unwrap().iter().all(|p| p.id != wrong));
        assert!(store.list_propositions(Some("retired")).unwrap().is_empty());
        assert!(store.known_proposition_texts().unwrap().iter().any(|t| t.contains("marine biology")));
        assert!(store.resolve_proposition(&actor(), &wrong, "remove", None).is_err());
        // Make private: hidden while the vault is locked (the default).
        store.resolve_proposition(&actor(), &quiet, "make_private", None).unwrap();
        assert!(store.list_propositions(None).unwrap().iter().all(|p| p.id != quiet));
        assert!(store.brain_propositions().unwrap().iter().all(|p| p.id != quiet));
        assert!(store.resolve_proposition(&actor(), &quiet, "make_private", None).is_err());
    }

    #[test]
    fn test_proposition_lifecycle() {
        let store = Store::open_in_memory().unwrap();

        // Declared → confirmed immediately.
        let declared = store
            .add_proposition(
                &actor(),
                "work",
                "seeking",
                "a cofounder for the game-economy startup",
                "declared",
                &[],
                None,
            )
            .unwrap();
        // Inferred without evidence is rejected.
        assert!(store
            .add_proposition(
                &actor(),
                "work",
                "strength",
                "ships fast under pressure",
                "inferred",
                &[],
                None,
            )
            .is_err());
        // Inferred with evidence → proposed.
        let inferred = store
            .add_proposition(
                &actor(),
                "work",
                "strength",
                "ships fast under pressure",
                "inferred",
                &["ep_1".into()],
                None,
            )
            .unwrap();
        assert_eq!(store.pending_propositions().unwrap(), 1);

        // Identical re-proposal is a no-op returning the same id.
        let again = store
            .add_proposition(
                &actor(),
                "work",
                "strength",
                "ships fast under pressure",
                "inferred",
                &["ep_2".into()],
                None,
            )
            .unwrap();
        assert_eq!(inferred, again);
        assert_eq!(store.pending_propositions().unwrap(), 1);

        // Confirm with an edit: text updates, origin stays inferred.
        store
            .resolve_proposition(
                &actor(),
                &inferred,
                "confirm",
                Some("ships production systems fast"),
            )
            .unwrap();
        let all = store.list_propositions(Some("confirmed")).unwrap();
        assert_eq!(all.len(), 2);
        let row = all.iter().find(|p| p.id == inferred).unwrap();
        assert_eq!(row.text, "ships production systems fast");
        assert_eq!(row.origin, "inferred");
        assert_eq!(row.evidence, vec!["ep_1".to_string()]);

        // Seeking rows feed the opportunity scanner.
        let seeking = store.confirmed_seeking().unwrap();
        assert_eq!(seeking.len(), 1);
        assert_eq!(seeking[0].0, declared);

        // Retire the declared one; it leaves BRAIN.md's set.
        store
            .resolve_proposition(&actor(), &declared, "retire", None)
            .unwrap();
        assert_eq!(store.brain_propositions().unwrap().len(), 1);
        assert!(store.confirmed_seeking().unwrap().is_empty());
    }

    #[test]
    fn test_private_sphere_vault_guarded() {
        let store = Store::open_in_memory().unwrap();
        store
            .add_proposition(
                &actor(),
                "private",
                "seeking",
                "something only the vault may know",
                "declared",
                &[],
                None,
            )
            .unwrap();
        store
            .add_proposition(
                &actor(),
                "personal",
                "identity",
                "a lifelong builder of worlds",
                "declared",
                &[],
                None,
            )
            .unwrap();

        // Locked (default): private sphere invisible.
        assert_eq!(store.list_propositions(None).unwrap().len(), 1);
        // Unlocked: visible.
        store.vault_unlock(5);
        assert_eq!(store.list_propositions(None).unwrap().len(), 2);
        store.vault_lock();
        assert_eq!(store.list_propositions(None).unwrap().len(), 1);
        // BRAIN.md never sees it, vault or no vault.
        store.vault_unlock(5);
        assert!(store
            .brain_propositions()
            .unwrap()
            .iter()
            .all(|p| p.sphere != "private"));
    }
}
