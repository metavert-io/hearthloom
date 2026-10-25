//! Sleep-time consolidation (Letta's pattern): the background cycle where
//! embedding/LLM spend happens — never on the query path.
//!
//! v1 cycle: backfill chunk embeddings → run intents → regenerate
//! BRAIN.md. LLM-powered steps (episodic→semantic compression, self-model
//! refresh, entity dedup adjudication) hook in here in a later pass; the
//! cycle is deliberately useful with no provider configured.
//!
//! # THE PASS CONTRACT (2026-07-19 — the 1-vCPU driver-starvation fix)
//!
//! On a 1-vCPU tenant every runtime worker matters: a pass that runs
//! sync store/CPU work on a runtime thread — or holds the (unfair)
//! store mutex for a long stretch from ANY thread while HTTP handlers
//! lock it inline — parks all workers, the tokio IO driver goes
//! unpolled, the accept backlog fills, and the whole node 502s even
//! though it is "working". Every pass MUST therefore be one of:
//!
//! (a) **spawn_blocking'd whole** — pure store+CPU passes (intents,
//!     sweeps, location backfill, note settling, brain index). The
//!     runtime threads stay free; AND any internal loop over many rows
//!     must still take the store lock in SMALL batches with ≥40ms
//!     sleeps between them (the refresh_local_sources pattern) so
//!     handlers blocked on the mutex get through.
//! (b) **chunked with awaits** — passes that naturally await (LLM
//!     calls, HTTP) may run inline but must never do >~100ms of sync
//!     work between await points.
//!
//! Each pass logs its elapsed time (PassClock) — when a node grinds,
//! the hog is identified from one cycle's log, not guessed.

use std::time::Duration;

use hearthloom_core::brain_index;
use hearthloom_core::provider::{LlmProvider, Tier};
use hearthloom_core::store::{Actor, Store};
use hearthloom_core::{EpisodeDraft, EventTime, Home, IngestBatch, TimeUncertainty};
use serde_json::{json, Value};

use crate::AppState;

/// How often the cycle runs (plus once shortly after startup).
const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// Chunks embedded per batch between yield points.
const EMBED_BATCH: usize = 128;

/// Per-pass wall-clock instrumentation (see THE PASS CONTRACT above).
/// Passes run sequentially, so lap time = pass time; one log line per
/// pass per cycle names the hog when a node grinds.
struct PassClock {
    last: std::time::Instant,
}

impl PassClock {
    fn start() -> Self {
        Self { last: std::time::Instant::now() }
    }
    fn lap(&mut self, pass: &str) {
        let ms = self.last.elapsed().as_millis();
        if ms > 25 {
            tracing::info!("consolidation pass {pass}: {ms}ms");
        }
        self.last = std::time::Instant::now();
    }
}

/// A user-configurable idle-time automation (☰ → Settings). Gated by
/// consolidation_state `settings.<key>.enabled` / `.frequency` /
/// `.last_run` — absent keys mean the defaults below, so there is no
/// migration and no behavior change until a switch is touched.
pub struct Automation {
    pub key: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    pub default_frequency: &'static str,
    /// Whether the pass spends LLM tokens (shown on the Settings page;
    /// the degraded-mode rule is unchanged — no provider, no spend).
    pub llm: bool,
    /// The recommended cognition tier (how much reasoning to spend) for
    /// this pass. The user can override it per-automation on Settings via
    /// `settings.<key>.tier`; absent → this default, so no behavior change
    /// until a switch is touched. Ignored for `llm: false` passes.
    pub default_tier: Tier,
}

pub const AUTOMATIONS: &[Automation] = &[
    Automation {
        key: "daily_digest",
        label: "Daily digest",
        detail: "An LLM summary of recent activity, stored as an episode. \
                 Feeds BRAIN.md's self-model section.",
        default_frequency: "daily",
        llm: true,
        default_tier: Tier::Medium,
    },
    Automation {
        key: "chapters",
        label: "Monthly chapters",
        detail: "Short chapter headings for each month of the record — \
                 the Dial's widest-zoom labels.",
        default_frequency: "cycle",
        llm: true,
        default_tier: Tier::Medium,
    },
    Automation {
        key: "friend_activity",
        label: "Friend activity",
        detail: "Checks what connected brains share with you and \
                 notifies on changes — new shares, checklist progress, \
                 completions. Network reads only.",
        default_frequency: "cycle",
        llm: false,
        default_tier: Tier::Low,
    },
    Automation {
        key: "gmail_pull",
        label: "Gmail live pull",
        detail: "Pulls new mail from Gmail over IMAP into the live \
                 archive (searchable at need); Sent/Personal/Starred \
                 become episodes. Runs only when GMAIL_IMAP_USER and \
                 GMAIL_IMAP_PASSWORD are configured.",
        default_frequency: "cycle",
        llm: false,
        default_tier: Tier::Low,
    },
    Automation {
        key: "photos_day_finalize",
        label: "Photo day finalize",
        detail: "Seals each closed day of photo locations (Mac scan + \
                 phone stream, unioned) into its photos episode; a day \
                 that grows later re-mints and supersedes. Fully local.",
        default_frequency: "daily",
        llm: false,
        default_tier: Tier::Low,
    },
    Automation {
        key: "health_day_finalize",
        label: "Health day finalize",
        detail: "Seals each closed day of live device health samples \
                 into its health_day episode — the same aggregation the \
                 export path uses. Fully local.",
        default_frequency: "daily",
        llm: false,
        default_tier: Tier::Low,
    },
    Automation {
        key: "visits_finalize",
        label: "Visit finalize",
        detail: "Clusters each closed day's location observations — \
                 photo geotags, phone visits, calendar venues, every \
                 source — into canonical visit episodes, collapsing \
                 cross-source redundancy. A calendar event alone counts \
                 only if you allow it (Integrations → Pair a device). \
                 Fully local.",
        default_frequency: "daily",
        llm: false,
        default_tier: Tier::Low,
    },
    Automation {
        key: "semantic_pairs",
        label: "Semantic pairs",
        detail: "Cross-source nearest-neighbor episodes from existing \
                 embeddings — the Galaxy's comets. Fully local.",
        default_frequency: "cycle",
        llm: false,
        default_tier: Tier::Low,
    },
    Automation {
        key: "opportunity_scan",
        label: "Opportunity scan",
        detail: "Confirmed seeking propositions matched against new \
                 observed episodes. Embeddings only, fully local.",
        default_frequency: "cycle",
        llm: false,
        default_tier: Tier::Low,
    },
    Automation {
        key: "relationship_briefs",
        label: "Relationship briefs",
        detail: "A short evidence-cited history for each inner-circle \
                 person, refreshed when 60 days stale.",
        default_frequency: "cycle",
        llm: true,
        default_tier: Tier::Medium,
    },
    Automation {
        key: "lint",
        label: "Record lint",
        detail: "Report-only sweep for contradictions, superseded claims, \
                 orphan entities and coverage gaps — findings go to Tend.",
        default_frequency: "weekly",
        llm: true,
        default_tier: Tier::Medium,
    },
    Automation {
        key: "topic_pages",
        label: "Living topic pages",
        detail: "Hub notes the consolidator drafts and revises — \"what I \
                 know about X\", with citations. Revisions go through Tend.",
        default_frequency: "weekly",
        llm: true,
        default_tier: Tier::High,
    },
    Automation {
        key: "weekly_review",
        label: "Weekly review",
        detail: "One rollup episode per week: the important new ideas, \
                 old↔new connections, hubs forming, what to feed next.",
        default_frequency: "weekly",
        llm: true,
        default_tier: Tier::High,
    },
];

/// Frequency names → minimum hours between runs (0 = every cycle).
pub const FREQUENCIES: &[(&str, f64)] = &[
    ("cycle", 0.0),
    ("daily", 22.0),
    ("weekly", 6.5 * 24.0),
    ("monthly", 29.0 * 24.0),
];

/// Current (enabled, frequency) for an automation, defaults applied.
pub fn automation_setting(store: &Store, key: &str) -> (bool, String) {
    let enabled = store
        .consolidation_state(&format!("settings.{key}.enabled"))
        .ok()
        .flatten()
        .map(|v| v != "false")
        .unwrap_or(true);
    let default = AUTOMATIONS
        .iter()
        .find(|a| a.key == key)
        .map(|a| a.default_frequency)
        .unwrap_or("cycle");
    let frequency = store
        .consolidation_state(&format!("settings.{key}.frequency"))
        .ok()
        .flatten()
        .filter(|f| FREQUENCIES.iter().any(|(name, _)| name == f))
        .unwrap_or_else(|| default.to_string());
    (enabled, frequency)
}

/// The cognition tier an automation should spend at, config-resolved.
/// Reads `settings.<key>.tier` (low|medium|high); an absent or unparseable
/// value falls back to the pass's `default_tier`. This is the single point
/// each LLM pass calls instead of a hardcoded `Tier`, so Settings drives
/// spend with no behavior change until a user picks a level.
pub fn automation_tier(store: &Store, key: &str) -> Tier {
    let default = AUTOMATIONS
        .iter()
        .find(|a| a.key == key)
        .map(|a| a.default_tier)
        .unwrap_or(Tier::Medium);
    store
        .consolidation_state(&format!("settings.{key}.tier"))
        .ok()
        .flatten()
        .and_then(|v| Tier::parse(&v))
        .unwrap_or(default)
}

/// Should this automation run this cycle? Slightly-short thresholds
/// (22h for daily, 6.5d for weekly) keep a cadence from drifting later
/// every period when cycles don't land exactly on the boundary.
fn automation_due(store: &Store, key: &str) -> bool {
    let (enabled, frequency) = automation_setting(store, key);
    if !enabled {
        return false;
    }
    let hours = FREQUENCIES
        .iter()
        .find(|(name, _)| *name == frequency)
        .map(|(_, h)| *h)
        .unwrap_or(0.0);
    if hours == 0.0 {
        return true;
    }
    match store
        .state_age_hours(&format!("settings.{key}.last_run"))
        .ok()
        .flatten()
    {
        Some(age) => age >= hours,
        None => true,
    }
}

/// Stamp `last_ran_at` — the DISPLAY timestamp shown on the Settings page,
/// updated by BOTH scheduled and manual runs.
fn stamp_last_ran(store: &Store, key: &str) {
    let _ = store.set_consolidation_state(
        &format!("settings.{key}.last_ran_at"),
        &hearthloom_core::time::now_utc(),
    );
}

/// Stamp an automation's last SCHEDULED run (the schedule anchor, so a
/// failed pass retries next cycle instead of waiting out its period). Also
/// updates the display timestamp. A MANUAL run deliberately does NOT call
/// this — it stamps only `last_ran_at`, so it never resets the schedule.
fn automation_ran(store: &Store, key: &str) {
    let _ = store.set_consolidation_state(
        &format!("settings.{key}.last_run"),
        &hearthloom_core::time::now_utc(),
    );
    stamp_last_ran(store, key);
}

/// Claim a scheduled automation: `Some(guard)` when it's due AND not already
/// in flight (a manual trigger may hold it — then `None`, and the pass waits
/// for next cycle). Hold the guard until the pass finishes, then call
/// `automation_ran`. This is the single-flight point the manual endpoint
/// also goes through, so scheduled + manual can never overlap.
fn due_claim(state: &AppState, key: &str) -> Option<crate::AutomationGuard> {
    if !automation_due(&state.store, key) {
        return None;
    }
    // The paid thinking passes run only where this node's role says so
    // (multi-node M6: on / off / auto-lease) — one node is enough.
    if AUTOMATIONS.iter().any(|a| a.key == key && a.llm) && !crate::agents::thinking_here(state) {
        return None;
    }
    state.try_start_automation(key)
}

/// Run ONE automation by key — the manual-trigger path (POST
/// /v1/settings/automations/{key}/run). Bypasses the enabled/frequency gate
/// (a manual run is an explicit exception) and stamps only `last_ran_at`.
/// The caller holds the single-flight guard for the task's lifetime.
pub async fn run_automation(state: &AppState, key: &str, actor: &Actor) -> anyhow::Result<Value> {
    let auto = AUTOMATIONS
        .iter()
        .find(|a| a.key == key)
        .ok_or_else(|| anyhow::anyhow!("unknown automation: {key}"))?;
    if auto.llm && state.llm.is_none() {
        anyhow::bail!("this automation needs an LLM provider, which isn't configured");
    }
    let llm = state.llm.clone();
    let out = match key {
        "daily_digest" => {
            let w = semantic_summary(&state.store, &state.home, llm.unwrap().as_ref()).await?;
            json!({ "digest_written": w })
        }
        "chapters" => {
            let n = write_chapters(state, llm.unwrap().as_ref(), actor).await?;
            json!({ "chapters_written": n })
        }
        "opportunity_scan" => json!({ "opportunities_found": state.store.scan_opportunities(actor)? }),
        "friend_activity" => {
            json!({ "notifications": crate::network_out::friend_activity_scan(state) })
        }
        "photos_day_finalize" => {
            let st = state.clone();
            let (minted, superseded) =
                tokio::task::spawn_blocking(move || crate::live_photos::finalize_photo_days(&st))
                    .await??;
            json!({ "days_minted": minted, "days_superseded": superseded })
        }
        "gmail_pull" => {
            let st = state.clone();
            let (fetched, eps, more) =
                tokio::task::spawn_blocking(move || crate::gmail_pull::pull_once(&st)).await??;
            json!({ "messages_fetched": fetched, "episodes_new": eps, "more_pending": more })
        }
        "health_day_finalize" => {
            let st = state.clone();
            let (days, eps) =
                tokio::task::spawn_blocking(move || crate::live_health::finalize_health_days(&st))
                    .await??;
            json!({ "days_finalized": days, "episodes_new": eps })
        }
        "visits_finalize" => {
            let st = state.clone();
            let (days, minted, superseded) =
                tokio::task::spawn_blocking(move || crate::visits::finalize_visits(&st)).await??;
            json!({ "days": days, "visits_minted": minted, "visits_superseded": superseded })
        }
        "semantic_pairs" => {
            let store = state.store.clone();
            let n = tokio::task::spawn_blocking(move || store.refresh_semantic_pairs(80)).await??;
            json!({ "semantic_pairs": n })
        }
        "relationship_briefs" => {
            let n = write_relationship_briefs(state, llm.unwrap().as_ref(), actor).await?;
            json!({ "relationship_briefs": n })
        }
        "lint" => json!({ "lint_findings": run_lint(state, actor).await? }),
        "topic_pages" => {
            let n = tend_topic_pages(state, llm.unwrap().as_ref(), actor).await?;
            json!({ "topic_pages_proposed": n })
        }
        "weekly_review" => {
            let w = write_weekly_review(state, llm.unwrap().as_ref(), actor).await?;
            json!({ "weekly_review_written": w })
        }
        _ => anyhow::bail!("unknown automation: {key}"),
    };
    stamp_last_ran(&state.store, key);
    Ok(out)
}

