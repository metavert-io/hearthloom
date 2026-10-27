//! Personal eval harness — tokens per correct answer, applied to brain
//! queries. Questions about your own life with known answers are YOUR
//! data, so they live in your data home (`~/.hearthloom/eval/<suite>.yaml`),
//! never in the repository; the repository ships invented examples
//! (`eval/examples/`) that `hearthloom init` and the first `hearthloom eval`
//! copy there for you to replace. The runner reports pass/fail and an
//! estimated token cost per answer.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::client::Client;

#[derive(Debug, Deserialize)]
struct EvalFile {
    #[serde(default)]
    description: String,
    questions: Vec<Question>,
}

#[derive(Debug, Deserialize)]
struct Question {
    id: String,
    question: String,
    /// Which surface answers it: search | recall | entity.
    tool: String,
    #[serde(default)]
    args: serde_yaml::Mapping,
    /// Pass iff every string appears (case-insensitive) in the response.
    expect_contains: Vec<String>,
    /// A template case still waiting for your own ground truth: reported,
    /// not run. Fill it in and delete the line.
    #[serde(default)]
    skip: Option<String>,
}

/// The invented example suites, embedded so `init` can scaffold them
/// wherever the binary runs (no checkout needed).
pub const EXAMPLES: &[(&str, &str)] = &[
    (
        "personal-eval",
        include_str!("../../../eval/examples/personal-eval.yaml"),
    ),
    (
        "ask-eval",
        include_str!("../../../eval/examples/ask-eval.yaml"),
    ),
];

/// Copy the example suites into `<home>/eval/` wherever one is missing;
/// returns the files it created. Existing suites are never touched.
pub fn scaffold(home_root: &Path) -> Result<Vec<PathBuf>> {
    let dir = home_root.join("eval");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut created = Vec::new();
    for (name, body) in EXAMPLES {
        let path = dir.join(format!("{name}.yaml"));
        if !path.exists() {
            std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
            created.push(path);
        }
    }
    Ok(created)
}

/// Which file a run reads: an explicit path wins; otherwise the named
/// suite in the data home (`<home>/eval/<suite>.yaml`), scaffolded from
/// the example on first use.
pub fn resolve(home_root: &Path, suite: &str, file: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(file) = file {
        return Ok(file);
    }
    let path = home_root.join("eval").join(format!("{suite}.yaml"));
    if !path.exists() && scaffold(home_root)?.contains(&path) {
        println!(
            "created {} from the example suite — replace its cases with facts from your own record\n",
            path.display()
        );
    }
    if !path.exists() {
        bail!(
            "no eval suite at {} (suites live in {})",
            path.display(),
            home_root.join("eval").display()
        );
    }
    Ok(path)
}

pub async fn run(client: &Client, path: &Path) -> Result<()> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let eval: EvalFile =
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    if !eval.description.is_empty() {
        println!("{}\n", eval.description);
    }

    let mut passed = 0;
    let mut skipped = 0;
    let mut total_tokens = 0usize;
    for q in &eval.questions {
        if let Some(reason) = &q.skip {
            skipped += 1;
            println!(
                "SKIP {:<24} {:>6} tok  {} (needs your ground truth: {reason})",
                q.id, 0, q.question
            );
            continue;
        }
        let response = ask(client, q).await;
        // SKIP: an offline virtual source is an environmental condition,
        // not a regression — report it loudly but leave it ungraded.
        let (verdict, tokens, note) = match response {
            Ok(value) => {
                let text = value.to_string().to_lowercase();
                let tokens = estimate_tokens(&text);
                let missing: Vec<&String> = q
                    .expect_contains
                    .iter()
                    .filter(|needle| !text.contains(&needle.to_lowercase()))
                    .collect();
                if missing.is_empty() {
                    ("PASS", tokens, String::new())
                } else {
                    ("FAIL", tokens, format!("missing {missing:?}"))
                }
            }
            Err(err) if err.to_string().contains("is offline") => ("SKIP", 0, err.to_string()),
            Err(err) => ("FAIL", 0, err.to_string()),
        };
        total_tokens += tokens;
        match verdict {
            "PASS" => passed += 1,
            "SKIP" => skipped += 1,
            _ => {}
        }
        println!(
            "{} {:<24} {:>6} tok  {} {}",
            verdict,
            q.id,
            tokens,
            q.question,
            if note.is_empty() {
                String::new()
            } else {
                format!("— {note}")
            },
        );
    }
    let graded = eval.questions.len() - skipped;
    println!(
        "\n{passed}/{graded} passed{} · ~{total_tokens} answer tokens total",
        if skipped > 0 {
            format!(" ({skipped} skipped)")
        } else {
            String::new()
        }
    );
    if passed < graded {
        anyhow::bail!("eval failures");
    }
    Ok(())
}

