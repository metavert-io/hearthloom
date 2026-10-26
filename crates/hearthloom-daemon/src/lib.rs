//! The Hearthloom daemon: the ONLY process that opens the SQLite file.
//! Everything — CLI, agents, adapters, future dashboard — talks to this
//! axum server on loopback (design law #4).
//!
//! Surface: a plain HTTP API under /v1 plus a stateless streamable-HTTP
//! MCP endpoint at /mcp with exactly four tools.

pub mod apm;
mod agents;
mod budget;
mod retire;
mod bus;
mod consolidate;
mod dashboard;
mod downloads;
mod embeddings_dl;
mod export;
mod gmail_pull;
mod health_index;
mod http;
mod inbox;
mod integrations;
mod live_health;
mod live_ingest;
pub mod llm;
mod live_photos;
mod visits;
mod mbox_index;
mod mcp;
mod mdvault;
mod monitor;
mod network_keys;
mod network_out;
mod notion;
mod peerkey;
pub mod plugins;
pub mod registry_client;
mod provisioner;
mod rlm;
mod scoped;
mod sidecar;
mod trips;
mod sync;
mod views;
mod virtual_source;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use hearthloom_core::config::Config;
use hearthloom_core::embeddings::Embedder;
use hearthloom_core::provider::{self, LlmProvider, TierModels};
use hearthloom_core::{Home, Store};

/// Resolve the active LLM provider from DB config (syncable, non-secret)
/// over env/YAML fallback; the key comes from the sealed env per provider.
/// DB is the source of truth so provider/tier choices ride the peer mirror.
pub fn resolve_llm(store: &Store, config: &Config) -> Result<Box<dyn LlmProvider>> {
    resolve_llm_with(store, &config.provider.llm)
}

