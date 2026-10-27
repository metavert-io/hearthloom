//! `hearthloom bench` — query-path latency on the REAL substrate.
//!
//! "Retrieval is the product" (design law #3) and the performance
//! invariants each encode a fixed daemon freeze; this makes both
//! measurable: P50/P95 wall-clock over the same HTTP surface every
//! agent and dashboard view uses, seeded with the record's own top
//! entities so numbers reflect real shapes, not toy queries. No LLM is
//! on any of these paths — that's the point being certified.
//!
//! Honest-measurement notes: one warm-up round per endpoint is
//! discarded (statement/page caches); the vault state and episode
//! count are printed alongside, because results are only comparable
//! at similar substrate size and lock state.

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::client::Client;

const ITERATIONS: usize = 20;
const WARMUP: usize = 2;

struct Timing {
    label: String,
    example: String,
    millis: Vec<f64>,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64) * p / 100.0).ceil() as usize;
    sorted[idx.clamp(1, sorted.len()) - 1]
}

pub async fn run(client: &Client, iterations: Option<usize>) -> Result<()> {
    let iterations = iterations.unwrap_or(ITERATIONS).max(3);
    client.ensure_daemon().await?;
    // A dedicated client so per-request measurements don't include the
    // CLI client's ensure-daemon health probe.
    let mut headers = reqwest::header::HeaderMap::new();
    // Same key resolution as Client::with_url — env, then the keychain
    // key `auth login` stored (bench 401'd on auth-locked nodes before).
    let key = std::env::var("HEARTHLOOM_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
        .or_else(|| crate::ops::stored_key(client.base_url()));
    if let Some(key) = key {
        if let Ok(value) = format!("Bearer {}", key.trim()).parse() {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
    }
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .default_headers(headers)
        .build()?;
    let base = client.base_url().to_string();

    // Seed from the record itself: top entities drive traverse starts
    // and realistic search terms; stats give the context line.
    let stats = client.get("/v1/stats", &[]).await?;
    let episodes = stats.get("episodes").and_then(|e| e.as_i64()).unwrap_or(0);
    let vault_locked = client
        .get("/v1/vault", &[])
        .await
        .ok()
        .and_then(|v| v.get("locked").and_then(|l| l.as_bool()))
        .unwrap_or(true);
    let entities: Vec<String> = stats
        .get("top_entities")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if episodes == 0 {
        return Err(anyhow!(
            "the record is empty — bench needs a real substrate"
        ));
    }
    let mut terms: Vec<String> = entities
        .iter()
        .filter_map(|name| name.split_whitespace().next())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_lowercase())
        .collect();
    for fallback in ["meeting", "project", "email"] {
        if terms.len() < 4 {
            terms.push(fallback.into());
        }
    }
    // Recent recall dates, spread across a quarter.
    let today = chrono::Utc::now().date_naive();
    let dates: Vec<String> = (0..iterations)
        .map(|i| {
            (today - chrono::Duration::days(3 + (i as i64) * 7))
                .format("%Y-%m-%d")
                .to_string()
        })
        .collect();
    let year_ago = (today - chrono::Duration::days(365))
        .format("%Y-%m-%d")
        .to_string();
    let today_s = today.format("%Y-%m-%d").to_string();

    // (label, per-iteration request builder)
    type Case = (
        String,
        String,
        Box<dyn Fn(usize) -> (String, Vec<(String, String)>)>,
    );
    let terms2 = terms.clone();
    let entities2 = entities.clone();
    let dates2 = dates.clone();
    let (ya, ts) = (year_ago.clone(), today_s.clone());
    let mut cases: Vec<Case> = vec![
        (
            "search (hybrid FTS+vec)".into(),
            format!(
                "/v1/search?q={}",
                terms.first().cloned().unwrap_or_default()
            ),
            Box::new(move |i| {
                (
                    "/v1/search".into(),
                    vec![
                        ("q".into(), terms2[i % terms2.len()].clone()),
                        ("limit".into(), "10".into()),
                    ],
                )
            }),
        ),
        (
            "recall (day reconstruction)".into(),
            format!("/v1/recall?on={}", dates[0]),
            Box::new(move |i| {
                (
                    "/v1/recall".into(),
                    vec![("on".into(), dates2[i % dates2.len()].clone())],
                )
            }),
        ),
        (
            "timeline notable (year window)".into(),
            format!("/v1/timeline/notable?from={year_ago}&to={today_s}"),
            Box::new(move |_| {
                (
                    "/v1/timeline/notable".into(),
                    vec![
                        ("from".into(), ya.clone()),
                        ("to".into(), ts.clone()),
                        ("k".into(), "24".into()),
                    ],
                )
            }),
        ),
    ];
    if !entities.is_empty() {
        cases.push((
            "traverse (depth 2, top 12)".into(),
            format!("/v1/traverse?start={}", entities[0]),
            Box::new(move |i| {
                (
                    "/v1/traverse".into(),
                    vec![
                        ("start".into(), entities2[i % entities2.len()].clone()),
                        ("depth".into(), "2".into()),
                        ("top".into(), "12".into()),
                    ],
                )
            }),
        ));
    }

    println!(
        "hearthloom bench — {} episodes, vault {}, {} iterations/endpoint (+{} warm-up)\n",
        episodes,
        if vault_locked { "locked" } else { "open" },
        iterations,
        WARMUP,
    );

    let mut results: Vec<Timing> = Vec::new();
    for (label, example, build) in &cases {
        let mut millis = Vec::with_capacity(iterations);
        for i in 0..(iterations + WARMUP) {
            let (path, params) = build(i);
            let query: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let started = std::time::Instant::now();
            let resp = http
                .get(format!("{base}{path}"))
                .query(&query)
                .send()
                .await?;
            if !resp.status().is_success() {
                return Err(anyhow!(
                    "{path} answered {} — is the API key valid?",
                    resp.status()
                ));
            }
            let _body: Value = resp.json().await?;
            if i >= WARMUP {
                millis.push(started.elapsed().as_secs_f64() * 1e3);
            }
        }
        millis.sort_by(|a, b| a.total_cmp(b));
        results.push(Timing {
            label: label.clone(),
            example: example.clone(),
            millis,
        });
    }

    println!(
        "{:<34} {:>7} {:>8} {:>8} {:>8}",
        "endpoint", "p50", "p95", "max", "mean"
    );
    for t in &results {
        let mean = t.millis.iter().sum::<f64>() / t.millis.len() as f64;
        println!(
            "{:<34} {:>6.0}ms {:>7.0}ms {:>7.0}ms {:>7.0}ms",
            t.label,
            percentile(&t.millis, 50.0),
            percentile(&t.millis, 95.0),
            t.millis.last().copied().unwrap_or(0.0),
            mean,
        );
    }
    println!("\nexample requests:");
    for t in &results {
        println!("  {}", t.example);
    }
    println!(
        "\nno LLM call is on any of these paths (design law #3); \
         embedding lookup only if a local model is present."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn test_percentiles_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&v, 95.0), 95.0);
        // small n clamps sanely
        assert_eq!(percentile(&[7.0], 95.0), 7.0);
        assert_eq!(percentile(&[], 50.0), 0.0);
        let three = [1.0, 2.0, 3.0];
        assert_eq!(percentile(&three, 50.0), 2.0);
        assert_eq!(percentile(&three, 95.0), 3.0);
    }
}