pub async fn run_loop(state: AppState) {
    tokio::time::sleep(STARTUP_DELAY).await;
    loop {
        match consolidate(&state).await {
            Ok(summary) => tracing::info!("consolidation: {summary}"),
            Err(err) => tracing::warn!("consolidation failed: {err}"),
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

/// Re-ingest local sources at most this often.
const REFRESH_EVERY: Duration = Duration::from_secs(20 * 60 * 60);
/// Scope-classification budget per cycle (LLM calls are batched 40/call).
const CLASSIFY_PER_CYCLE: usize = 120;
const CLASSIFY_BATCH: usize = 40;

/// One consolidation cycle. Also callable on demand via POST /v1/consolidate.
pub async fn consolidate(state: &AppState) -> anyhow::Result<Value> {
    // LLM role rule (2026-07-12): the always-on node is the consolidator.
    // A LOCAL node with a healthy, key-holding peer delegates all
    // idle-time LLM passes (their outputs sync back as episodes with
    // deterministic ids); it takes the role back automatically when the
    // peer goes unseen for 24h or loses its key. User-invoked Ask is
    // untouched — that spends wherever the user asked.
    let delegated = crate::sync::llm_delegated_to_peer(state);
    let shadow;
    let state: &AppState = if delegated && state.llm.is_some() {
        tracing::info!("consolidation: LLM passes delegated to the always-on peer");
        shadow = AppState { llm: None, ..state.clone() };
        &shadow
    } else {
        state
    };
    let actor = Actor::new("agent:consolidation");
    let mut clock = PassClock::start();

    // 0. Keep the brain current: re-ingest known local sources (idempotent
    // adapters; new material only lands once) at most every 20h.
    let refreshed = match refresh_local_sources(state, &actor).await {
        Ok(refreshed) => refreshed,
        Err(err) => {
            tracing::warn!("source refresh failed: {err:#}");
            Value::Null
        }
    };
    clock.lap("refresh_sources");

    // 0.5 Location index backfill FIRST — it costs seconds (photo GPS
    // lines parse in core; workout GPX starts read here, since the
    // daemon knows the archive mount) and must never queue behind the
    // hours-long embedding backfill.
    let locations = index_locations(state, &actor).await.unwrap_or_else(|err| {
        tracing::warn!("location backfill failed: {err:#}");
        0
    });
    clock.lap("index_locations");

    // 1. Embedding backfill (CPU-bound → blocking pool, batched).
    let embed_started = std::time::Instant::now();
    let mut embedded = 0usize;
    if let Some(embedder) = state.embedder.clone() {
        loop {
            let store = state.store.clone();
            let embedder = embedder.clone();
            let batch: usize = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
                let pending = store.chunks_needing_embedding(EMBED_BATCH)?;
                for (chunk_id, text) in &pending {
                    let vector = embedder.embed(text)?;
                    store.set_chunk_embedding(chunk_id, &vector)?;
                }
                Ok(pending.len())
            })
            .await??;
            embedded += batch;
            if batch < EMBED_BATCH {
                break;
            }
            // Native embedding is fast enough to re-grab the (unfair)
            // store mutex before any waiting HTTP handler wins it —
            // hours of backfill would freeze the dashboard. A breath
            // between batches keeps reads flowing (same rule as every
            // bulk-apply loop; see the mbox/health/refresh notes).
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        }
    }
    // The embedder runs on this machine and costs nothing, but it is a
    // model run like any other: one ledger row per backfill that did work.
    if embedded > 0 {
        if let Err(err) = state.store.record_local_model_run(
            crate::monitor::node_kind(),
            "embed_backfill",
            hearthloom_core::embeddings::MODEL_NAME,
            embedded,
            embed_started.elapsed().as_millis() as u64,
        ) {
            tracing::warn!("llm_usage record failed (embed_backfill): {err:#}");
        }
    }
    let coverage = state.store.embedding_coverage()?;
    clock.lap("embed_backfill");

    // 1.1 ANN-index backfill (vec_chunks ← chunks.embedding): pure store
    // work, ≤500/batch with a breath between batches (THE PASS CONTRACT).
    // A no-op once the index covers every embedded chunk — dual-write on
    // set_chunk_embedding keeps it that way.
    let mut vec_indexed = 0usize;
    if !state.store.vec_index_ready().unwrap_or(false) {
        loop {
            let store = state.store.clone();
            let batch = tokio::task::spawn_blocking(move || store.vec_index_backfill_batch(500))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                .unwrap_or_else(|err| {
                    tracing::warn!("vec index backfill failed: {err:#}");
                    0
                });
            vec_indexed += batch;
            if batch < 500 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        }
    }
    clock.lap("vec_index");

    // 1.5 Intent decomposition: split each new goal into a few standing
    // queries (one LLM call), embedded for the semantic match leg. Runs
    // before the check so a fresh goal matches this very cycle.
    let decomposed = match &state.llm {
        Some(llm) => decompose_intents(state, llm.as_ref(), &actor)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!("intent decomposition failed: {err:#}");
                0
            }),
        None => 0,
    };
    clock.lap("decompose_intents");

    // 2. Standing queries (pure store+CPU → blocking pool; a failure —
    // e.g. a malformed backlog — skips the pass, never the cycle).
    let fired = {
        let store = state.store.clone();
        let intents_actor = actor.clone();
        tokio::task::spawn_blocking(move || store.run_intents(&intents_actor))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .unwrap_or_else(|err| {
                tracing::warn!("standing queries failed: {err:#}");
                Vec::new()
            })
    };
    clock.lap("run_intents");

    // 3. Episodic→semantic compression: an LLM digest of recent activity,
    // stored as an agent-trust episode (never editing high-trust rows).
    // BRAIN.md's self-model section reads the latest digest.
    let summary_written = match (&state.llm, due_claim(state, "daily_digest")) {
        (Some(llm), Some(_g)) => {
            match semantic_summary(&state.store, &state.home, llm.as_ref()).await {
                Ok(written) => {
                    automation_ran(&state.store, "daily_digest");
                    written
                }
                Err(err) => {
                    tracing::warn!("semantic summary failed: {err}");
                    false
                }
            }
        }
        _ => false,
    };
    clock.lap("daily_digest");

    // 3b. Idle-time scope refinement: coarse account-level labels get a
    // per-episode LLM verdict (work vs personal), mutation-logged.
    let classified = match &state.llm {
        Some(llm) => match classify_scopes(state, llm.as_ref(), &actor).await {
            Ok(counts) => counts,
            Err(err) => {
                tracing::warn!("scope classification failed: {err:#}");
                (0, 0)
            }
        },
        None => (0, 0),
    };
    clock.lap("classify_scopes");

    // 3c. Entity extraction: mine Person/Org/Project/Place mentions out
    // of prose episodes (LazyGraphRAG discipline: promoted content only,
    // capped per cycle; the graph grows where evidence accumulates).
    let extracted = match &state.llm {
        Some(llm) => match extract_entities(state, llm.as_ref(), &actor).await {
            Ok(counts) => counts,
            Err(err) => {
                tracing::warn!("entity extraction failed: {err:#}");
                (0, 0)
            }
        },
        None => (0, 0),
    };
    // fresh interpretation → hint the peer so verdicts replicate in
    // seconds instead of at the tick (poll reconciles as always)
    if extracted.0 > 0 {
        crate::bus::touch(state, "extraction.applied");
    }
    clock.lap("extract_entities");


    // 3.9 Chapter headings: idle-time LLM titles per month — the
    // Dial's widest-zoom label tier. Land as kind=chapter episodes
    // (source consolidation, Month uncertainty); inputs hard-exclude
    // private (cloud-pass rule). Backfills a couple of dozen months
    // per cycle.
    let chapters = match (&state.llm, due_claim(state, "chapters")) {
        (Some(llm), Some(_g)) => {
            match write_chapters(state, llm.as_ref(), &actor).await {
                Ok(n) => {
                    automation_ran(&state.store, "chapters");
                    n
                }
                Err(err) => {
                    tracing::warn!("chapter headings failed: {err:#}");
                    0
                }
            }
        }
        _ => 0,
    };
    clock.lap("chapters");

    // 3.93 Cascades: expand this cycle's fresh intent matches along the
    // four discovery dimensions and land each as a kind=cascade episode.
    let cascades = match &state.llm {
        Some(llm) => expand_cascades(state, llm.as_ref(), &actor)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!("cascade expansion failed: {err:#}");
                0
            }),
        None => 0,
    };
    clock.lap("cascades");

    // 3.95 Self-model growth: propose evidence-cited propositions about
    // who the user is (origin=inferred, status=proposed → a Tend yes/no).
    // Inputs are digests + chapter headings — never private material.
    let propositions = match &state.llm {
        Some(llm) => grow_self_model(state, llm.as_ref(), &actor)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!("self-model growth failed: {err:#}");
                0
            }),
        None => 0,
    };
    clock.lap("self_model");

    // 3.97 Opportunity scan: confirmed seeking × new observed episodes,
    // embeddings only, fully local — no LLM on this path (store+CPU →
    // blocking pool).
    let opportunities = match due_claim(state, "opportunity_scan") {
        Some(_g) => {
            let store = state.store.clone();
            let scan_actor = actor.clone();
            match tokio::task::spawn_blocking(move || store.scan_opportunities(&scan_actor))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            {
                Ok(n) => {
                    automation_ran(&state.store, "opportunity_scan");
                    n
                }
                Err(err) => {
                    tracing::warn!("opportunity scan failed: {err:#}");
                    0
                }
            }
        }
        None => 0,
    };
    clock.lap("opportunity_scan");

    // 3.955 Workout metric backfill: codi-era workout episodes carry
    // payload JSON in-body; derive their metric_points (local, cheap,
    // idempotent — feeds Pulse and the exercise autocomplete).
    let workout_metrics = {
        let store = state.store.clone();
        let home = state.home.clone();
        tokio::task::spawn_blocking(move || store.backfill_workout_metrics(&home, 100))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .unwrap_or_else(|err| {
                tracing::warn!("workout metric backfill failed: {err:#}");
                0
            })
    };
    clock.lap("workout_metrics");

    // 3.96 Settle notes: any active note quiet for 48h gets its current
    // version snapshotted into the record (local, no LLM).
    let notes_settled = {
        let store = state.store.clone();
        let home = state.home.clone();
        let settle_actor = actor.clone();
        tokio::task::spawn_blocking(move || store.settle_notes(&settle_actor, &home, 48))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .unwrap_or_else(|err| {
                tracing::warn!("note settle failed: {err:#}");
                0
            })
    };

    // Back-fill dates onto any dateless loom notes so accepted topic-page
    // memories surface as recent in Notes instead of sinking to the bottom.
    let _ = state.store.date_undated_loom_notes();
    clock.lap("settle_notes");

    // 3.97 Galaxy comets: semantic pairs among the record's notable
    // episodes, from EXISTING chunk embeddings (content vectors only —
    // the name-embedding lesson). Cross-source pairs only; full
    // replace; the /v1/galaxy view just reads the table (law #3).
    let semantic_pairs = match due_claim(state, "semantic_pairs") {
        Some(_g) => {
            let store = state.store.clone();
            match tokio::task::spawn_blocking(move || store.refresh_semantic_pairs(80))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            {
                Ok(n) => {
                    automation_ran(&state.store, "semantic_pairs");
                    n
                }
                Err(err) => {
                    tracing::warn!("semantic pair refresh failed: {err:#}");
                    0
                }
            }
        }
        None => 0,
    };
    clock.lap("semantic_pairs");

    // 3.971 Health day finalize: seal each closed day of live device
    // samples (v44 health_sample) into its health_day episode via the
    // SAME apple_health::DayTable the export path uses — live and
    // export converge on day:YYYY-MM-DD by construction. Pure
    // store+CPU, spawn_blocking'd whole (THE PASS CONTRACT).
    let health_days = match due_claim(state, "health_day_finalize") {
        Some(_g) => {
            let st = state.clone();
            match tokio::task::spawn_blocking(move || crate::live_health::finalize_health_days(&st))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            {
                Ok((days, _eps)) => {
                    automation_ran(&state.store, "health_day_finalize");
                    days
                }
                Err(err) => {
                    tracing::warn!("health day finalize failed: {err:#}");
                    0
                }
            }
        }
        None => 0,
    };
    if health_days > 0 {
        tracing::info!("health day finalize: {health_days} day(s) sealed");
    }
    clock.lap("health_day_finalize");

    // 3.9715 Photo day finalize: photos#DAY from the point UNION (Mac +
    // phone), re-minting under the supersession pattern when a day grows.
    let photo_days = match due_claim(state, "photos_day_finalize") {
        Some(_g) => {
            let st = state.clone();
            match tokio::task::spawn_blocking(move || crate::live_photos::finalize_photo_days(&st))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            {
                Ok((m, s)) => {
                    automation_ran(&state.store, "photos_day_finalize");
                    m + s
                }
                Err(err) => {
                    tracing::warn!("photo day finalize failed: {err:#}");
                    0
                }
            }
        }
        None => 0,
    };
    if photo_days > 0 {
        tracing::info!("photo day finalize: {photo_days} day(s) minted/refreshed");
    }
    clock.lap("photos_day_finalize");

    // 3.97157 Visit finalize (v51 Part A): cluster each closed day's
    // location observations — photo geotags, CLVisit dwells, calendar
    // venues, ALL sources — into canonical visit episodes, deduping the
    // cross-source redundancy at cluster time. Pure store+CPU,
    // spawn_blocking'd whole (THE PASS CONTRACT).
    let visit_days = match due_claim(state, "visits_finalize") {
        Some(_g) => {
            let st = state.clone();
            match tokio::task::spawn_blocking(move || crate::visits::finalize_visits(&st))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            {
                Ok((days, minted, superseded)) => {
                    automation_ran(&state.store, "visits_finalize");
                    if minted + superseded > 0 {
                        tracing::info!(
                            "visit finalize: {days} day(s), {minted} minted, {superseded} superseded"
                        );
                    }
                    days
                }
                Err(err) => {
                    tracing::warn!("visit finalize failed: {err:#}");
                    0
                }
            }
        }
        None => 0,
    };
    let _ = visit_days;
    clock.lap("visits_finalize");

    // 3.97158 Contact apply drain (v51): peer-synced or handler-missed
    // contact cards become Person entities + contact_card episodes.
    // Cheap no-op when nothing is pending.
    {
        let st = state.clone();
        match tokio::task::spawn_blocking(move || {
            crate::live_ingest::apply_pending_contacts(&st, 500)
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r)
        {
            Ok(n) if n > 0 => tracing::info!("contact apply: {n} card(s) resolved"),
            Ok(_) => {}
            Err(err) => tracing::warn!("contact apply failed: {err:#}"),
        }
    }
    clock.lap("contacts_apply");

    // 3.97155 Gmail live pull (Tier 3 federation): new mail over IMAP
    // into the live mbox + sidecar; Core strata materialize as
    // episodes. A silent no-op until GMAIL_IMAP_* creds are configured.
    if let Some(_g) = due_claim(state, "gmail_pull") {
        let st = state.clone();
        match tokio::task::spawn_blocking(move || crate::gmail_pull::pull_once(&st))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
        {
            Ok((fetched, eps, more)) => {
                if fetched > 0 {
                    tracing::info!(
                        "gmail pull: {fetched} message(s), {eps} episode(s){}",
                        if more { ", more pending" } else { "" }
                    );
                }
                automation_ran(&state.store, "gmail_pull");
            }
            Err(err) => tracing::warn!("gmail pull failed: {err:#}"),
        }
    }
    clock.lap("gmail_pull");

    // 3.9716 Friend activity: notify on changes to what connected
    // brains share with us (network reads; block_in_place inside).
    if let Some(_g) = due_claim(state, "friend_activity") {
        let n = crate::network_out::friend_activity_scan(state);
        if n > 0 {
            tracing::info!("friend activity: {n} notification(s)");
        }
        automation_ran(&state.store, "friend_activity");
    }
    clock.lap("friend_activity");

    // 3.972 Note neighbors: the resonant lane of the note connections
    // panel — nearest notes from their settled snapshots' existing
    // chunk embeddings. Local, cheap, full replace.
    let note_neighbors = {
        let store = state.store.clone();
        tokio::task::spawn_blocking(move || store.refresh_note_neighbors(4))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .unwrap_or_else(|err| {
                tracing::warn!("note neighbor refresh failed: {err:#}");
                0
            })
    };
    clock.lap("note_neighbors");

    // 3.973 Machine-address sweep: noreply/bounce/random-token senders
    // are never people — reclassify Person → Service (rulebook in
    // crate::machine; new ingests are caught at the chokepoint, this
    // covers history and rulebook growth).
    let machine_swept = {
        let store = state.store.clone();
        let sweep_actor = actor.clone();
        tokio::task::spawn_blocking(move || store.sweep_machine_persons(&sweep_actor))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .unwrap_or_else(|err| {
                tracing::warn!("machine sweep failed: {err:#}");
                0
            })
    };
    if machine_swept > 0 {
        tracing::info!("machine sweep: {machine_swept} senders reclassified Person → Service");
    }
    clock.lap("machine_sweep");

    // 3.975 Case-duplicate sweep: "Ben ≟ ben" is not a question — merge
    // pending reviews whose names differ only by case and whose
    // identifying attributes don't conflict (local, reversible).
    let case_merged = {
        let store = state.store.clone();
        let sweep_actor = actor.clone();
        tokio::task::spawn_blocking(move || store.sweep_case_duplicates(&sweep_actor))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .unwrap_or_else(|err| {
                tracing::warn!("case-duplicate sweep failed: {err:#}");
                0
            })
    };
    clock.lap("case_sweep");

    // 3.964 The Me detector: the record's owner is self-evident
    // (dominant multi-source presence) — propose kind='me' once,
    // deterministically; the human confirms in the directory.
    let me_proposed = {
        let store = state.store.clone();
        let me_actor = actor.clone();
        tokio::task::spawn_blocking(move || store.propose_me(&me_actor))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .unwrap_or_else(|err| {
                tracing::warn!("me detection failed: {err:#}");
                false
            })
    };
    if me_proposed {
        tracing::info!("relationships: proposed the record's owner (kind=me)");
    }
    clock.lap("propose_me");

    // 3.965 Relationship prediction: how does the user relate to the
    // people showing up in their life? One LLM call per cycle over the
    // best-evidenced unclassified persons — proposals await the human
    // in the Relationships directory (origin never laundered).
    let relationships_predicted = match &state.llm {
        Some(llm) => predict_relationships(state, llm.as_ref(), &actor)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!("relationship prediction failed: {err:#}");
                0
            }),
        None => 0,
    };
    clock.lap("predict_relationships");

    // 3.966 Relationship briefs: a short evidence-cited history for each
    // inner-circle person, refreshed when 60 days stale.
    let briefs_written = match (&state.llm, due_claim(state, "relationship_briefs")) {
        (Some(llm), Some(_g)) => {
            match write_relationship_briefs(state, llm.as_ref(), &actor).await {
                Ok(n) => {
                    automation_ran(&state.store, "relationship_briefs");
                    n
                }
                Err(err) => {
                    tracing::warn!("relationship briefs failed: {err:#}");
                    0
                }
            }
        }
        _ => 0,
    };
    clock.lap("relationship_briefs");

    // 3.98 Merge candidates: new Person names near an existing Person's
    // name-embedding land in the review queue (the Tend yes/no; local
    // embeddings, no LLM, bias to under-merge preserved — this only
    // ever SUGGESTS).
    let merge_candidates = propose_merge_candidates(state)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!("merge-candidate scan failed: {err:#}");
            0
        });
    clock.lap("merge_candidates");

    // 3.99 The record lint: report-only findings → Tend (weekly by
    // default; ☰ → Settings). Structural checks (orphans, gaps) are
    // SQL; the contradiction/superseded check spends one LLM call over
    // embedding-shortlisted note pairs. Never deletes anything.
    let lint_findings = match due_claim(state, "lint") {
        Some(_g) => match run_lint(state, &actor).await {
            Ok(n) => {
                automation_ran(&state.store, "lint");
                n
            }
            Err(err) => {
                tracing::warn!("record lint failed: {err:#}");
                0
            }
        },
        None => 0,
    };
    clock.lap("lint");

    // 3.995 Weekly review: one rollup episode per week — the important
    // new ideas, old↔new connections, hubs forming, what to feed next.
    let weekly_review = match (&state.llm, due_claim(state, "weekly_review")) {
        (Some(llm), Some(_g)) => {
            match write_weekly_review(state, llm.as_ref(), &actor).await {
                Ok(written) => {
                    automation_ran(&state.store, "weekly_review");
                    written
                }
                Err(err) => {
                    tracing::warn!("weekly review failed: {err:#}");
                    false
                }
            }
        }
        _ => false,
    };
    clock.lap("weekly_review");

    // 3.9955 Retirement (D.12): finish applying/undoing any retirement this
    // node hasn't (one made elsewhere, or a crash mid-way), then rewrite
    // the summaries a retirement ticked — on the thinking node only.
    let retirements_caught_up = crate::retire::catch_up(state).await;
    let summaries_rewritten = match &state.llm {
        Some(llm) if crate::agents::thinking_here(state) => {
            match rewrite_summaries(state, llm.as_ref(), &actor).await {
                Ok(n) => n,
                Err(err) => {
                    tracing::warn!("summary rewrites failed: {err:#}");
                    0
                }
            }
        }
        _ => 0,
    };
    clock.lap("retirements");

    // 3.996 Living topic pages: consolidator-drafted hub notes,
    // revisions proposed through Tend (quarantine diff cards).
    let topic_pages = match (&state.llm, due_claim(state, "topic_pages")) {
        (Some(llm), Some(_g)) => {
            match tend_topic_pages(state, llm.as_ref(), &actor).await {
                Ok(n) => {
                    automation_ran(&state.store, "topic_pages");
                    n
                }
                Err(err) => {
                    tracing::warn!("topic pages failed: {err:#}");
                    0
                }
            }
        }
        _ => 0,
    };
    clock.lap("topic_pages");

    // 4. Brain index (byte-stable; only rewrite when content changed).
    // Store reads + fs → blocking pool.
    let changed = {
        let store = state.store.clone();
        let path = state.home.brain_index_path();
        tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            let content = brain_index::generate(&store)?;
            let changed =
                std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str());
            if changed {
                std::fs::write(&path, &content)?;
            }
            Ok(changed)
        })
        .await??
    };
    clock.lap("brain_index");

    // 5. Monthly spend budget: announce a newly crossed threshold.
    {
        let budget_state = state.clone();
        match tokio::task::spawn_blocking(move || crate::budget::check(&budget_state)).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => tracing::warn!("budget check failed: {err:#}"),
            Err(err) => tracing::warn!("budget check failed: {err}"),
        }
    }
    clock.lap("budget_check");

    Ok(json!({
        "chunks_embedded": embedded,
        "embedding_coverage": {"embedded": coverage.0, "total": coverage.1},
        "vec_indexed": vec_indexed,
        "intents_fired": fired
            .iter()
            .map(|(name, count)| json!({"intent": name, "new_matches": count}))
            .collect::<Vec<_>>(),
        "semantic_summary_written": summary_written,
        "llm_configured": state.llm.is_some(),
        "sources_refreshed": refreshed,
        "scopes_classified": classified.0,
        "scopes_relabeled": classified.1,
        "episodes_mined": extracted.0,
        "entities_linked": extracted.1,
        "brain_index_updated": changed,
        "locations_indexed": locations,
        "chapters_written": chapters,
        "propositions_proposed": propositions,
        "intents_decomposed": decomposed,
        "cascades_written": cascades,
        "opportunities_found": opportunities,
        "merge_candidates_queued": merge_candidates,
        "case_duplicates_merged": case_merged,
        "notes_settled": notes_settled,
        "workout_metrics_backfilled": workout_metrics,
        "relationships_predicted": relationships_predicted,
        "relationship_briefs": briefs_written,
        "semantic_pairs": semantic_pairs,
        "note_neighbors": note_neighbors,
        "lint_findings": lint_findings,
        "weekly_review_written": weekly_review,
        "retirements_caught_up": retirements_caught_up,
        "summaries_rewritten": summaries_rewritten,
        "topic_pages_proposed": topic_pages,
    }))
}

