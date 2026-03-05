//! Provider failover chain with outbound throttling and health tracking.
//!
//! `ProviderChain` wraps a primary `LlmProvider` with a list of fallbacks.
//! When the primary fails with a retryable error (rate limit, auth, server error),
//! it automatically tries the next provider in the chain.

use std::{pin::Pin, sync::Arc, time::Instant};

use {async_trait::async_trait, tokio_stream::Stream, tracing::warn};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, histogram, labels, llm as llm_metrics};

use crate::{
    model::{ChatMessage, CompletionResponse, LlmProvider, StreamEvent},
    provider_health::{ProviderHealthTracker, global_tracker},
    rate_limiter::{ProviderRateLimiter, RateLimitDecision},
};

pub use crate::classify::{
    ErrorRoutingAction, ProviderErrorKind, classify_error, classify_error_message,
    extract_retry_after_ms, parse_retry_after_ms, parse_retry_delay_ms_from_fragment,
};

/// A provider entry in the failover chain.
struct ChainEntry {
    provider: Arc<dyn LlmProvider>,
}

/// Failover chain that tries providers in order.
///
/// Implements `LlmProvider` itself so callers don't need to know about failover.
pub struct ProviderChain {
    chain: Vec<ChainEntry>,
    rate_limiter: Option<Arc<ProviderRateLimiter>>,
    health_tracker: Arc<ProviderHealthTracker>,
}

impl ProviderChain {
    /// Build a chain from a list of providers (primary first, then fallbacks).
    pub fn new(providers: Vec<Arc<dyn LlmProvider>>) -> Self {
        let config = moltis_config::discover_and_load();
        let chain = providers
            .into_iter()
            .map(|provider| ChainEntry { provider })
            .collect();
        Self {
            chain,
            rate_limiter: ProviderRateLimiter::from_config(&config.tools.provider_rate_limit),
            health_tracker: global_tracker(),
        }
    }

    /// Build a chain with one provider (no failover). Useful as a passthrough.
    pub fn single(provider: Arc<dyn LlmProvider>) -> Self {
        Self::new(vec![provider])
    }

    /// Override the outbound provider rate limiter.
    pub fn with_rate_limiter(mut self, limiter: Option<Arc<ProviderRateLimiter>>) -> Self {
        self.rate_limiter = limiter;
        self
    }

    /// Override provider-health tracking sink.
    pub fn with_health_tracker(mut self, tracker: Arc<ProviderHealthTracker>) -> Self {
        self.health_tracker = tracker;
        self
    }

    fn primary(&self) -> &ChainEntry {
        &self.chain[0]
    }
}

#[async_trait]
impl LlmProvider for ProviderChain {
    fn name(&self) -> &str {
        self.primary().provider.name()
    }

    fn id(&self) -> &str {
        self.primary().provider.id()
    }

    fn supports_tools(&self) -> bool {
        self.primary().provider.supports_tools()
    }

    fn context_window(&self) -> u32 {
        self.primary().provider.context_window()
    }

