//! The ONE way to call the LLM provider (0.5.58): times the call and
//! lands a row in the v50 `llm_usage` ledger — success or error — so
//! token accounting can never be dodged by a new call site. Every
//! `provider.complete(...)` in the daemon goes through `call`; if you
//! are adding an LLM call site, use this, not the provider directly
//! (a direct call silently escapes the reconciliation surface — the
//! exact defect this module fixed).

use anyhow::Result;
use hearthloom_core::provider::{LlmProvider, Stop, TextSink, Tier, Usage};
use hearthloom_core::store::Store;
use std::time::Instant;

/// One accounted completion: the text plus what the trace records.
pub struct LlmReply {
    pub text: String,
    pub model: String,
    pub usage: Option<Usage>,
    pub ms: u64,
    /// Truncated/refused replies come back Ok — the caller decides
    /// whether partial text is usable (a digest) or not (Ask's JSON).
    pub stop: Stop,
}

/// Complete via `llm`, record the ledger row, return the reply.
/// `purpose` is the pass name or "ask"; `ref_id` links Ask rounds to
/// their ask_log row. Ledger-write failures are logged and swallowed —
/// accounting must never break the calling pass.
#[allow(clippy::too_many_arguments)]
pub async fn call(
    store: &Store,
    llm: &dyn LlmProvider,
    purpose: &str,
    ref_id: Option<&str>,
    tier: Tier,
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<LlmReply> {
    call_streaming(store, llm, purpose, ref_id, tier, system, prompt, max_tokens, None).await
}

/// `call`, with the reply's text handed to `on_text` as it is written
/// (when the provider can stream). Same ledger row, same reply.
#[allow(clippy::too_many_arguments)]
pub async fn call_streaming(
    store: &Store,
    llm: &dyn LlmProvider,
    purpose: &str,
    ref_id: Option<&str>,
    tier: Tier,
    system: &str,
    prompt: &str,
    max_tokens: u32,
    on_text: Option<TextSink<'_>>,
) -> Result<LlmReply> {
    let started = Instant::now();
    let result = match on_text {
        Some(on_text) => llm.complete_streaming(tier, system, prompt, max_tokens, on_text).await,
        None => llm.complete(tier, system, prompt, max_tokens).await,
    };
    let ms = started.elapsed().as_millis() as u64;
    let node = crate::monitor::node_kind();
    match result {
        Ok(completion) => {
            let usage = completion.usage.clone().unwrap_or_default();
            if let Err(err) = store.record_llm_usage(
                node,
                purpose,
                ref_id,
                llm.provider_for(tier),
                &completion.model,
                tier.as_str(),
                &usage,
                ms,
                "ok",
            ) {
                tracing::warn!("llm_usage record failed ({purpose}): {err:#}");
            }
            Ok(LlmReply {
                text: completion.text,
                model: completion.model,
                usage: completion.usage,
                ms,
                stop: completion.stop,
            })
        }
        Err(err) => {
            // Failed calls still bill input on some providers, but the
            // response carried no usage — record zeros so error rate
            // and latency stay visible in the ledger.
            if let Err(rec_err) = store.record_llm_usage(
                node,
                purpose,
                ref_id,
                llm.provider_for(tier),
                llm.model_for(tier),
                tier.as_str(),
                &Usage::default(),
                ms,
                "error",
            ) {
                tracing::warn!("llm_usage record failed ({purpose}): {rec_err:#}");
            }
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hearthloom_core::provider::{Completion, Tier};

    struct MockLlm {
        fail: bool,
    }

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
            _prompt: &str,
            _max: u32,
        ) -> anyhow::Result<Completion> {
            if self.fail {
                anyhow::bail!("provider exploded");
            }
            Ok(Completion {
                text: "hello".into(),
                model: "mock-1".into(),
                usage: Some(Usage { input_tokens: 42, output_tokens: 7, ..Default::default() }),
                stop: Stop::End,
            })
        }
        fn model_for(&self, _tier: Tier) -> &str {
            "mock-1"
        }
        async fn validate_model(&self, _model: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_call_lands_ledger_rows_success_and_error() {
        let store = Store::open_in_memory().unwrap();
        let ok = call(&store, &MockLlm { fail: false }, "ask", Some("ask_x"),
            Tier::High, "s", "p", 100).await.unwrap();
        assert_eq!(ok.text, "hello");
        assert_eq!(ok.usage.as_ref().unwrap().input_tokens, 42);

        // Errors surface to the caller AND still land a ledger row.
        assert!(call(&store, &MockLlm { fail: true }, "ask", Some("ask_x"),
            Tier::High, "s", "p", 100).await.is_err());

        store.vault_unlock(5);
        let rows = store.llm_usage_for_ref("ask_x").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["status"], "ok");
        assert_eq!(rows[0]["input_tokens"], 42);
        assert_eq!(rows[0]["model"], "mock-1");
        assert_eq!(rows[1]["status"], "error");
        assert_eq!(rows[1]["model"], "mock-1"); // model_for fills it in
        store.vault_lock();
    }
}
