//! The LLM token-accounting ledger (v50). One append-only row per LLM
//! API call — Ask rounds and every consolidation pass — written by the
//! daemon's `llm::call` wrapper. Tokens are the stored truth; dollar
//! figures are computed at read time (provider::pricing_for) so price
//! drift never corrupts history.
//!
//! Read surfaces (`llm_usage_summary`, `llm_usage_for_ref`) are
//! BLANKET-GATED on the vault: traces and spend patterns can reveal
//! vaulted activity, so with the vault locked they refuse outright
//! (Jon 2026-08-16 — deliberately stricter than scope_guard filtering).
//! The peer-sync legs are NOT gated: peering replicates private data
//! by design (telemetry precedent).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::Store;
use crate::provider::Usage;
use crate::time::now_utc;

impl Store {
    fn require_vault_open(&self) -> Result<()> {
        if self.vault_status().0 {
            return Err(anyhow!("vault locked"));
        }
        Ok(())
    }

    /// Append one ledger row. Never gates on the vault — writes must
    /// not depend on read posture. Errors are the caller's to swallow
    /// (accounting must not break the app; llm::call logs and moves on).
    #[allow(clippy::too_many_arguments)]
    pub fn record_llm_usage(
        &self,
        node: &str,
        purpose: &str,
        ref_id: Option<&str>,
        provider: &str,
        model: &str,
        tier: &str,
        usage: &Usage,
        latency_ms: u64,
        status: &str,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO llm_usage (id, ts, node, purpose, ref_id, provider, model, tier,
                                    input_tokens, output_tokens, cache_creation_tokens,
                                    cache_read_tokens, latency_ms, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                format!("lu_{}", uuid::Uuid::new_v4().simple()),
                now_utc(),
                node,
                purpose,
                ref_id,
                provider,
                model,
                tier,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_tokens,
                usage.cache_read_tokens,
                latency_ms as i64,
                status,
            ],
        )?;
        Ok(())
    }

    /// One ledger row for a run of a model on this machine (v53) — the
    /// embedding backfill. No tokens: `items` is what the run processed.
    /// `model` is the bare name; it is stored as `local:<name>` so the
    /// price table reads it as free.
    pub fn record_local_model_run(
        &self,
        node: &str,
        purpose: &str,
        model: &str,
        items: usize,
        latency_ms: u64,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO llm_usage (id, ts, node, purpose, provider, model, tier, latency_ms, status, items)
             VALUES (?1, ?2, ?3, ?4, 'local', ?5, 'local', ?6, 'ok', ?7)",
            rusqlite::params![
                format!("lu_{}", uuid::Uuid::new_v4().simple()),
                now_utc(),
                node,
                purpose,
                format!("{}{model}", crate::provider::LOCAL_MODEL_PREFIX),
                latency_ms as i64,
                items as i64,
            ],
        )?;
        Ok(())
    }

    /// Token totals per model in `[from, to)`, every node — what the
    /// monthly-budget check prices. NOT vault-gated: it feeds a threshold
    /// test inside the daemon and is never served as-is; the read
    /// surfaces above stay gated.
    pub fn llm_usage_totals(&self, from: &str, to: &str) -> Result<Vec<(String, Usage)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT model, SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_creation_tokens), SUM(cache_read_tokens)
             FROM llm_usage WHERE ts >= ?1 AND ts < ?2 GROUP BY 1",
        )?;
        let rows = stmt
            .query_map([from, to], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    Usage {
                        input_tokens: row.get(1)?,
                        output_tokens: row.get(2)?,
                        cache_creation_tokens: row.get(3)?,
                        cache_read_tokens: row.get(4)?,
                    },
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Aggregates for the Monitoring viewer. Every grouping keeps the
    /// model dimension so the caller can price each row (cost is never
    /// stored). `node` filters to one node's spend; None = both.
    pub fn llm_usage_summary(&self, days: i64, node: Option<&str>) -> Result<Value> {
        self.require_vault_open()?;
        let since = (chrono::Utc::now() - chrono::Duration::days(days))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let conn = self.lock();
        let group = |sql: &str| -> Result<Vec<Value>> {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt
                .query_map(rusqlite::params![since, node], |row| {
                    Ok(json!({
                        "key": row.get::<_, String>(0)?,
                        "model": row.get::<_, String>(1)?,
                        "calls": row.get::<_, i64>(2)?,
                        "input_tokens": row.get::<_, i64>(3)?,
                        "output_tokens": row.get::<_, i64>(4)?,
                        "cache_creation_tokens": row.get::<_, i64>(5)?,
                        "cache_read_tokens": row.get::<_, i64>(6)?,
                        "avg_latency_ms": row.get::<_, f64>(7)?,
                        "errors": row.get::<_, i64>(8)?,
                    }))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        };
        let agg = "COUNT(*), SUM(input_tokens), SUM(output_tokens),
                   SUM(cache_creation_tokens), SUM(cache_read_tokens),
                   AVG(latency_ms), SUM(status != 'ok')";
        let daily = group(&format!(
            "SELECT substr(ts, 1, 10), model, {agg} FROM llm_usage
             WHERE ts > ?1 AND (?2 IS NULL OR node = ?2)
             GROUP BY 1, 2 ORDER BY 1"
        ))?;
        let purposes = group(&format!(
            "SELECT purpose, model, {agg} FROM llm_usage
             WHERE ts > ?1 AND (?2 IS NULL OR node = ?2)
             GROUP BY 1, 2 ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC"
        ))?;
        let models = group(&format!(
            "SELECT provider || ' · ' || tier, model, {agg} FROM llm_usage
             WHERE ts > ?1 AND (?2 IS NULL OR node = ?2)
             GROUP BY 1, 2 ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC"
        ))?;
        Ok(json!({ "since": since, "daily": daily, "purposes": purposes, "models": models }))
    }

    /// Every ledger row for one ref (an ask id) — the per-chat debug
    /// drill-down joins these against the ask transcript.
    /// The spend report's raw material: one row per (UTC day, provider,
    /// model, tier, purpose) in `[from, to)` — fine enough that the
    /// client can roll it up along any axis without another query.
    /// Tokens only; the daemon prices rows at read time. Vault-gated.
    pub fn llm_spend_cells(&self, from: &str, to: &str, node: Option<&str>) -> Result<Vec<Value>> {
        self.require_vault_open()?;
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT substr(ts, 1, 10), provider, model, tier, purpose,
                    COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_creation_tokens), SUM(cache_read_tokens),
                    SUM(status != 'ok'), SUM(latency_ms), SUM(items)
             FROM llm_usage
             WHERE ts >= ?1 AND ts < ?2 AND (?3 IS NULL OR node = ?3)
             GROUP BY 1, 2, 3, 4, 5 ORDER BY 1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![from, to, node], |row| {
                Ok(json!({
                    "day": row.get::<_, String>(0)?,
                    "provider": row.get::<_, String>(1)?,
                    "model": row.get::<_, String>(2)?,
                    "tier": row.get::<_, String>(3)?,
                    "purpose": row.get::<_, String>(4)?,
                    "calls": row.get::<_, i64>(5)?,
                    "input_tokens": row.get::<_, i64>(6)?,
                    "output_tokens": row.get::<_, i64>(7)?,
                    "cache_creation_tokens": row.get::<_, i64>(8)?,
                    "cache_read_tokens": row.get::<_, i64>(9)?,
                    "errors": row.get::<_, i64>(10)?,
                    "latency_ms": row.get::<_, i64>(11)?,
                    "items": row.get::<_, i64>(12)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-question spend in `[from, to)`: one row per (ask id, model)
    /// across the root loop AND its `map` sub-calls. Vault-gated.
    pub fn llm_spend_by_ask(&self, from: &str, to: &str, node: Option<&str>) -> Result<Vec<Value>> {
        self.require_vault_open()?;
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ref_id, model, MIN(ts), COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_creation_tokens), SUM(cache_read_tokens)
             FROM llm_usage
             WHERE ts >= ?1 AND ts < ?2 AND (?3 IS NULL OR node = ?3)
               AND ref_id IS NOT NULL AND purpose IN ('ask', 'ask_map')
             GROUP BY 1, 2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![from, to, node], |row| {
                Ok(json!({
                    "ask_id": row.get::<_, String>(0)?,
                    "model": row.get::<_, String>(1)?,
                    "ts": row.get::<_, String>(2)?,
                    "calls": row.get::<_, i64>(3)?,
                    "input_tokens": row.get::<_, i64>(4)?,
                    "output_tokens": row.get::<_, i64>(5)?,
                    "cache_creation_tokens": row.get::<_, i64>(6)?,
                    "cache_read_tokens": row.get::<_, i64>(7)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// When the ledger begins — spend before this was never recorded.
    pub fn llm_usage_first_ts(&self) -> Result<Option<String>> {
        self.require_vault_open()?;
        let conn = self.lock();
        Ok(conn.query_row("SELECT MIN(ts) FROM llm_usage", [], |row| row.get(0))?)
    }

    pub fn llm_usage_for_ref(&self, ref_id: &str) -> Result<Vec<Value>> {
        self.require_vault_open()?;
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, node, purpose, provider, model, tier, input_tokens,
                    output_tokens, cache_creation_tokens, cache_read_tokens,
                    latency_ms, status
             FROM llm_usage WHERE ref_id = ?1 ORDER BY ts",
        )?;
        let rows = stmt
            .query_map([ref_id], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "ts": row.get::<_, String>(1)?,
                    "node": row.get::<_, String>(2)?,
                    "purpose": row.get::<_, String>(3)?,
                    "provider": row.get::<_, String>(4)?,
                    "model": row.get::<_, String>(5)?,
                    "tier": row.get::<_, String>(6)?,
                    "input_tokens": row.get::<_, i64>(7)?,
                    "output_tokens": row.get::<_, i64>(8)?,
                    "cache_creation_tokens": row.get::<_, i64>(9)?,
                    "cache_read_tokens": row.get::<_, i64>(10)?,
                    "latency_ms": row.get::<_, i64>(11)?,
                    "status": row.get::<_, String>(12)?,
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Peer-sync export: rows past a ts watermark (telemetry pattern —
    /// append-only, id-keyed, replay-safe). NOT vault-gated.
    pub fn llm_usage_since(&self, since: &str, limit: usize) -> Result<Value> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, node, purpose, ref_id, provider, model, tier, input_tokens,
                    output_tokens, cache_creation_tokens, cache_read_tokens, latency_ms, status,
                    items
             FROM llm_usage WHERE synced_at > ?1 ORDER BY synced_at LIMIT ?2",
        )?;
        let items: Vec<Value> = stmt
            .query_map(rusqlite::params![since, limit as i64], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?, "ts": row.get::<_, String>(1)?,
                    "node": row.get::<_, String>(2)?, "purpose": row.get::<_, String>(3)?,
                    "ref_id": row.get::<_, Option<String>>(4)?,
                    "provider": row.get::<_, String>(5)?, "model": row.get::<_, String>(6)?,
                    "tier": row.get::<_, String>(7)?,
                    "input_tokens": row.get::<_, i64>(8)?, "output_tokens": row.get::<_, i64>(9)?,
                    "cache_creation_tokens": row.get::<_, i64>(10)?,
                    "cache_read_tokens": row.get::<_, i64>(11)?,
                    "latency_ms": row.get::<_, i64>(12)?, "status": row.get::<_, String>(13)?,
                    "items": row.get::<_, i64>(14)?,
                }))
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let next = super::page_cursor(&conn, "synced_at", "FROM llm_usage WHERE synced_at > ?1", since, limit)?;
        Ok(json!({ "items": items, "next": next }))
    }

    /// Apply a peer's batch — INSERT OR IGNORE by id (replay-safe).
    pub fn apply_llm_usage(&self, batch: &Value) -> Result<usize> {
        let conn = self.lock();
        let mut applied = 0usize;
        for r in batch.get("items").and_then(|v| v.as_array()).unwrap_or(&vec![]) {
            applied += conn.execute(
                "INSERT OR IGNORE INTO llm_usage (id, ts, node, purpose, ref_id, provider,
                     model, tier, input_tokens, output_tokens, cache_creation_tokens,
                     cache_read_tokens, latency_ms, status, items)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                rusqlite::params![
                    r.get("id").and_then(|v| v.as_str()),
                    r.get("ts").and_then(|v| v.as_str()),
                    r.get("node").and_then(|v| v.as_str()),
                    r.get("purpose").and_then(|v| v.as_str()),
                    r.get("ref_id").and_then(|v| v.as_str()),
                    r.get("provider").and_then(|v| v.as_str()),
                    r.get("model").and_then(|v| v.as_str()),
                    r.get("tier").and_then(|v| v.as_str()),
                    r.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                    r.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                    r.get("cache_creation_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                    r.get("cache_read_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                    r.get("latency_ms").and_then(|v| v.as_i64()).unwrap_or(0),
                    r.get("status").and_then(|v| v.as_str()).unwrap_or("ok"),
                    r.get("items").and_then(|v| v.as_i64()).unwrap_or(0),
                ],
            )?;
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: i64, output: i64) -> Usage {
        Usage { input_tokens: input, output_tokens: output, ..Default::default() }
    }

    #[test]
    fn test_ledger_records_and_summarizes() {
        let store = Store::open_in_memory().unwrap();
        store.vault_unlock(5);
        store
            .record_llm_usage("local", "ask", Some("ask_1"), "anthropic",
                "claude-opus-4-8", "high", &usage(1000, 200), 850, "ok")
            .unwrap();
        store
            .record_llm_usage("local", "daily_digest", None, "anthropic",
                "claude-sonnet-4-6", "medium", &usage(5000, 800), 1200, "ok")
            .unwrap();
        store
            .record_llm_usage("local", "ask", Some("ask_1"), "anthropic",
                "claude-opus-4-8", "high", &usage(0, 0), 300, "error")
            .unwrap();

        let s = store.llm_usage_summary(1, None).unwrap();
        assert_eq!(s["purposes"].as_array().unwrap().len(), 2);
        let ask = s["purposes"].as_array().unwrap().iter()
            .find(|p| p["key"] == "ask").unwrap();
        assert_eq!(ask["calls"], 2);
        assert_eq!(ask["input_tokens"], 1000);
        assert_eq!(ask["errors"], 1);

        let rows = store.llm_usage_for_ref("ask_1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["latency_ms"], 850);
        store.vault_lock();
    }

    #[test]
    fn test_local_model_runs_are_ledger_rows_and_sync() {
        let a = Store::open_in_memory().unwrap();
        a.record_local_model_run("local", "embed_backfill", "all-MiniLM-L6-v2", 300, 4200).unwrap();
        a.record_llm_usage("local", "ask", None, "anthropic",
            "claude-opus-4-8", "high", &usage(1000, 200), 850, "ok").unwrap();
        a.vault_unlock(5);
        let cells = a.llm_spend_cells("2000-01-01", "2999-01-01", None).unwrap();
        let run = cells.iter().find(|c| c["purpose"] == "embed_backfill").unwrap();
        assert_eq!(run["model"], "local:all-MiniLM-L6-v2");
        assert_eq!(run["provider"], "local");
        assert_eq!((run["items"].as_i64(), run["calls"].as_i64()), (Some(300), Some(1)));
        assert_eq!(run["input_tokens"], 0);
        assert!(crate::provider::pricing_for(run["model"].as_str().unwrap()).is_some(), "free, not unpriced");
        a.vault_lock();

        // The budget's raw material is readable with the vault locked.
        let totals = a.llm_usage_totals("2000-01-01", "2999-01-01").unwrap();
        assert_eq!(totals.len(), 2);
        let opus = totals.iter().find(|(m, _)| m == "claude-opus-4-8").unwrap();
        assert_eq!((opus.1.input_tokens, opus.1.output_tokens), (1000, 200));
        assert!(a.llm_usage_totals("2999-01-01", "2999-02-01").unwrap().is_empty());

        // `items` crosses the sync leg; a batch from a peer that predates
        // the field applies with 0.
        let b = Store::open_in_memory().unwrap();
        let mut batch = a.llm_usage_since("", 100).unwrap();
        assert_eq!(b.apply_llm_usage(&batch).unwrap(), 2);
        b.vault_unlock(5);
        let synced = b.llm_spend_cells("2000-01-01", "2999-01-01", None).unwrap();
        assert_eq!(synced.iter().find(|c| c["purpose"] == "embed_backfill").unwrap()["items"], 300);
        b.vault_lock();
        let c = Store::open_in_memory().unwrap();
        for row in batch["items"].as_array_mut().unwrap() {
            row.as_object_mut().unwrap().remove("items");
        }
        assert_eq!(c.apply_llm_usage(&batch).unwrap(), 2);
    }

    #[test]
    fn test_reads_blanket_gated_on_vault() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_llm_usage("local", "ask", Some("a"), "anthropic",
                "claude-opus-4-8", "high", &usage(1, 1), 1, "ok")
            .unwrap();
        // Locked: both read surfaces refuse outright (no filtered view).
        assert!(store.llm_usage_summary(1, None).is_err());
        assert!(store.llm_usage_for_ref("a").is_err());
        // Sync legs stay open — peering replicates regardless.
        let batch = store.llm_usage_since("", 10).unwrap();
        assert_eq!(batch["items"].as_array().unwrap().len(), 1);
        store.vault_unlock(5);
        assert!(store.llm_usage_summary(1, None).is_ok());
        store.vault_lock();
    }

    #[test]
    fn test_apply_is_replay_safe_union() {
        let a = Store::open_in_memory().unwrap();
        let b = Store::open_in_memory().unwrap();
        a.record_llm_usage("local", "ask", None, "anthropic",
            "claude-haiku-4-5", "low", &usage(10, 5), 40, "ok").unwrap();
        b.record_llm_usage("cloud", "extraction", None, "anthropic",
            "claude-sonnet-4-6", "medium", &usage(20, 8), 90, "ok").unwrap();
        let batch = a.llm_usage_since("", 100).unwrap();
        assert_eq!(b.apply_llm_usage(&batch).unwrap(), 1);
        assert_eq!(b.apply_llm_usage(&batch).unwrap(), 0); // replay = no-op
        b.vault_unlock(5);
        let s = b.llm_usage_summary(1, None).unwrap();
        assert_eq!(s["purposes"].as_array().unwrap().len(), 2); // union
        let s_local = b.llm_usage_summary(1, Some("local")).unwrap();
        assert_eq!(s_local["purposes"].as_array().unwrap().len(), 1);
        b.vault_lock();
    }
}
