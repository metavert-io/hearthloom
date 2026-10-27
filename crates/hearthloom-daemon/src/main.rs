//! Thin daemon binary; the CLI can also run the same daemon in-process
//! via `hearthloom daemon run` (see hearthloom-daemon's lib).

use anyhow::Result;

// The daemon must keep serving WHILE consolidation grinds: on a
// 1-vCPU cloud machine the default runtime gets ONE worker and a
// boot-time backfill starves the accept loop into 502s (2026-07-18).
#[tokio::main(worker_threads = 4)]
async fn main() -> Result<()> {
    // The Sentry layer is attached unconditionally: it stays inert until
    // (and unless) run() finds a SENTRY_DSN in the sealed env and binds
    // a client (apm.rs).
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(hearthloom_daemon::apm::tracing_layer())
        .init();
    let home = hearthloom_core::Home::resolve()?;
    hearthloom_daemon::run(home).await
}
