//! Observability collector (v35): a 60s loop sampling the daemon's own
//! vitals and probing its dependencies, all written node-tagged so the
//! peer exchange interleaves both nodes' telemetry on either dashboard.
//!
//! Checks (ok/fail ticks): daemon (self — a written tick IS liveness),
//! peer reachability (when paired), anthropic_api reachability (when an
//! LLM key is configured; unauthenticated HEAD — never spends tokens).
//! Samples (numeric): cpu_pct, mem_mb (this process via `ps`), disk_free_gb
//! (the home volume), db_mb (the substrate file).

use serde_json::json;

use crate::AppState;

const SAMPLE_SECS: u64 = 60;

/// Which kind of node is this? Cloud posture = bound beyond loopback.
pub fn node_kind() -> &'static str {
    match std::env::var("HEARTHLOOM_HOST") {
        Ok(h) if h != "127.0.0.1" && !h.is_empty() => "cloud",
        _ => "local",
    }
}

/// Structured event helper — never breaks the caller.
pub fn log(
    state: &AppState,
    level: &str,
    event: &str,
    message: &str,
    meta: Option<serde_json::Value>,
) {
    let _ = state
        .store
        .log_event(node_kind(), level, event, message, meta);
}

/// CPU% between ticks needs the previous reading: (proc jiffies, total jiffies).
static LAST_CPU: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);

fn sample_process(state: &AppState) {
    let node = node_kind();
    let pid = std::process::id().to_string();
    let mut sampled = false;
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "rss=,pcpu=", "-p", &pid])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.split_whitespace();
        if let (Some(rss), Some(cpu)) = (parts.next(), parts.next()) {
            if let Ok(kb) = rss.parse::<f64>() {
                let _ = state.store.record_health(
                    node,
                    "mem_mb",
                    None,
                    Some((kb / 1024.0).round()),
                    Some("mb"),
                );
                sampled = true;
            }
            if let Ok(pct) = cpu.parse::<f64>() {
                let _ = state
                    .store
                    .record_health(node, "cpu_pct", None, Some(pct), Some("%"));
            }
        }
    }
    // The tenant image ships no `ps` (procps absent — the silent
    // "not enough data" on cloud Monitoring, 2026-07-29): read
    // /proc/self directly on Linux. mem = VmRSS; cpu% = delta of
    // process jiffies over delta of total jiffies between ticks.
    if !sampled {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            if let Some(kb) = status.lines().find_map(|l| {
                l.strip_prefix("VmRSS:")
                    .and_then(|r| r.split_whitespace().next())
                    .and_then(|n| n.parse::<f64>().ok())
            }) {
                let _ = state.store.record_health(
                    node,
                    "mem_mb",
                    None,
                    Some((kb / 1024.0).round()),
                    Some("mb"),
                );
            }
        }
        let proc_jiffies = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| {
                // fields 14+15 (utime+stime), AFTER the parenthesized comm
                let rest = s.rsplit_once(") ")?.1;
                let mut f = rest.split_whitespace();
                let ut: u64 = f.nth(11)?.parse().ok()?;
                let st: u64 = f.next()?.parse().ok()?;
                Some(ut + st)
            });
        let total_jiffies = std::fs::read_to_string("/proc/stat").ok().and_then(|s| {
            let line = s.lines().next()?;
            let sum: u64 = line
                .split_whitespace()
                .skip(1)
                .filter_map(|n| n.parse::<u64>().ok())
                .sum();
            Some(sum)
        });
        if let (Some(p), Some(t)) = (proc_jiffies, total_jiffies) {
            let mut last = LAST_CPU.lock().unwrap();
            if let Some((lp, lt)) = *last {
                if t > lt {
                    let ncpu = std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(1);
                    let pct = 100.0 * (p.saturating_sub(lp)) as f64 / (t - lt) as f64 * ncpu as f64;
                    let _ = state.store.record_health(
                        node,
                        "cpu_pct",
                        None,
                        Some((pct * 10.0).round() / 10.0),
                        Some("%"),
                    );
                }
            }
            *last = Some((p, t));
        }
    }
    // disk free on the home volume
    if let Ok(out) = std::process::Command::new("df")
        .args(["-k", &state.home.root().to_string_lossy()])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().nth(1) {
            if let Some(avail) = line.split_whitespace().nth(3) {
                if let Ok(kb) = avail.parse::<f64>() {
                    let _ = state.store.record_health(
                        node,
                        "disk_free_gb",
                        None,
                        Some((kb / 1024.0 / 1024.0 * 10.0).round() / 10.0),
                        Some("gb"),
                    );
                }
            }
        }
    }
    if let Ok(meta) = std::fs::metadata(state.home.db_path()) {
        let _ = state.store.record_health(
            node,
            "db_mb",
            None,
            Some((meta.len() as f64 / 1024.0 / 1024.0).round()),
            Some("mb"),
        );
    }
}

