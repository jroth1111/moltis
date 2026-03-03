//! Provider failover chain with per-provider circuit breakers.
//!
//! `ProviderChain` wraps a primary `LlmProvider` with a list of fallbacks.
//! When the primary fails with a retryable error (rate limit, auth, server error),
//! it automatically tries the next provider in the chain, skipping any that have
//! their circuit breaker tripped.

use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use {async_trait::async_trait, tokio_stream::Stream, tracing::warn};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, histogram, labels, llm as llm_metrics};

use crate::{
    classify,
    model::{ChatMessage, CompletionResponse, LlmProvider, StreamEvent},
    provider_health::ProviderHealthTracker,
    rate_limiter::{ProviderRateLimiter, RateLimitDecision},
};

// ── Circuit breaker (same pattern as embeddings_fallback.rs) ─────────────

/// Circuit breaker state for a single provider.
struct ProviderState {
    consecutive_failures: AtomicUsize,
    last_failure: Mutex<Option<Instant>>,
}

impl ProviderState {
    fn new() -> Self {
        Self {
            consecutive_failures: AtomicUsize::new(0),
            last_failure: Mutex::new(None),
        }
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::SeqCst);
    }

    fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
        *self.last_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }

    /// Returns `true` when the circuit is open (provider should be skipped).
    fn is_tripped(&self, threshold: usize, cooldown: Duration) -> bool {
        let failures = self.consecutive_failures.load(Ordering::SeqCst);
        if failures < threshold {
            return false;
        }
        let last = self.last_failure.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(t) if t.elapsed() < cooldown => true,
            _ => {
                drop(last);
                self.consecutive_failures.store(0, Ordering::SeqCst);
                false
            },
        }
    }
}

/// A provider entry in the failover chain.
struct ChainEntry {
    provider: Arc<dyn LlmProvider>,
    state: ProviderState,
}

/// Maximum time to wait for a rate limit slot before trying failover.
const RATE_LIMIT_WAIT_CAP: Duration = Duration::from_secs(10);

/// Failover chain that tries providers in order, with circuit breakers.
///
/// Implements `LlmProvider` itself so callers don't need to know about failover.
pub struct ProviderChain {
    chain: Vec<ChainEntry>,
    cb_threshold: usize,
    cb_cooldown: Duration,
    health: Arc<ProviderHealthTracker>,
    rate_limiter: Option<Arc<ProviderRateLimiter>>,
}

impl ProviderChain {
    /// Build a chain from a list of providers (primary first, then fallbacks).
    pub fn new(providers: Vec<Arc<dyn LlmProvider>>) -> Self {
        let chain = providers
            .into_iter()
            .map(|provider| ChainEntry {
                provider,
                state: ProviderState::new(),
            })
            .collect();
        Self {
            chain,
            cb_threshold: 3,
            cb_cooldown: Duration::from_secs(60),
            health: Arc::new(ProviderHealthTracker::default_window()),
            rate_limiter: None,
        }
    }

    /// Build a chain with a shared health tracker.
    ///
    /// Use this when you want multiple `ProviderChain` instances to aggregate
    /// health data into the same tracker (e.g. across sessions).
    pub fn with_health_tracker(
        providers: Vec<Arc<dyn LlmProvider>>,
        health: Arc<ProviderHealthTracker>,
    ) -> Self {
        let chain = providers
            .into_iter()
            .map(|provider| ChainEntry {
                provider,
                state: ProviderState::new(),
            })
            .collect();
        Self {
            chain,
            cb_threshold: 3,
            cb_cooldown: Duration::from_secs(60),
            health,
            rate_limiter: None,
        }
    }

    /// Build a chain with one provider (no failover). Useful as a passthrough.
    pub fn single(provider: Arc<dyn LlmProvider>) -> Self {
        Self::new(vec![provider])
    }

    /// Override the circuit breaker threshold and cooldown.
    pub fn with_circuit_breaker(mut self, threshold: usize, cooldown: Duration) -> Self {
        self.cb_threshold = threshold;
        self.cb_cooldown = cooldown;
        self
    }