/// The `custom` endpoint's address: DB config, else the
/// `HEARTHLOOM_LLM_BASE_URL` env. NODE-LOCAL by design (never in the LLM
/// sync envelope): it is usually a loopback server only this machine
/// can reach.
pub fn custom_base_url(store: &Store) -> Option<String> {
    store
        .consolidation_state("llm.custom.base_url")
        .ok()
        .flatten()
        .or_else(|| std::env::var("HEARTHLOOM_LLM_BASE_URL").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `resolve_llm` with the YAML/env fallback provider passed in. On top
/// of the active provider, any tier may be routed to a different one via
/// `llm.route.<tier>.provider` + `llm.route.<tier>.model` — node-local
/// keys (see `RoutedProvider`), stored apart from `llm.model.<tier>` so
/// a routed model id never rides the envelope onto a peer whose tier is
/// served by another provider.
pub fn resolve_llm_with(store: &Store, fallback_provider: &str) -> Result<Box<dyn LlmProvider>> {
    let state_of = |key: &str| {
        store
            .consolidation_state(key)
            .ok()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let provider = state_of("llm.provider").unwrap_or_else(|| fallback_provider.to_string());
    let defaults = TierModels::defaults_for(&provider);
    let models = TierModels {
        low: state_of("llm.model.low").unwrap_or(defaults.low),
        medium: state_of("llm.model.medium").unwrap_or(defaults.medium),
        high: state_of("llm.model.high").unwrap_or(defaults.high),
    };
    let key_for = |name: &str| {
        std::env::var(crate::integrations::provider_key_env(name)).unwrap_or_default()
    };
    let base_url = custom_base_url(store);
    // `llm.effort.<tier>`: run a tier's model at another effort level
    // (always-thinking models only). Node-local, like the routes.
    let efforts = provider::Efforts([
        state_of("llm.effort.low"),
        state_of("llm.effort.medium"),
        state_of("llm.effort.high"),
    ]);
    let base = provider::build_provider_full(
        &provider, &key_for(&provider), models, base_url.as_deref(), efforts.clone(),
    )?;

    let mut routed = provider::RoutedProvider::new(base);
    for tier in [provider::Tier::Low, provider::Tier::Medium, provider::Tier::High] {
        let t = tier.as_str();
        let Some(name) = state_of(&format!("llm.route.{t}.provider")) else {
            continue;
        };
        let routed_model = state_of(&format!("llm.route.{t}.model"));
        // Same provider with no model named is no route at all. Same
        // provider with a DIFFERENT model is a real one: it changes this
        // node's model for the tier without touching `llm.model.<tier>`,
        // which syncs to the peer (how a model swap is trialled on the
        // laptop without switching the always-on node's passes).
        if name == provider && routed_model.is_none() {
            continue;
        }
        let model = routed_model
            .unwrap_or_else(|| TierModels::defaults_for(&name).get(tier).to_string());
        let one = TierModels { low: model.clone(), medium: model.clone(), high: model };
        // A route that can't serve (no key, no address, unknown name)
        // falls back to the active provider — a typo must not silence a tier.
        match provider::build_provider_full(&name, &key_for(&name), one, base_url.as_deref(), efforts.clone()) {
            // (A same-provider override is as usable as the base itself —
            // falling back would only swap the model back, not rescue it.)
            Ok(p) if p.available() || name == provider => routed = routed.route(tier, p),
            Ok(_) => tracing::warn!("llm route {t} → {name} is not usable (no key/address); using {provider}"),
            Err(err) => tracing::warn!("llm route {t} → {name} ignored: {err:#}"),
        }
    }
    Ok(if routed.has_routes() { Box::new(routed) } else { routed.into_base() })
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub home: Home,
    /// Local embedder, when configured and the model files exist.
    pub embedder: Option<Arc<Embedder>>,
    /// LLM provider, when configured with a usable key. Only the
    /// consolidation cycle calls it (design law #3).
    pub llm: Option<Arc<dyn LlmProvider>>,
    /// Cached posture rule: an admin user exists, so every request must
    /// authenticate. Set at startup, flipped by /v1/auth/bootstrap —
    /// cached so the middleware never takes the store lock per request.
    pub auth_locked: Arc<std::sync::atomic::AtomicBool>,
    /// The push bus (15b): replay ring + live fan-out for node-to-node
    /// event hints. Notify-then-fetch — see bus.rs.
    pub bus: Arc<bus::Bus>,
    /// The provisioner killswitch, cached like auth_locked so the
    /// middleware never takes the store lock per request. Flipped only
    /// by the control plane (provisioner.rs) — no PAT can touch it.
    pub killswitch: Arc<std::sync::atomic::AtomicBool>,
    /// Single-flight registry: automation keys currently running (scheduled
    /// OR manually triggered). The scheduled loop and the manual-run
    /// endpoint both go through `try_start_automation`, so a manual run and
    /// a scheduled run of the same pass can never overlap — whoever inserts
    /// the key first runs; the other backs off.
    pub running: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    /// Embed a query if an embedder is loaded (spawn_blocking-free: MiniLM
    /// on 128 tokens is ~ms).
    pub fn query_embedding(&self, query: &str) -> Option<Vec<f32>> {
        self.embedder
            .as_ref()
            .and_then(|embedder| embedder.embed(query).ok())
    }

    /// Atomically claim an automation key. Returns an RAII guard that
    /// releases on drop, or `None` if the key is already running.
    pub fn try_start_automation(&self, key: &str) -> Option<AutomationGuard> {
        let mut set = self.running.lock().unwrap();
        if set.contains(key) {
            return None;
        }
        set.insert(key.to_string());
        Some(AutomationGuard {
            running: self.running.clone(),
            key: key.to_string(),
        })
    }

    pub fn is_automation_running(&self, key: &str) -> bool {
        self.running.lock().unwrap().contains(key)
    }
}

/// Releases its automation key from the running set when dropped — so a
/// panicking or early-returning pass never leaves a key stuck "running".
pub struct AutomationGuard {
    running: Arc<Mutex<HashSet<String>>>,
    key: String,
}

impl Drop for AutomationGuard {
    fn drop(&mut self) {
        self.running.lock().unwrap().remove(&self.key);
    }
}

fn load_embedder(config: &Config, home: &Home) -> Result<Option<Arc<Embedder>>> {
    match config.provider.embeddings.as_str() {
        "none" => Ok(None),
        "local-onnx" => Ok(Some(Arc::new(Embedder::load(&home.models_dir())?))),
        // auto: use the model if present, degrade silently if not.
        _ => {
            if home.models_dir().join("all-MiniLM-L6-v2.onnx").exists() {
                match Embedder::load(&home.models_dir()) {
                    Ok(embedder) => Ok(Some(Arc::new(embedder))),
                    Err(err) => {
                        tracing::warn!("embedder failed to load, continuing without: {err}");
                        Ok(None)
                    }
                }
            } else {
                tracing::info!(
                    "no embedding model in {} — search is FTS-only (drop \
                     all-MiniLM-L6-v2.onnx + tokenizer.json there to enable hybrid search)",
                    home.models_dir().display()
                );
                Ok(None)
            }
        }
    }
}

/// Run the daemon until shutdown. Binds localhost only by default; no
/// auth is required for v0 loopback, but the bearer-token plumbing in
/// http::auth is the seam where scoped tokens land.
pub async fn run(home: Home) -> Result<()> {
    home.init()?;
    let config = Config::load(&home.config_dir())?;
    // Optional APM: Config::load just decrypted the sealed env, so
    // SENTRY_DSN (if the operator configured one) is now visible. No
    // DSN → None and everything downstream is a no-op. The guard lives
    // as long as run() (i.e. the process) and flushes on drop.
    let _sentry_guard = apm::init();
    // a staged restore (wizard upload) swaps in BEFORE the store opens
    export::apply_incoming_restore(&home)?;
    let store = Store::open(&home.db_path())
        .with_context(|| format!("opening {}", home.db_path().display()))?;
    // User consent (Settings toggle, else the distribution default) —
    // gates every event/transaction client-side from this moment.
    apm::set_consent(apm::resolve_consent(&store));
    let embedder = load_embedder(&config, &home)?;
    if embedder.is_some() {
        tracing::info!("local embedder loaded (hybrid search enabled)");
    }
    let llm: Option<Arc<dyn LlmProvider>> = {
        let provider = resolve_llm(&store, &config)?;
        if provider.available() {
            tracing::info!("LLM provider {} available for consolidation", provider.name());
            Some(Arc::from(provider))
        } else {
            None
        }
    };
    let auth_locked = Arc::new(std::sync::atomic::AtomicBool::new(
        store.auth_required().unwrap_or(false),
    ));
    let killswitch = Arc::new(std::sync::atomic::AtomicBool::new(
        provisioner::load_killswitch(&store),
    ));
    let state = AppState {
        store: Arc::new(store),
        home,
        embedder,
        llm,
        auth_locked,
        bus: Default::default(),
        killswitch,
        running: Default::default(),
    };

    // Sleep-time consolidation: LLM/embedding spend happens here, at idle,
    // never on the query path.
    tokio::spawn(consolidate::run_loop(state.clone()));
    // The inbox drop folder: exports placed there ingest themselves.
    tokio::spawn(inbox::run_loop(state.clone()));
    // Hybrid peer sync: dormant until a peer is paired (Integrations).
    tokio::spawn(sync::run_loop(state.clone()));
    tokio::spawn(agents::run_loop(state.clone()));
    // The push bus subscriber (15b): one outbound SSE subscription to
    // the paired peer; feature-detects an old peer and stays poll-only.
    tokio::spawn(bus::run_subscriber(state.clone()));
    // Observability: vitals + dependency probes every minute (v35).
    tokio::spawn(monitor::run_loop(state.clone()));
    // Ember sidecars the daemon supervises itself (hosted/Linux nodes —
    // launchd keeps its own units on a Mac): start the enabled ones.
    {
        let state = state.clone();
        tokio::task::spawn_blocking(move || plugins::start_enabled_sidecars(&state));
    }
    // Best-effort shutdown event (SIGTERM/ctrl-c; SIGKILL can't be seen —
    // its absence in the log IS the crash signal).
    {
        let state = state.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            monitor::log(&state, "warn", "shutdown", "daemon stopping (signal)", None);
            // Supervised embers get SIGTERM while the API still serves —
            // their last flush (up to 5s each) needs it — so the wait runs
            // on the blocking pool, never on a runtime worker.
            let _ = tokio::task::spawn_blocking(sidecar::stop_all).await;
            std::process::exit(0);
        });
    }

    let addr: SocketAddr = format!("{}:{}", config.daemon.host, config.daemon.port)
        .parse()
        .context("bad daemon host/port")?;
    let app = http::router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr} (is another daemon running?)"))?;
    tracing::info!("hearthloom daemon listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthloom_core::provider::Tier;

    #[test]
    fn test_resolve_llm_routes_a_tier() {
        let store = Store::open_in_memory().unwrap();
        // No routes: the plain active provider, every tier.
        let llm = resolve_llm_with(&store, "anthropic").unwrap();
        assert_eq!(llm.provider_for(Tier::Low), "anthropic");

        // Low → a local endpoint; its model id lives apart from llm.model.low.
        store.set_consolidation_state("llm.custom.base_url", "http://localhost:11434/v1").unwrap();
        store.set_consolidation_state("llm.route.low.provider", "custom").unwrap();
        store.set_consolidation_state("llm.route.low.model", "qwen3:8b").unwrap();
        let llm = resolve_llm_with(&store, "anthropic").unwrap();
        assert_eq!(llm.provider_for(Tier::Low), "custom");
        assert_eq!(llm.model_for(Tier::Low), "qwen3:8b");
        assert_eq!(llm.provider_for(Tier::High), "anthropic");
        assert_eq!(llm.model_for(Tier::High), "claude-opus-4-8");
        assert!(llm.available(), "a keyless local route is still usable");

        // Same provider, different model: a node-local model override.
        store.set_consolidation_state("llm.route.high.provider", "anthropic").unwrap();
        store.set_consolidation_state("llm.route.high.model", "claude-opus-5-5").unwrap();
        let llm = resolve_llm_with(&store, "anthropic").unwrap();
        assert_eq!(llm.model_for(Tier::High), "claude-opus-5-5");
        assert_eq!(llm.provider_for(Tier::High), "anthropic");
        assert_eq!(llm.model_for(Tier::Medium), "claude-sonnet-4-6");
        store.set_consolidation_state("llm.route.high.model", "").unwrap();
        assert_eq!(resolve_llm_with(&store, "anthropic").unwrap().model_for(Tier::High), "claude-opus-4-8");

        // A route that can't serve (no address) falls back to the base.
        store.set_consolidation_state("llm.custom.base_url", "").unwrap();
        if std::env::var("HEARTHLOOM_LLM_BASE_URL").is_err() {
            let llm = resolve_llm_with(&store, "anthropic").unwrap();
            assert_eq!(llm.provider_for(Tier::Low), "anthropic");
        }
    }
}
