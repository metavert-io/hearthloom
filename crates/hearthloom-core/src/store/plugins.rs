//! The LOCAL plugin registry (v42) — which plugins are installed on
//! THIS node, what the user actually granted them, and their KV store.
//! This is not a marketplace: distribution/signing is deferred to
//! the provisioner (docs/plugins.md); install here means "the user pointed
//! at a package directory and approved its scopes".
//!
//! Two tables, both exported:
//! - `plugins` — one row per installed plugin: the manifest as
//!   installed, granted scopes (what the user approved, not what was
//!   requested), the PAT it's bound to, enabled flag. Registry changes
//!   mint mutations (the api_keys discipline).
//! - `plugin_kv` — daemon-mediated key/value storage per plugin (law
//!   #4: plugins never open files or the db themselves). Operational,
//!   like consolidation_state: no mutation per write, watermark column
//!   for the peering sync leg.

use anyhow::{anyhow, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;

use super::{record_mutation, Actor, Store};
use crate::time::now_utc;

#[derive(Debug, Clone, Serialize)]
pub struct PluginRow {
    pub name: String,
    pub version: String,
    pub manifest: serde_json::Value,
    pub granted_scopes: Vec<String>,
    pub api_key_id: String,
    /// "mirror" replicates this plugin's KV + files to the peer;
    /// "local-only" (default) stays on this node.
    pub sync: String,
    pub enabled: bool,
    pub installed_at: String,
    pub updated_at: String,
}

fn row_to_plugin(row: &rusqlite::Row) -> rusqlite::Result<PluginRow> {
    let manifest: String = row.get(2)?;
    let scopes: String = row.get(3)?;
    Ok(PluginRow {
        name: row.get(0)?,
        version: row.get(1)?,
        manifest: serde_json::from_str(&manifest).unwrap_or(serde_json::Value::Null),
        granted_scopes: serde_json::from_str(&scopes).unwrap_or_default(),
        api_key_id: row.get(4)?,
        sync: row.get(5)?,
        enabled: row.get::<_, i64>(6)? != 0,
        installed_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

const PLUGIN_COLS: &str =
    "name, version, manifest, granted_scopes, api_key_id, sync, enabled, installed_at, updated_at";

impl Store {
    /// Install (or reinstall/upgrade — upsert by name). The caller has
    /// already validated the manifest and minted the key.
    #[allow(clippy::too_many_arguments)]
    pub fn install_plugin(
        &self,
        actor: &Actor,
        name: &str,
        version: &str,
        manifest: &serde_json::Value,
        granted_scopes: &[String],
        api_key_id: &str,
        sync: &str,
    ) -> Result<PluginRow> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("a plugin needs a name"));
        }
        if !matches!(sync, "mirror" | "local-only") {
            return Err(anyhow!("sync must be \"mirror\" or \"local-only\""));
        }
        let now = now_utc();
        {
            let conn = self.lock();
            conn.execute(
                "INSERT INTO plugins (name, version, manifest, granted_scopes, api_key_id,
                                      sync, enabled, installed_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)
                 ON CONFLICT(name) DO UPDATE SET
                     version = excluded.version,
                     manifest = excluded.manifest,
                     granted_scopes = excluded.granted_scopes,
                     api_key_id = excluded.api_key_id,
                     sync = excluded.sync,
                     updated_at = excluded.updated_at",
                rusqlite::params![
                    name,
                    version,
                    manifest.to_string(),
                    serde_json::json!(granted_scopes).to_string(),
                    api_key_id,
                    sync,
                    now,
                ],
            )?;
            record_mutation(
                &conn,
                actor,
                "plugin_installed",
                &format!("plugin_installed:{name}:{version}:{now}"),
                &serde_json::json!({
                    "name": name, "version": version,
                    "granted_scopes": granted_scopes, "sync": sync,
                }),
            )?;
        }
        self.plugin(name)?
            .ok_or_else(|| anyhow!("plugin vanished after install"))
    }

    /// Uninstall removes the registry row (the mutations log keeps the
    /// history). The caller revokes the key and handles the data dir —
    /// which is listed to the user, never silently deleted.
    pub fn uninstall_plugin(&self, actor: &Actor, name: &str) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute("DELETE FROM plugins WHERE name = ?1", [name])?;
        if changed == 0 {
            return Err(anyhow!("no installed plugin {name:?}"));
        }
        conn.execute("DELETE FROM plugin_kv WHERE plugin = ?1", [name])?;
        record_mutation(
            &conn,
            actor,
            "plugin_uninstalled",
            &format!("plugin_uninstalled:{name}:{}", now_utc()),
            &serde_json::json!({"name": name}),
        )?;
        Ok(())
    }

    pub fn set_plugin_enabled(&self, actor: &Actor, name: &str, enabled: bool) -> Result<()> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE plugins SET enabled = ?2, updated_at = ?3 WHERE name = ?1",
            rusqlite::params![name, enabled as i64, now_utc()],
        )?;
        if changed == 0 {
            return Err(anyhow!("no installed plugin {name:?}"));
        }
        record_mutation(
            &conn,
            actor,
            "plugin_toggled",
            &format!("plugin_toggled:{name}:{enabled}:{}", now_utc()),
            &serde_json::json!({"name": name, "enabled": enabled}),
        )?;
        Ok(())
    }

    pub fn plugin(&self, name: &str) -> Result<Option<PluginRow>> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {PLUGIN_COLS} FROM plugins WHERE name = ?1"),
            [name],
            row_to_plugin,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_plugins(&self) -> Result<Vec<PluginRow>> {
        let conn = self.lock();
        let mut stmt =
            stmt_ok(conn.prepare(&format!("SELECT {PLUGIN_COLS} FROM plugins ORDER BY name")))?;
        let rows = stmt.query_map([], row_to_plugin)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// The storage-namespace binding: which plugin does this API key
    /// belong to? A key reaches ONLY its own plugin's storage.
    pub fn plugin_for_key(&self, api_key_id: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT name FROM plugins WHERE api_key_id = ?1 AND enabled = 1",
            [api_key_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    // ————— plugin KV (operational; no per-write mutations) —————

    pub fn plugin_kv_set(&self, plugin: &str, key: &str, value: &str) -> Result<()> {
        if key.trim().is_empty() {
            return Err(anyhow!("a key is required"));
        }
        let conn = self.lock();
        conn.execute(
            "INSERT INTO plugin_kv (plugin, key, value, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(plugin, key) DO UPDATE SET
                 value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![plugin, key, value, now_utc()],
        )?;
        Ok(())
    }

    pub fn plugin_kv_get(&self, plugin: &str, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT value FROM plugin_kv WHERE plugin = ?1 AND key = ?2",
            [plugin, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn plugin_kv_delete(&self, plugin: &str, key: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM plugin_kv WHERE plugin = ?1 AND key = ?2",
            [plugin, key],
        )?;
        Ok(n > 0)
    }

    pub fn plugin_kv_list(&self, plugin: &str) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = stmt_ok(conn.prepare(
            "SELECT key, value, updated_at FROM plugin_kv WHERE plugin = ?1 ORDER BY key",
        ))?;
        let rows = stmt.query_map([plugin], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// KV rows of mirror-sync'd plugins changed since a watermark — the
    /// peering leg's read side (chunked by the caller's limit).
    /// The relay cursor after `plugin_kv_since(since, limit)` (v57).
    pub fn plugin_kv_cursor(&self, since: &str, limit: usize) -> Result<String> {
        let conn = self.lock();
        super::page_cursor(&conn, "k.synced_at",
            "FROM plugin_kv k JOIN plugins p ON p.name = k.plugin WHERE p.sync = 'mirror' AND k.synced_at > ?1",
            since, limit)
    }

    pub fn plugin_kv_since(
        &self,
        since: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.lock();
        let mut stmt = stmt_ok(conn.prepare(
            "SELECT k.plugin, k.key, k.value, k.updated_at
             FROM plugin_kv k JOIN plugins p ON p.name = k.plugin
             WHERE p.sync = 'mirror' AND k.synced_at > ?1
             ORDER BY k.synced_at LIMIT ?2",
        ))?;
        let rows = stmt.query_map(rusqlite::params![since, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Apply a peer's KV batch: last-writer-wins by updated_at, and only
    /// for plugins THIS node also has installed with sync = mirror (a
    /// peer can never create storage for a plugin the user didn't
    /// install here). Returns how many rows were applied.
    pub fn apply_plugin_kv(&self, batch: &[(String, String, String, String)]) -> Result<usize> {
        let conn = self.lock();
        let mut applied = 0usize;
        for (plugin, key, value, updated_at) in batch {
            let eligible: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM plugins WHERE name = ?1 AND sync = 'mirror'",
                    [plugin],
                    |row| row.get(0),
                )
                .optional()?;
            if eligible.is_none() {
                continue;
            }
            let n = conn.execute(
                "INSERT INTO plugin_kv (plugin, key, value, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(plugin, key) DO UPDATE SET
                     value = excluded.value, updated_at = excluded.updated_at
                 WHERE excluded.updated_at > plugin_kv.updated_at",
                rusqlite::params![plugin, key, value, updated_at],
            )?;
            applied += n;
        }
        Ok(applied)
    }
}

/// prepare() → anyhow bridge that keeps the call sites one-liners.
fn stmt_ok(r: rusqlite::Result<rusqlite::Statement<'_>>) -> Result<rusqlite::Statement<'_>> {
    r.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> (Store, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.db")).unwrap();
        (store, tmp)
    }

    fn manifest() -> serde_json::Value {
        serde_json::json!({"protocol": 1, "name": "demo", "version": "0.1.0"})
    }

    #[test]
    fn test_plugin_lifecycle() {
        let (store, _tmp) = test_store();
        let actor = Actor::new("human:test");
        let row = store
            .install_plugin(&actor, "demo", "0.1.0", &manifest(),
                &["read".into()], "ak_1", "local-only")
            .unwrap();
        assert!(row.enabled);
        assert_eq!(store.plugin_for_key("ak_1").unwrap().as_deref(), Some("demo"));

        // reinstall upserts (upgrade path), keeps one row
        store
            .install_plugin(&actor, "demo", "0.2.0", &manifest(),
                &["read".into()], "ak_1", "mirror")
            .unwrap();
        assert_eq!(store.list_plugins().unwrap().len(), 1);
        assert_eq!(store.plugin("demo").unwrap().unwrap().version, "0.2.0");

        // disabled plugins lose the storage binding
        store.set_plugin_enabled(&actor, "demo", false).unwrap();
        assert!(store.plugin_for_key("ak_1").unwrap().is_none());
        store.set_plugin_enabled(&actor, "demo", true).unwrap();

        store.uninstall_plugin(&actor, "demo").unwrap();
        assert!(store.plugin("demo").unwrap().is_none());
        assert!(store.uninstall_plugin(&actor, "demo").is_err());
    }

    #[test]
    fn test_plugin_kv_and_sync_filter() {
        let (store, _tmp) = test_store();
        let actor = Actor::new("human:test");
        store
            .install_plugin(&actor, "m", "1", &manifest(), &[], "ak_m", "mirror")
            .unwrap();
        store
            .install_plugin(&actor, "l", "1", &manifest(), &[], "ak_l", "local-only")
            .unwrap();
        store.plugin_kv_set("m", "watermark", "2026-01-01").unwrap();
        store.plugin_kv_set("l", "secret-ish", "stays-home").unwrap();

        // only the mirror plugin's rows leave via the sync read
        let out = store.plugin_kv_since("", 100).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "m");

        // apply: mirror rows land, unknown/local-only plugins are refused
        let applied = store
            .apply_plugin_kv(&[
                ("m".into(), "peer-key".into(), "v".into(), "2027-01-01T00:00:00Z".into()),
                ("l".into(), "nope".into(), "v".into(), "2027-01-01T00:00:00Z".into()),
                ("ghost".into(), "nope".into(), "v".into(), "2027-01-01T00:00:00Z".into()),
            ])
            .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(store.plugin_kv_get("m", "peer-key").unwrap().as_deref(), Some("v"));
        assert!(store.plugin_kv_get("l", "nope").unwrap().is_none());

        // last-writer-wins: an older peer write never clobbers newer
        store.plugin_kv_set("m", "peer-key", "newer-local").unwrap();
        store
            .apply_plugin_kv(&[("m".into(), "peer-key".into(), "old".into(), "2020-01-01T00:00:00Z".into())])
            .unwrap();
        assert_eq!(
            store.plugin_kv_get("m", "peer-key").unwrap().as_deref(),
            Some("newer-local")
        );

        // delete works
        assert!(store.plugin_kv_delete("m", "watermark").unwrap());
        assert!(!store.plugin_kv_delete("m", "watermark").unwrap());
    }
}
