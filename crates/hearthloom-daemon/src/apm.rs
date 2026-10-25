//! Optional Sentry APM (errors + performance traces), designed for an
//! open-source local-first project — so it is DOUBLY gated:
//!
//! 1. **Build/operator config** — the SDK activates only when
//!    `SENTRY_DSN` is present in the environment at boot (on a personal
//!    node that means `~/.hearthloom/.env.sealed`, never the repo; on a
//!    hosted tenant, machine env). No DSN → `init` returns None
//!    and every hook in this module is a quiet no-op. Nothing in the
//!    published source names any Sentry account.
//! 2. **User consent** — the Settings toggle ("share error reports",
//!    stored as consolidation_state `telemetry.error_reporting`). Until
//!    it is ON, `before_send` drops every event and `traces_sampler`
//!    returns 0, so nothing leaves the process even on a DSN-configured
//!    build. Distributions choose their default via
//!    `SENTRY_DEFAULT_OPTIN=1` (a hosted tenant's machine env may
//!    set it; self-hosted builds default OFF) — an explicit user
//!    choice always wins over the distribution default.
//!
//! Privacy posture (deliberately stricter than Sentry's quickstart):
//! `send_default_pii` stays FALSE — request headers carry bearer PATs
//! and session cookies, and this daemon's whole design promises the
//! substrate never leaks. What ships: panics, `tracing::error!` events
//! (log lines, which never contain episode bodies), and HTTP transaction
//! timings named by axum route template. The iOS companion's Sentry SDK
//! sends `sentry-trace`/`baggage` headers with its requests; the tower
//! layer continues those traces, which is what stitches phone → daemon
//! end-to-end in Sentry's UI.

use std::sync::atomic::{AtomicBool, Ordering};

use hearthloom_core::store::Store;

/// Runtime consent flag — consulted by before_send/traces_sampler on
/// every event, so the Settings toggle applies instantly, no restart.
static CONSENT: AtomicBool = AtomicBool::new(false);

pub(crate) fn consented() -> bool {
    CONSENT.load(Ordering::Relaxed)
}

pub(crate) fn set_consent(on: bool) {
    CONSENT.store(on, Ordering::Relaxed);
}

/// The stored user choice, else the distribution default.
/// consolidation_state `telemetry.error_reporting`: "on" | "off";
/// unset → `SENTRY_DEFAULT_OPTIN` truthy = on.
pub(crate) fn resolve_consent(store: &Store) -> bool {
    match store
        .consolidation_state("telemetry.error_reporting")
        .ok()
        .flatten()
        .as_deref()
    {
        Some("on") => true,
        Some(_) => false,
        None => std::env::var("SENTRY_DEFAULT_OPTIN")
            .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false),
    }
}

/// Environment label: explicit `SENTRY_ENVIRONMENT` wins; else a
/// hosted tenant (identified by its provisioner control-plane
/// secret) reports as "tenant", anything else as "local".
fn environment() -> String {
    std::env::var("SENTRY_ENVIRONMENT").unwrap_or_else(|_| {
        if std::env::var("HEARTHLOOM_PROVISIONER_SECRET").is_ok() {
            "tenant".into()
        } else {
            "local".into()
        }
    })
}

/// Initialize the SDK iff the operator configured a DSN. Call AFTER the
/// sealed env has loaded (Config::load) — that's where SENTRY_DSN lives
/// on a personal node. The returned guard must live for the process
/// (run() holds it; drop flushes pending events on exit).
pub(crate) fn init() -> Option<sentry::ClientInitGuard> {
    let dsn = std::env::var("SENTRY_DSN").ok().filter(|d| !d.trim().is_empty())?;
    let rate: f32 = std::env::var("SENTRY_TRACES_SAMPLE_RATE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0.1);
    // ClientOptions is non_exhaustive — build by mutation.
    let mut options = sentry::ClientOptions::default();
    options.release = sentry::release_name!();
    options.environment = Some(environment().into());
    // NEVER true: headers carry PATs/session cookies (see module doc).
    options.send_default_pii = false;
    options.before_send = Some(std::sync::Arc::new(|event| {
        if consented() {
            Some(event)
        } else {
            None
        }
    }));
    let options =
        options.traces_sampler(move |ctx: &sentry::TransactionContext| {
            if !consented() {
                return 0.0;
            }
            // health probes (fly checks, the iOS shell, launchd
            // watchdogs) would dominate the quota with zero signal
            if ctx.name() == "GET /health" || ctx.name().ends_with("/health") {
                return 0.0;
            }
            match ctx.sampled() {
                // continue the caller's decision (the iOS app's
                // distributed trace) so phone→daemon traces stitch
                Some(true) => 1.0,
                Some(false) => 0.0,
                None => rate,
            }
        });
    let guard = sentry::init((dsn, options));
    tracing::info!(
        "sentry APM active (release {:?}, environment {}, traces rate {}, consent {})",
        sentry::release_name!(),
        environment(),
        rate,
        consented(),
    );
    Some(guard)
}

/// The tracing bridge: ERROR events become Sentry events, WARN/INFO
/// become breadcrumbs on whatever ships next. Harmless when no client
/// is bound (the layer is a no-op) — both binaries attach it
/// unconditionally at subscriber build time, before the DSN is even
/// known.
pub fn tracing_layer<S>() -> sentry::integrations::tracing::SentryLayer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    sentry::integrations::tracing::layer()
}

/// One JSON status blob for the Settings surface.
pub(crate) fn status(store: &Store) -> serde_json::Value {
    let configured = sentry::Hub::current()
        .client()
        .map(|c| c.is_enabled())
        .unwrap_or(false);
    let stored = store
        .consolidation_state("telemetry.error_reporting")
        .ok()
        .flatten();
    serde_json::json!({
        "configured": configured,
        "enabled": consented(),
        "explicit_choice": stored.is_some(),
        "default_optin": std::env::var("SENTRY_DEFAULT_OPTIN")
            .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false),
        "environment": environment(),
        "release": sentry::release_name!(),
    })
}