/// Cascades expanded per cycle (each is ~40 lines of prompt; all seeds
/// share ONE LLM call).
const CASCADES_PER_CYCLE: usize = 6;

async fn expand_cascades(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<usize> {
    let seeds = state.store.uncascaded_matches(CASCADES_PER_CYCLE)?;
    if seeds.is_empty() {
        return Ok(0);
    }
    let mut contexts = Vec::with_capacity(seeds.len());
    for seed in &seeds {
        contexts.push(state.store.cascade_context(&seed.episode_id, &seed.day)?);
    }

    let system = "You expand matches from a personal knowledge base into short, useful \
        briefs. A standing goal matched a new episode; around it you get related people \
        and their connections, nearby events in time, episodes at the same place, and \
        textual neighbors. Write what genuinely connects — why this match matters for \
        the goal and which threads are worth pulling. Concrete, 60-120 words, no \
        headers, no flattery. Respond ONLY with a JSON object mapping each seed index \
        to {\"title\": \"5-9 word heading\", \"body\": \"the brief\"}. Omit an index \
        when the match is trivial noise.";
    let mut prompt = String::from("Seeds:\n\n");
    for (i, (seed, ctx)) in seeds.iter().zip(&contexts).enumerate() {
        prompt.push_str(&format!(
            "index: {i}\ngoal: {} ({})\nmatched episode [{}] {} — {} ({})\n",
            seed.intent_name, seed.intent_query, seed.episode_id, seed.day, seed.title, seed.kind
        ));
        let list = |label: &str, lines: &[String]| {
            if lines.is_empty() {
                String::new()
            } else {
                format!("{label}:\n{}\n", lines.join("\n"))
            }
        };
        let fmt = |items: &[(String, String)]| -> Vec<String> {
            items.iter().map(|(id, line)| format!("[{id}] {line}")).collect()
        };
        prompt.push_str(&list("entities", &ctx.entities));
        prompt.push_str(&list("connections", &ctx.entity_neighbors));
        prompt.push_str(&list("same weeks", &fmt(&ctx.temporal)));
        prompt.push_str(&list("same place", &fmt(&ctx.place)));
        prompt.push_str(&list("textual neighbors", &fmt(&ctx.semantic)));
        prompt.push('\n');
    }
    let response = crate::llm::call(&state.store, llm, "cascades", None, Tier::High, system, &prompt, 2500).await?.text;
    let json_text = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let briefs: serde_json::Map<String, Value> = serde_json::from_str(json_text)
        .map_err(|e| anyhow::anyhow!("unparseable cascade response: {e}"))?;

    let mut episodes = Vec::new();
    for (i, (seed, ctx)) in seeds.iter().zip(&contexts).enumerate() {
        let Some(brief) = briefs.get(&i.to_string()) else {
            continue; // judged noise — the match stays, just unexpanded
        };
        let (Some(title), Some(body)) = (
            brief.get("title").and_then(|v| v.as_str()),
            brief.get("body").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let mut cited: Vec<&str> = vec![seed.episode_id.as_str()];
        cited.extend(
            ctx.temporal
                .iter()
                .chain(&ctx.place)
                .chain(&ctx.semantic)
                .map(|(id, _)| id.as_str()),
        );
        episodes.push(hearthloom_core::EpisodeDraft {
            source: "consolidation".into(),
            source_ref: format!("cascade:{}:{}", seed.intent_id, seed.episode_id),
            kind: "cascade".into(),
            title: title.trim().to_string(),
            body: format!(
                "goal: {}\nmatched: [{}] {}\n\n{}\n\ndrawn from:\n{}",
                seed.intent_name,
                seed.episode_id,
                seed.title,
                body.trim(),
                cited.join("\n"),
            ),
            event_time: hearthloom_core::EventTime {
                utc: Some(format!("{}T12:00:00Z", seed.day)),
                tz_offset_minutes: None,
                uncertainty: hearthloom_core::TimeUncertainty::Day,
            },
            trust: "agent".into(),
            scope: Some(seed.scope.clone()),
            entities: vec![],
            edges: vec![],
        });
    }
    if episodes.is_empty() {
        return Ok(0);
    }
    let report = state.store.apply_batch(
        &state.home,
        &hearthloom_core::IngestBatch {
            actor: actor.actor.clone(),
            session_id: None,
            episodes,
        },
    )?;
    if report.episodes_new > 0 {
        tracing::info!("cascades: {} written", report.episodes_new);
    }
    Ok(report.episodes_new)
}

/// Immediate setup for a freshly created goal — creating one is a
/// deliberate user act (the Ask-style spend exception): decompose it
/// into standing queries NOW, scan the existing record so it knows
/// what's already true, and run the forward watch once. Spawned from
/// the add-goal endpoint; failures degrade to next-idle-cycle setup.
pub async fn setup_goal(state: AppState, intent_id: String) {
    let actor = Actor::new("agent:goal-setup");
    if let Some(llm) = &state.llm {
        if let Err(err) = decompose_intents(&state, llm.as_ref(), &actor).await {
            tracing::warn!("goal setup decomposition failed: {err:#}");
        }
    }
    let store = state.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        store.run_intent_backlog(&actor, &intent_id, 200)
    })
    .await;
    match result {
        Ok(Ok(n)) => tracing::info!("goal setup: {n} backlog matches"),
        Ok(Err(err)) => tracing::warn!("goal backlog scan failed: {err:#}"),
        Err(err) => tracing::warn!("goal setup task failed: {err}"),
    }
}