    fn emits_metrics(&self) -> bool {
        true
    }

    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<CompletionResponse> {
        let mut errors = Vec::new();
        #[cfg(feature = "metrics")]
        let start = Instant::now();

        'providers: for entry in &self.chain {
            let provider_name = entry.provider.name().to_string();
            let model_id = entry.provider.id().to_string();
            let attempt_start = Instant::now();

            if let Some(ref limiter) = self.rate_limiter {
                loop {
                    match limiter.acquire(&provider_name, &model_id) {
                        RateLimitDecision::Allowed => break,
                        RateLimitDecision::Wait(delay) => {
                            warn!(
                                provider = %provider_name,
                                model = %model_id,
                                delay_ms = delay.as_millis() as u64,
                                "outbound provider limiter queued request"
                            );
                            tokio::time::sleep(delay).await;
                        },
                        RateLimitDecision::Rejected(retry_after) => {
                            warn!(
                                provider = %provider_name,
                                model = %model_id,
                                retry_after_ms = retry_after.as_millis() as u64,
                                "outbound provider limiter rejected request; trying next provider"
                            );
                            errors.push(format!(
                                "{}: local provider rate limiter active (retry_after={}ms)",
                                entry.provider.id(),
                                retry_after.as_millis()
                            ));
                            continue 'providers;
                        },
                    }
                }
            }

            match entry.provider.complete(messages, tools).await {
                Ok(resp) => {
                    self.health_tracker.record_success(
                        &provider_name,
                        &model_id,
                        attempt_start.elapsed().as_millis() as u64,
                    );

                    // Record metrics on successful completion
                    #[cfg(feature = "metrics")]
                    {
                        let duration = start.elapsed().as_secs_f64();

                        counter!(
                            llm_metrics::COMPLETIONS_TOTAL,
                            labels::PROVIDER => provider_name.clone(),
                            labels::MODEL => model_id.clone()
                        )
                        .increment(1);

                        counter!(
                            llm_metrics::INPUT_TOKENS_TOTAL,
                            labels::PROVIDER => provider_name.clone(),
                            labels::MODEL => model_id.clone()
                        )
                        .increment(u64::from(resp.usage.input_tokens));

                        counter!(
                            llm_metrics::OUTPUT_TOKENS_TOTAL,
                            labels::PROVIDER => provider_name.clone(),
                            labels::MODEL => model_id.clone()
                        )
                        .increment(u64::from(resp.usage.output_tokens));

                        counter!(
                            llm_metrics::CACHE_READ_TOKENS_TOTAL,
                            labels::PROVIDER => provider_name.clone(),
                            labels::MODEL => model_id.clone()
                        )
                        .increment(u64::from(resp.usage.cache_read_tokens));

                        counter!(
                            llm_metrics::CACHE_WRITE_TOKENS_TOTAL,
                            labels::PROVIDER => provider_name.clone(),
                            labels::MODEL => model_id.clone()
                        )
                        .increment(u64::from(resp.usage.cache_write_tokens));

                        histogram!(
                            llm_metrics::COMPLETION_DURATION_SECONDS,
                            labels::PROVIDER => provider_name,
                            labels::MODEL => model_id
                        )
                        .record(duration);
                    }

                    return Ok(resp);
                },
                Err(e) => {
                    let kind = classify_error(&e);
                    self.health_tracker.record_failure(
                        &provider_name,
                        &model_id,
                        attempt_start.elapsed().as_millis() as u64,
                        provider_error_kind_label(kind),
                    );

                    if matches!(
                        kind,
                        ProviderErrorKind::RateLimit | ProviderErrorKind::NonRetryableRateLimit
                    ) && let Some(retry_after_ms) = parse_retry_after_ms(&e.to_string())
                        && let Some(ref limiter) = self.rate_limiter
                    {
                        limiter.note_retry_after_ms(&provider_name, &model_id, retry_after_ms);
                    }

                    // Record error metrics
                    #[cfg(feature = "metrics")]
                    {
                        counter!(
                            llm_metrics::COMPLETION_ERRORS_TOTAL,
                            labels::PROVIDER => provider_name.clone(),
                            labels::MODEL => model_id.clone(),
                            labels::ERROR_TYPE => format!("{kind:?}")
                        )
                        .increment(1);
                    }

                    if !kind.should_failover() {
                        // Non-retryable error — propagate immediately.
                        return Err(e);
                    }

                    warn!(
                        provider = entry.provider.id(),
                        error = %e,
                        kind = ?kind,
                        "provider failed, trying next in chain"
                    );
                    errors.push(format!("{}: {e}", entry.provider.id()));
                },
            }
        }