async fn ask(client: &Client, q: &Question) -> Result<Value> {
    let arg = |key: &str| -> Option<String> {
        q.args
            .get(serde_yaml::Value::String(key.into()))
            .and_then(|v| match v {
                serde_yaml::Value::String(s) => Some(s.clone()),
                serde_yaml::Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
    };
    match q.tool.as_str() {
        "search" => {
            let query = arg("query").context("search eval needs args.query")?;
            client.get("/v1/search", &[("q", query)]).await
        }
        "recall" => {
            let mut params: Vec<(&str, String)> = Vec::new();
            for key in ["on", "from", "to", "entity", "detail"] {
                if let Some(value) = arg(key) {
                    params.push((key, value));
                }
            }
            client.get("/v1/recall", &params).await
        }
        "entity" => {
            let name = arg("name").context("entity eval needs args.name")?;
            client
                .get(&format!("/v1/entity/{}", crate::urlencode(&name)), &[])
                .await
        }
        "sql" => {
            // Guarded read-only SQL over POST /v1/sql (admin scope —
            // read_query's TEMP-VIEW vault shadowing applies).
            let query = arg("query").context("sql eval needs args.query")?;
            client
                .post("/v1/sql", &serde_json::json!({ "query": query }))
                .await
        }
        "vsearch" => {
            // A virtual source (HearthFS mount, mbox archive) — best-effort
            // by design; an offline mount SKIPS the case (reported, not
            // graded): the eval still surfaces that the mount is down
            // without poisoning the tally with a known-environmental miss.
            let source = arg("source").context("vsearch eval needs args.source")?;
            let query = arg("query").context("vsearch eval needs args.query")?;
            client
                .get(
                    &format!("/v1/sources/virtual/{}/search", crate::urlencode(&source)),
                    &[("q", query)],
                )
                .await
        }
        other => anyhow::bail!("unknown eval tool: {other}"),
    }
}

/// Rough token estimate (~4 bytes/token) — good enough for trend lines.
fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/* ————— the Ask eval (opt-in: spends real LLM tokens) ————— */

#[derive(Debug, Deserialize)]
struct AskEvalFile {
    #[serde(default)]
    description: String,
    questions: Vec<AskQuestion>,
}

#[derive(Debug, Deserialize)]
struct AskQuestion {
    id: String,
    question: String,
    /// Every string must appear in the answer (case-insensitive).
    #[serde(default)]
    expect_contains: Vec<String>,
    /// At least one must appear (when non-empty).
    #[serde(default)]
    expect_any: Vec<String>,
    /// None may appear — the retraction / stale-fact guard.
    #[serde(default)]
    expect_not_contains: Vec<String>,
    /// Minimum citations that SURVIVED the evidence check.
    #[serde(default)]
    min_citations: usize,
    /// Fail if the engine needed more tool rounds than this.
    #[serde(default)]
    max_rounds: Option<u64>,
    /// A case that still needs the owner's ground truth: reported, not run.
    #[serde(default)]
    skip: Option<String>,
}

/// What one ask cost, summed from its transcript. The root loop's rounds
/// only — `map` sub-calls land in the llm_usage ledger under the same
/// ask id (`/v1/ask/{id}/trace`, vault-gated) and are not in this sum.
#[derive(Default)]
struct AskCost {
    tokens_in: i64,
    tokens_out: i64,
    cache_read: i64,
    /// Tokens WRITTEN to the provider cache (billed above input price).
    cache_write: i64,
    /// Time inside Hearthloom's own tools and pre-ask setup.
    tool_ms: i64,
    /// Model calls in the root loop.
    calls: i64,
    /// (total ms, calls) per tool — names the slow one.
    tools: std::collections::BTreeMap<String, (i64, i64)>,
    llm_ms: i64,
    usd: f64,
    unpriced: bool,
}

fn ask_cost(transcript: &Value) -> AskCost {
    let mut cost = AskCost::default();
    for step in transcript.as_array().into_iter().flatten() {
        let n = |key: &str| step.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        cost.tool_ms += n("tool_ms") + n("inventory_ms");
        let tool = step.get("tool").and_then(|t| t.as_str()).or_else(|| {
            (step.get("kind").and_then(|k| k.as_str()) == Some("setup")).then_some("(setup)")
        });
        if let Some(tool) = tool {
            let slot = cost.tools.entry(tool.to_string()).or_default();
            slot.0 += n("tool_ms") + n("inventory_ms");
            slot.1 += 1;
        }
        let (tin, tout, cached) = (n("tokens_in"), n("tokens_out"), n("cache_read"));
        let written = n("cache_write");
        if tin == 0 && tout == 0 && cached == 0 && written == 0 {
            continue;
        }
        cost.cache_write += written;
        cost.calls += 1;
        cost.tokens_in += tin;
        cost.tokens_out += tout;
        cost.cache_read += cached;
        cost.llm_ms += n("llm_ms");
        let model = step.get("model").and_then(|m| m.as_str()).unwrap_or("");
        match hearthloom_core::provider::pricing_for(model) {
            Some(p) => {
                cost.usd += (tin as f64 * p.input
                    + tout as f64 * p.output
                    + cached as f64 * p.cache_read
                    + written as f64 * p.cache_write)
                    / 1_000_000.0;
            }
            None => cost.unpriced = true,
        }
    }
    cost
}

/// Grade one answer; returns the reasons it failed (empty = pass).
fn grade_ask(q: &AskQuestion, response: &Value) -> Vec<String> {
    let answer = response["answer"].as_str().unwrap_or("").to_lowercase();
    let mut problems = Vec::new();
    let missing: Vec<&String> = q
        .expect_contains
        .iter()
        .filter(|needle| !answer.contains(&needle.to_lowercase()))
        .collect();
    if !missing.is_empty() {
        problems.push(format!("missing {missing:?}"));
    }
    if !q.expect_any.is_empty()
        && !q
            .expect_any
            .iter()
            .any(|n| answer.contains(&n.to_lowercase()))
    {
        problems.push(format!("none of {:?}", q.expect_any));
    }
    let present: Vec<&String> = q
        .expect_not_contains
        .iter()
        .filter(|needle| answer.contains(&needle.to_lowercase()))
        .collect();
    if !present.is_empty() {
        problems.push(format!("must not say {present:?}"));
    }
    let citations = response["citations"].as_array().map(Vec::len).unwrap_or(0);
    if citations < q.min_citations {
        problems.push(format!(
            "{citations} citations, wanted ≥{}",
            q.min_citations
        ));
    }
    // An invented citation is a failure on EVERY case, asked for or not.
    let dropped = response["dropped_citations"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    if dropped > 0 {
        problems.push(format!("{dropped} invented citation(s)"));
    }
    if let (Some(cap), Some(rounds)) = (q.max_rounds, response["rounds"].as_u64()) {
        if rounds > cap {
            problems.push(format!("{rounds} rounds, wanted ≤{cap}"));
        }
    }
    problems
}

/// `hearthloom eval --ask`: every question goes through the full Ask
/// engine (POST /v1/query) — REAL provider tokens. Reports pass/fail plus
/// rounds, tokens, cached share, dollars and latency per question, so a
/// model/prompt/caching change can be judged against a baseline.
pub async fn run_ask(client: &Client, path: &Path, only: &[String]) -> Result<()> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let eval: AskEvalFile =
        serde_yaml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if !eval.description.is_empty() {
        println!("{}\n", eval.description);
    }
    // `--only a,b`: rerun just those ids (e.g. the ones a billing stop cut off).
    let questions: Vec<&AskQuestion> = eval
        .questions
        .iter()
        .filter(|q| only.is_empty() || only.iter().any(|id| id == &q.id))
        .collect();
    let runnable = questions.iter().filter(|q| q.skip.is_none()).count();
    println!("running {runnable} questions through Ask — this spends real LLM tokens\n");

    let (mut passed, mut skipped, mut unpriced) = (0usize, 0usize, false);
    let mut total = AskCost::default();
    let (mut total_rounds, mut total_wall_ms) = (0u64, 0u128);
    // Questions the engine actually answered — the averages divide by
    // this, so errored runs don't flatter the per-question figures.
    let mut answered = 0usize;
    let mut not_run = 0usize;
    for (index, q) in questions.iter().enumerate() {
        if let Some(reason) = &q.skip {
            skipped += 1;
            println!("SKIP {:<26} {} — {reason}", q.id, q.question);
            continue;
        }
        let started = std::time::Instant::now();
        let response = client
            .post(
                "/v1/query",
                &serde_json::json!({ "question": q.question, "actor": "human:eval" }),
            )
            .await;
        let wall_ms = started.elapsed().as_millis();
        match response {
            Ok(response) => {
                answered += 1;
                total_wall_ms += wall_ms;
                let problems = grade_ask(q, &response);
                let cost = ask_cost(&response["transcript"]);
                let rounds = response["rounds"].as_u64().unwrap_or(0);
                total_rounds += rounds;
                total.tokens_in += cost.tokens_in;
                total.tokens_out += cost.tokens_out;
                total.cache_read += cost.cache_read;
                total.cache_write += cost.cache_write;
                total.llm_ms += cost.llm_ms;
                total.tool_ms += cost.tool_ms;
                total.calls += cost.calls;
                for (tool, (ms, count)) in &cost.tools {
                    let slot = total.tools.entry(tool.clone()).or_default();
                    slot.0 += ms;
                    slot.1 += count;
                }
                total.usd += cost.usd;
                unpriced |= cost.unpriced;
                if problems.is_empty() {
                    passed += 1;
                }
                println!(
                    "{} {:<26} {}r {:>7} in ({:>7} cached) {:>5} out  ${:.4}  {:>5.1}s  {}",
                    if problems.is_empty() { "PASS" } else { "FAIL" },
                    q.id,
                    rounds,
                    cost.tokens_in + cost.cache_read + cost.cache_write,
                    cost.cache_read,
                    cost.tokens_out,
                    cost.usd,
                    wall_ms as f64 / 1000.0,
                    if problems.is_empty() {
                        String::new()
                    } else {
                        format!("— {}", problems.join("; "))
                    },
                );
                // A content miss is only diagnosable with the answer in hand.
                if problems.iter().any(|p| !p.contains("rounds, wanted")) {
                    let excerpt: String = response["answer"]
                        .as_str()
                        .unwrap_or("")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(320)
                        .collect();
                    println!("       answer: {excerpt}…");
                }
            }
            Err(err) => {
                let message = format!("{err:#}");
                println!("FAIL {:<26} — {message}", q.id);
                // A billing/credential refusal fails every later question
                // the same way — stop instead of hammering the provider.
                let lower = message.to_lowercase();
                if lower.contains("credit balance")
                    || lower.contains("billing")
                    || lower.contains("401")
                    || lower.contains("invalid x-api-key")
                {
                    let rest: Vec<&str> = questions[index + 1..]
                        .iter()
                        .filter(|q| q.skip.is_none())
                        .map(|q| q.id.as_str())
                        .collect();
                    not_run = rest.len();
                    println!(
                        "\nSTOPPED — the provider is refusing calls (billing/credentials). Not run: {}\n\
                         Fix it at the provider, then finish with:  --only {},{}",
                        rest.join(", "),
                        q.id,
                        rest.join(",")
                    );
                    break;
                }
            }
        }
    }
    let n = answered.max(1) as f64;
    let all_in = total.tokens_in + total.cache_read + total.cache_write;
    println!(
        "\n{passed}/{runnable} passed{}{} · {answered} answered · {:.1} rounds avg · {} in ({}% cached) / {} out · ${:.4} total (${:.4}/question){} · {:.1}s avg",
        if skipped > 0 { format!(" ({skipped} skipped — need ground truth)") } else { String::new() },
        if not_run > 0 { format!(" ({not_run} not run)") } else { String::new() },
        total_rounds as f64 / n,
        all_in,
        if all_in > 0 { total.cache_read * 100 / all_in } else { 0 },
        total.tokens_out,
        total.usd,
        total.usd / n,
        if unpriced { " + unpriced calls" } else { "" },
        total_wall_ms as f64 / 1000.0 / n,
    );
    // Where the seconds went: the provider (and how fast it wrote), our
    // own tools, and whatever is left (network, map sub-calls, SSE).
    let wall = total_wall_ms as f64 / 1000.0;
    let (llm, tools) = (total.llm_ms as f64 / 1000.0, total.tool_ms as f64 / 1000.0);
    if wall > 0.0 && total.calls > 0 {
        println!(
            "time: {:.0}% in the model ({:.1}s/question over {:.1} calls, {:.1}s per call, {:.0} output tokens/s) · {:.0}% in Hearthloom's tools ({:.1}s/question) · {:.0}% other",
            llm / wall * 100.0,
            llm / n,
            total.calls as f64 / n,
            llm / total.calls as f64,
            if llm > 0.0 { total.tokens_out as f64 / llm } else { 0.0 },
            tools / wall * 100.0,
            tools / n,
            ((wall - llm - tools) / wall * 100.0).max(0.0),
        );
    }
    let mut tools: Vec<(&String, &(i64, i64))> = total.tools.iter().collect();
    tools.sort_by_key(|(_, (ms, _))| std::cmp::Reverse(*ms));
    if !tools.is_empty() {
        println!(
            "tools: {}",
            tools
                .iter()
                .take(6)
                .map(|(name, (ms, count))| format!(
                    "{name} {:.1}s total over {count} ({:.1}s each)",
                    *ms as f64 / 1000.0,
                    *ms as f64 / 1000.0 / (*count).max(1) as f64
                ))
                .collect::<Vec<_>>()
                .join(" · ")
        );
    }
    println!("(root-loop tokens only; `map` sub-calls are in the usage ledger per ask)");
    if passed < runnable {
        anyhow::bail!("ask eval failures");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn q(yaml: &str) -> AskQuestion {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn test_grade_ask() {
        let case = q(
            "{id: a, question: q, expect_contains: [July], expect_any: [lease, rent], \
                      expect_not_contains: [Gazette], min_citations: 1, max_rounds: 3}",
        );
        let good = json!({"answer": "In july the LEASE was signed.", "citations": ["ep_1"],
                          "dropped_citations": [], "rounds": 2});
        assert!(grade_ask(&case, &good).is_empty());

        let bad = json!({"answer": "The Gazette covered it.", "citations": [],
                         "dropped_citations": ["ep_x"], "rounds": 5});
        let problems = grade_ask(&case, &bad).join("; ");
        for expected in [
            "missing",
            "none of",
            "must not say",
            "citations, wanted",
            "invented",
            "rounds, wanted",
        ] {
            assert!(problems.contains(expected), "{expected} in {problems}");
        }
        // An invented citation fails even a case that asked for nothing.
        let lax = q("{id: b, question: q}");
        assert_eq!(grade_ask(&lax, &bad).len(), 1);
    }

    #[test]
    fn test_ask_cost_sums_priced_steps() {
        let transcript = json!([
            {"kind": "seed", "tool": "search"},
            {"kind": "tool", "model": "claude-opus-5-5", "tokens_in": 1000, "tokens_out": 100,
             "cache_read": 10000, "cache_write": 2000, "llm_ms": 900},
            {"kind": "tool"},
            {"kind": "answer", "model": "mystery-llm", "tokens_in": 5, "tokens_out": 5, "llm_ms": 10},
        ]);
        let cost = ask_cost(&transcript);
        assert_eq!(
            (cost.tokens_in, cost.tokens_out, cost.cache_read),
            (1005, 105, 10000)
        );
        // 1000×$4 + 100×$20 + 10000×$0.20 + 2000×$5 (write = 1.25× input), per million.
        assert!((cost.usd - 0.018).abs() < 1e-9, "{}", cost.usd);
        assert_eq!(cost.cache_write, 2000);
        assert!(cost.unpriced);
    }
}