async fn probe_dependencies(state: &AppState) {
    let node = node_kind();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .unwrap_or_default();
    // Every store touch in this loop goes through spawn_blocking: a
    // monitor tick that parks a runtime worker on the store lock while
    // a slow query holds it is one of the four workers the 2026-07-19
    // tenant wedge consumed. Reads first (which peer probe applies)…
    let st = state.clone();
    let Ok((peer_url, peer_inbound)) = tokio::task::spawn_blocking(move || {
        let url = st.store.peer().ok().flatten().map(|(_, url)| url);
        let inbound = st.store.consolidation_state("peer_inbound").ok().flatten();
        (url, inbound)
    })
    .await
    else {
        return;
    };
    // …then the network probes, store-free…
    // general internet reachability — separates "my network is down"
    // from "a dependency is down" when comparing nodes
    let net_ok = client.get("https://one.one.one.one/").send().await.is_ok();
    // Anthropic reachability from EVERY node, LLM key or not — the
    // point is comparing what each node can see. Unauthenticated; any
    // HTTP answer (401 included) proves the wire. Never spends tokens.
    let anthropic_ok = client
        .get("https://api.anthropic.com/v1/models")
        .send()
        .await
        .map(|r| r.status().as_u16() < 500)
        .unwrap_or(false);
    // the peer: outbound probe when paired (the initiating node), else
    // inbound-heartbeat freshness (the receiving node — it never dials).
    let peer_ok: Option<bool> = if let Some(url) = peer_url {
        Some(
            client
                .get(format!("{url}/health"))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false),
        )
    } else {
        peer_inbound
            .and_then(|inbound| serde_json::from_str::<serde_json::Value>(&inbound).ok())
            .and_then(|v| {
                v.get("last_contact")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .map(|last| {
                let fresh = (chrono::Utc::now() - chrono::Duration::minutes(15))
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string();
                last.as_str() >= fresh.as_str()
            })
    };
    // …and one blocking hop writes all the points (the daemon tick
    // included — writing it proves the daemon + store are alive).
    let st = state.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = st
            .store
            .record_health(node, "daemon", Some(true), None, None);
        let _ = st
            .store
            .record_health(node, "internet", Some(net_ok), None, None);
        let _ = st
            .store
            .record_health(node, "anthropic_api", Some(anthropic_ok), None, None);
        if let Some(ok) = peer_ok {
            let _ = st
                .store
                .record_health(node, "peer_node", Some(ok), None, None);
        }
    })
    .await;
}

pub async fn run_loop(state: AppState) {
    // startup + version-change events
    let version = env!("CARGO_PKG_VERSION");
    let last = state
        .store
        .consolidation_state("last_seen_version")
        .ok()
        .flatten();
    if last.as_deref() != Some(version) {
        log(
            &state,
            "info",
            "version_changed",
            &format!(
                "running v{version}{}",
                last.map(|l| format!(" (was v{l})")).unwrap_or_default()
            ),
            None,
        );
        let _ = state
            .store
            .set_consolidation_state("last_seen_version", version);
    }
    log(
        &state,
        "info",
        "startup",
        &format!("daemon started (v{version}, {})", node_kind()),
        Some(json!({"version": version, "pid": std::process::id()})),
    );
    let mut ticks = 0u64;
    loop {
        ticks += 1;
        let prune = ticks.is_multiple_of(60);
        // sample_process shells out to `ps` and writes health points —
        // blocking work; keep it (and the hourly prune) off the
        // runtime workers.
        let st = state.clone();
        let _ = tokio::task::spawn_blocking(move || {
            sample_process(&st);
            // single-runner leases for mirror-sync'd sidecar embers
            crate::plugins::runner_tick(&st);
            if prune {
                let _ = st.store.prune_telemetry();
            }
        })
        .await;
        probe_dependencies(&state).await;
        tokio::time::sleep(std::time::Duration::from_secs(SAMPLE_SECS)).await;
    }
}