        anyhow::bail!(
            "all providers in failover chain failed: {}",
            errors.join("; ")
        )
    }

    fn stream(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        self.stream_with_tools(messages, vec![])
    }

    fn stream_with_tools(
        &self,
        messages: Vec<ChatMessage>,
        tools: Vec<serde_json::Value>,
    ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
        // For streaming, we choose a provider up-front.
        // If the stream yields an Error event, we can't transparently retry
        // mid-stream, so we pick the best available provider upfront.
        let mut selected = None;
        let mut limiter_rejections: Vec<String> = Vec::new();
        for entry in &self.chain {
            let provider_name = entry.provider.name().to_string();
            let model_id = entry.provider.id().to_string();

            if let Some(ref limiter) = self.rate_limiter {
                match limiter.acquire(&provider_name, &model_id) {
                    RateLimitDecision::Allowed => {
                        selected = Some((entry, None));
                        break;
                    },
                    RateLimitDecision::Wait(delay) => {
                        selected = Some((entry, Some(delay)));
                        break;
                    },
                    RateLimitDecision::Rejected(retry_after) => {
                        limiter_rejections.push(format!(
                            "{}: local provider rate limiter active (retry_after={}ms)",
                            entry.provider.id(),
                            retry_after.as_millis()
                        ));
                        continue;
                    },
                }
            } else {
                selected = Some((entry, None));
                break;
            }
        }

        let Some((selected, initial_delay)) = selected else {
            let message = if limiter_rejections.is_empty() {
                "all providers in failover chain unavailable for streaming".to_string()
            } else {
                format!(
                    "all providers in failover chain unavailable for streaming: {}",
                    limiter_rejections.join("; ")
                )
            };
            return Box::pin(tokio_stream::once(StreamEvent::Error(message)));
        };

        let provider_name = selected.provider.name().to_string();
        let model_id = selected.provider.id().to_string();
        let health_tracker = Arc::clone(&self.health_tracker);
        let limiter = self.rate_limiter.clone();
        let start = Instant::now();
        let mut first_delta_recorded = false;
        let provider = Arc::clone(&selected.provider);

        let wrapped = async_stream::stream! {
            if let Some(delay) = initial_delay {
                tokio::time::sleep(delay).await;
                if let Some(ref limiter) = limiter {
                    loop {
                        match limiter.acquire(&provider_name, &model_id) {
                            RateLimitDecision::Allowed => break,
                            RateLimitDecision::Wait(next_delay) => {
                                warn!(
                                    provider = %provider_name,
                                    model = %model_id,
                                    delay_ms = next_delay.as_millis() as u64,
                                    "outbound provider limiter queued streaming request"
                                );
                                tokio::time::sleep(next_delay).await;
                            },
                            RateLimitDecision::Rejected(retry_after) => {
                                yield StreamEvent::Error(format!(
                                    "{}: local provider rate limiter active (retry_after={}ms)",
                                    provider.id(),
                                    retry_after.as_millis()
                                ));
                                return;
                            },
                        }
                    }
                }
            }

            use tokio_stream::StreamExt;
            let mut inner = provider.stream_with_tools(messages, tools);
            while let Some(event) = inner.next().await {
                match &event {
                    StreamEvent::Delta(_) if !first_delta_recorded => {
                        first_delta_recorded = true;
                        #[cfg(feature = "metrics")]
                        histogram!(
                            llm_metrics::TIME_TO_FIRST_TOKEN_SECONDS,
                            labels::PROVIDER => provider_name.clone(),
                            labels::MODEL => model_id.clone()
                        )
                        .record(start.elapsed().as_secs_f64());
                    },
                    StreamEvent::Done(usage) => {
                        health_tracker.record_success(
                            &provider_name,
                            &model_id,
                            start.elapsed().as_millis() as u64,
                        );
                        #[cfg(feature = "metrics")]
                        {
                            let duration_secs = start.elapsed().as_secs_f64();
                            counter!(
                                llm_metrics::COMPLETIONS_TOTAL,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone()
                            )
                            .increment(1);
                            counter!(
                                llm_metrics::INPUT_TOKENS_TOTAL,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone()
                            )
                            .increment(u64::from(usage.input_tokens));
                            counter!(
                                llm_metrics::OUTPUT_TOKENS_TOTAL,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone()
                            )
                            .increment(u64::from(usage.output_tokens));
                            counter!(
                                llm_metrics::CACHE_READ_TOKENS_TOTAL,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone()
                            )
                            .increment(u64::from(usage.cache_read_tokens));
                            counter!(
                                llm_metrics::CACHE_WRITE_TOKENS_TOTAL,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone()
                            )
                            .increment(u64::from(usage.cache_write_tokens));
                            histogram!(
                                llm_metrics::COMPLETION_DURATION_SECONDS,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone()
                            )
                            .record(duration_secs);

                            if duration_secs > 0.0 {
                                let tps = usage.output_tokens as f64 / duration_secs;
                                histogram!(
                                    llm_metrics::TOKENS_PER_SECOND,
                                    labels::PROVIDER => provider_name.clone(),
                                    labels::MODEL => model_id.clone()
                                )
                                .record(tps);
                            }
                        }
                    },
                    StreamEvent::Error(msg) => {
                        let kind = classify_error_message(msg);
                        health_tracker.record_failure(
                            &provider_name,
                            &model_id,
                            start.elapsed().as_millis() as u64,
                            provider_error_kind_label(kind),
                        );

                        if matches!(kind, ProviderErrorKind::RateLimit | ProviderErrorKind::NonRetryableRateLimit)
                            && let Some(retry_after_ms) = parse_retry_after_ms(msg)
                            && let Some(ref limiter) = limiter
                        {
                            limiter.note_retry_after_ms(&provider_name, &model_id, retry_after_ms);
                        }

                        #[cfg(feature = "metrics")]
                        {
                            histogram!(
                                llm_metrics::COMPLETION_DURATION_SECONDS,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone()
                            )
                            .record(start.elapsed().as_secs_f64());
                            counter!(
                                llm_metrics::COMPLETION_ERRORS_TOTAL,
                                labels::PROVIDER => provider_name.clone(),
                                labels::MODEL => model_id.clone(),
                                labels::ERROR_TYPE => format!("{kind:?}")
                            )
                            .increment(1);
                        }
                    },
                    _ => {},
                }

                yield event;
            }
        };
        Box::pin(wrapped)
    }
}

