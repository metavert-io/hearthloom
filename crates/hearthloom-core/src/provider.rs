//! Pluggable LLM and embedding providers.
//!
//! Design law #3: retrieval is the product — LLM tokens are spent at ingest
//! and idle time, never on the hot query path. The system runs
//! degraded-but-useful with NO provider configured (`NoProvider`).
//!
//! Cognition is TIERED. Every internal call declares a `Tier` (low | medium
//! | high) describing how much reasoning it needs; the active provider maps
//! each tier to a concrete model id the user chose. One provider is active
//! at a time (Anthropic by default); OpenAI, Gemini, and Grok are drop-in
//! alternatives behind the same trait. `custom` is any OpenAI-compatible
//! endpoint (Ollama, vLLM, OpenRouter…), and a single tier can be routed
//! to a different provider than the active one (`RoutedProvider`).

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::config::ProviderConfig;

/// How much reasoning a call needs. The user maps each tier to a model
/// (redundancy is fine — sonnet for both medium and high, say). This says
/// how they want to spend, not what the models can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Low,
    Medium,
    High,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::Medium => "medium",
            Tier::High => "high",
        }
    }
    pub fn parse(s: &str) -> Option<Tier> {
        match s.trim().to_lowercase().as_str() {
            "low" => Some(Tier::Low),
            "medium" | "med" => Some(Tier::Medium),
            "high" => Some(Tier::High),
            _ => None,
        }
    }
}

/// The three model ids the active provider serves per tier.
#[derive(Debug, Clone)]
pub struct TierModels {
    pub low: String,
    pub medium: String,
    pub high: String,
}

impl TierModels {
    pub fn get(&self, tier: Tier) -> &str {
        match tier {
            Tier::Low => &self.low,
            Tier::Medium => &self.medium,
            Tier::High => &self.high,
        }
    }
    /// Seed defaults per provider — the "latest frontier" ids we know now.
    /// Users override these in Integrations (dropdown + specify-your-own).
    pub fn defaults_for(provider: &str) -> TierModels {
        match provider {
            "openai" => TierModels {
                low: "gpt-5-mini".into(),
                medium: "gpt-5".into(),
                high: "gpt-5".into(),
            },
            "gemini" => TierModels {
                low: "gemini-2.5-flash".into(),
                medium: "gemini-2.5-pro".into(),
                high: "gemini-2.5-pro".into(),
            },
            "grok" => TierModels {
                low: "grok-4-fast".into(),
                medium: "grok-4".into(),
                high: "grok-4".into(),
            },
            // custom endpoints have no knowable defaults — the user names
            // the model ids their server exposes.
            "custom" => TierModels {
                low: String::new(),
                medium: String::new(),
                high: String::new(),
            },
            // anthropic / default
            _ => TierModels {
                low: "claude-haiku-4-5".into(),
                medium: "claude-sonnet-4-6".into(),
                high: "claude-opus-4-8".into(),
            },
        }
    }
}

/// Token usage reported by a provider for one completion call. Every
/// provider's response carries these numbers — they were parsed and
/// DISCARDED until 0.5.58 (the llm_usage ledger). Cache fields are
/// Anthropic/OpenAI cache-read style counters; providers without a
/// cache concept leave them 0.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
}

/// One completion: the text plus the accounting the API reported.
/// `usage` is None when the provider's response carried no usage block
/// (never the case for the four real providers, but the type is honest).
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub model: String,
    pub usage: Option<Usage>,
    pub stop: Stop,
}

/// Why the model stopped, normalized across providers. A reply that hit
/// the token cap or was refused still arrives as HTTP 200 with partial
/// or empty text — callers that parse the text (the Ask loop) must tell
/// those apart from a malformed reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Stop {
    #[default]
    End,
    /// Hit `max_tokens` — the text is cut off mid-thought.
    Truncated,
    /// The provider's safety layer declined the request.
    Refused,
}

impl Stop {
    pub fn as_str(self) -> &'static str {
        match self {
            Stop::End => "end",
            Stop::Truncated => "truncated",
            Stop::Refused => "refused",
        }
    }
    /// Map a provider's raw stop/finish reason onto the three cases.
    pub fn from_raw(raw: Option<&str>) -> Stop {
        match raw.unwrap_or("").to_ascii_lowercase().as_str() {
            "max_tokens" | "length" | "model_context_window_exceeded" => Stop::Truncated,
            "refusal" | "content_filter" | "safety" | "recitation" | "prohibited_content"
            | "blocklist" | "spii" => Stop::Refused,
            _ => Stop::End,
        }
    }
}

/// USD per 1M tokens for one model family. Tokens are the stored truth;
/// dollar figures are computed at READ time from this table (or the
/// `llm_pricing` config override) — prices drift, ledgers don't.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

/// Ledger model ids for models that run on this machine start with this.
pub const LOCAL_MODEL_PREFIX: &str = "local:";