    /// Attach a shared outbound rate limiter.
    pub fn with_rate_limiter(mut self, limiter: Arc<ProviderRateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    fn primary(&self) -> &ChainEntry {
        &self.chain[0]
    }

    /// Get a reference to the underlying health tracker (for sharing with API routes).
    #[must_use]
    pub fn health_tracker(&self) -> &Arc<ProviderHealthTracker> {
        &self.health
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

    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<CompletionResponse> {
        let mut errors = Vec::new();
        #[cfg(feature = "metrics")]
        let start = Instant::now();

        for entry in &self.chain {
            if entry.state.is_tripped(self.cb_threshold, self.cb_cooldown) {
                continue;
            }

            let provider_name = entry.provider.name().to_string();
            let model_id = entry.provider.id().to_string();

            // Check outbound rate limit before calling the provider.
            if let Some(ref rl) = self.rate_limiter {
                match rl.check(&provider_name, &model_id) {
                    RateLimitDecision::Wait(wait) => {
                        if wait > RATE_LIMIT_WAIT_CAP {
                            // Too long to wait — skip to next provider.
                            #[cfg(feature = "metrics")]
                            counter!("moltis_provider_rate_limit_rejected").increment(1);

                            warn!(
                                provider = %provider_name,
                                model = %model_id,
                                wait_ms = wait.as_millis() as u64,
                                "rate limit wait exceeds cap, trying next provider"
                            );
                            errors.push(format!(
                                "{}: rate limited (wait {}ms > cap {}ms)",
                                model_id,
                                wait.as_millis(),
                                RATE_LIMIT_WAIT_CAP.as_millis()
                            ));
                            continue;
                        }
                        // Wait within the cap.
                        #[cfg(feature = "metrics")]
                        counter!("moltis_provider_rate_limit_queued").increment(1);

                        tracing::debug!(
                            provider = %provider_name,
                            model = %model_id,
                            wait_ms = wait.as_millis() as u64,
                            "waiting for rate limit slot"
                        );
                        tokio::time::sleep(wait).await;
                    },
                    RateLimitDecision::Allowed => {},
                }
            }

            let call_start = Instant::now();
            match entry.provider.complete(messages, tools).await {
                Ok(resp) => {
                    entry.state.record_success();
                    self.health.record_success(
                        &provider_name,
                        &model_id,
                        call_start.elapsed().as_millis() as u64,
                    );

                    // Record the call for rate limiting.
                    if let Some(ref rl) = self.rate_limiter {
                        rl.record(&provider_name, &model_id);
                    }

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
                    let kind = classify::classify_anyhow(&e);
                    entry.state.record_failure();
                    self.health.record_error(
                        &provider_name,
                        &model_id,
                        call_start.elapsed().as_millis() as u64,
                        &format!("{kind:?}"),
                    );

                    // Record the call for rate limiting even on failure.
                    if let Some(ref rl) = self.rate_limiter {
                        rl.record(&provider_name, &model_id);

                        // If it was a rate limit error, apply retry-after hint.
                        if kind == classify::ProviderErrorKind::RateLimit {
                            let wait_ms = classify::extract_retry_after_ms(&e.to_string(), 60_000)
                                .unwrap_or(10_000);
                            rl.apply_retry_after(
                                &provider_name,
                                &model_id,
                                Duration::from_millis(wait_ms),
                            );
                        }
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
        // For streaming, we try the first non-tripped, non-rate-limited provider.
        // If the stream yields an Error event, we can't transparently retry mid-stream,
        // so we pick the best available provider upfront.
        let mut selected_entry = None;
        for entry in &self.chain {
            if entry.state.is_tripped(self.cb_threshold, self.cb_cooldown) {
                continue;
            }

            // Check rate limiter — skip providers that would require waiting
            // longer than the cap.
            if let Some(ref rl) = self.rate_limiter {
                let provider_name = entry.provider.name();
                let model_id = entry.provider.id();
                if let RateLimitDecision::Wait(wait) = rl.check(provider_name, model_id)
                    && wait > RATE_LIMIT_WAIT_CAP
                {
                    #[cfg(feature = "metrics")]
                    counter!("moltis_provider_rate_limit_rejected").increment(1);
                    continue;
                }
                rl.record(provider_name, model_id);
            }

            selected_entry = Some(entry);
            break;
        }
        // All tripped or rate limited — try primary anyway.
        if selected_entry.is_none()
            && let Some(ref rl) = self.rate_limiter
        {
            let p = self.primary();
            rl.record(p.provider.name(), p.provider.id());
        }
        let entry = selected_entry.unwrap_or_else(|| self.primary());
        let provider_name = entry.provider.name().to_string();
        let model_id = entry.provider.id().to_string();
        let inner = entry.provider.stream_with_tools(messages, tools);

        let health = Arc::clone(&self.health);
        let start = Instant::now();
        let wrapped = {
            use tokio_stream::StreamExt;
            inner.map(move |event| {
                match &event {
                    StreamEvent::Done(_) => {
                        health.record_success(
                            &provider_name,
                            &model_id,
                            start.elapsed().as_millis() as u64,
                        );
                    },
                    StreamEvent::Error(msg) => {
                        health.record_error(
                            &provider_name,
                            &model_id,
                            start.elapsed().as_millis() as u64,
                            msg,
                        );
                    },
                    _ => {},
                }
                event
            })
        };
        Box::pin(wrapped)
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            classify::ProviderErrorKind,
            model::{ChatMessage, StreamEvent, Usage},
        },
        async_trait::async_trait,
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
    async fn failover_on_server_error() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "primary",
                error_msg: "500 internal server error",
            }),
            Arc::new(SuccessProvider { id: "fallback" }),
        ]);

        let resp = chain.complete(&[], &[]).await.unwrap();
        assert_eq!(resp.text.as_deref(), Some("ok"));
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
        assert!(
            err.to_string()
                .contains("all providers in failover chain failed")
        );
    }

    #[tokio::test]
    async fn circuit_breaker_trips_after_three_failures() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "flaky",
                error_msg: "500 internal server error",
            }),
            Arc::new(SuccessProvider { id: "backup" }),
        ]);

        // Fail 3 times to trip the circuit breaker on the first provider.
        for _ in 0..3 {
            let _ = chain.complete(&[], &[]).await;
        }

        // After tripping, the flaky provider should be skipped.
        assert!(chain.chain[0].state.is_tripped(3, Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn stream_uses_first_non_tripped() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "tripped",
                error_msg: "500 error",
            }),
            Arc::new(SuccessProvider { id: "backup" }),
        ]);

        // Trip the first provider.
        for _ in 0..3 {
            let _ = chain.complete(&[], &[]).await;
        }

        // Stream should use backup.
        let mut stream = chain.stream(vec![]);
        let event = stream.next().await.unwrap();
        assert!(matches!(event, StreamEvent::Done(_)));
    }

    #[test]
    fn classify_rate_limit() {
        let err = anyhow::anyhow!("429 Too Many Requests: rate limit exceeded");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::RateLimit
        );
    }

    #[test]
    fn classify_auth() {
        let err = anyhow::anyhow!("401 Unauthorized");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::AuthError
        );
    }

    #[test]
    fn classify_server() {
        let err = anyhow::anyhow!("502 Bad Gateway");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::ServerError
        );
    }

    #[test]
    fn classify_context_window() {
        let err = anyhow::anyhow!("context_length_exceeded: maximum context length is 200000");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::ContextWindow
        );
    }

    #[test]
    fn classify_billing() {
        let err = anyhow::anyhow!("insufficient_quota: billing limit reached");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::BillingExhausted
        );
    }

    #[test]
    fn classify_invalid_request() {
        let err = anyhow::anyhow!("400 Bad Request: invalid JSON");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn classify_unknown() {
        let err = anyhow::anyhow!("connection reset by peer");
        assert_eq!(classify::classify_anyhow(&err), ProviderErrorKind::Unknown);
    }

    #[test]
    fn should_failover_mapping() {
        assert!(ProviderErrorKind::RateLimit.should_failover());
        assert!(ProviderErrorKind::AuthError.should_failover());
        assert!(ProviderErrorKind::ServerError.should_failover());
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
            classify::classify_anyhow(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn classify_non_retryable_model_not_available() {
        let err = anyhow::anyhow!("model not available for your account");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn classify_non_retryable_upgrade_plan() {
        let err = anyhow::anyhow!("Please upgrade your plan to access this feature");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn classify_non_retryable_org_access() {
        let err = anyhow::anyhow!("Your organization does not have access to this model");
        assert_eq!(
            classify::classify_anyhow(&err),
            ProviderErrorKind::NonRetryableRateLimit
        );
    }

    #[test]
    fn parse_retry_after_seconds() {
        assert_eq!(
            classify::parse_retry_after_ms("rate limited, retry after 30"),
            Some(30_000)
        );
        assert_eq!(
            classify::parse_retry_after_ms("Retry-After: 5.5"),
            Some(5_500)
        );
        assert_eq!(classify::parse_retry_after_ms("no hint here"), None);
    }

    #[test]
    fn parse_retry_after_integer() {
        assert_eq!(
            classify::parse_retry_after_ms("retry-after: 60"),
            Some(60_000)
        );
    }

    #[test]
    fn non_retryable_rate_limit_should_failover() {
        assert!(ProviderErrorKind::NonRetryableRateLimit.should_failover());
    }

    #[tokio::test]
    async fn configurable_cb_threshold_one_failure() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "flaky",
                error_msg: "500 internal server error",
            }),
            Arc::new(SuccessProvider { id: "backup" }),
        ])
        .with_circuit_breaker(1, Duration::from_secs(60));

        // First failure should trip the breaker (threshold=1).
        let _ = chain.complete(&[], &[]).await;
        assert!(chain.chain[0].state.is_tripped(1, Duration::from_secs(60)));
    }

    #[tokio::test]
    async fn configurable_cb_cooldown() {
        let chain = ProviderChain::new(vec![
            Arc::new(FailingProvider {
                id: "flaky",
                error_msg: "500 internal server error",
            }),
            Arc::new(SuccessProvider { id: "backup" }),
        ])
        .with_circuit_breaker(1, Duration::from_millis(50));

        let _ = chain.complete(&[], &[]).await;
        assert!(
            chain.chain[0]
                .state
                .is_tripped(1, Duration::from_millis(50))
        );

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !chain.chain[0]
                .state
                .is_tripped(1, Duration::from_millis(50))
        );
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

    // ── Health tracker integration tests ─────────────────────────────

    #[tokio::test]
    async fn complete_records_health_success() {
        let chain = ProviderChain::single(Arc::new(SuccessProvider { id: "test-model" }));
        chain.complete(&[], &[]).await.unwrap();

        let snap = chain.health_tracker().snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].provider, "success");
        assert_eq!(snap[0].model, "test-model");
        assert_eq!(snap[0].successes, 1);
        assert_eq!(snap[0].errors, 0);
    }

    #[tokio::test]
    async fn complete_records_health_error() {
        let chain = ProviderChain::single(Arc::new(FailingProvider {
            id: "bad-model",
            error_msg: "context_length_exceeded",
        }));
        let _ = chain.complete(&[], &[]).await;

        let snap = chain.health_tracker().snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].errors, 1);
        assert_eq!(snap[0].successes, 0);
        assert!(snap[0].errors_by_class.contains_key("ContextWindow"));
    }

    #[tokio::test]
    async fn stream_records_health_on_done() {
        let events = vec![
            StreamEvent::Delta("hi".into()),
            StreamEvent::Done(Usage {
                input_tokens: 1,
                output_tokens: 1,
                ..Default::default()
            }),
        ];
        let chain = ProviderChain::single(Arc::new(ScriptedStreamProvider { events }));

        let mut stream = chain.stream(vec![]);
        while stream.next().await.is_some() {}

        let snap = chain.health_tracker().snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].successes, 1);
        assert_eq!(snap[0].errors, 0);
    }

    #[tokio::test]
    async fn stream_records_health_on_error() {
        let events = vec![StreamEvent::Error("boom".into())];
        let chain = ProviderChain::single(Arc::new(ScriptedStreamProvider { events }));

        let mut stream = chain.stream(vec![]);
        while stream.next().await.is_some() {}

        let snap = chain.health_tracker().snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].errors, 1);
        assert_eq!(snap[0].errors_by_class.get("boom"), Some(&1));
    }
}