fn provider_error_kind_label(kind: ProviderErrorKind) -> &'static str {
    match kind {
        ProviderErrorKind::RateLimit => "rate_limit",
        ProviderErrorKind::AuthError => "auth_error",
        ProviderErrorKind::ServerError => "server_error",
        ProviderErrorKind::Timeout => "timeout",
        ProviderErrorKind::BillingExhausted => "billing_exhausted",
        ProviderErrorKind::NonRetryableRateLimit => "non_retryable_rate_limit",
        ProviderErrorKind::ContextWindow => "context_window",
        ProviderErrorKind::InvalidRequest => "invalid_request",
        ProviderErrorKind::Unknown => "unknown",
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::model::{ChatMessage, StreamEvent, Usage},
        async_trait::async_trait,
        std::{sync::Mutex, time::Duration},
        tokio_stream::StreamExt,
    };

    /// A mock provider that always succeeds.
    struct SuccessProvider {
        id: &'static str,
    }

    #[async_trait]
    impl LlmProvider for SuccessProvider {
        fn name(&self) -> &str {
            "success"
        }

        fn id(&self) -> &str {
            self.id
        }

        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: Some("ok".into()),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
                ..Default::default()
            })
        }

        fn stream(
            &self,
            _messages: Vec<ChatMessage>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::once(StreamEvent::Done(Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            })))
        }
    }

    /// A mock provider that always fails with a configurable error message.
    struct FailingProvider {
        id: &'static str,
        error_msg: &'static str,
    }

    #[async_trait]
    impl LlmProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }

        fn id(&self) -> &str {
            self.id
        }

        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> anyhow::Result<CompletionResponse> {
            anyhow::bail!("{}", self.error_msg)
        }

        fn stream(
            &self,
            _messages: Vec<ChatMessage>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::once(StreamEvent::Error(
                self.error_msg.into(),
            )))
        }
    }

    #[tokio::test]
    async fn primary_succeeds_no_failover() {
        let chain = ProviderChain::new(vec![
            Arc::new(SuccessProvider { id: "primary" }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ]);

        let resp = chain.complete(&[], &[]).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("ok"));
        assert_eq!(chain.id(), "primary");
    }

    #[test]
    fn chain_reports_internal_metrics_emission() {
        let chain = ProviderChain::single(Arc::new(SuccessProvider { id: "primary" }));
        assert!(chain.emits_metrics());
    }

    #[tokio::test]
    async fn failover_on_rate_limit() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "primary",
                error_msg: "429 rate limit exceeded",
            }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ]);

        let resp = chain.complete(&[], &[]).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn no_failover_on_server_error() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "primary",
                error_msg: "500 internal server error",
            }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ]);

        let err = chain.complete(&[], &[]).await.unwrap_err();
        assert!(err.to_string().contains("500 internal server error"));
    }

    #[tokio::test]
    async fn failover_on_auth_error() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "primary",
                error_msg: "401 unauthorized: invalid api key",
            }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ]);

        let resp = chain.complete(&[], &[]).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn rate_limiter_rejects_primary_then_fallback_succeeds() {
        let mut cfg = moltis_config::schema::ProviderRateLimitConfig {
            enabled: true,
            wait_on_limit: false,
            ..Default::default()
        };
        cfg.defaults.max_requests_per_window = 0;
        cfg.providers.insert(
            "success".to_string(),
            moltis_config::schema::ProviderRateLimitWindowConfig {
                window_secs: 60,
                max_requests_per_window: 10,
            },
        );
        let limiter = ProviderRateLimiter::from_config(&cfg).unwrap();

        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "primary",
                error_msg: "500 internal server error",
            }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ])
        .with_rate_limiter(Some(limiter));

        let resp = chain.complete(&[], &[]).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("ok"));
    }

    struct StreamingTextProvider {
        id: &'static str,
        text: &'static str,
    }

    #[async_trait]
    impl LlmProvider for StreamingTextProvider {
        fn name(&self) -> &str {
            "streaming-text"
        }

        fn id(&self) -> &str {
            self.id
        }

        fn supports_tools(&self) -> bool {
            true
        }

        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: Some(self.text.to_string()),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
                ..Default::default()
            })
        }

        fn stream(
            &self,
            _messages: Vec<ChatMessage>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            self.stream_with_tools(vec![], vec![])
        }

        fn stream_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            _tools: Vec<serde_json::Value>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::iter(vec![
                StreamEvent::Delta(self.text.to_string()),
                StreamEvent::Done(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                }),
            ]))
        }
    }

    #[tokio::test]
    async fn streaming_rate_limiter_rejects_primary_then_uses_fallback() {
        let mut cfg = moltis_config::schema::ProviderRateLimitConfig {
            enabled: true,
            wait_on_limit: false,
            ..Default::default()
        };
        cfg.defaults.max_requests_per_window = 10;
        let limiter = ProviderRateLimiter::from_config(&cfg).unwrap();
        limiter.note_retry_after_ms("streaming-text", "primary", 10_000);

        let chain = ProviderChain::new(vec![
            Arc::new(StreamingTextProvider {
                id: "primary",
                text: "from-primary",
            }),
            Arc::new(StreamingTextProvider {
                id: "fallback",
                text: "from-fallback",
            }),
        ])
        .with_rate_limiter(Some(limiter));

        let mut stream = chain.stream(vec![]);
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::Delta(delta) = event {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "from-fallback");
    }

    #[tokio::test]
    async fn streaming_rate_limiter_waits_then_uses_primary() {
        let mut cfg = moltis_config::schema::ProviderRateLimitConfig {
            enabled: true,
            wait_on_limit: true,
            ..Default::default()
        };
        cfg.defaults.max_requests_per_window = 10;
        let limiter = ProviderRateLimiter::from_config(&cfg).unwrap();
        limiter.note_retry_after_ms("streaming-text", "primary", 40);

        let chain = ProviderChain::new(vec![
            Arc::new(StreamingTextProvider {
                id: "primary",
                text: "from-primary",
            }),
            Arc::new(StreamingTextProvider {
                id: "fallback",
                text: "from-fallback",
            }),
        ])
        .with_rate_limiter(Some(limiter));

        let start = Instant::now();
        let mut stream = chain.stream(vec![]);
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            if let StreamEvent::Delta(delta) = event {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "from-primary");
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn no_failover_on_context_window() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "primary",
                error_msg: "context_length_exceeded: too many tokens",
            }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ]);

        let err = chain.complete(&[], &[]).await.unwrap_err();
        assert!(err.to_string().contains("context_length_exceeded"));
    }

    #[tokio::test]
    async fn no_failover_on_invalid_request() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "primary",
                error_msg: "400 bad request: invalid_request",
            }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ]);

        let err = chain.complete(&[], &[]).await.unwrap_err();
        assert!(err.to_string().contains("bad request"));
    }

    #[tokio::test]
    async fn all_providers_fail() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "a",
                error_msg: "429 rate limit",
            }),
            Arc::new(FailingProvider {
                id: "b",
                error_msg: "503 service unavailable",
            }),
        ]);

        let err = chain.complete(&[], &[]).await.unwrap_err();
        assert!(err.to_string().contains("503 service unavailable"));
    }

    #[test]
    fn classify_rate_limit() {
        let err = anyhow::anyhow!("429 Too Many Requests: rate limit exceeded");
        assert_eq!(classify_error(&err), ProviderErrorKind::RateLimit);
    }

    #[test]
    fn classify_auth() {
        let err = anyhow::anyhow!("401 Unauthorized");
        assert_eq!(classify_error(&err), ProviderErrorKind::AuthError);
    }

    #[test]
    fn classify_server() {
        let err = anyhow::anyhow!("502 Bad Gateway");
        assert_eq!(classify_error(&err), ProviderErrorKind::ServerError);
    }

    #[test]
    fn classify_context_window() {
        let err = anyhow::anyhow!("context_length_exceeded: maximum context length is 200000");
        assert_eq!(classify_error(&err), ProviderErrorKind::ContextWindow);
    }

    #[test]
    fn classify_billing() {
        let err = anyhow::anyhow!("insufficient_quota: billing limit reached");
        assert_eq!(classify_error(&err), ProviderErrorKind::BillingExhausted);
    }

    #[test]
    fn classify_invalid_request() {
        let err = anyhow::anyhow!("400 Bad Request: invalid JSON");
        assert_eq!(classify_error(&err), ProviderErrorKind::InvalidRequest);
    }

    #[test]
    fn classify_unknown() {
        let err = anyhow::anyhow!("connection reset by peer");
        assert_eq!(classify_error(&err), ProviderErrorKind::Unknown);
    }

    #[test]
    fn should_failover_mapping() {
        assert!(ProviderErrorKind::RateLimit.should_failover());
        assert!(ProviderErrorKind::AuthError.should_failover());
        assert!(!ProviderErrorKind::ServerError.should_failover());
        assert!(!ProviderErrorKind::Timeout.should_failover());
        assert!(ProviderErrorKind::BillingExhausted.should_failover());
        assert!(ProviderErrorKind::Unknown.should_failover());
        assert!(!ProviderErrorKind::ContextWindow.should_failover());
        assert!(!ProviderErrorKind::InvalidRequest.should_failover());
    }

    #[test]
    fn single_provider_chain() {
        let chain = ProviderChain::single(Arc::new(SuccessProvider { id: "only" }));
        assert_eq!(chain.id(), "only");
        assert_eq!(chain.chain.len(), 1);
    }

    #[tokio::test]
    async fn records_provider_health_on_successful_completion() {
        let tracker = Arc::new(ProviderHealthTracker::new(Duration::from_secs(60), 100));
        let chain = ProviderChain::single(Arc::new(SuccessProvider { id: "health-model" }))
            .with_health_tracker(Arc::clone(&tracker));

        let _ = chain.complete(&[], &[]).await.unwrap();

        let snapshot = tracker.snapshot();
        let stats = snapshot
            .providers
            .iter()
            .find(|item| item.provider == "success" && item.model == "health-model")
            .unwrap();
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.success_count, 1);
        assert_eq!(stats.error_count, 0);
    }

    #[tokio::test]
    async fn records_provider_health_on_failed_completion() {
        let tracker = Arc::new(ProviderHealthTracker::new(Duration::from_secs(60), 100));
        let chain = ProviderChain::single(Arc::new(FailingProvider {
            id: "health-model",
            error_msg: "500 internal server error",
        }))
        .with_health_tracker(Arc::clone(&tracker));

        let _ = chain.complete(&[], &[]).await;

        let snapshot = tracker.snapshot();
        let stats = snapshot
            .providers
            .iter()
            .find(|item| item.provider == "failing" && item.model == "health-model")
            .unwrap();
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.success_count, 0);
        assert_eq!(stats.error_count, 1);
        assert!(stats.error_rate_by_class.contains_key("server_error"));
    }

    // ── Regression: stream_with_tools must forward tools to the provider ──

    /// A mock provider that records whether stream_with_tools received tools.
    struct ToolTrackingProvider {
        received_tools: Mutex<Option<Vec<serde_json::Value>>>,
    }

    impl ToolTrackingProvider {
        fn new() -> Self {
            Self {
                received_tools: Mutex::new(None),
            }
        }

        fn received_tools_count(&self) -> usize {
            self.received_tools
                .lock()
                .unwrap()
                .as_ref()
                .map_or(0, |t| t.len())
        }
    }

    #[async_trait]
    impl LlmProvider for ToolTrackingProvider {
        fn name(&self) -> &str {
            "tool-tracker"
        }

        fn id(&self) -> &str {
            "tool-tracker"
        }

        fn supports_tools(&self) -> bool {
            true
        }

        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: Some("ok".into()),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    ..Default::default()
                },
                ..Default::default()
            })
        }

        fn stream(
            &self,
            _messages: Vec<ChatMessage>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::once(StreamEvent::Done(Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            })))
        }

        fn stream_with_tools(
            &self,
            _messages: Vec<ChatMessage>,
            tools: Vec<serde_json::Value>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            *self.received_tools.lock().unwrap() = Some(tools);
            Box::pin(tokio_stream::once(StreamEvent::Done(Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            })))
        }
    }

    #[tokio::test]
    async fn chain_stream_with_tools_forwards_tools() {
        // Regression test: before the fix, ProviderChain::stream_with_tools()
        // used the default trait impl which dropped tools and called stream().
        let tracker = Arc::new(ToolTrackingProvider::new());
        let chain = ProviderChain::single(tracker.clone());

        let tools = vec![serde_json::json!({
            "name": "test_tool",
            "description": "A test",
            "parameters": {"type": "object"}
        })];

        let mut stream = chain.stream_with_tools(vec![], tools);
        while stream.next().await.is_some() {}

        assert_eq!(
            tracker.received_tools_count(),
            1,
            "ProviderChain must forward tools to the underlying provider's stream_with_tools()"
        );
    }

    #[tokio::test]
    async fn chain_stream_with_tools_forwards_empty_tools() {
        let tracker = Arc::new(ToolTrackingProvider::new());
        let chain = ProviderChain::single(tracker.clone());

        let mut stream = chain.stream_with_tools(vec![], vec![]);
        while stream.next().await.is_some() {}

        assert_eq!(tracker.received_tools_count(), 0);
    }

    #[test]
    fn classify_non_retryable_rate_limit() {
        let err = anyhow::anyhow!("Your plan does not include access to this model");
        assert_eq!(
            classify_error(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn classify_non_retryable_model_not_available() {
        let err = anyhow::anyhow!("model not available for your account");
        assert_eq!(
            classify_error(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn classify_non_retryable_upgrade_plan() {
        let err = anyhow::anyhow!("Please upgrade your plan to access this feature");
        assert_eq!(
            classify_error(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn classify_non_retryable_org_access() {
        let err = anyhow::anyhow!("Your organization does not have access to this model");
        assert_eq!(
            classify_error(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn parse_retry_after_seconds() {
        assert_eq!(
            parse_retry_after_ms("rate limited, retry after 30"),
            Some(30_000)
        );
        assert_eq!(parse_retry_after_ms("Retry-After: 5.5"), Some(5_500));
        assert_eq!(parse_retry_after_ms("no hint here"), None);
    }

    #[test]
    fn parse_retry_after_integer() {
        assert_eq!(parse_retry_after_ms("retry-after: 60"), Some(60_000));
    }

    #[test]
    fn non_retryable_rate_limit_should_failover() {
        assert!(ProviderErrorKind::NonRetryableRateLimit.should_failover());
    }

    // ── Streaming metrics wrapper tests ──────────────────────────────

    /// A provider that emits a configurable sequence of stream events.
    struct ScriptedStreamProvider {
        events: Vec<StreamEvent>,
    }

    #[async_trait]
    impl LlmProvider for ScriptedStreamProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn id(&self) -> &str {
            "scripted-model"
        }

        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                text: Some("ok".into()),
                tool_calls: vec![],
                usage: Usage::default(),
                ..Default::default()
            })
        }

        fn stream(
            &self,
            _messages: Vec<ChatMessage>,
        ) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send + '_>> {
            Box::pin(tokio_stream::iter(self.events.clone()))
        }
    }

    #[tokio::test]
    async fn stream_with_tools_preserves_all_events() {
        let events = vec![
            StreamEvent::Delta("hello ".into()),
            StreamEvent::Delta("world".into()),
            StreamEvent::Done(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
        ];
        let chain = ProviderChain::single(Arc::new(ScriptedStreamProvider {
            events: events.clone(),
        }));

        let mut stream = chain.stream(vec![]);
        let mut collected = Vec::new();
        while let Some(ev) = stream.next().await {
            collected.push(ev);
        }

        assert_eq!(collected.len(), 3, "all events should pass through");
        assert!(matches!(&collected[0], StreamEvent::Delta(t) if t == "hello "));
        assert!(matches!(&collected[1], StreamEvent::Delta(t) if t == "world"));
        assert!(matches!(&collected[2], StreamEvent::Done(_)));
    }

    #[tokio::test]
    async fn stream_with_tools_passes_error_events() {
        let events = vec![
            StreamEvent::Delta("partial".into()),
            StreamEvent::Error("something broke".into()),
        ];
        let chain = ProviderChain::single(Arc::new(ScriptedStreamProvider { events }));

        let mut stream = chain.stream(vec![]);
        let mut collected = Vec::new();
        while let Some(ev) = stream.next().await {
            collected.push(ev);
        }

        assert_eq!(collected.len(), 2);
        assert!(matches!(&collected[1], StreamEvent::Error(msg) if msg == "something broke"));
    }
}