/// Built-in price table, matched by substring (dated/custom ids land on
/// the family); specific generations are matched BEFORE their family so
/// a cheaper successor never prices at the older rate. Anthropic figures
/// from the API docs (cached 2026-09: fable-5.x $10/$50, opus-5.5 $4/$20,
/// opus-5/4.x $5/$25, sonnet-5.x $2/$10, sonnet-4.6 $3/$15, haiku-4.5
/// $1/$5; cache write 1.25x input; cache read 0.1x input except
/// fable-5.1 $0.25 and opus-5.5 $0.20). Non-Anthropic figures are
/// order-of-magnitude — override via `llm_pricing` in config when
/// reconciling those providers. Returns None for unknown models: the
/// UI shows tokens with "no price on file", never a fabricated cost.
pub fn pricing_for(model: &str) -> Option<ModelPricing> {
    let m = model.to_lowercase();
    let p = |input: f64, output: f64| ModelPricing {
        input,
        output,
        cache_write: input * 1.25,
        cache_read: input * 0.1,
    };
    let with_read = |input: f64, output: f64, cache_read: f64| ModelPricing {
        cache_read,
        ..p(input, output)
    };
    // A model that runs on this machine (ledger model ids `local:<name>`,
    // e.g. the embedder) is free — priced at zero, which is different
    // from "no price on file".
    if m.starts_with(LOCAL_MODEL_PREFIX) {
        return Some(p(0.0, 0.0));
    }
    if m.contains("fable-5-1") || m.contains("mythos-5-1") {
        Some(with_read(10.0, 50.0, 0.25))
    } else if m.contains("fable") || m.contains("mythos") {
        Some(p(10.0, 50.0))
    } else if m.contains("opus-5-5") {
        Some(with_read(4.0, 20.0, 0.20))
    } else if m.contains("opus") {
        Some(p(5.0, 25.0))
    } else if m.contains("sonnet-5") {
        Some(p(2.0, 10.0))
    } else if m.contains("sonnet") {
        Some(p(3.0, 15.0))
    } else if m.contains("haiku") {
        Some(p(1.0, 5.0))
    } else if m.contains("mini") || m.contains("nano") {
        Some(p(0.25, 2.0))
    } else if m.contains("gpt-5") || m.contains("gpt-4o") {
        Some(p(1.25, 10.0))
    } else if m.contains("flash") {
        Some(p(0.30, 2.50))
    } else if m.contains("gemini") {
        Some(p(1.25, 10.0))
    } else if m.contains("grok") && m.contains("fast") {
        Some(p(0.20, 0.50))
    } else if m.contains("grok") {
        Some(p(3.0, 15.0))
    } else {
        None
    }
}

/// Cost in USD for a usage row against a pricing entry.
pub fn usage_cost_usd(pricing: &ModelPricing, u: &Usage) -> f64 {
    (u.input_tokens as f64 * pricing.input
        + u.output_tokens as f64 * pricing.output
        + u.cache_creation_tokens as f64 * pricing.cache_write
        + u.cache_read_tokens as f64 * pricing.cache_read)
        / 1_000_000.0
}

/// Where a streamed reply's text goes, piece by piece.
pub type TextSink<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// Splits a `text/event-stream` body into the `data:` payload of each
/// event. Bytes are buffered until an event is whole, so a UTF-8
/// sequence split across network chunks is never decoded half-way.
#[derive(Default)]
struct SseBuffer {
    pending: Vec<u8>,
}

impl SseBuffer {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some((end, gap)) = sse_boundary(&self.pending) {
            let event: Vec<u8> = self.pending.drain(..end + gap).take(end).collect();
            let text = String::from_utf8_lossy(&event);
            let data: Vec<&str> = text
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            if !data.is_empty() {
                out.push(data.join("\n"));
            }
        }
        out
    }
}

/// The first blank line in `buf`: (offset, length of the separator).
fn sse_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2));
    let crlf = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// A streamed Anthropic message, rebuilt from its events into the same
/// `Completion` the non-streamed call returns.
struct AnthropicStream {
    text: String,
    model: String,
    usage: Usage,
    saw_usage: bool,
    stop: Option<String>,
    done: bool,
}

impl AnthropicStream {
    fn new(model: &str) -> Self {
        AnthropicStream {
            text: String::new(),
            model: model.to_string(),
            usage: Usage::default(),
            saw_usage: false,
            stop: None,
            done: false,
        }
    }
    /// Counters arrive twice: input and cache at `message_start`, the
    /// final output count at `message_delta`. Keep whatever each carries.
    fn take_usage(&mut self, u: &Value) {
        if !u.is_object() {
            return;
        }
        self.saw_usage = true;
        for (field, slot) in [
            ("input_tokens", &mut self.usage.input_tokens),
            ("output_tokens", &mut self.usage.output_tokens),
            (
                "cache_creation_input_tokens",
                &mut self.usage.cache_creation_tokens,
            ),
            ("cache_read_input_tokens", &mut self.usage.cache_read_tokens),
        ] {
            if let Some(n) = u[field].as_i64() {
                *slot = n;
            }
        }
    }
    /// Apply one event; returns the visible text it added, if any.
    /// Thinking deltas are not text and are dropped here.
    fn on_event(&mut self, event: &Value) -> Result<Option<String>> {
        match event["type"].as_str().unwrap_or("") {
            "message_start" => {
                if let Some(model) = event["message"]["model"].as_str() {
                    self.model = model.to_string();
                }
                self.take_usage(&event["message"]["usage"]);
            }
            "content_block_delta" if event["delta"]["type"] == "text_delta" => {
                if let Some(piece) = event["delta"]["text"].as_str() {
                    self.text.push_str(piece);
                    return Ok(Some(piece.to_string()));
                }
            }
            "message_delta" => {
                if let Some(stop) = event["delta"]["stop_reason"].as_str() {
                    self.stop = Some(stop.to_string());
                }
                self.take_usage(&event["usage"]);
            }
            "message_stop" => self.done = true,
            "error" => return Err(anyhow!("anthropic API error mid-reply: {}", event["error"])),
            _ => {}
        }
        Ok(None)
    }
    fn finish(self) -> Result<Completion> {
        if !self.done {
            return Err(anyhow!(
                "anthropic stream ended before the reply was complete"
            ));
        }
        Ok(Completion {
            text: self.text,
            model: self.model,
            usage: self.saw_usage.then_some(self.usage),
            stop: Stop::from_raw(self.stop.as_deref()),
        })
    }
}

