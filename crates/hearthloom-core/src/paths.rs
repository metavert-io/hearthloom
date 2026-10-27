//! The Hearthloom home directory: one SQLite file plus plain files.
//!
//! Layout under `~/.hearthloom/` (overridable via `HEARTHLOOM_HOME`):
//!
//! ```text
//! hearthloom.db      the substrate (episodes, graph, indexes, logs)
//! raw/YYYY/MM/DD/    raw episode content, date-organized, human-browsable
//! config/{env}.yaml  configuration
//! BRAIN.md           the regenerated brain index
//! daemon.log         daemon output when auto-spawned in the background
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

const SKILL_MD: &str = r#"---
name: hearthloom
description: Query the local second brain (episodes, entities, temporal recall) via CLI or MCP instead of grepping raw files.
---

# Hearthloom — query idioms

This directory is a personal second brain: `hearthloom.db` (SQLite substrate),
`raw/YYYY/MM/DD/` (verbatim episode content), `BRAIN.md` (current index —
read it first). Do not write to any of these; the daemon is the sole writer.

Prefer the query surfaces over reading raw files:

- `hearthloom search <query>` — hybrid FTS+vector over all episodes
- `hearthloom recall --on YYYY-MM-DD` — what happened that day (local time)
- `hearthloom entity <name>` — dossier with relations and cited episodes
- `hearthloom events --since <cursor>` — change feed
- MCP (streamable HTTP, http://127.0.0.1:43278/mcp): `search`, `traverse`,
  `recall`, `propose_memory` — recall/search take `detail: summary|full`
- To remember something: propose_memory (goes to quarantine for review),
  never direct writes

Raw files under `raw/` are the citation targets: episode ids in query
results map to `raw/<date>/<episode-id>.txt`.

To ingest an export (Takeout zip, .mbox, .ics, conversations.json): move
it into `inbox/` — the daemon ingests it and files it under
`inbox/processed/` (failures land in `inbox/failed/` with an error note).
"#;

#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Resolve the home directory: `HEARTHLOOM_HOME` or `~/.hearthloom`.
    pub fn resolve() -> Result<Self> {
        if let Ok(dir) = std::env::var("HEARTHLOOM_HOME") {
            return Ok(Home {
                root: PathBuf::from(dir),
            });
        }
        let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
        Ok(Home {
            root: Path::new(&home).join(".hearthloom"),
        })
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Home { root: root.into() }
    }

    /// Create the directory layout and a default config if absent.
    /// Idempotent; asks no questions.
    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(self.raw_dir()).context("creating raw content dir")?;
        fs::create_dir_all(self.config_dir()).context("creating config dir")?;
        let default_config = self.config_dir().join("development.yaml");
        if !default_config.exists() {
            fs::write(&default_config, crate::config::DEFAULT_CONFIG_YAML)
                .context("writing default config")?;
        }
        fs::create_dir_all(self.inbox_dir()).context("creating inbox dir")?;
        fs::create_dir_all(self.notes_dir()).context("creating notes dir")?;
        fs::create_dir_all(self.inbox_dir().join("processed"))?;
        fs::create_dir_all(self.inbox_dir().join("failed"))?;
        // The T0 filesystem tier: any file-capable agent that lands in
        // ~/.hearthloom learns the query idioms from SKILL.md (~100-token
        // standing cost) and the current state from BRAIN.md.
        let skill = self.root.join("SKILL.md");
        if !skill.exists() {
            fs::write(&skill, SKILL_MD).context("writing SKILL.md")?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("hearthloom.db")
    }

    pub fn raw_dir(&self) -> PathBuf {
        self.root.join("raw")
    }

    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    pub fn brain_index_path(&self) -> PathBuf {
        self.root.join("BRAIN.md")
    }

    /// Local embedding models (all-MiniLM-L6-v2.onnx + tokenizer.json).
    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    /// Drop folder: archives/exports placed here are ingested by the
    /// daemon automatically (moved to processed/ or failed/ afterwards).
    pub fn inbox_dir(&self) -> PathBuf {
        self.root.join("inbox")
    }

    /// Notes live as plain markdown files (law #1: the user owns the
    /// substrate); the DB holds only the index + FTS. One file per note.
    pub fn notes_dir(&self) -> PathBuf {
        self.root.join("notes")
    }

    /// Write a note's markdown file. Returns the path relative to root.
    pub fn write_note(&self, slug: &str, content: &str) -> Result<String> {
        let rel = format!("notes/{slug}.md");
        if !stays_inside(&rel) {
            anyhow::bail!("note slug {slug:?} would write outside the notes folder");
        }
        let path = self.root.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
        Ok(rel)
    }

    pub fn read_note(&self, rel: &str) -> Result<String> {
        fs::read_to_string(self.root.join(rel)).map_err(Into::into)
    }

    /// Media preserved from note imports (images an embed referenced),
    /// keyed by the owning note's origin_ref. Served vault-guarded.
    pub fn notes_media_dir(&self) -> PathBuf {
        self.root.join("notes/media")
    }

    pub fn daemon_log_path(&self) -> PathBuf {
        self.root.join("daemon.log")
    }

    pub fn exists(&self) -> bool {
        self.db_path().exists() || self.raw_dir().exists()
    }

    /// Write raw episode content under `raw/YYYY/MM/DD/<episode_id>.txt`,
    /// dated by event time (falling back to ingest date). Returns the
    /// content_ref (path relative to the home root). Raw content is never
    /// discarded: the whole graph is re-derivable from these files.
    pub fn write_raw(&self, episode_id: &str, date: &str, content: &str) -> Result<String> {
        // `date` is YYYY-MM-DD.
        let parts: Vec<&str> = date.splitn(3, '-').collect();
        let (y, m, d) = match parts.as_slice() {
            [y, m, d] => (*y, *m, *d),
            _ => ("0000", "00", "00"),
        };
        let rel = format!("raw/{y}/{m}/{d}/{episode_id}.txt");
        if !stays_inside(&rel) {
            anyhow::bail!("raw path {rel:?} would write outside the raw folder");
        }
        let path = self.root.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
        Ok(rel)
    }
}

/// A path relative to the home root that can't leave it: not absolute, no
/// empty, `.` or `..` segment, no backslash or NUL. Slugs and dates come
/// from synced rows and ingest input, so the writers check, not only their
/// callers.
fn stays_inside(rel: &str) -> bool {
    !rel.starts_with('/')
        && !rel.contains(['\\', '\0'])
        && rel
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_and_write_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        assert!(home.config_dir().join("development.yaml").exists());

        let rel = home.write_raw("ep_abc", "2026-03-15", "hello").unwrap();
        assert_eq!(rel, "raw/2026/03/15/ep_abc.txt");
        assert_eq!(fs::read_to_string(home.root().join(&rel)).unwrap(), "hello");

        // Idempotent re-init.
        home.init().unwrap();
    }

    #[test]
    fn test_writers_stay_inside_the_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("hl"));
        home.init().unwrap();
        assert_eq!(
            home.write_note("ideas/plan", "x").unwrap(),
            "notes/ideas/plan.md"
        );
        for slug in ["../../escape", "a/../../b", "/etc/x", "a\\b"] {
            assert!(
                home.write_note(slug, "x").is_err(),
                "{slug:?} must be refused"
            );
        }
        assert!(home.write_raw("../../escape", "2026-03-15", "x").is_err());
        assert!(home.write_raw("ep_x", "..-..-..", "x").is_err());
        assert!(!tmp.path().join("escape.md").exists() && !tmp.path().join("escape.txt").exists());
    }
}
