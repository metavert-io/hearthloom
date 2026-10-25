//! The monthly AI-spend budget (☰ → Spend). The user names a dollar
//! figure for the calendar month (UTC, like every day on the Spend page);
//! the consolidation cycle prices the month's ledger rows from BOTH nodes
//! and raises one notification at 80% and one at 100%.
//!
//! Nothing here stops a call — it is an alert, not a cap. The figure is
//! an estimate from the price table; a model with no price on file adds
//! nothing to it.
//!
//! Sync: the budget rides the operational mirror (`llm_budget`, LWW by
//! its own stamp — the profile-card pattern), so it follows this node to
//! the always-on peer. Which thresholds were already announced is
//! node-local on purpose: the two nodes raise the same notification
//! (same origin + dedupe key) and the notification exchange merges them.
//!
//! Vault: the alert says a threshold was crossed and what the budget is,
//! never the amount spent — the breakdown stays behind the vault on the
//! Spend page.

use anyhow::Result;
use chrono::{Datelike, NaiveDate, Utc};
use hearthloom_core::provider::usage_cost_usd;
use hearthloom_core::store::Actor;

use crate::AppState;

pub const KEY: &str = "llm.budget.monthly_usd";
pub const KEY_STAMP: &str = "llm.budget.updated_at";
/// `<YYYY-MM>:<level>` — the highest threshold announced this month.
const KEY_ALERTED: &str = "llm.budget.alerted";

const LEVELS: [u8; 2] = [100, 80];

/// The budget in dollars, if one is set.
pub fn current(state: &AppState) -> Option<f64> {
    state
        .store
        .consolidation_state(KEY)
        .ok()
        .flatten()
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|b| b.is_finite() && *b > 0.0)
}

/// Set (or clear, with None) the budget on this node.
pub fn set(state: &AppState, budget: Option<f64>) -> Result<()> {
    let value = budget.map(|b| format!("{b:.2}")).unwrap_or_default();
    store(state, &value, &hearthloom_core::time::now_utc())
}

/// Take the other node's budget if it is newer than ours. Returns
/// whether it was adopted.
pub fn adopt(state: &AppState, value: &str, stamp: &str) -> Result<bool> {
    let ours = state.store.consolidation_state(KEY_STAMP)?.unwrap_or_default();
    if stamp <= ours.as_str() {
        return Ok(false);
    }
    store(state, value, stamp)?;
    Ok(true)
}

/// A changed budget is measured afresh: thresholds already announced
/// against the old figure say nothing about the new one.
fn store(state: &AppState, value: &str, stamp: &str) -> Result<()> {
    state.store.set_consolidation_state(KEY, value)?;
    state.store.set_consolidation_state(KEY_STAMP, stamp)?;
    state.store.set_consolidation_state(KEY_ALERTED, "")?;
    Ok(())
}