/// OpenAI's usage block as ours. `prompt_tokens` INCLUDES cached tokens;
/// split them so input_tokens means "full-price input" on every provider.
fn openai_usage(u: &Value) -> Usage {
    let cached = u["prompt_tokens_details"]["cached_tokens"]
        .as_i64()
        .unwrap_or(0);
    Usage {
        input_tokens: u["prompt_tokens"].as_i64().unwrap_or(0) - cached,
        output_tokens: u["completion_tokens"].as_i64().unwrap_or(0),
        cache_creation_tokens: 0,
        cache_read_tokens: cached,
    }
}

/// A streamed chat completion, rebuilt from its chunks.
struct OpenAiStream {
    text: String,
    model: String,
    usage: Option<Usage>,
    stop: Option<String>,
    done: bool,
}

impl OpenAiStream {
    fn new(model: &str) -> Self {
        OpenAiStream {
            text: String::new(),
            model: model.to_string(),
            usage: None,
            stop: None,
            done: false,
        }
    }
    /// Apply one `data:` payload; returns the visible text it added.
    fn on_data(&mut self, data: &str) -> Result<Option<String>> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(None);
        }
        let Ok(chunk) = serde_json::from_str::<Value>(data) else {
            return Ok(None);
        };
        if let Some(error) = chunk.get("error") {
            return Err(anyhow!("API error mid-reply: {error}"));
        }
        if let Some(model) = chunk["model"].as_str() {
            self.model = model.to_string();
        }
        // The usage chunk comes last, with an empty `choices`.
        if chunk["usage"].is_object() {
            self.usage = Some(openai_usage(&chunk["usage"]));
        }
        let choice = &chunk["choices"][0];
        if let Some(stop) = choice["finish_reason"].as_str() {
            self.stop = Some(stop.to_string());
        }
        match choice["delta"]["content"].as_str() {
            Some(piece) if !piece.is_empty() => {
                self.text.push_str(piece);
                Ok(Some(piece.to_string()))
            }
            _ => Ok(None),
        }
    }
    fn finish(self) -> Result<Completion> {
        if !self.done && self.stop.is_none() {
            return Err(anyhow!("stream ended before the reply was complete"));
        }
        Ok(Completion {
            text: self.text,
            model: self.model,
            usage: self.usage,
            stop: Stop::from_raw(self.stop.as_deref()),
        })
    }
}

/// A tiered text-completion provider.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    /// True if this provider can actually serve requests.
    fn available(&self) -> bool;
    /// Complete at the given reasoning tier (resolves tier → model id).
    async fn complete(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Completion>;
    /// `complete`, handing each piece of the reply's visible text to
    /// `on_text` while the provider is still writing it. The returned
    /// `Completion` is the same either way — text, usage and stop reason
    /// in full. Default: not streamed (one call, `on_text` never runs), so
    /// a provider without a streaming transport stays correct.
    async fn complete_streaming(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        _on_text: TextSink<'_>,
    ) -> Result<Completion> {
        self.complete(tier, system, prompt, max_tokens).await
    }
    /// The model id a tier resolves to (for accounting — known even
    /// when the call itself errors).
    fn model_for(&self, tier: Tier) -> &str;
    /// The provider that serves a tier (for accounting). Differs from
    /// `name()` only when a tier is routed elsewhere (`RoutedProvider`).
    fn provider_for(&self, _tier: Tier) -> &str {
        self.name()
    }
    /// Cheap API-backed check that a model id is usable (onboarding /
    /// Integrations validation). Default: a 1-token completion.
    async fn validate_model(&self, model: &str) -> Result<()>;
}

/// An embedding provider. Local ONNX models + sqlite-vec.
pub trait EmbeddingProvider: Send + Sync {
    fn name(&self) -> &str;
    fn dimensions(&self) -> usize;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// No provider configured: cognition features report unavailable; all fast
/// paths (FTS, graph traversal, recall) work fully.
pub struct NoProvider;

#[async_trait]
impl LlmProvider for NoProvider {
    fn name(&self) -> &str {
        "none"
    }
    fn available(&self) -> bool {
        false
    }
    async fn complete(&self, _t: Tier, _s: &str, _p: &str, _m: u32) -> Result<Completion> {
        Err(anyhow!(
            "no LLM provider configured; set one in Integrations (fast paths are unaffected)"
        ))
    }
    fn model_for(&self, _tier: Tier) -> &str {
        ""
    }
    async fn validate_model(&self, _model: &str) -> Result<()> {
        Err(anyhow!("no LLM provider configured"))
    }
}

/// Anthropic Messages API (raw HTTP; no official Rust SDK).
pub struct AnthropicProvider {
    models: TierModels,
    api_key: String,
    client: reqwest::Client,
    /// Per-tier effort overrides (low, medium, high) — see `Efforts`.
    efforts: Efforts,
}

/// Optional per-tier `effort` overrides, indexed low/medium/high. Unset
/// means "the tier's own name" on always-thinking models. Lets a tier run
/// a strong model at a lighter effort — on Opus 5.5 the measured gap
/// between `high` and `medium` is most of the bill.
#[derive(Debug, Clone, Default)]
pub struct Efforts(pub [Option<String>; 3]);

impl Efforts {
    pub const LEVELS: &'static [&'static str] = &["low", "medium", "high", "xhigh", "max"];
    fn get(&self, tier: Tier) -> Option<&str> {
        let slot = match tier {
            Tier::Low => 0,
            Tier::Medium => 1,
            Tier::High => 2,
        };
        self.0[slot].as_deref()
    }
}