/// Split undecomposed intents into 3-5 standing queries each (one LLM
/// call for the batch), embed them, mark decomposed. With no provider the
/// intent still works — FTS on its own query — so this is pure upgrade.
async fn decompose_intents(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<usize> {
    let pending = state.store.intents_needing_decomposition(12)?;
    if pending.is_empty() {
        return Ok(0);
    }
    let system = "You decompose personal monitoring goals into standing search queries. \
        For each goal, produce 3-5 short queries (1-4 concrete keywords each) covering \
        distinct phrasings and aspects of the goal — the words that would actually \
        appear in matching emails, articles, notes or calendar events. Respond ONLY \
        with a JSON object mapping each goal id to an array of query strings.";
    let mut prompt = String::from("Decompose these goals:\n\n");
    for (id, name, query) in &pending {
        prompt.push_str(&format!("id: {id}\nname: {name}\ngoal: {query}\n\n"));
    }
    let response = crate::llm::call(&state.store, llm, "intent_decomposition", None, Tier::Medium, system, &prompt, 1200).await?.text;
    let json_text = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let verdicts: serde_json::Map<String, Value> = serde_json::from_str(json_text)
        .map_err(|e| anyhow::anyhow!("unparseable decomposition response: {e}"))?;

    let mut done = 0usize;
    for (id, _, _) in &pending {
        let queries: Vec<(String, Option<Vec<f32>>)> = verdicts
            .get(id)
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .take(5)
                    .map(|q| {
                        let embedding =
                            state.embedder.as_ref().and_then(|e| e.embed(q).ok());
                        (q.to_string(), embedding)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Mark decomposed even on an empty answer — retrying every cycle
        // on a goal the model can't split would burn budget forever.
        state.store.add_standing_queries(actor, id, &queries)?;
        done += 1;
    }
    if done > 0 {
        tracing::info!("intents: {done} decomposed");
    }
    Ok(done)
}

/// New Persons examined per cycle for merge candidates.
const MERGE_SCAN_PER_CYCLE: usize = 100;
/// Name-embedding similarity floor for suggesting a same-person review.
/// Learned the hard way TWICE: MiniLM clusters all person-name-shaped
/// strings, and at mail scale even ONE shared real token ("david",
/// "info") floods the queue by the thousands. Candidacy now requires
/// ≥2 shared whole tokens OR one name's tokens being a subset of the
/// other's ("Jon Hale" ⊆ "Jon Hale (Harbor Labs)"); the embedding is
/// a high tiebreak only. Precision over recall — the queue must stay
/// unclogged or Tend dies.
const MERGE_NAME_THRESHOLD: f32 = 0.92;

fn name_tokens(name: &str) -> std::collections::HashSet<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(String::from)
        .collect()
}

/// Does an email localpart plausibly BE this full name? ("mreyes" /
/// "morgan.reyes" / "reyes.morgan" ↔ "Morgan Reyes"). Strong evidence:
/// the record usually knows the full name behind an address (mail
/// headers mint both), so a match here outranks name-token heuristics
/// (Jon, 2026-07-29). Rules: the last name (≥4 chars) must appear in
/// the localpart, plus either the first name or its initial in the
/// right shape.
fn email_matches_name(localpart: &str, full_name: &str) -> bool {
    let lp: String = localpart
        .to_lowercase()
        .chars()
        .take_while(|c| *c != '+')
        .filter(|c| !c.is_ascii_digit())
        .collect();
    let tokens: Vec<String> = full_name
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    if tokens.len() < 2 {
        return false;
    }
    let first = &tokens[0];
    let last = &tokens[tokens.len() - 1];
    if last.len() < 4 || !lp.contains(last.as_str()) {
        return false;
    }
    let parts: Vec<&str> = lp.split(['.', '_', '-']).filter(|p| !p.is_empty()).collect();
    // morgan.reyes / reyes.morgan
    if parts.iter().any(|p| *p == first) {
        return true;
    }
    // mreyes / morganreyes (first-initial or whole first glued to last)
    let glued: String = parts.concat();
    glued == format!("{}{last}", &first[..1]) || glued == format!("{first}{last}")
}

async fn propose_merge_candidates(state: &AppState) -> anyhow::Result<usize> {
    let since = state
        .store
        .consolidation_state("merge_scan_created_at")?
        .unwrap_or_default();
    let fresh = state
        .store
        .persons_created_since(&since, MERGE_SCAN_PER_CYCLE)?;
    if fresh.is_empty() {
        return Ok(0);
    }

    // ——— the EMAIL-EVIDENCE leg (embedder-free): a fresh person whose
    // full name matches another person's email localpart — or whose own
    // email localpart matches another's full name — queues at 0.99: the
    // strongest signal Tend has short of an identical address.
    let mut queued = 0usize;
    for (id, name, _) in fresh.iter() {
        let tokens: Vec<&str> = name.split_whitespace().collect();
        if tokens.len() >= 2 {
            let last = tokens[tokens.len() - 1].to_lowercase();
            if last.len() >= 4 {
                for (cand_id, cand_name, email) in
                    state.store.persons_with_email_like(&last, id, 8)?
                {
                    let localpart = email.split('@').next().unwrap_or("");
                    if email_matches_name(localpart, name)
                        && state.store.queue_merge_candidate(
                            id,
                            &cand_id,
                            0.99,
                            &format!("email evidence: {email} ↔ {name} (their card: {cand_name})"),
                        )?
                    {
                        queued += 1;
                    }
                }
            }
        }
        // inverse: my own address's localpart spells someone's full name
        for email in state.store.entity_emails(id)? {
            let localpart = email.split('@').next().unwrap_or("").to_string();
            // the longest dotted token is the likely surname
            if let Some(seed) = localpart
                .split(['.', '_', '-'])
                .filter(|p| p.len() >= 4)
                .max_by_key(|p| p.len())
            {
                for (cand_id, cand_name) in state.store.persons_sharing_token(seed, id, 8)? {
                    if email_matches_name(&localpart, &cand_name)
                        && state.store.queue_merge_candidate(
                            id,
                            &cand_id,
                            0.99,
                            &format!("email evidence: {email} ↔ {cand_name}"),
                        )?
                    {
                        queued += 1;
                    }
                }
            }
        }
    }

    let Some(embedder) = state.embedder.clone() else {
        // no embedder → the name-similarity leg can't run, but email
        // evidence already did; advance the watermark honestly.
        let mut watermark = since.clone();
        for (_, _, created_at) in fresh.iter() {
            watermark = watermark.max(created_at.clone());
        }
        state
            .store
            .set_consolidation_state("merge_scan_created_at", &watermark)?;
        return Ok(queued);
    };

    let mut cache: std::collections::HashMap<String, Vec<f32>> =
        std::collections::HashMap::new();
    let mut embed = |name: &str| -> Option<Vec<f32>> {
        if let Some(v) = cache.get(name) {
            return Some(v.clone());
        }
        let v = embedder.embed(name).ok()?;
        cache.insert(name.to_string(), v.clone());
        Some(v)
    };

    let mut watermark = since;
    for (i, (id, name, created_at)) in fresh.iter().enumerate() {
        watermark = watermark.max(created_at.clone());
        let tokens_a = name_tokens(name);
        let Some(vec_a) = embed(name) else { continue };
        let mut seen_candidates = std::collections::HashSet::new();
        // Precision pass (Jon 2026-07-29 — "matches to people who simply
        // have the same first name"): candidates gather first, then
        // 1. CONFLICTING SURNAMES are affirmative evidence of
        //    distinctness — two multi-token names whose last tokens
        //    differ never queue, however similar the rest;
        // 2. a BARE single-token name ("Ben") subset-matches every
        //    "Ben X", so those pairs need a higher bar (0.92) and only
        //    the single best candidate queues per bare name — one
        //    question, not one per acquaintance sharing the name.
        let mut best_bare: Option<(String, String, f32)> = None;
        for token in tokens_a.iter().take(3) {
            for (cand_id, cand_name) in
                state.store.persons_sharing_token(token, id, 12)?
            {
                if !seen_candidates.insert(cand_id.clone()) {
                    continue;
                }
                if cand_name.eq_ignore_ascii_case(name) {
                    continue; // exact same-name pairs already queue at upsert
                }
                let tokens_b = name_tokens(&cand_name);
                let shared = tokens_a.intersection(&tokens_b).count();
                let subset = tokens_a.is_subset(&tokens_b) || tokens_b.is_subset(&tokens_a);
                if shared < 2 && !subset {
                    continue;
                }
                // rule 1: surname conflict → distinct, skip
                let last_a = name.split_whitespace().last().map(str::to_lowercase);
                let last_b = cand_name.split_whitespace().last().map(str::to_lowercase);
                let multi_a = name.split_whitespace().count() > 1;
                let multi_b = cand_name.split_whitespace().count() > 1;
                if multi_a && multi_b && last_a != last_b {
                    continue;
                }
                let Some(vec_b) = embed(&cand_name) else { continue };
                let similarity =
                    hearthloom_core::embeddings::cosine_similarity(&vec_a, &vec_b);
                // rule 2: bare-name subset pairs park in best_bare
                if (!multi_a || !multi_b) && subset && shared < 2 {
                    if similarity >= 0.92
                        && best_bare.as_ref().map(|(_, _, s)| similarity > *s).unwrap_or(true)
                    {
                        best_bare = Some((cand_id.clone(), cand_name.clone(), similarity));
                    }
                    continue;
                }
                if similarity >= MERGE_NAME_THRESHOLD
                    && state.store.queue_merge_candidate(
                        id,
                        &cand_id,
                        similarity as f64,
                        &format!("name similarity {similarity:.2} ({name} ≈ {cand_name})"),
                    )?
                {
                    queued += 1;
                }
            }
        }
        if let Some((cand_id, cand_name, similarity)) = best_bare {
            if state.store.queue_merge_candidate(
                id,
                &cand_id,
                similarity as f64,
                &format!("bare-name match {similarity:.2} ({name} ≈ {cand_name})"),
            )? {
                queued += 1;
            }
        }
        // Hot-loop rule: the LIKE scans re-take the store lock — breathe.
        if i % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    state
        .store
        .set_consolidation_state("merge_scan_created_at", &watermark)?;
    if queued > 0 {
        tracing::info!("merge candidates: {queued} queued for review");
    }
    Ok(queued)
}

/// Unconfirmed relationship predictions allowed to pile up.
const RELATIONSHIP_BACKLOG_MAX: i64 = 12;
/// Persons classified per cycle (one LLM call for the batch).
const PREDICT_PER_CYCLE: usize = 8;

async fn predict_relationships(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<usize> {
    if state.store.pending_relationships()? >= RELATIONSHIP_BACKLOG_MAX {
        return Ok(0);
    }
    let people = state.store.persons_needing_prediction(PREDICT_PER_CYCLE)?;
    if people.is_empty() {
        return Ok(0);
    }
    // Who "the user" is comes from the record itself (the `me` entity),
    // never from code: every install classifies relationships to its OWN
    // owner, and no one else's name is sent to their model.
    let owner = state.store.owner_name()?.unwrap_or_else(|| "the user".to_string());
    let mut prompt = format!(
        "The user is {owner}. For each person below, their evidence mix and \
         recent episode samples (each sample starts with its episode id):\n\n"
    );
    let mut valid_evidence: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (id, name, n) in &people {
        let (mix, samples) = state.store.person_evidence_brief(id)?;
        prompt.push_str(&format!("person: {id}\nname: {name} ({n} episodes this year)\n"));
        prompt.push_str(&format!("mix: {}\n", mix.join(", ")));
        for sample in &samples {
            prompt.push_str(&format!("- {sample}\n"));
        }
        prompt.push('\n');
        valid_evidence.insert(
            id.clone(),
            samples
                .iter()
                .filter_map(|s| s.split_whitespace().next().map(String::from))
                .collect(),
        );
    }
    prompt.push_str(
        "Classify how the user relates to each person. Respond ONLY with a JSON object \
         mapping each person id to {\"kind\": \"friend|family|colleague|collaborator|\
         client|investor|advisor|acquaintance|service\", \"evidence\": [\"episode ids \
         from that person's samples\"], \"confidence\": \"high|low\"}. Omit a person \
         when the evidence is too thin to say. Mailing lists, newsletters and \
         automated senders are 'service'.",
    );
    let system = "You classify the user's relationships from their own episode \
        evidence (mail, calendars, messages, commits, photos). Conservative and \
        literal: shared commits/projects → collaborator; shared employer → \
        colleague; social plans and photos → friend; same family name + domestic \
        context → family; newsletters/robots → service. A human will confirm or \
        reject every proposal.";
    let response = crate::llm::call(&state.store, llm, "relationship_predict", None, Tier::Medium, system, &prompt, 1500).await?.text;
    let json_text = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let verdicts: serde_json::Map<String, Value> = serde_json::from_str(json_text)
        .map_err(|e| anyhow::anyhow!("unparseable relationship response: {e}"))?;

    let mut proposed = 0usize;
    for (id, _, _) in &people {
        let Some(verdict) = verdicts.get(id) else { continue };
        let Some(kind) = verdict.get("kind").and_then(|v| v.as_str()) else { continue };
        if verdict.get("confidence").and_then(|v| v.as_str()) == Some("low") {
            continue; // low-confidence guesses are noise, not proposals
        }
        let evidence: Vec<String> = verdict
            .get("evidence")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|e| {
                        valid_evidence
                            .get(id)
                            .map(|ok| ok.iter().any(|v| v == e))
                            .unwrap_or(false)
                    })
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if evidence.is_empty() {
            continue; // hallucinated citations kill the proposal
        }
        match state
            .store
            .add_relationship(actor, id, kind, "inferred", &evidence, None, None)
        {
            Ok(_) => proposed += 1,
            Err(err) => tracing::debug!("relationship proposal skipped: {err}"),
        }
    }
    if proposed > 0 {
        tracing::info!("relationships: {proposed} predictions proposed");
    }
    Ok(proposed)
}

/// Relationship briefs written per cycle (one LLM call each — they need
/// individual evidence context).
const BRIEFS_PER_CYCLE: usize = 3;

async fn write_relationship_briefs(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<usize> {
    let people = state.store.persons_needing_brief(BRIEFS_PER_CYCLE)?;
    if people.is_empty() {
        return Ok(0);
    }
    let month = &hearthloom_core::time::now_utc()[..7];
    let mut written = 0usize;
    for (id, name) in &people {
        let (mix, samples) = state.store.person_evidence_brief(id)?;
        if samples.is_empty() {
            continue;
        }
        let system = "You write a short relationship brief for the user's second \
            brain: who this person is to them, how the connection started, its arc, \
            and where it stands now — 60-120 words, factual, warm but unsentimental, \
            grounded ONLY in the evidence given. Never invent contact or feelings.";
        let prompt = format!(
            "Person: {name}\nEvidence mix: {}\nRecent episodes (id date title):\n{}\n\n\
             Write the brief as plain markdown.",
            mix.join(", "),
            samples.join("\n"),
        );
        let brief = crate::llm::call(&state.store, llm, "relationship_briefs", None, automation_tier(&state.store, "relationship_briefs"), system, &prompt, 500).await?.text;
        let report = state.store.apply_batch(
            &state.home,
            &hearthloom_core::IngestBatch {
                actor: actor.actor.clone(),
                session_id: None,
                episodes: vec![hearthloom_core::EpisodeDraft {
                    source: "consolidation".into(),
                    source_ref: format!("brief:{id}:{month}"),
                    kind: "relationship_brief".into(),
                    title: format!("How things stand with {name}"),
                    body: format!(
                        "{}\n\ndrawn from:\n{}",
                        brief.trim(),
                        samples
                            .iter()
                            .filter_map(|s| s.split_whitespace().next())
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                    event_time: hearthloom_core::EventTime {
                        utc: Some(format!("{month}-15T12:00:00Z")),
                        tz_offset_minutes: None,
                        uncertainty: hearthloom_core::TimeUncertainty::Month,
                    },
                    trust: "agent".into(),
                    scope: Some("personal".into()),
                    entities: vec![hearthloom_core::EntityRef {
                        entity_type: "Person".into(),
                        name: name.clone(),
                        attributes: vec![],
                        role: "subject".into(),
                    }],
                    edges: vec![],
                }],
            },
        )?;
        written += report.episodes_new;
    }
    if written > 0 {
        tracing::info!("relationship briefs: {written} written");
    }
    Ok(written)
}

/// Don't flood Tend: skip growth while this many proposals sit unreviewed.
const PROPOSAL_BACKLOG_MAX: i64 = 12;

/// Propose new self-model propositions from digest + chapter evidence.
/// One LLM call per cycle, at most 6 proposals, evidence ids validated
/// against the exact input set (a hallucinated citation kills the row).
async fn grow_self_model(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<usize> {
    if state.store.pending_propositions()? >= PROPOSAL_BACKLOG_MAX {
        return Ok(0);
    }
    let (digests, chapters) = state.store.self_model_grower_inputs(6, 24)?;
    if digests.is_empty() && chapters.is_empty() {
        return Ok(0);
    }
    let known = state.store.known_proposition_texts()?;

    let mut prompt = String::from("Evidence — recent activity digests:\n");
    let mut valid_ids = std::collections::HashSet::new();
    for (id, _title) in &digests {
        if let Some(body) = state.store.episode_body(id)? {
            prompt.push_str(&format!("[{id}]\n"));
            for line in body.lines().take(16) {
                prompt.push_str(line);
                prompt.push('\n');
            }
            prompt.push('\n');
            valid_ids.insert(id.clone());
        }
    }
    prompt.push_str("Evidence — monthly chapter headings (newest first):\n");
    for (id, title) in &chapters {
        prompt.push_str(&format!("[{id}] {title}\n"));
        valid_ids.insert(id.clone());
    }
    if !known.is_empty() {
        prompt.push_str("\nAlready known (do NOT re-propose or rephrase these):\n");
        for text in known.iter().rev().take(60) {
            prompt.push_str(&format!("- {text}\n"));
        }
    }
    prompt.push_str(
        "\nPropose NEW propositions about who this person is. Respond ONLY with a \
         JSON array (possibly empty) of objects: {\"sphere\": \"work|personal|family|health\", \
         \"kind\": \"identity|offering|seeking|strength|constraint\", \"text\": \"one \
         concrete sentence\", \"evidence\": [\"episode ids from the brackets above\"]}. \
         At most 6. Only what the evidence clearly supports.",
    );
    let system = "You maintain the self-model of Hearthloom, the user's local second \
        brain: durable propositions about who they are — identity, what they can offer, \
        what they are seeking, strengths, constraints — grouped by life sphere. You \
        propose; a human confirms. Be conservative and specific; never flatter, never \
        invent. Seeking propositions are especially valuable (they power opportunity \
        detection) but only when the evidence shows an active want.";

    let response = crate::llm::call(&state.store, llm, "self_model", None, Tier::High, system, &prompt, 1200).await?.text;
    let json_text = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let proposals: Vec<Value> = serde_json::from_str(json_text)
        .map_err(|e| anyhow::anyhow!("unparseable self-model response: {e}"))?;

    let mut added = 0usize;
    for p in proposals.iter().take(6) {
        let (Some(sphere), Some(kind), Some(text)) = (
            p.get("sphere").and_then(|v| v.as_str()),
            p.get("kind").and_then(|v| v.as_str()),
            p.get("text").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        // The grower never writes the private sphere.
        if sphere == "private" {
            continue;
        }
        let evidence: Vec<String> = p
            .get("evidence")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|id| valid_ids.contains(*id))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        if evidence.is_empty() {
            continue; // inferred requires real, non-hallucinated evidence
        }
        let embedding = state.embedder.as_ref().and_then(|e| e.embed(text).ok());
        match state.store.add_proposition(
            actor,
            sphere,
            kind,
            text,
            "inferred",
            &evidence,
            embedding.as_deref(),
        ) {
            Ok(_) => added += 1,
            Err(err) => tracing::debug!("proposition skipped: {err}"),
        }
    }
    if added > 0 {
        tracing::info!("self-model: {added} propositions proposed");
    }
    Ok(added)
}

/// One LLM call titles up to a dozen months at a time: given each
/// month's representative (never-private) titles, write a short
/// chapter heading. Idempotent per month via source_ref chapter:YYYY-MM.
const CHAPTER_MONTHS_PER_CYCLE: usize = 24;

/// Tell the user that an idle pass wrote something meant to be read —
/// a weekly review, last month's chapter heading. One notification per
/// `dedupe` key: both of a user's nodes may write the same thing, and the
/// notification exchange merges them.
fn notify_written(state: &AppState, title: &str, body: &str, link: (&str, &str), dedupe: &str) {
    if let Err(err) = state.store.notify(
        &Actor::new("system:daemon"),
        "system:daemon",
        "info",
        title,
        body,
        Some((link.0, Some(link.1))),
        Some(dedupe),
    ) {
        tracing::warn!("could not post the {dedupe} notification: {err:#}");
        return;
    }
    crate::bus::touch(state, "notification.created");
}

/// The month before `today`'s, as YYYY-MM — the one chapter heading
/// worth announcing (everything older is backfill).
fn previous_month(today: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    let first = today.with_day(1).unwrap_or(today);
    (first - chrono::Duration::days(1)).format("%Y-%m").to_string()
}

/// One heading per month from its sampled titles: `month → heading`.
async fn chapter_headings(
    state: &AppState,
    llm: &dyn LlmProvider,
    inputs: &[(String, Vec<(String, String)>)],
) -> anyhow::Result<serde_json::Map<String, Value>> {
    let system = "You write chapter headings for months of a person's life, given a \
        sample of that month's calendar events, articles, workouts, photos and commits. \
        Each heading: 3-7 words, evocative but concrete, no dates, no quotes. Respond \
        ONLY with a JSON object mapping each YYYY-MM to its heading.";
    let mut prompt = String::from("Title these months:\n\n");
    for (month, titles) in inputs {
        prompt.push_str(&format!("{month}:\n"));
        for (_, t) in titles {
            prompt.push_str(&format!("- {}\n", hearthloom_core::one_line(t).chars().take(80).collect::<String>()));
        }
        prompt.push('\n');
    }
    let response = crate::llm::call(&state.store, llm, "chapters", None,
        automation_tier(&state.store, "chapters"), system, &prompt, 1500)
        .await?.text;
    let json_text = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(json_text).map_err(|e| anyhow::anyhow!("unparseable chapter response: {e}"))
}

/// A chapter episode. Its body lists the titles it was written from, one
/// `- title` line each — titles other people wrote (a calendar invite's)
/// and a heading a model wrote from them, so every one is `one_line`d: a
/// line break in either could otherwise list a title the chapter never
/// read (retire.rs reads that list for chapters that recorded no inputs).
fn chapter_draft(
    month: &str,
    heading: &str,
    titles: &[(String, String)],
    source_ref: String,
) -> anyhow::Result<hearthloom_core::EpisodeDraft> {
    let heading = hearthloom_core::one_line(heading);
    Ok(hearthloom_core::EpisodeDraft {
        source: "consolidation".into(),
        source_ref,
        kind: "chapter".into(),
        title: heading.clone(),
        body: format!(
            "chapter heading for {month}: {heading}\n\nwritten from:\n{}",
            titles
                .iter()
                .map(|(_, t)| format!("- {}", hearthloom_core::one_line(t)))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        event_time: hearthloom_core::EventTime::parse_rfc3339(
            &format!("{month}-15T12:00:00Z"),
            hearthloom_core::TimeUncertainty::Month,
        )?,
        trust: "agent".into(),
        scope: Some("personal".into()),
        entities: vec![],
        edges: vec![],
    })
}

async fn write_chapters(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<usize> {
    let months = state.store.months_needing_chapters(CHAPTER_MONTHS_PER_CYCLE)?;
    if months.is_empty() {
        return Ok(0);
    }
    let mut written = 0usize;
    for chunk in months.chunks(12) {
        let mut inputs: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for month in chunk {
            let titles = state.store.chapter_inputs(month, 8)?;
            if !titles.is_empty() {
                inputs.push((month.clone(), titles));
            }
        }
        if inputs.is_empty() {
            continue;
        }
        let headings = chapter_headings(state, llm, &inputs).await?;
        let mut episodes = Vec::new();
        let mut read: Vec<(String, Vec<String>)> = Vec::new();
        let just_ended = previous_month(chrono::Utc::now().date_naive());
        let mut announce: Option<String> = None;
        for (month, titles) in &inputs {
            let Some(heading) = headings.get(month).and_then(|h| h.as_str()) else {
                continue;
            };
            if *month == just_ended {
                announce = Some(hearthloom_core::one_line(heading));
            }
            let source_ref = format!("chapter:{month}");
            read.push((
                hearthloom_core::ids::episode_id("consolidation", &source_ref),
                titles.iter().map(|(id, _)| id.clone()).collect(),
            ));
            episodes.push(chapter_draft(month, heading, titles, source_ref)?);
        }
        if !episodes.is_empty() {
            let report = state.store.apply_batch(
                &state.home,
                &hearthloom_core::IngestBatch {
                    actor: actor.actor.clone(),
                    session_id: None,
                    episodes,
                },
            )?;
            written += report.episodes_new;
            // What each heading read — a later retirement finds it exactly.
            for (id, inputs) in &read {
                state.store.record_summary_inputs(id, inputs)?;
            }
            // Only the month that just ended is news; the backfill of
            // older months stays quiet. (Inputs exclude private, so the
            // heading is safe to show with the vault locked.)
            if let (Some(heading), true) = (announce, report.episodes_new > 0) {
                let month_name = chrono::NaiveDate::parse_from_str(&format!("{just_ended}-01"), "%Y-%m-%d")
                    .map(|d| d.format("%B").to_string())
                    .unwrap_or_else(|_| just_ended.clone());
                notify_written(
                    state,
                    &format!("{month_name} has a chapter heading: “{heading}”"),
                    "Hearthloom titled last month from what happened in it. It labels that \
                     month on the timeline and in your biography.",
                    ("#/biography", "open the biography"),
                    &format!("chapter:{just_ended}"),
                );
            }
        }
    }
    if written > 0 {
        tracing::info!("chapters: {written} month headings written");
    }
    Ok(written)
}

/// Open lint findings allowed to pile up before the sweep pauses.
const LINT_BACKLOG_MAX: i64 = 12;
/// Note pairs judged per lint run (one LLM call for the batch).
const LINT_PAIRS_PER_RUN: usize = 8;

/// The record lint (report-only, D of the 2026-07-12 batch): orphan
/// entities and coverage gaps by SQL; contradictions/superseded claims
/// by one LLM call over embedding-shortlisted note pairs. Every finding
/// is a Tend card; nothing is ever changed or deleted here.
async fn run_lint(state: &AppState, actor: &Actor) -> anyhow::Result<usize> {
    if state.store.lint_pending_count()? >= LINT_BACKLOG_MAX {
        return Ok(0);
    }
    let mut added = 0usize;

    // Orphans: one rollup card keyed by the exact id set — resolving it
    // (either way) keeps it resolved until the set changes.
    let orphans = state.store.lint_orphan_entities(25)?;
    if !orphans.is_empty() {
        let ids: Vec<String> = orphans.iter().map(|(id, _, _)| id.clone()).collect();
        let body = orphans
            .iter()
            .map(|(_, name, ty)| format!("- {name} ({ty})"))
            .collect::<Vec<_>>()
            .join("\n");
        if state.store.add_lint_finding(
            actor,
            "orphan",
            &ids.join(","),
            &format!(
                "{} entit{} with no evidence and no connections",
                orphans.len(),
                if orphans.len() == 1 { "y" } else { "ies" }
            ),
            &format!(
                "These never-declared entities have zero supporting episodes and no \
                 live edges — likely extraction noise or leftovers from merges:\n\n{body}"
            ),
            &ids,
        )? {
            added += 1;
        }
    }

    // Coverage gaps: cited often, developed nowhere.
    let gaps = state.store.lint_coverage_gaps(5)?;
    if !gaps.is_empty() {
        let ids: Vec<String> = gaps.iter().map(|(id, _, _)| id.clone()).collect();
        let body = gaps
            .iter()
            .map(|(_, name, n)| format!("- {name} — cited in {n} episodes these 90 days"))
            .collect::<Vec<_>>()
            .join("\n");
        if state.store.add_lint_finding(
            actor,
            "gap",
            &ids.join(","),
            "Topics the record keeps citing but no note develops",
            &format!(
                "Heavily referenced, never written about — candidates for a note \
                 or a living topic page:\n\n{body}"
            ),
            &ids,
        )? {
            added += 1;
        }
    }

    // Contradiction / superseded check: LLM over the resonant-pair
    // shortlist (private notes excluded at the SQL — cloud-pass rule).
    let Some(llm) = &state.llm else {
        return Ok(added);
    };
    let pairs = state.store.lint_pair_candidates(LINT_PAIRS_PER_RUN)?;
    if pairs.is_empty() {
        return Ok(added);
    }
    let excerpt = |path: &str| -> String {
        state
            .home
            .read_note(path)
            .unwrap_or_default()
            .chars()
            .take(1200)
            .collect()
    };
    let mut prompt = String::from("Note pairs from a personal knowledge base:\n\n");
    for (i, p) in pairs.iter().enumerate() {
        prompt.push_str(&format!(
            "pair: {i}\nnote A: \"{}\" (last edited {})\n{}\n\nnote B: \"{}\" (last edited {})\n{}\n\n---\n\n",
            p.title_a,
            p.updated_a.as_deref().map(|d| &d[..10]).unwrap_or("unknown"),
            excerpt(&p.path_a),
            p.title_b,
            p.updated_b.as_deref().map(|d| &d[..10]).unwrap_or("unknown"),
            excerpt(&p.path_b),
        ));
    }
    prompt.push_str(
        "For each pair, do the notes make claims that CONTRADICT each other, or does \
         the newer one SUPERSEDE the older (same subject, materially updated facts)? \
         Respond ONLY with a JSON object mapping each pair index to \
         {\"verdict\": \"contradiction|superseded|consistent\", \"explanation\": \
         \"one concrete sentence quoting the conflicting claims\"}. Notes that merely \
         share a topic are consistent — flag only real conflicts.",
    );
    let system = "You lint a personal knowledge base for internal conflicts. \
        Conservative and literal: most related notes are consistent; a finding must \
        point at specific incompatible claims. A human reviews every finding.";
    let response = crate::llm::call(&state.store, llm.as_ref(), "lint", None,
        automation_tier(&state.store, "lint"), system, &prompt, 1500)
        .await?.text;
    let json_text = response
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let verdicts: serde_json::Map<String, Value> = serde_json::from_str(json_text)
        .map_err(|e| anyhow::anyhow!("unparseable lint response: {e}"))?;
    for (i, p) in pairs.iter().enumerate() {
        let Some(v) = verdicts.get(&i.to_string()) else { continue };
        let Some(verdict) = v.get("verdict").and_then(|x| x.as_str()) else { continue };
        if !matches!(verdict, "contradiction" | "superseded") {
            continue;
        }
        let explanation = v
            .get("explanation")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let mut key = [p.id_a.clone(), p.id_b.clone()];
        key.sort();
        if state.store.add_lint_finding(
            actor,
            verdict,
            &key.join(","),
            &format!(
                "{}: \"{}\" ↔ \"{}\"",
                if verdict == "contradiction" { "Contradiction" } else { "Superseded" },
                p.title_a,
                p.title_b
            ),
            &format!(
                "{explanation}\n\nnotes:\n- note:{} {}\n- note:{} {}",
                p.id_a, p.title_a, p.id_b, p.title_b
            ),
            &[p.id_a.clone(), p.id_b.clone()],
        )? {
            added += 1;
        }
    }
    if added > 0 {
        tracing::info!("record lint: {added} findings queued for Tend");
    }
    Ok(added)
}

/// F of the batch: one rollup episode per week (idempotent by the week's
/// Monday). Consults the lint pass's open coverage-gap findings for the
/// "what to feed next" section.
async fn write_weekly_review(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<bool> {
    let week = hearthloom_core::time::current_week_start();
    let source_ref = format!("review:{week}");
    if state.store.episode_exists("consolidation", &source_ref)? {
        return Ok(false);
    }
    let inputs = state.store.weekly_review_inputs(7)?;
    if inputs.episodes.len() < 5 {
        return Ok(false); // too quiet a week to be worth a review
    }
    let written = compose_weekly_review(state, llm, actor, &week, source_ref, &inputs).await?;
    if written {
        tracing::info!("weekly review written for week of {week}");
        notify_written(
            state,
            "This week's review is ready",
            &format!(
                "A short look back at the week of {week}: the ideas that mattered, how they \
                 connect to older material, and what to feed next."
            ),
            (&format!("#/day/{week}"), "read it"),
            &format!("weekly-review:{week}"),
        );
    }
    Ok(written)
}

/// Write one weekly review from `inputs` under `source_ref`, recording
/// what it read. True when it landed new.
async fn compose_weekly_review(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
    week: &str,
    source_ref: String,
    inputs: &hearthloom_core::store::ReviewInputs,
) -> anyhow::Result<bool> {
    let (notes, entities) = (inputs.notes.as_slice(), inputs.entities.as_slice());
    let review_id = hearthloom_core::ids::episode_id("consolidation", &source_ref);
    let gaps: Vec<String> = state
        .store
        .lint_pending(20)?
        .into_iter()
        .filter(|f| f.kind == "gap")
        .map(|f| f.body)
        .collect();
    let mut prompt = format!("Week of {week}.\n\nEpisodes this week:\n");
    for (_, line) in &inputs.episodes {
        prompt.push_str(&format!("- {line}\n"));
    }
    if !notes.is_empty() {
        prompt.push_str("\nNotes touched this week:\n");
        for t in notes {
            prompt.push_str(&format!("- {t}\n"));
        }
    }
    if !entities.is_empty() {
        prompt.push_str("\nMost-cited people/orgs/projects this week:\n");
        for e in entities {
            prompt.push_str(&format!("- {e}\n"));
        }
    }
    if !gaps.is_empty() {
        prompt.push_str("\nKnown coverage gaps (cited often, never written about):\n");
        for g in gaps.iter().take(2) {
            prompt.push_str(&format!("{g}\n"));
        }
    }
    prompt.push_str(
        "\nWrite the weekly review as markdown with exactly these four sections:\n\
         ## The three ideas that mattered\n## Old ↔ new connections\n\
         ## Hubs forming\n## What to feed next\n\
         Concrete and grounded in the material above; 150-250 words total; no flattery.",
    );
    let system = "You write the weekly review for Hearthloom, the user's local second \
        brain — the Karpathy loop's reflection step: what new ideas landed, how they \
        connect to older material, which topics are becoming hubs, and what input the \
        record is missing. Grounded only in the evidence given.";
    let review = crate::llm::call(&state.store, llm, "weekly_review", None, automation_tier(&state.store, "weekly_review"), system, &prompt, 900).await?.text;
    let report = state.store.apply_batch(
        &state.home,
        &IngestBatch {
            actor: actor.actor.clone(),
            session_id: None,
            episodes: vec![EpisodeDraft {
                source: "consolidation".into(),
                source_ref,
                kind: "review".into(),
                title: format!("Weekly review — week of {week}"),
                body: review.trim().to_string(),
                event_time: EventTime::parse_rfc3339(
                    &format!("{week}T12:00:00Z"),
                    TimeUncertainty::Day,
                )?,
                trust: "agent".into(),
                scope: Some("personal".into()),
                entities: vec![],
                edges: vec![],
            }],
        },
    )?;
    if report.episodes_new > 0 {
        let read: Vec<String> = inputs.episodes.iter().map(|(id, _)| id.clone()).collect();
        state.store.record_summary_inputs(&review_id, &read)?;
    }
    Ok(report.episodes_new > 0)
}

/// Topic pages proposed per run (one LLM call each — they need their
/// own evidence context).
const TOPIC_PAGES_PER_RUN: usize = 2;

/// E of the batch: living topic pages. The consolidator DRAFTS "what I
/// know about X" hub notes with citations and proposes each through
/// quarantine (kind=topic_page); accepting upserts the loom note
/// (origin='loom', origin_ref=topic:<entity>) — the human gate stays.
async fn tend_topic_pages(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<usize> {
    let candidates = state.store.topic_page_candidates(TOPIC_PAGES_PER_RUN)?;
    let mut proposed = 0usize;
    for (entity_id, name, citations, note_id, note_path) in candidates {
        let (mix, samples) = state.store.person_evidence_brief(&entity_id)?;
        if samples.is_empty() {
            continue;
        }
        let existing = note_path
            .as_deref()
            .map(|p| {
                state
                    .home
                    .read_note(p)
                    .unwrap_or_default()
                    .chars()
                    .take(2000)
                    .collect::<String>()
            })
            .filter(|s| !s.is_empty());
        let system = "You maintain living topic pages in Hearthloom, the user's local \
            second brain: one hub note per emerging topic — \"what I know about X\" — \
            revised as evidence accumulates. Factual, compact markdown grounded ONLY \
            in the evidence given, every claim traceable: cite episode ids inline as \
            [ep:<id>]. Link related people/orgs with @[Name] and related notes with \
            [[Note Title]] where they genuinely belong. A human reviews every draft.";
        let prompt = format!(
            "Topic: {name} ({citations} citations these 90 days)\nEvidence mix: {}\n\
             Recent episodes (id date title):\n{}\n\n{}\
             Write the page body as markdown (120-250 words), starting with a one-line \
             summary. Sections as the material warrants. No title heading — the note's \
             title carries it.",
            mix.join(", "),
            samples.join("\n"),
            existing
                .as_deref()
                .map(|body| format!(
                    "Current page (revise it — keep what still holds, fold in the new):\n{body}\n\n"
                ))
                .unwrap_or_default(),
        );
        let draft = crate::llm::call(&state.store, llm, "topic_pages", None, automation_tier(&state.store, "topic_pages"), system, &prompt, 800).await?.text;
        state.store.propose_memories(
            actor,
            &[json!({
                "kind": "topic_page",
                "title": format!("What I know about {name}"),
                "text": draft.trim(),
                "origin_ref": format!("topic:{entity_id}"),
                "entity": name,
                "revises_note": note_id,
            })],
        )?;
        proposed += 1;
    }
    if proposed > 0 {
        tracing::info!("topic pages: {proposed} drafts proposed through Tend");
    }
    Ok(proposed)
}

/// Index location points from un-scanned episodes: photos via the store
/// parser; workouts by reading their GPX route's first trackpoint from
/// the apple-health archive.
async fn index_locations(state: &AppState, actor: &Actor) -> anyhow::Result<usize> {
    let store = state.store.clone();
    let actor = actor.clone();
    let gpx_root = crate::virtual_source::resolve(state, "apple-health")
        .ok()
        .and_then(|def| crate::virtual_source::mount_path(&def).ok());
    tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
        // Small lock-bounded batches with a breath between them (THE
        // PASS CONTRACT): one 5000-body read under a single lock hold
        // starved every reader on a fresh peer digesting a bulk sync.
        let mut added = 0usize;
        let mut budget = 5000usize;
        loop {
            let batch = budget.min(250);
            if batch == 0 {
                break;
            }
            let (scanned, points) = store.location_backfill(&actor, batch)?;
            added += points;
            budget -= scanned.min(budget);
            if scanned < batch {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        // Workout GPX starts (route files live under the archive mount) —
        // same batching for the body reads. Workouts ONLY: other
        // apple-health bodies (clinical records) quote provider text.
        let mut health_budget = 2000usize;
        loop {
            let batch = health_budget.min(250);
            if batch == 0 {
                break;
            }
            let pending = store.location_pending("apple-health", "workout", batch)?;
            for (id, day, body) in &pending {
                let mut points = Vec::new();
                if let Some((lat, lon)) = gpx_root.as_deref().and_then(|root| workout_route_start(root, body)) {
                    points.push((None, day.clone(), lat, lon));
                    added += 1;
                }
                // Mark scanned either way — routeless workouts are done.
                store.insert_location_points(id, &points)?;
            }
            health_budget -= pending.len().min(health_budget);
            if pending.len() < batch {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        Ok(added)
    })
    .await?
}

/// Where a workout's route starts: its `route:` line (archive-relative,
/// as apple_health writes it — `/workout-routes/route_….gpx`) joined onto
/// the archive mount by `safe_join`, so a `..`, absolute or prefix
/// component never reads outside it; then the file's first trackpoint.
fn workout_route_start(root: &std::path::Path, body: &str) -> Option<(f64, f64)> {
    let rel = body.lines().find_map(|l| l.strip_prefix("route: "))?;
    let path = crate::virtual_source::safe_join(root, rel.trim()).ok()?;
    first_trkpt(&std::fs::read_to_string(path).ok()?)
}

/// First `<trkpt lat=".." lon="..">` of a GPX document.
fn first_trkpt(gpx: &str) -> Option<(f64, f64)> {
    let at = gpx.find("<trkpt")?;
    let tag_end = gpx[at..].find('>')? + at;
    let tag = &gpx[at..tag_end];
    let attr = |name: &str| -> Option<f64> {
        let needle = format!("{name}=\"");
        let start = tag.find(&needle)? + needle.len();
        let end = tag[start..].find('\"')? + start;
        tag[start..end].parse().ok()
    };
    let (lat, lon) = (attr("lat")?, attr("lon")?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Re-run the local adapters over known sources. Returns per-source new
/// episode counts (Null when skipped because a refresh ran recently).
async fn refresh_local_sources(state: &AppState, actor: &Actor) -> anyhow::Result<Value> {
    // At most once per REFRESH_EVERY, tracked via the mutations log.
    if let Some(ts) = state.store.latest_mutation_ts("sources_refreshed")? {
        if let Ok(then) = chrono::DateTime::parse_from_rfc3339(&ts) {
            let age = chrono::Utc::now().signed_duration_since(then.with_timezone(&chrono::Utc));
            if age.to_std().unwrap_or_default() < REFRESH_EVERY {
                return Ok(Value::Null);
            }
        }
    }

    let store = state.store.clone();
    let home = state.home.clone();
    let repos = state.store.known_repo_paths()?;
    let summary = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let mut per_source = serde_json::Map::new();
        let mut apply = |name: &str,
                         drafts: anyhow::Result<Vec<hearthloom_core::EpisodeDraft>>|
         -> anyhow::Result<()> {
            let drafts = match drafts {
                Ok(drafts) => drafts,
                Err(err) => {
                    tracing::debug!("refresh {name}: {err:#}");
                    return Ok(());
                }
            };
            let mut new = 0usize;
            for chunk in drafts.chunks(200) {
                let report = store.apply_batch(
                    &home,
                    &hearthloom_core::IngestBatch {
                        actor: format!("adapter:{name}"),
                        session_id: None,
                        episodes: chunk.to_vec(),
                    },
                )?;
                new += report.episodes_new;
                // Let readers through: HTTP handlers share the store
                // mutex, and a refresh must never wedge the dashboard.
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
            if new > 0 {
                per_source.insert(name.to_string(), serde_json::json!(new));
            }
            Ok(())
        };

        for repo in &repos {
            let path = std::path::Path::new(repo);
            if path.join(".git").exists() {
                apply("git", hearthloom_adapters::git::ingest(path, None))?;
            }
        }
        if let Ok(homedir) = std::env::var("HOME") {
            apply(
                "claude-code",
                hearthloom_adapters::claude_code::ingest(
                    &std::path::Path::new(&homedir).join(".claude/projects"),
                    None,
                ),
            )?;
        }
        if let Ok(dir) = hearthloom_adapters::chrome::default_data_dir() {
            apply("chrome", hearthloom_adapters::chrome::ingest(&dir, None))?;
        }
        // TCC-gated sources refresh too when the daemon has access.
        if let Ok(library) = hearthloom_adapters::photos::default_library() {
            apply("photos", hearthloom_adapters::photos::ingest(&library, None))?;
            // v47: the same scan feeds the durable photo-point union so
            // Mac and phone photos reconcile (idempotent by key).
            if let Ok(points) = hearthloom_adapters::photos::scan_points(&library) {
                let rows: Vec<hearthloom_core::store::PhotoPointIn> = points
                    .iter()
                    .filter_map(|p| {
                        Some(hearthloom_core::store::PhotoPointIn {
                            key: p.key(),
                            filename: p.filename.clone(),
                            taken_at: chrono::DateTime::from_timestamp(p.at_unix, 0)?
                                .format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                            tz_offset_minutes: p.tz_offset_minutes.unwrap_or(0),
                            lat: p.latitude,
                            lon: p.longitude,
                            place: p.place.clone(),
                        })
                    })
                    .collect();
                let _ = store.photos_add_points("mac", &rows);
            }
        }
        Ok(Value::Object(per_source))
    })
    .await??;

    // Stamp the refresh (unique key per run).
    {
        let ts = hearthloom_core::time::now_utc();
        let conn_actor = actor.clone();
        let store = state.store.clone();
        let summary_clone = summary.clone();
        tokio::task::spawn_blocking(move || {
            store.record_refresh(&conn_actor, &ts, &summary_clone)
        })
        .await??;
    }
    Ok(summary)
}

/// Extraction budget per cycle (batched 8 episodes per LLM call).
const EXTRACT_PER_CYCLE: usize = 48;
const EXTRACT_BATCH: usize = 8;
/// Per-cycle wall-clock ceiling for the extraction drain loop — a big
/// backfill finishes in hours, not weeks, without monopolizing a cycle.
const EXTRACT_CYCLE_BUDGET_SECS: u64 = 20 * 60;

/// Tri-class LLM failure handling (design locked 2026-07-17): billing-
/// class stops ALL spend until the user acts; everything else is
/// transient and the pending pool is its own resume state.
#[derive(PartialEq)]
pub enum LlmFailure {
    Billing,
    Transient,
}

pub fn classify_llm_error(err: &str) -> LlmFailure {
    let e = err.to_lowercase();
    let billing = ["401", "403", "402", "authentication", "invalid x-api-key",
        "insufficient_quota", "quota exceeded", "credit", "billing", "payment"];
    if billing.iter().any(|m| e.contains(m)) {
        LlmFailure::Billing
    } else {
        LlmFailure::Transient
    }
}

pub fn llm_blocked(store: &Store) -> Option<String> {
    store.consolidation_state("llm.blocked").ok().flatten().filter(|v| !v.is_empty())
}

/// Billing-class failure: stop spend NOW, raise the critical bell (it
/// tints the masthead, persists, and syncs to the peer), and stay
/// blocked until the user explicitly resumes (key re-save or the AI
/// provider card action). Transient failures NEVER set this.
fn set_llm_blocked(state: &AppState, detail: &str) {
    let _ = state.store.set_consolidation_state(
        "llm.blocked",
        &serde_json::json!({"class": "billing", "detail": detail,
                            "since": hearthloom_core::time::now_utc()}).to_string(),
    );
    crate::provisioner::notify_system(
        state, "critical", "AI provider needs attention",
        &format!("thinking is paused — the provider refused with a billing/credentials                   problem ({detail}). Fix it at the provider, then resume from                   Integrations → AI provider."),
        Some("llm-blocked"),
    );
}

/// Sources eligible for LLM entity extraction, with their RECOMMENDED tier
/// (`None` = Disabled by default). The prose sources default on; the two
/// big/structural ones default off but stay user-overridable (Jon wants
/// full ingest possible even for mbox — Phase 3b). A user maps each to
/// low|medium|high|disabled via `extract.<source>.tier` in consolidation
/// state; the pre-Phase-3b behavior is exactly the `Some(_)` rows here.
/// Sources NOT listed mint entities structurally and are never extracted.
pub const EXTRACT_DEFAULTS: &[(&str, Option<Tier>)] = &[
    ("chatlog-claude", Some(Tier::High)),
    ("chatlog-chatgpt", Some(Tier::Medium)),
    ("openai", Some(Tier::Medium)),
    ("imessage", Some(Tier::Medium)),
    ("substack", Some(Tier::Low)),
    // OCR text from the phone's vision captures (v51): merchants → Org,
    // business-card people → Person — the extraction pass IS the scan
    // pipeline's entity miner (the drafts deliberately mint none).
    ("ios-scan", Some(Tier::Medium)),
    // Disabled by default — a Takeout-scale mail archive or numeric health
    // aggregates would burn the extraction budget for months, and both
    // adapters already mint their entities structurally (the design log
    // 2026-07-08). Overridable to force a full-text extraction pass.
    ("mbox", None),
    ("apple-health", None),
];

/// The extraction tier configured for one source: `extract.<source>.tier`
/// (low|medium|high|disabled), else its recommended default. `None` =
/// Disabled (or an unknown source).
pub fn extraction_tier(store: &Store, source: &str) -> Option<Tier> {
    let default = EXTRACT_DEFAULTS
        .iter()
        .find(|(s, _)| *s == source)
        .map(|(_, t)| *t)?;
    match store
        .consolidation_state(&format!("extract.{source}.tier"))
        .ok()
        .flatten()
    {
        Some(v) if matches!(v.trim().to_lowercase().as_str(), "disabled" | "off" | "none") => None,
        Some(v) => Tier::parse(&v).or(default),
        None => default,
    }
}

/// The sources currently enabled for extraction and the tier each spends at.
fn enabled_extraction_sources(store: &Store) -> Vec<(&'static str, Tier)> {
    EXTRACT_DEFAULTS
        .iter()
        .filter_map(|(src, _)| extraction_tier(store, src).map(|t| (*src, t)))
        .collect()
}

/// Mine entity mentions from prose episodes. Returns (episodes, links).
/// Per source (Phase 3b): only configured-enabled sources are pulled, and
/// each source's batch is completed at that source's configured tier.
async fn extract_entities(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<(usize, usize)> {
    let tiers = enabled_extraction_sources(&state.store);
    if tiers.is_empty() {
        return Ok((0, 0));
    }
    if let Some(reason) = llm_blocked(&state.store) {
        tracing::info!("extraction skipped — llm blocked: {reason}");
        return Ok((0, 0));
    }
    let source_list: Vec<&str> = tiers.iter().map(|(s, _)| *s).collect();
    // all-episodes: prompt text, not a query
    let system = "You extract named entities from episodes of a personal knowledge base. \
        Only extract entities EXPLICITLY NAMED in the text: real people (full or first \
        names, not 'you'/'the user'/'Claude'/'ChatGPT'), organizations/companies, named \
        projects/products, and places. Skip generic terms, technologies-as-nouns, and \
        anything you are not sure is a proper name. Respond ONLY with a JSON object \
        mapping each episode id to an array of {\"type\": \"Person\"|\"Org\"|\"Project\"|\"Place\"|\"Event\", \
        \"name\": \"...\"} — an empty array when nothing qualifies.";

    // BACKFILL SEMANTICS (Jon 2026-07-17): an initial import of a large
    // collection gets interpreted PROMPTLY, not trickled at 48/cycle for
    // weeks — drain the backlog until empty or this cycle's time budget
    // is spent (the rest continues next cycle). Small per-request batches
    // and breathing between pulls keep the lock rules intact.
    let cycle_started = std::time::Instant::now();
    let mut mined = 0usize;
    let mut linked = 0usize;
    loop {
    let pending = state
        .store
        .episodes_needing_extraction(&source_list, EXTRACT_PER_CYCLE)?;
    if pending.is_empty() {
        break;
    }
    // Group pending rows by source so each source's batches run at its tier.
    let mut by_source: std::collections::HashMap<
        &str,
        Vec<&hearthloom_core::store::PendingExtraction>,
    > = std::collections::HashMap::new();
    for row in &pending {
        by_source.entry(row.1.as_str()).or_default().push(row);
    }

    for (source, tier) in &tiers {
        let Some(rows) = by_source.get(source) else {
            continue;
        };
        for batch in rows.chunks(EXTRACT_BATCH) {
            let mut prompt = String::from("Extract entities from these episodes:\n\n");
            for (id, _, title, _, excerpt) in batch {
                prompt.push_str(&format!(
                    "id: {id}\ntitle: {title}\ntext: {}\n\n",
                    excerpt.replace('\n', " ")
                ));
            }
            // transient failures retry in place; billing-class failures
            // stop ALL spend and raise the bell (resume is explicit)
            let mut response = None;
            for attempt in 0..3u32 {
                match crate::llm::call(&state.store, llm, "extraction", None, *tier, system, &prompt, 2500).await {
                    Ok(r) => {
                        response = Some(r.text);
                        break;
                    }
                    Err(err) => {
                        let msg = format!("{err:#}");
                        if classify_llm_error(&msg) == LlmFailure::Billing {
                            set_llm_blocked(state, &msg);
                            return Ok((mined, linked));
                        }
                        tracing::warn!("extraction call failed (attempt {}): {msg}", attempt + 1);
                        tokio::time::sleep(std::time::Duration::from_secs(2 << attempt)).await;
                    }
                }
            }
            let Some(response) = response else {
                // still transient after retries: end the cycle early —
                // the pending pool resumes next cycle automatically
                return Ok((mined, linked));
            };
            let json_text = response
                .trim()
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();
            let verdicts: serde_json::Map<String, Value> = match serde_json::from_str(json_text) {
                Ok(v) => v,
                Err(e) => {
                    // poison batch: mark failed (requeue-able) and move on —
                    // and CAPTURE why, or the episodes vanish from "N to go"
                    // with no trace (the Integrations card reports this).
                    tracing::warn!("unparseable extraction response — marking batch failed: {e}");
                    let ids: Vec<String> = batch.iter().map(|(id, ..)| id.clone()).collect();
                    let _ = state.store.mark_extraction_failed(&ids);
                    let mut msg = format!("the AI's answer wasn't usable ({e})");
                    msg.truncate(160);
                    let _ = state.store.set_consolidation_state(
                        &format!("extract.{source}.last_error"),
                        &serde_json::json!({
                            "error": msg,
                            "episodes": ids.len(),
                            "ts": hearthloom_core::time::now_utc(),
                        })
                        .to_string(),
                    );
                    continue;
                }
            };
            for (id, _, _, scope, _) in batch {
                let entities: Vec<(String, String)> = verdicts
                    .get(id)
                    .and_then(|v| v.as_array())
                    .map(|list| {
                        list.iter()
                            .filter_map(|item| {
                                Some((
                                    item.get("type")?.as_str()?.to_string(),
                                    item.get("name")?.as_str()?.to_string(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                linked += state.store.record_verdict(
                    actor, id, scope, &entities, tier.as_str(), llm.name())?;
                mined += 1;
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(120)).await; // unfair mutex
    if cycle_started.elapsed() > std::time::Duration::from_secs(EXTRACT_CYCLE_BUDGET_SECS) {
        tracing::info!(
            "extraction cycle budget spent after {mined} episodes — the rest continues next cycle"
        );
        break;
    }
    }
    Ok((mined, linked))
}

/// Ask the LLM to refine coarse scopes. Returns (classified, relabeled).
async fn classify_scopes(
    state: &AppState,
    llm: &dyn LlmProvider,
    actor: &Actor,
) -> anyhow::Result<(usize, usize)> {
    let pending = state
        .store
        .episodes_needing_scope_classification(CLASSIFY_PER_CYCLE)?;
    if pending.is_empty() {
        return Ok((0, 0));
    }
    let system = "You classify episodes of a personal knowledge base by life context. \
        For each item decide: 'work' (professional/company/client matters), 'personal' \
        (private life, hobbies, curiosity, family), or 'unclear'. Respond ONLY with a \
        JSON object mapping each id to one of those three strings.";

    let mut classified = 0usize;
    let mut relabeled = 0usize;
    for batch in pending.chunks(CLASSIFY_BATCH) {
        let mut prompt = String::from("Classify these episodes:\n\n");
        for (id, title, scope, snippet) in batch {
            prompt.push_str(&format!(
                "id: {id}\ncurrent label: {scope}\ntitle: {title}\nexcerpt: {}\n\n",
                snippet.replace('\n', " ")
            ));
        }
        let response = crate::llm::call(&state.store, llm, "classification", None, Tier::Low, system, &prompt, 2000).await?.text;
        // Tolerate code fences around the JSON.
        let json_text = response
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        let verdicts: serde_json::Map<String, Value> = serde_json::from_str(json_text)
            .map_err(|e| anyhow::anyhow!("unparseable classifier response: {e}"))?;
        for (id, _, current, _) in batch {
            let verdict = verdicts.get(id).and_then(|v| v.as_str());
            let new_scope = match verdict {
                Some("work") | Some("personal") if verdict != Some(current.as_str()) => verdict,
                _ => None,
            };
            if state
                .store
                .apply_scope_classification(actor, id, new_scope)?
            {
                relabeled += 1;
            }
            classified += 1;
        }
    }
    Ok((classified, relabeled))
}

/// At most one digest per day, and only when episodes arrived since the
/// last one. Returns true if a new digest episode was written.
async fn semantic_summary(store: &Store, home: &Home, llm: &dyn LlmProvider) -> anyhow::Result<bool> {
    let today = hearthloom_core::time::now_utc()[..10].to_string();
    let last = store.latest_digest_written()?;
    if let Some(previous) = &last {
        if previous.source_ref.ends_with(&today) {
            return Ok(false); // today's digest already exists
        }
    }
    let recent = store.recent_episodes(120)?;
    // Only digest real activity, not previous digests.
    let activity: Vec<&hearthloom_core::store::EpisodeRow> = recent
        .iter()
        .filter(|episode| episode.source != "consolidation")
        .collect();
    if activity.is_empty() {
        return Ok(false);
    }
    if let Some(previous) = &last {
        // Nothing new since the last digest → skip.
        if activity
            .iter()
            .all(|episode| episode.ingest_time <= previous.ingest_time)
        {
            return Ok(false);
        }
    }
    compose_digest(store, home, llm, &today, format!("summary:{today}"), &activity).await?;
    Ok(true)
}

/// Write one daily digest of `activity` under `source_ref`, recording what
/// it read. True when it landed new.
async fn compose_digest(
    store: &Store,
    home: &Home,
    llm: &dyn LlmProvider,
    today: &str,
    source_ref: String,
    activity: &[&hearthloom_core::store::EpisodeRow],
) -> anyhow::Result<bool> {
    let digest_id = hearthloom_core::ids::episode_id("consolidation", &source_ref);
    let mut listing = String::new();
    for episode in activity.iter().take(120) {
        listing.push_str(&format!(
            "- {} [{}/{}] ({}) {}\n",
            episode
                .event_time
                .as_deref()
                .map(|t| &t[..10])
                .unwrap_or("????-??-??"),
            episode.source,
            episode.kind,
            episode.scope,
            episode.title.chars().take(110).collect::<String>(),
        ));
    }
    let system = "You are the sleep-time consolidation process of Hearthloom, the user's \
        local second brain. You compress recent episodic memory into durable semantic \
        memory. Be specific and factual; never invent activity that is not in the input.";
    let prompt = format!(
        "Recent episodes (newest first):\n\n{listing}\n\
         Write two sections in plain markdown:\n\
         ## Activity digest\n5-10 bullets compressing this into semantic facts \
         (projects worked on and what changed, people interacted with, decisions made). \
         Prefer patterns over single events.\n\
         ## Self-model notes\n2-4 bullets on what this activity suggests about the \
         user's current focus and priorities. Mark inferences as inferences."
    );
    let digest = crate::llm::call(store, llm, "daily_digest", None, automation_tier(store, "daily_digest"), system, &prompt, 1200).await?.text;

    let batch = IngestBatch {
        actor: "agent:consolidation".into(),
        session_id: None,
        episodes: vec![EpisodeDraft {
            source: "consolidation".into(),
            source_ref,
            kind: "summary".into(),
            title: format!("Consolidation digest {today}"),
            body: digest,
            event_time: EventTime {
                utc: Some(format!("{today}T00:00:00Z")),
                tz_offset_minutes: None,
                uncertainty: TimeUncertainty::Day,
            },
            trust: "agent".into(),
            scope: Some("personal".into()),
            entities: vec![],
            edges: vec![],
        }],
    };
    let report = store.apply_batch(home, &batch)?;
    if report.episodes_new > 0 {
        let read: Vec<String> = activity.iter().take(120).map(|e| e.id.clone()).collect();
        store.record_summary_inputs(&digest_id, &read)?;
    }
    Ok(report.episodes_new > 0)
}

/// Summaries rewritten per cycle after a retirement (one LLM call each).
const REWRITES_PER_CYCLE: usize = 4;

/// D.12: write again the summaries a retirement ticked — from what each one
/// read, minus what was retired (or, for one written before summaries
/// recorded their inputs, its time window as it stands now) — under
/// `<ref>@<retirement>`, at the same place on the timeline. Runs where the
/// thinking role is; a job that comes out empty is marked done.
async fn rewrite_summaries(state: &AppState, llm: &dyn LlmProvider, actor: &Actor) -> anyhow::Result<usize> {
    let mut written = 0usize;
    for job in state.store.rewrite_jobs(REWRITES_PER_CYCLE)? {
        let window = hearthloom_core::store::base_ref(&job.source_ref);
        let outcome = match job.kind.as_str() {
            "summary" => {
                let day = window.trim_start_matches("summary:").to_string();
                let rows = match state.store.recorded_inputs(&job.summary_id)? {
                    Some(rows) => rows,
                    None => state.store.digest_inputs_as_of(&job.ingest_time, 120)?,
                };
                let activity: Vec<&hearthloom_core::store::EpisodeRow> =
                    rows.iter().filter(|e| e.source != "consolidation").collect();
                if activity.is_empty() {
                    Ok(false)
                } else {
                    compose_digest(&state.store, &state.home, llm, &day, job.new_ref.clone(), &activity).await
                }
            }
            "review" => {
                let week = window.trim_start_matches("review:").to_string();
                let from = chrono::DateTime::parse_from_rfc3339(&job.ingest_time)
                    .map(|t| (t.with_timezone(&chrono::Utc) - chrono::Duration::days(7))
                        .format("%Y-%m-%dT%H:%M:%SZ").to_string())
                    .unwrap_or_else(|_| job.ingest_time.clone());
                let mut inputs = state.store.weekly_review_inputs_between(&from, &job.ingest_time)?;
                if let Some(rows) = state.store.recorded_inputs(&job.summary_id)? {
                    inputs.episodes = rows
                        .iter()
                        .filter(|e| !e.title.is_empty() && e.kind != "health_day" && e.kind != "chapter")
                        .map(|e| {
                            let day = e.event_time.as_deref().unwrap_or(&e.ingest_time);
                            (e.id.clone(), format!("{} [{}] {}", &day[..day.len().min(10)], e.kind, e.title))
                        })
                        .collect();
                }
                if inputs.episodes.is_empty() {
                    Ok(false)
                } else {
                    compose_weekly_review(state, llm, actor, &week, job.new_ref.clone(), &inputs).await
                }
            }
            "chapter" => {
                let month = window.trim_start_matches("chapter:").to_string();
                let titles: Vec<(String, String)> = match state.store.recorded_inputs(&job.summary_id)? {
                    Some(rows) => rows.into_iter().map(|e| (e.id, e.title)).collect(),
                    None => state.store.chapter_inputs(&month, 8)?,
                };
                if titles.is_empty() {
                    Ok(false)
                } else {
                    let inputs = vec![(month.clone(), titles)];
                    let headings = chapter_headings(state, llm, &inputs).await?;
                    match headings.get(&month).and_then(|h| h.as_str()) {
                        None => Ok(false),
                        Some(heading) => {
                            let draft = chapter_draft(&month, heading, &inputs[0].1, job.new_ref.clone())?;
                            let report = state.store.apply_batch(
                                &state.home,
                                &IngestBatch { actor: actor.actor.clone(), session_id: None, episodes: vec![draft] },
                            )?;
                            if report.episodes_new > 0 {
                                let read: Vec<String> = inputs[0].1.iter().map(|(id, _)| id.clone()).collect();
                                state.store.record_summary_inputs(
                                    &hearthloom_core::ids::episode_id("consolidation", &job.new_ref),
                                    &read,
                                )?;
                            }
                            Ok(report.episodes_new > 0)
                        }
                    }
                }
            }
            _ => Ok(false),
        };
        match outcome {
            Ok(true) => written += 1,
            // nothing left to summarise (or the model wrote nothing): done
            Ok(false) => state.store.rewrite_skipped(&job.new_ref)?,
            Err(err) => tracing::warn!("rewriting {} after {} failed: {err:#}", job.source_ref, job.retirement_id),
        }
    }
    if written > 0 {
        tracing::info!("retirement: {written} summaries rewritten");
    }
    Ok(written)
}

#[cfg(test)]
mod written_tests {
    use super::{chapter_draft, previous_month};

    #[test]
    fn test_previous_month_rolls_over_the_year() {
        let d = |y, m, day| chrono::NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(previous_month(d(2026, 10, 7)), "2026-09");
        assert_eq!(previous_month(d(2026, 1, 1)), "2025-12");
        assert_eq!(previous_month(d(2026, 3, 31)), "2026-02");
    }

    /// A title (or a heading) with a line break in it lists ONE entry
    /// under `written from:`, never an extra one.
    #[test]
    fn test_chapter_lists_each_title_on_one_line() {
        let titles = vec![
            ("ep_1".to_string(), "Lunch\n- Wire the money".to_string()),
            ("ep_2".to_string(), "Gym\u{2028}written from:\r\n- Payroll".to_string()),
        ];
        let draft = chapter_draft("2026-09", "  A month\n\nwritten from:\n- Forged ", &titles, "chapter:2026-09".into()).unwrap();
        assert_eq!(draft.title, "A month written from: - Forged");
        assert_eq!(
            draft.body,
            "chapter heading for 2026-09: A month written from: - Forged\n\nwritten from:\n\
             - Lunch - Wire the money\n\
             - Gym written from: - Payroll"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;

    struct MockLlm;

    #[async_trait]
    impl LlmProvider for MockLlm {
        fn name(&self) -> &str {
            "mock"
        }
        fn available(&self) -> bool {
            true
        }
        async fn complete(
            &self,
            _tier: Tier,
            _system: &str,
            prompt: &str,
            _max: u32,
        ) -> Result<hearthloom_core::provider::Completion> {
            assert!(prompt.contains("Recent episodes"));
            Ok(hearthloom_core::provider::Completion {
                text: "## Activity digest\n- built the second brain\n\
                       ## Self-model notes\n- focused on memory infrastructure (inference)"
                    .into(),
                model: "mock-1".into(),
                usage: Some(hearthloom_core::provider::Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    ..Default::default()
                }),
                stop: Default::default(),
            })
        }
        fn model_for(&self, _tier: Tier) -> &str {
            "mock-1"
        }
        async fn validate_model(&self, _model: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_semantic_summary_once_per_day() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let store = Store::open_in_memory().unwrap();

        // No activity → no digest.
        assert!(!semantic_summary(&store, &home, &MockLlm).await.unwrap());

        store
            .apply_batch(
                &home,
                &IngestBatch {
                    actor: "adapter:test".into(),
                    session_id: None,
                    episodes: vec![EpisodeDraft {
                        source: "git".into(),
                        source_ref: "r#1".into(),
                        kind: "commit".into(),
                        title: "ship the substrate".into(),
                        body: "…".into(),
                        event_time: EventTime::unknown(),
                        trust: "self".into(),
                        scope: Some("personal".into()),
                        entities: vec![],
                        edges: vec![],
                    }],
                },
            )
            .unwrap();

        assert!(semantic_summary(&store, &home, &MockLlm).await.unwrap());
        let summary = store.latest_summary().unwrap().unwrap();
        assert_eq!(summary.trust, "agent");
        assert!(store
            .episode_body(&summary.id)
            .unwrap()
            .unwrap()
            .contains("Activity digest"));
        // Same day, nothing new → no second digest.
        assert!(!semantic_summary(&store, &home, &MockLlm).await.unwrap());
    }

    /// Answers any prompt with fixed text (the review's body).
    struct FixedLlm(&'static str);

    #[async_trait]
    impl LlmProvider for FixedLlm {
        fn name(&self) -> &str {
            "fixed"
        }
        fn available(&self) -> bool {
            true
        }
        async fn complete(&self, _t: Tier, _s: &str, _p: &str, _m: u32) -> Result<hearthloom_core::provider::Completion> {
            Ok(hearthloom_core::provider::Completion {
                text: self.0.into(),
                model: "fixed-1".into(),
                usage: None,
                stop: Default::default(),
            })
        }
        fn model_for(&self, _tier: Tier) -> &str {
            "fixed-1"
        }
        async fn validate_model(&self, _model: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_weekly_review_announces_itself_once() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(Store::open(&home.db_path()).unwrap()),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let episodes = (0..6)
            .map(|i| EpisodeDraft {
                source: "git".into(),
                source_ref: format!("r#{i}"),
                kind: "commit".into(),
                title: format!("commit number {i}"),
                body: "…".into(),
                event_time: EventTime::parse_rfc3339(&now, TimeUncertainty::Exact).unwrap(),
                trust: "self".into(),
                scope: Some("personal".into()),
                entities: vec![],
                edges: vec![],
            })
            .collect();
        state
            .store
            .apply_batch(&state.home, &IngestBatch { actor: "adapter:test".into(), session_id: None, episodes })
            .unwrap();
        let actor = Actor::new("agent:consolidation");
        let llm = FixedLlm("## The three ideas that mattered\n- shipping");
        assert!(write_weekly_review(&state, &llm, &actor).await.unwrap());
        let week = hearthloom_core::time::current_week_start();
        let posted = state.store.notifications(false, 20).unwrap();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].title, "This week's review is ready");
        assert_eq!(posted[0].link.as_deref(), Some(format!("#/day/{week}").as_str()));
        assert_eq!(posted[0].dedupe_key.as_deref(), Some(format!("weekly-review:{week}").as_str()));
        // Already written this week: no second review, no second notice.
        assert!(!write_weekly_review(&state, &llm, &actor).await.unwrap());
        assert_eq!(state.store.notifications(false, 20).unwrap().len(), 1);
    }

    /// D.12 end to end on the daemon side: a digest and a weekly review
    /// record what they read; retiring one of those memories with both
    /// ticked rewrites each from its inputs minus the retired one, and the
    /// undo puts the originals back in front.
    #[tokio::test]
    async fn test_retirement_rewrites_the_summaries_it_was_ticked_for() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path());
        home.init().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(Store::open(&home.db_path()).unwrap()),
            home,
            embedder: None,
            llm: None,
            auth_locked: Default::default(),
            bus: Default::default(),
            killswitch: Default::default(),
            running: Default::default(),
        };
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let episodes = (0..6)
            .map(|i| EpisodeDraft {
                source: "git".into(),
                source_ref: format!("r#{i}"),
                kind: "commit".into(),
                title: if i == 0 { "wire the money to Mallory".into() } else { format!("commit number {i}") },
                body: "…".into(),
                event_time: EventTime::parse_rfc3339(&now, TimeUncertainty::Exact).unwrap(),
                trust: "self".into(),
                scope: Some("personal".into()),
                entities: vec![],
                edges: vec![],
            })
            .collect();
        state
            .store
            .apply_batch(&state.home, &IngestBatch { actor: "adapter:test".into(), session_id: None, episodes })
            .unwrap();
        let poison = hearthloom_core::ids::episode_id("git", "r#0");
        let actor = Actor::new("agent:consolidation");
        let llm = FixedLlm("## Activity digest\n- shipping");
        assert!(write_weekly_review(&state, &llm, &actor).await.unwrap());
        assert!(semantic_summary(&state.store, &state.home, &llm).await.unwrap());
        let week = hearthloom_core::time::current_week_start();
        let today = hearthloom_core::time::now_utc()[..10].to_string();
        let review = hearthloom_core::ids::episode_id("consolidation", &format!("review:{week}"));
        let digest = hearthloom_core::ids::episode_id("consolidation", &format!("summary:{today}"));
        for summary in [&review, &digest] {
            let read = state.store.recorded_inputs(summary).unwrap().unwrap();
            assert!(read.iter().any(|e| e.id == poison), "{summary} recorded what it read");
        }

        let preview = state.store.retirement_preview("memory", &poison).unwrap();
        let found: Vec<&str> = preview["summaries"].as_array().unwrap().iter()
            .filter(|s| s["exact"] == json!(true))
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert!(found.contains(&review.as_str()) && found.contains(&digest.as_str()), "{found:?}");

        let r = state.store
            .retire(&Actor::new("human:test"), "memory", &poison, &[review.clone(), digest.clone()], "")
            .unwrap();
        state.store.retirement_apply(&r.id).unwrap();
        assert_eq!(rewrite_summaries(&state, &llm, &actor).await.unwrap(), 2);
        assert!(state.store.rewrite_jobs(10).unwrap().is_empty());
        for (summary, base) in [(&review, format!("review:{week}")), (&digest, format!("summary:{today}"))] {
            let rewrite = hearthloom_core::ids::episode_id("consolidation", &format!("{base}@{}", r.id));
            assert!(state.store.episode(&rewrite).unwrap().is_some(), "{base} rewritten and live");
            assert!(state.store.episode(summary).unwrap().is_none(), "{base} original retired");
            let read = state.store.recorded_inputs(&rewrite).unwrap().unwrap();
            assert!(!read.is_empty() && read.iter().all(|e| e.id != poison), "{base} rewritten without it");
        }

        state.store.retirement_undo(&Actor::new("human:test"), &r.id).unwrap();
        for (summary, base) in [(&review, format!("review:{week}")), (&digest, format!("summary:{today}"))] {
            let rewrite = hearthloom_core::ids::episode_id("consolidation", &format!("{base}@{}", r.id));
            assert!(state.store.episode(summary).unwrap().is_some(), "{base} original back");
            assert!(state.store.episode(&rewrite).unwrap().is_none(), "{base} rewrite stepped back");
        }
        assert!(state.store.episode(&poison).unwrap().is_some());
    }

    #[test]
    fn test_automation_tier_resolution() {
        let store = Store::open_in_memory().unwrap();
        // Absent config → each pass's recommended default (Phase-1 tiers).
        assert_eq!(automation_tier(&store, "daily_digest"), Tier::Medium);
        assert_eq!(automation_tier(&store, "topic_pages"), Tier::High);
        // A stored override wins; garbage falls back to the default.
        store
            .set_consolidation_state("settings.daily_digest.tier", "high")
            .unwrap();
        assert_eq!(automation_tier(&store, "daily_digest"), Tier::High);
        store
            .set_consolidation_state("settings.topic_pages.tier", "nonsense")
            .unwrap();
        assert_eq!(automation_tier(&store, "topic_pages"), Tier::High);
    }

    #[test]
    fn test_extraction_tier_and_enabled_sources() {
        let store = Store::open_in_memory().unwrap();
        // Recommended defaults: prose sources on, big/structural off.
        assert_eq!(extraction_tier(&store, "chatlog-claude"), Some(Tier::High));
        assert_eq!(extraction_tier(&store, "substack"), Some(Tier::Low));
        assert_eq!(extraction_tier(&store, "mbox"), None);
        assert_eq!(extraction_tier(&store, "git"), None); // never eligible
        // Pre-Phase-3b behavior: exactly the Some(_) rows are enabled.
        let enabled: Vec<&str> = enabled_extraction_sources(&store)
            .iter()
            .map(|(s, _)| *s)
            .collect();
        assert!(enabled.contains(&"chatlog-claude"));
        assert!(enabled.contains(&"imessage"));
        assert!(!enabled.contains(&"mbox"));
        // Force mbox on and disable substack — both overrides take effect.
        store
            .set_consolidation_state("extract.mbox.tier", "low")
            .unwrap();
        store
            .set_consolidation_state("extract.substack.tier", "disabled")
            .unwrap();
        assert_eq!(extraction_tier(&store, "mbox"), Some(Tier::Low));
        assert_eq!(extraction_tier(&store, "substack"), None);
    }
}

#[cfg(test)]
mod email_evidence_tests {
    use super::email_matches_name;

    #[test]
    fn test_email_matches_name_shapes() {
        // the shapes mail actually mints
        assert!(email_matches_name("mreyes", "Morgan Reyes"));
        assert!(email_matches_name("morgan.reyes", "Morgan Reyes"));
        assert!(email_matches_name("reyes.morgan", "Morgan Reyes"));
        assert!(email_matches_name("morganreyes", "Morgan Reyes"));
        assert!(email_matches_name("morgan.reyes89", "Morgan Reyes"));
        // same first name, different person — the flood Jon reported
        assert!(!email_matches_name("jsmith", "Morgan Reyes"));
        assert!(!email_matches_name("morgan.smith", "Morgan Reyes"));
        // bare names never match on email evidence (nothing to anchor)
        assert!(!email_matches_name("mreyes", "Morgan"));
        // short surnames are too ambiguous ("li")
        assert!(!email_matches_name("mli", "Morgan Li"));
    }
}

#[cfg(test)]
mod route_tests {
    use super::workout_route_start;

    const GPX: &str = r#"<gpx><trk><trkseg><trkpt lat="42.3601" lon="-71.0589"><ele>5</ele></trkpt></trkseg></trk></gpx>"#;

    /// A workout's route is read inside the archive mount and nowhere
    /// else: `..`, an absolute path or a prefix component never opens a
    /// file outside it.
    #[test]
    fn test_workout_route_stays_inside_the_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("apple-health");
        std::fs::create_dir_all(archive.join("workout-routes")).unwrap();
        std::fs::write(archive.join("workout-routes/route_1.gpx"), GPX).unwrap();
        // the same trackpoints just OUTSIDE the archive
        std::fs::write(tmp.path().join("outside.gpx"), GPX).unwrap();

        let body = |route: &str| format!("workout: Running\nstart: x\nend: y\nroute: {route}\n");
        let start = Some((42.3601, -71.0589));
        assert_eq!(workout_route_start(&archive, &body("/workout-routes/route_1.gpx")), start);
        assert_eq!(workout_route_start(&archive, &body("workout-routes/route_1.gpx")), start);
        for escape in [
            "../outside.gpx",
            "/../outside.gpx",
            "workout-routes/../../outside.gpx",
            "./../outside.gpx",
        ] {
            assert_eq!(workout_route_start(&archive, &body(escape)), None, "{escape:?} must be refused");
        }
        // an absolute path is taken as archive-relative: nothing there
        let absolute = tmp.path().join("outside.gpx").display().to_string();
        assert_eq!(workout_route_start(&archive, &body(&absolute)), None);
        assert_eq!(workout_route_start(&archive, "workout: Running\n"), None);
    }
}
