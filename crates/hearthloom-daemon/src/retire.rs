//! Reversible retirement, the daemon side (D.12; the store side and the
//! rules are in hearthloom-core store/retire.rs). Applying and undoing run
//! on the blocking pool — a whole source can be tens of thousands of
//! episodes, batched ≤200 per lock hold — one at a time per node, and the
//! consolidation cycle catches up with retirements made on another node or
//! interrupted by a restart.

use std::sync::Mutex;

use hearthloom_core::store::RetireCounts;

use crate::AppState;

/// One apply/undo at a time on this node (they are idempotent, but two
/// interleaved runs would just do the same work twice).
static RUNNING: Mutex<()> = Mutex::new(());

/// Apply one retirement here now (blocking pool).
pub async fn apply_now(state: &AppState, id: &str) -> anyhow::Result<RetireCounts> {
    let store = state.store.clone();
    let id = id.to_string();
    let counts = tokio::task::spawn_blocking(move || {
        let _one = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        store.retirement_apply(&id)
    })
    .await??;
    crate::bus::touch(state, "operational.changed");
    Ok(counts)
}

/// Undo one retirement (the synced event + this node's rows).
pub async fn undo_now(state: &AppState, actor: &str, id: &str) -> anyhow::Result<usize> {
    let store = state.store.clone();
    let (actor, id) = (hearthloom_core::store::Actor::new(actor), id.to_string());
    let restored = tokio::task::spawn_blocking(move || {
        let _one = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        store.retirement_undo(&actor, &id)
    })
    .await??;
    crate::bus::touch(state, "operational.changed");
    Ok(restored)
}

/// Apply or undo here every retirement this node hasn't caught up with.
/// Returns how many it processed.
pub async fn catch_up(state: &AppState) -> usize {
    let store = state.store.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        let _one = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        let pending = store.retirements_pending_here()?;
        for (id, undo) in &pending {
            if *undo {
                store.retirement_undo_local(id)?;
            } else {
                store.retirement_apply(id)?;
            }
        }
        Ok(pending.len())
    })
    .await;
    match result {
        Ok(Ok(0)) => 0,
        Ok(Ok(n)) => {
            tracing::info!("retirement: caught up with {n}");
            crate::bus::touch(state, "operational.changed");
            n
        }
        Ok(Err(err)) => {
            tracing::warn!("retirement catch-up failed: {err:#}");
            0
        }
        Err(err) => {
            tracing::warn!("retirement catch-up panicked: {err}");
            0
        }
    }
}