/// Models whose reasoning is ALWAYS on (Opus 5.x, Sonnet 5.x, Fable):
/// thinking tokens count against `max_tokens`, and effort — not a
/// thinking switch — is the only depth control.
pub fn always_thinks(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("opus-5") || m.contains("sonnet-5") || m.contains("fable") || m.contains("mythos")
}

/// The `output_config.effort` to send, if any. Only the always-thinking
/// generation gets one: there the API default differs by model (Opus 5.5
/// defaults to medium, Sonnet 5.5 to high), so the TIER decides instead —
/// high/medium/low mean what they say. Earlier models are sent nothing,
/// exactly as before (Haiku 4.5 rejects the parameter outright).
pub fn effort_for(model: &str, tier: Tier) -> Option<&'static str> {
    always_thinks(model).then_some(tier.as_str())
}

/// `effort_for` with the user's per-tier override applied (only where an
/// effort is sent at all, and only for a level the API knows).
fn effort_with(model: &str, tier: Tier, efforts: &Efforts) -> Option<&'static str> {
    let default = effort_for(model, tier)?;
    Some(
        efforts
            .get(tier)
            .and_then(|want| {
                Efforts::LEVELS
                    .iter()
                    .find(|level| **level == want)
                    .copied()
            })
            .unwrap_or(default),
    )
}

/// Reasoning headroom added to a caller's `max_tokens` on always-thinking
/// models. Call sites size their cap for the visible answer (a 1,200-token
/// digest, a 2,500-token extraction batch); without headroom the thinking
/// would eat it and the reply would arrive truncated.
const THINKING_HEADROOM: u32 = 8_000;

impl AnthropicProvider {
    pub fn new(models: TierModels, api_key: impl Into<String>) -> Self {
        AnthropicProvider {
            models,
            api_key: api_key.into(),
            client: reqwest::Client::new(),
            efforts: Efforts::default(),
        }
    }
    pub fn with_efforts(mut self, efforts: Efforts) -> Self {
        self.efforts = efforts;
        self
    }
    async fn call(
        &self,
        model: &str,
        tier: Option<Tier>,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        on_text: Option<TextSink<'_>>,
    ) -> Result<Completion> {
        if self.api_key.is_empty() {
            return Err(anyhow!("anthropic provider configured but no API key set"));
        }
        // The system prompt goes out as ONE cached block: a multi-round
        // caller (the Ask loop re-sends ~10k tokens of schema + BRAIN.md
        // every round) pays the write once and reads at a fraction of the
        // input price after. The breakpoint only helps when the system
        // text is byte-stable across calls — keep volatile text (dates,
        // the question) in `prompt`. Prefixes under the model's cacheable
        // minimum are silently not cached, so short pass prompts are
        // unaffected. Verify with `cache_read_tokens` in the ledger.
        let system_value = if system.is_empty() {
            json!("")
        } else {
            json!([{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"},
            }])
        };
        // `tier` is None only for the 1-token model probe, which must
        // stay a bare request.
        let effort = tier.and_then(|t| effort_with(model, t, &self.efforts));
        let max_tokens = if effort.is_some() {
            max_tokens + THINKING_HEADROOM
        } else {
            max_tokens
        };
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system_value,
            "messages": [{"role": "user", "content": prompt}],
        });
        if let Some(effort) = effort {
            body["output_config"] = json!({ "effort": effort });
        }
        if on_text.is_some() {
            body["stream"] = json!(true);
        }
        let mut resp = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let value: Value = resp.json().await?;
            return Err(anyhow!("anthropic API error {status}: {value}"));
        }
        if let Some(on_text) = on_text {
            let mut events = SseBuffer::default();
            let mut message = AnthropicStream::new(model);
            while let Some(chunk) = resp.chunk().await? {
                for data in events.push(&chunk) {
                    let Ok(event) = serde_json::from_str::<Value>(&data) else {
                        continue;
                    };
                    if let Some(piece) = message.on_event(&event)? {
                        on_text(&piece);
                    }
                }
            }
            return message.finish();
        }
        let value: Value = resp.json().await?;
        let text = value["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let usage = value.get("usage").map(|u| Usage {
            input_tokens: u["input_tokens"].as_i64().unwrap_or(0),
            output_tokens: u["output_tokens"].as_i64().unwrap_or(0),
            cache_creation_tokens: u["cache_creation_input_tokens"].as_i64().unwrap_or(0),
            cache_read_tokens: u["cache_read_input_tokens"].as_i64().unwrap_or(0),
        });
        // The response echoes the resolved model id (dated snapshots) —
        // prefer it for the ledger; fall back to what we asked for.
        let model = value["model"].as_str().unwrap_or(model).to_string();
        let stop = Stop::from_raw(value["stop_reason"].as_str());
        Ok(Completion {
            text,
            model,
            usage,
            stop,
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }
    fn available(&self) -> bool {
        !self.api_key.is_empty()
    }
    async fn complete(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Completion> {
        self.call(
            self.models.get(tier),
            Some(tier),
            system,
            prompt,
            max_tokens,
            None,
        )
        .await
    }
    async fn complete_streaming(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        on_text: TextSink<'_>,
    ) -> Result<Completion> {
        self.call(
            self.models.get(tier),
            Some(tier),
            system,
            prompt,
            max_tokens,
            Some(on_text),
        )
        .await
    }
    fn model_for(&self, tier: Tier) -> &str {
        self.models.get(tier)
    }
    async fn validate_model(&self, model: &str) -> Result<()> {
        self.call(model, None, "", "ping", 1, None)
            .await
            .map(|_| ())
    }
}

/// OpenAI-compatible chat-completions provider — also serves Grok (x.ai)
/// and any compatible endpoint by swapping the base URL (`custom`:
/// Ollama, vLLM, LM Studio, OpenRouter — the key is optional there, a
/// loopback server usually has none).
pub struct OpenAiCompatProvider {
    name: &'static str,
    base_url: String,
    models: TierModels,
    api_key: String,
    client: reqwest::Client,
}

/// Normalize a user-typed base URL: trimmed, no trailing slash, and
/// http(s) only. `http://localhost:11434/v1` is the Ollama shape.
pub fn normalize_base_url(raw: &str) -> Result<String> {
    let url = raw.trim().trim_end_matches('/');
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(anyhow!("base URL must start with http:// or https://"));
    }
    if url.len() < "http://a".len() || url.contains(char::is_whitespace) {
        return Err(anyhow!("base URL is not a valid address"));
    }
    Ok(url.to_string())
}