/// `[first day of this month, first day of the next)`, as ISO days.
fn month_bounds(today: NaiveDate) -> (String, String) {
    let start = today.with_day(1).unwrap_or(today);
    let next = if start.month() == 12 {
        NaiveDate::from_ymd_opt(start.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(start.year(), start.month() + 1, 1)
    }
    .unwrap_or(start);
    (start.format("%Y-%m-%d").to_string(), next.format("%Y-%m-%d").to_string())
}

/// The highest threshold `spent` has reached, if any.
fn level_reached(spent: f64, budget: f64) -> Option<u8> {
    LEVELS.into_iter().find(|level| spent >= budget * f64::from(*level) / 100.0)
}

/// Is `level` news this month, given what was last announced?
fn is_news(month: &str, level: u8, alerted: &str) -> bool {
    match alerted.split_once(':') {
        Some((m, l)) if m == month => l.parse::<u8>().is_ok_and(|done| level > done),
        _ => true,
    }
}

/// This month's estimated spend across every node, from the ledger.
fn month_spend(state: &AppState, today: NaiveDate) -> Result<f64> {
    let (from, to) = month_bounds(today);
    Ok(state
        .store
        .llm_usage_totals(&from, &to)?
        .iter()
        .filter_map(|(model, usage)| {
            crate::http::priced(&state.home, model).map(|p| usage_cost_usd(&p, usage))
        })
        .sum())
}

/// Compare this month's spend with the budget and announce a newly
/// crossed threshold, once. Cheap (one grouped query); runs every
/// consolidation cycle and right after the budget changes.
pub fn check(state: &AppState) -> Result<Option<u8>> {
    let Some(budget) = current(state) else {
        return Ok(None);
    };
    let today = Utc::now().date_naive();
    let month = today.format("%Y-%m").to_string();
    let Some(level) = level_reached(month_spend(state, today)?, budget) else {
        return Ok(None);
    };
    let alerted = state.store.consolidation_state(KEY_ALERTED)?.unwrap_or_default();
    if !is_news(&month, level, &alerted) {
        return Ok(None);
    }
    let (title, severity) = if level >= 100 {
        ("AI spending has reached this month's budget", "warn")
    } else {
        ("AI spending has reached 80% of this month's budget", "info")
    };
    let body = format!(
        "Your budget for {} is ${budget:.2}, counted across both nodes from the price table. \
         Nothing is switched off — this is a heads-up. The breakdown is on the Spend page \
         (unlock the vault to see it).",
        today.format("%B"),
    );
    state.store.notify(
        &Actor::new("system:daemon"),
        "system:daemon",
        severity,
        title,
        &body,
        Some(("#/spend", Some("open Spend"))),
        Some(&format!("llm-budget:{month}:{level}")),
    )?;
    state.store.set_consolidation_state(KEY_ALERTED, &format!("{month}:{level}"))?;
    crate::bus::touch(state, "notification.created");
    Ok(Some(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_thresholds_and_once_per_month() {
        assert_eq!(level_reached(39.99, 50.0), None);
        assert_eq!(level_reached(40.0, 50.0), Some(80));
        assert_eq!(level_reached(49.99, 50.0), Some(80));
        assert_eq!(level_reached(50.0, 50.0), Some(100));
        assert_eq!(level_reached(900.0, 50.0), Some(100));

        assert!(is_news("2026-10", 80, ""));
        assert!(!is_news("2026-10", 80, "2026-10:80"));
        assert!(is_news("2026-10", 100, "2026-10:80"));
        assert!(!is_news("2026-10", 80, "2026-10:100"), "never steps back down");
        assert!(is_news("2026-11", 80, "2026-10:100"), "a new month starts over");
        assert!(is_news("2026-10", 80, "garbage"));
    }

    // (A runtime, because announcing touches the push bus.)
    #[tokio::test]
    async fn test_check_announces_each_threshold_once() {
        use hearthloom_core::provider::Usage;
        let tmp = tempfile::tempdir().unwrap();
        let home = hearthloom_core::paths::Home::at(tmp.path());
        home.init().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(hearthloom_core::store::Store::open(&home.db_path()).unwrap()),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        // Opus input is $5 per million tokens: 8M tokens = $40.
        let spend = |millions: i64| {
            let usage = Usage { input_tokens: millions * 1_000_000, ..Default::default() };
            state
                .store
                .record_llm_usage("cloud", "extraction", None, "anthropic", "claude-opus-4-8", "high", &usage, 1, "ok")
                .unwrap();
        };
        let alerts = || state.store.notifications(false, 50).unwrap().len();

        spend(8);
        assert_eq!(check(&state).unwrap(), None, "no budget, no alert");
        set(&state, Some(50.0)).unwrap();
        assert_eq!(current(&state), Some(50.0));
        assert_eq!(check(&state).unwrap(), Some(80));
        assert_eq!(check(&state).unwrap(), None, "said once");
        // Free on-machine runs and unpriced models add nothing.
        state.store.record_local_model_run("local", "embed_backfill", "all-MiniLM-L6-v2", 9_000_000, 5).unwrap();
        let big = Usage { input_tokens: 900_000_000, ..Default::default() };
        state.store.record_llm_usage("local", "ask", None, "custom", "qwen-unpriced", "low", &big, 1, "ok").unwrap();
        assert_eq!(check(&state).unwrap(), None);
        spend(2);
        assert_eq!(check(&state).unwrap(), Some(100));
        assert_eq!(check(&state).unwrap(), None);
        assert_eq!(alerts(), 2);

        // A new figure is measured afresh; a cleared one is silent.
        set(&state, Some(60.0)).unwrap();
        assert_eq!(check(&state).unwrap(), Some(80));
        set(&state, None).unwrap();
        assert_eq!((current(&state), check(&state).unwrap()), (None, None));

        // The other node's budget wins only when it is the newer word.
        assert!(!adopt(&state, "75.00", "2000-01-01T00:00:00Z").unwrap());
        assert_eq!(current(&state), None);
        assert!(adopt(&state, "75.00", "2999-01-01T00:00:00Z").unwrap());
        assert_eq!(current(&state), Some(75.0));
    }

    #[test]
    fn test_month_bounds_roll_over_the_year() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(month_bounds(d(2026, 10, 7)), ("2026-10-01".into(), "2026-11-01".into()));
        assert_eq!(month_bounds(d(2026, 12, 31)), ("2026-12-01".into(), "2027-01-01".into()));
    }
}