impl OpenAiCompatProvider {
    pub fn openai(models: TierModels, api_key: impl Into<String>) -> Self {
        Self::new("openai", "https://api.openai.com/v1", models, api_key)
    }
    pub fn grok(models: TierModels, api_key: impl Into<String>) -> Self {
        Self::new("grok", "https://api.x.ai/v1", models, api_key)
    }
    /// Any OpenAI-compatible endpoint. An empty `base_url` yields a
    /// provider that reports unavailable rather than an error at boot.
    pub fn custom(base_url: &str, models: TierModels, api_key: impl Into<String>) -> Self {
        Self::new(
            "custom",
            base_url.trim().trim_end_matches('/'),
            models,
            api_key,
        )
    }
    fn is_custom(&self) -> bool {
        self.name == "custom"
    }
    fn new(name: &'static str, base: &str, models: TierModels, api_key: impl Into<String>) -> Self {
        OpenAiCompatProvider {
            name,
            base_url: base.into(),
            models,
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }
    async fn call(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        on_text: Option<TextSink<'_>>,
    ) -> Result<Completion> {
        if self.is_custom() {
            if self.base_url.is_empty() {
                return Err(anyhow!("custom provider configured but no base URL set"));
            }
            if model.trim().is_empty() {
                return Err(anyhow!(
                    "custom provider has no model id for this tier — set one in Integrations"
                ));
            }
        } else if self.api_key.is_empty() {
            return Err(anyhow!(
                "{} provider configured but no API key set",
                self.name
            ));
        }
        // `max_completion_tokens` is OpenAI's current name; compatible
        // servers (Ollama, vLLM, OpenRouter) still key on `max_tokens`.
        let cap_field = if self.is_custom() {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        let mut body = json!({
            "model": model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt},
            ],
        });
        body[cap_field] = json!(max_tokens);
        if on_text.is_some() {
            body["stream"] = json!(true);
            // Without this the stream carries no usage and the ledger
            // row would read zero tokens.
            body["stream_options"] = json!({"include_usage": true});
        }
        let mut req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let mut resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let value: Value = resp.json().await?;
            return Err(anyhow!("{} API error {status}: {value}", self.name));
        }
        if let Some(on_text) = on_text {
            let mut events = SseBuffer::default();
            let mut message = OpenAiStream::new(model);
            while let Some(chunk) = resp.chunk().await? {
                for data in events.push(&chunk) {
                    if let Some(piece) = message
                        .on_data(&data)
                        .map_err(|err| anyhow!("{} {err}", self.name))?
                    {
                        on_text(&piece);
                    }
                }
            }
            return message
                .finish()
                .map_err(|err| anyhow!("{} {err}", self.name));
        }
        let value: Value = resp.json().await?;
        let text = value["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let usage = value.get("usage").map(openai_usage);
        let model = value["model"].as_str().unwrap_or(model).to_string();
        let stop = Stop::from_raw(value["choices"][0]["finish_reason"].as_str());
        Ok(Completion {
            text,
            model,
            usage,
            stop,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        self.name
    }
    fn available(&self) -> bool {
        if self.is_custom() {
            !self.base_url.is_empty()
        } else {
            !self.api_key.is_empty()
        }
    }
    async fn complete(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Completion> {
        self.call(self.models.get(tier), system, prompt, max_tokens, None)
            .await
    }
    /// Streamed for OpenAI and Grok. A `custom` endpoint is not: servers
    /// differ on whether a stream reports usage at all, and a reply with
    /// no usage would land in the ledger as zero tokens.
    async fn complete_streaming(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        on_text: TextSink<'_>,
    ) -> Result<Completion> {
        let on_text = (!self.is_custom()).then_some(on_text);
        self.call(self.models.get(tier), system, prompt, max_tokens, on_text)
            .await
    }
    fn model_for(&self, tier: Tier) -> &str {
        self.models.get(tier)
    }
    async fn validate_model(&self, model: &str) -> Result<()> {
        self.call(model, "", "ping", 1, None).await.map(|_| ())
    }
}

/// Google Gemini (generateContent) provider.
pub struct GeminiProvider {
    models: TierModels,
    api_key: String,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(models: TierModels, api_key: impl Into<String>) -> Self {
        GeminiProvider {
            models,
            api_key: api_key.into(),
            client: reqwest::Client::new(),
        }
    }
    async fn call(
        &self,
        model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Completion> {
        if self.api_key.is_empty() {
            return Err(anyhow!("gemini provider configured but no API key set"));
        }
        let body = json!({
            "systemInstruction": {"parts": [{"text": system}]},
            "contents": [{"role": "user", "parts": [{"text": prompt}]}],
            "generationConfig": {"maxOutputTokens": max_tokens},
        });
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
        );
        let resp = self
            .client
            .post(url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let value: Value = resp.json().await?;
        if !status.is_success() {
            return Err(anyhow!("gemini API error {status}: {value}"));
        }
        let text = value["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let usage = value.get("usageMetadata").map(|u| {
            let cached = u["cachedContentTokenCount"].as_i64().unwrap_or(0);
            Usage {
                input_tokens: u["promptTokenCount"].as_i64().unwrap_or(0) - cached,
                output_tokens: u["candidatesTokenCount"].as_i64().unwrap_or(0),
                cache_creation_tokens: 0,
                cache_read_tokens: cached,
            }
        });
        let model = value["modelVersion"].as_str().unwrap_or(model).to_string();
        let stop = Stop::from_raw(value["candidates"][0]["finishReason"].as_str());
        Ok(Completion {
            text,
            model,
            usage,
            stop,
        })
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }
    fn available(&self) -> bool {
        !self.api_key.is_empty()
    }
    async fn complete(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Completion> {
        self.call(self.models.get(tier), system, prompt, max_tokens)
            .await
    }
    fn model_for(&self, tier: Tier) -> &str {
        self.models.get(tier)
    }
    async fn validate_model(&self, model: &str) -> Result<()> {
        self.call(model, "", "ping", 1).await.map(|_| ())
    }
}

/// The providers a user can pick among (for the wizard/Integrations menus).
pub const PROVIDERS: &[&str] = &["anthropic", "openai", "gemini", "grok", "custom"];

/// Approximate INPUT price in USD per 1M tokens, for the pre-flight cost
/// ESTIMATOR only (a rough "this will cost about…" warning before a big
/// import — never billing). Reads the same table as the ledger so the two
/// can't drift apart; unknown models fall back to a mid rate so the
/// warning never reads as "free".
pub fn approx_input_usd_per_mtok(model: &str) -> f64 {
    pricing_for(model).map(|p| p.input).unwrap_or(3.0)
}

/// Build a provider by name with an explicit key + tier map. The daemon
/// assembles these from stored config + sealed keys; this stays pure.
pub fn build_provider(
    name: &str,
    api_key: &str,
    models: TierModels,
) -> Result<Box<dyn LlmProvider>> {
    build_provider_at(name, api_key, models, None)
}

/// `build_provider` plus the base URL a `custom` endpoint needs (ignored
/// by the named providers, whose addresses are fixed).
pub fn build_provider_at(
    name: &str,
    api_key: &str,
    models: TierModels,
    base_url: Option<&str>,
) -> Result<Box<dyn LlmProvider>> {
    build_provider_full(name, api_key, models, base_url, Efforts::default())
}

/// `build_provider_at` plus per-tier effort overrides (Anthropic only —
/// the one provider this build sends an effort to).
pub fn build_provider_full(
    name: &str,
    api_key: &str,
    models: TierModels,
    base_url: Option<&str>,
    efforts: Efforts,
) -> Result<Box<dyn LlmProvider>> {
    match name {
        "none" | "" => Ok(Box::new(NoProvider)),
        "anthropic" => Ok(Box::new(
            AnthropicProvider::new(models, api_key).with_efforts(efforts),
        )),
        "openai" => Ok(Box::new(OpenAiCompatProvider::openai(models, api_key))),
        "grok" => Ok(Box::new(OpenAiCompatProvider::grok(models, api_key))),
        "gemini" => Ok(Box::new(GeminiProvider::new(models, api_key))),
        "custom" => Ok(Box::new(OpenAiCompatProvider::custom(
            base_url.unwrap_or(""),
            models,
            api_key,
        ))),
        other => Err(anyhow!("unknown LLM provider: {other}")),
    }
}

/// One provider per tier: the active provider serves every tier except
/// those explicitly routed elsewhere (e.g. Low → a local Qwen behind a
/// `custom` endpoint while Medium/High stay on the active provider).
/// Routes are node-local config — a loopback endpoint means nothing to a
/// peer — so they never ride the LLM sync envelope.
pub struct RoutedProvider {
    base: Box<dyn LlmProvider>,
    low: Option<Box<dyn LlmProvider>>,
    medium: Option<Box<dyn LlmProvider>>,
    high: Option<Box<dyn LlmProvider>>,
}

impl RoutedProvider {
    pub fn new(base: Box<dyn LlmProvider>) -> Self {
        RoutedProvider {
            base,
            low: None,
            medium: None,
            high: None,
        }
    }
    /// Route one tier to `provider` (built with that tier's model id).
    pub fn route(mut self, tier: Tier, provider: Box<dyn LlmProvider>) -> Self {
        match tier {
            Tier::Low => self.low = Some(provider),
            Tier::Medium => self.medium = Some(provider),
            Tier::High => self.high = Some(provider),
        }
        self
    }
    pub fn has_routes(&self) -> bool {
        self.low.is_some() || self.medium.is_some() || self.high.is_some()
    }
    /// Unwrap to the plain base provider (no routes configured).
    pub fn into_base(self) -> Box<dyn LlmProvider> {
        self.base
    }
    fn pick(&self, tier: Tier) -> &dyn LlmProvider {
        let routed = match tier {
            Tier::Low => &self.low,
            Tier::Medium => &self.medium,
            Tier::High => &self.high,
        };
        routed.as_deref().unwrap_or(self.base.as_ref())
    }
}

#[async_trait]
impl LlmProvider for RoutedProvider {
    fn name(&self) -> &str {
        self.base.name()
    }
    /// Usable if ANY tier can be served — a keyless base with a local
    /// Low route still runs the Low-tier passes.
    fn available(&self) -> bool {
        [Tier::Low, Tier::Medium, Tier::High]
            .iter()
            .any(|t| self.pick(*t).available())
    }
    async fn complete(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> Result<Completion> {
        self.pick(tier)
            .complete(tier, system, prompt, max_tokens)
            .await
    }
    async fn complete_streaming(
        &self,
        tier: Tier,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        on_text: TextSink<'_>,
    ) -> Result<Completion> {
        self.pick(tier)
            .complete_streaming(tier, system, prompt, max_tokens, on_text)
            .await
    }
    fn model_for(&self, tier: Tier) -> &str {
        self.pick(tier).model_for(tier)
    }
    fn provider_for(&self, tier: Tier) -> &str {
        self.pick(tier).name()
    }
    async fn validate_model(&self, model: &str) -> Result<()> {
        self.base.validate_model(model).await
    }
}

/// Build the configured LLM provider from the YAML/env config (the boot
/// default; runtime overrides from the store layer over this).
pub fn llm_from_config(config: &ProviderConfig) -> Result<Box<dyn LlmProvider>> {
    let models = config.tier_models();
    build_provider(&config.llm, &config.api_key, models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_no_provider_degrades() {
        let p = NoProvider;
        assert!(!p.available());
        assert!(p.complete(Tier::Medium, "s", "p", 16).await.is_err());
    }

    #[test]
    fn test_sse_buffer_waits_for_whole_events() {
        let mut sse = SseBuffer::default();
        // An event split mid-line, and a two-byte character split in half.
        let body = "event: a\ndata: {\"x\": \"é\"}\n\nevent: ping\r\ndata: two\r\n\r\n: comment\n\ndata: tail";
        let bytes = body.as_bytes();
        let cut = body.find('é').unwrap() + 1;
        assert!(sse.push(&bytes[..cut]).is_empty());
        let got = sse.push(&bytes[cut..]);
        assert_eq!(got, vec!["{\"x\": \"é\"}".to_string(), "two".to_string()]);
        // No blank line yet: the last event is still pending.
        assert_eq!(sse.push(b"\n\n"), vec!["tail".to_string()]);
    }

    #[test]
    fn test_anthropic_stream_rebuilds_the_completion() {
        let events = [
            json!({"type": "message_start", "message": {"model": "claude-opus-4-8-x",
                "usage": {"input_tokens": 12, "cache_creation_input_tokens": 3,
                          "cache_read_input_tokens": 900, "output_tokens": 1}}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "delta": {"type": "thinking_delta", "thinking": "hm"}}),
            json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "{\"ans"}}),
            json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "wer\": 1}"}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"},
                "usage": {"output_tokens": 77}}),
        ];
        let mut message = AnthropicStream::new("asked-for");
        let mut pieces = Vec::new();
        for event in &events {
            if let Some(piece) = message.on_event(event).unwrap() {
                pieces.push(piece);
            }
        }
        assert_eq!(pieces, vec!["{\"ans", "wer\": 1}"], "thinking is not text");
        // Cut off before message_stop: an error, never a short reply.
        let mut cut = AnthropicStream::new("m");
        cut.on_event(&events[3]).unwrap();
        assert!(cut.finish().is_err());

        message.on_event(&json!({"type": "message_stop"})).unwrap();
        let done = message.finish().unwrap();
        assert_eq!(done.text, "{\"answer\": 1}");
        assert_eq!(done.model, "claude-opus-4-8-x");
        assert_eq!(done.stop, Stop::Truncated);
        let usage = done.usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_tokens,
                usage.cache_read_tokens
            ),
            (12, 77, 3, 900)
        );

        let mut failed = AnthropicStream::new("m");
        let err = failed
            .on_event(&json!({"type": "error", "error": {"type": "overloaded_error"}}))
            .unwrap_err();
        assert!(err.to_string().contains("overloaded_error"));
    }

    #[test]
    fn test_openai_stream_rebuilds_the_completion() {
        let mut message = OpenAiStream::new("asked-for");
        let chunks = [
            r#"{"model": "gpt-x", "choices": [{"delta": {"role": "assistant", "content": ""}}]}"#,
            r#"{"model": "gpt-x", "choices": [{"delta": {"content": "hel"}}]}"#,
            r#"{"model": "gpt-x", "choices": [{"delta": {"content": "lo"}, "finish_reason": null}]}"#,
            r#"{"model": "gpt-x", "choices": [{"delta": {}, "finish_reason": "length"}]}"#,
            r#"{"model": "gpt-x", "choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 9,
                "prompt_tokens_details": {"cached_tokens": 40}}}"#,
            "[DONE]",
        ];
        let mut pieces = Vec::new();
        for chunk in chunks {
            if let Some(piece) = message.on_data(chunk).unwrap() {
                pieces.push(piece);
            }
        }
        assert_eq!(pieces, vec!["hel", "lo"]);
        let done = message.finish().unwrap();
        assert_eq!(done.text, "hello");
        assert_eq!(done.model, "gpt-x");
        assert_eq!(done.stop, Stop::Truncated);
        let usage = done.usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens
            ),
            (60, 9, 40)
        );

        assert!(OpenAiStream::new("m").finish().is_err(), "nothing arrived");
        assert!(OpenAiStream::new("m")
            .on_data(r#"{"error": {"message": "boom"}}"#)
            .is_err());
    }

    #[test]
    fn test_tier_parse_and_models() {
        assert_eq!(Tier::parse("HIGH"), Some(Tier::High));
        assert_eq!(Tier::parse("med"), Some(Tier::Medium));
        assert_eq!(Tier::parse("nope"), None);
        let m = TierModels::defaults_for("anthropic");
        assert_eq!(m.get(Tier::High), "claude-opus-4-8");
        assert_eq!(m.get(Tier::Low), "claude-haiku-4-5");
    }

    #[test]
    fn test_pricing_table() {
        // Anthropic families resolve; dated ids land on the family.
        let opus = pricing_for("claude-opus-4-8").unwrap();
        assert_eq!(opus.input, 5.0);
        assert_eq!(opus.output, 25.0);
        assert!(pricing_for("claude-haiku-4-5-20251001").is_some());
        // On-machine models are free, not unpriced.
        let local = pricing_for("local:all-MiniLM-L6-v2").unwrap();
        assert_eq!(
            (local.input, local.output, local.cache_read),
            (0.0, 0.0, 0.0)
        );
        // Successor generations price at THEIR rate, not the family's.
        let opus55 = pricing_for("claude-opus-5-5").unwrap();
        assert_eq!(
            (opus55.input, opus55.output, opus55.cache_read),
            (4.0, 20.0, 0.20)
        );
        let sonnet55 = pricing_for("claude-sonnet-5-5").unwrap();
        assert_eq!((sonnet55.input, sonnet55.output), (2.0, 10.0));
        assert_eq!(pricing_for("claude-sonnet-4-6").unwrap().input, 3.0);
        assert_eq!(pricing_for("claude-fable-5-1").unwrap().cache_read, 0.25);
        // The import estimator reads the same table (opus was 15 there).
        assert_eq!(approx_input_usd_per_mtok("claude-opus-4-8"), 5.0);
        assert_eq!(approx_input_usd_per_mtok("mystery-llm-9000"), 3.0);
        // Unknown models yield None — no fabricated cost.
        assert!(pricing_for("mystery-llm-9000").is_none());
        // Cost math: 1M in + 1M out on sonnet = 3 + 15.
        let sonnet = pricing_for("claude-sonnet-4-6").unwrap();
        let u = Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        };
        assert!((usage_cost_usd(&sonnet, &u) - 18.0).abs() < 1e-9);
    }

    #[test]
    fn test_build_provider() {
        let m = || TierModels::defaults_for("anthropic");
        assert_eq!(build_provider("none", "", m()).unwrap().name(), "none");
        assert_eq!(
            build_provider("anthropic", "k", m()).unwrap().name(),
            "anthropic"
        );
        assert_eq!(build_provider("openai", "k", m()).unwrap().name(), "openai");
        assert_eq!(build_provider("grok", "k", m()).unwrap().name(), "grok");
        assert_eq!(build_provider("gemini", "k", m()).unwrap().name(), "gemini");
        assert!(build_provider("mystery", "k", m()).is_err());
    }

    #[test]
    fn test_custom_provider_and_routing() {
        let one = |id: &str| TierModels {
            low: id.into(),
            medium: id.into(),
            high: id.into(),
        };
        // Keyless custom is available iff it has an address.
        let local = build_provider_at(
            "custom",
            "",
            one("qwen3:8b"),
            Some("http://localhost:11434/v1/"),
        )
        .unwrap();
        assert!(local.available());
        assert_eq!(local.name(), "custom");
        assert!(!build_provider_at("custom", "", one("x"), None)
            .unwrap()
            .available());
        assert!(normalize_base_url("ftp://x").is_err());
        assert_eq!(
            normalize_base_url(" http://localhost:11434/v1/ ").unwrap(),
            "http://localhost:11434/v1"
        );

        // Low routes to the local endpoint; the other tiers stay on base.
        let base = build_provider("anthropic", "k", TierModels::defaults_for("anthropic")).unwrap();
        let routed = RoutedProvider::new(base).route(Tier::Low, local);
        assert!(routed.has_routes());
        assert_eq!(routed.provider_for(Tier::Low), "custom");
        assert_eq!(routed.model_for(Tier::Low), "qwen3:8b");
        assert_eq!(routed.provider_for(Tier::High), "anthropic");
        assert_eq!(routed.model_for(Tier::High), "claude-opus-4-8");
        assert_eq!(routed.name(), "anthropic");
    }

    #[test]
    fn test_effort_only_for_always_thinking_models() {
        assert_eq!(effort_for("claude-opus-5-5", Tier::High), Some("high"));
        assert_eq!(
            effort_for("claude-sonnet-5-5", Tier::Medium),
            Some("medium")
        );
        assert_eq!(effort_for("claude-fable-5-1", Tier::Low), Some("low"));
        // Unchanged for everything that shipped before: no parameter at all.
        assert_eq!(effort_for("claude-opus-4-8", Tier::High), None);
        assert_eq!(effort_for("claude-sonnet-4-6", Tier::Medium), None);
        assert_eq!(effort_for("claude-haiku-4-5", Tier::Low), None);
        // An override lowers the effort where one is sent, and only there.
        let efforts = Efforts([None, None, Some("medium".into())]);
        assert_eq!(
            effort_with("claude-opus-5-5", Tier::High, &efforts),
            Some("medium")
        );
        assert_eq!(
            effort_with("claude-opus-5-5", Tier::Medium, &efforts),
            Some("medium")
        );
        assert_eq!(effort_with("claude-opus-4-8", Tier::High, &efforts), None);
        let junk = Efforts([None, None, Some("turbo".into())]);
        assert_eq!(
            effort_with("claude-opus-5-5", Tier::High, &junk),
            Some("high")
        );
    }

    #[test]
    fn test_stop_normalization() {
        assert_eq!(Stop::from_raw(Some("max_tokens")), Stop::Truncated);
        assert_eq!(Stop::from_raw(Some("length")), Stop::Truncated);
        assert_eq!(Stop::from_raw(Some("MAX_TOKENS")), Stop::Truncated);
        assert_eq!(Stop::from_raw(Some("refusal")), Stop::Refused);
        assert_eq!(Stop::from_raw(Some("SAFETY")), Stop::Refused);
        assert_eq!(Stop::from_raw(Some("end_turn")), Stop::End);
        assert_eq!(Stop::from_raw(None), Stop::End);
    }
}
