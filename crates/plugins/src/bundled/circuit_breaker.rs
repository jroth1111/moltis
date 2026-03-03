//! Circuit breaker hook for LLM provider failover.
//!
//! Tracks failures per `(provider, error kind)` and opens independent circuits so
//! transient failures (e.g. 5xx) do not share cooldown state with permanent
//! ones (e.g. exhausted billing quota).

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use {
    async_trait::async_trait,
    tokio::sync::RwLock,
    tracing::{info, warn},
};

use moltis_agents::classify::{ProviderErrorKind, classify_error_message};
use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

#[derive(Debug, Clone)]
enum CircuitState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen,
}

/// Rollout mode for circuit-breaker enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementLevel {
    Disabled,
    Observe,
    Warn,
    Enforce,
}

/// Snapshot of a single provider/error-class circuit.
#[derive(Debug, Clone, Serialize)]
pub struct CircuitSnapshot {
    pub provider: String,
    pub error_kind: String,
    pub enforcement: EnforcementLevel,
    pub threshold: u32,
    pub state: String,
    pub failure_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_for_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CircuitKey {
    provider: String,
    kind: ProviderErrorKind,
}

impl CircuitKey {
    fn new(provider: String, kind: ProviderErrorKind) -> Self {
        Self { provider, kind }
    }
}

pub struct CircuitBreakerHook {
    states: Arc<RwLock<HashMap<CircuitKey, CircuitState>>>,
    failure_counts: Arc<RwLock<HashMap<CircuitKey, u32>>>,
    default_failure_threshold: u32,
    reset_timeout: Duration,
    thresholds: HashMap<ProviderErrorKind, u32>,
    enforcement_levels: HashMap<ProviderErrorKind, EnforcementLevel>,
}

impl CircuitBreakerHook {
    pub fn new(failure_threshold: u32, reset_timeout_secs: u64) -> Self {
        let mut thresholds = HashMap::new();
        thresholds.insert(ProviderErrorKind::BillingExhausted, 1);
        thresholds.insert(ProviderErrorKind::AuthError, 1);
        thresholds.insert(ProviderErrorKind::NonRetryableRateLimit, 1);
        thresholds.insert(ProviderErrorKind::RateLimit, 3);
        thresholds.insert(ProviderErrorKind::ServerError, 5);
        thresholds.insert(ProviderErrorKind::Timeout, 5);

        let enforcement_levels = all_error_kinds()
            .into_iter()
            .map(|kind| (kind, EnforcementLevel::Observe))
            .collect();

        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            failure_counts: Arc::new(RwLock::new(HashMap::new())),
            default_failure_threshold: failure_threshold.max(1),
            reset_timeout: Duration::from_secs(reset_timeout_secs),
            thresholds,
            enforcement_levels,
        }
    }

    /// Override enforcement level for a specific provider error kind.
    #[must_use]
    pub fn with_enforcement(mut self, kind: ProviderErrorKind, level: EnforcementLevel) -> Self {
        self.enforcement_levels.insert(kind, level);
        self
    }

    /// Override failure threshold for a specific provider error kind.
    #[must_use]
    pub fn with_threshold(mut self, kind: ProviderErrorKind, threshold: u32) -> Self {
        self.thresholds.insert(kind, threshold.max(1));
        self
    }

    pub async fn snapshot(&self) -> Vec<CircuitSnapshot> {
        let states = self.states.read().await;
        let counts = self.failure_counts.read().await;

        let mut keys: Vec<CircuitKey> = states
            .keys()
            .chain(counts.keys())
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        keys.sort_by(|a, b| {
            a.provider
                .cmp(&b.provider)
                .then(error_kind_name(a.kind).cmp(error_kind_name(b.kind)))
        });

        keys.into_iter()
            .map(|key| {
                let state = states.get(&key).cloned().unwrap_or(CircuitState::Closed);
                let failure_count = *counts.get(&key).unwrap_or(&0);
                let (state_name, open_for_secs) = match state {
                    CircuitState::Closed => ("closed".to_string(), None),
                    CircuitState::HalfOpen => ("half_open".to_string(), None),
                    CircuitState::Open { opened_at } => {
                        ("open".to_string(), Some(opened_at.elapsed().as_secs()))
                    },
                };
                CircuitSnapshot {
                    provider: key.provider,
                    error_kind: error_kind_name(key.kind).to_string(),
                    enforcement: self.enforcement_for_kind(key.kind),
                    threshold: self.threshold_for_kind(key.kind),
                    state: state_name,
                    failure_count,
                    open_for_secs,
                }
            })
            .collect()
    }

    pub async fn reset_provider(&self, provider: &str) -> bool {
        let mut states = self.states.write().await;
        let before_states = states.len();
        states.retain(|key, _| key.provider != provider);

        let mut counts = self.failure_counts.write().await;
        let before_counts = counts.len();
        counts.retain(|key, _| key.provider != provider);

        states.len() != before_states || counts.len() != before_counts
    }

    pub async fn reset_all(&self) {
        self.states.write().await.clear();
        self.failure_counts.write().await.clear();
    }

    fn threshold_for_kind(&self, kind: ProviderErrorKind) -> u32 {
        self.thresholds
            .get(&kind)
            .copied()
            .unwrap_or(self.default_failure_threshold)
            .max(1)
    }

    fn enforcement_for_kind(&self, kind: ProviderErrorKind) -> EnforcementLevel {
        self.enforcement_levels
            .get(&kind)
            .copied()
            .unwrap_or(EnforcementLevel::Observe)
    }
}

#[async_trait]
impl HookHandler for CircuitBreakerHook {
    fn name(&self) -> &str {
        "circuit-breaker"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::BeforeLLMCall, HookEvent::AfterLLMCall]
    }

    fn priority(&self) -> i32 {
        50
    }

    async fn handle(&self, _event: HookEvent, payload: &HookPayload) -> Result<HookAction> {
        match payload {
            HookPayload::BeforeLLMCall { provider, .. } => {
                let mut states = self.states.write().await;
                let provider_keys: Vec<CircuitKey> = states
                    .keys()
                    .filter(|key| key.provider == *provider)
                    .cloned()
                    .collect();

                for key in provider_keys {
                    if let Some(state) = states.get_mut(&key) {
                        match state {
                            CircuitState::Open { opened_at } => {
                                if opened_at.elapsed() >= self.reset_timeout {
                                    *state = CircuitState::HalfOpen;
                                    info!(
                                        provider = %provider,
                                        error_kind = %error_kind_name(key.kind),
                                        "circuit-breaker: timeout elapsed, entering half-open"
                                    );
                                    continue;
                                }

                                match self.enforcement_for_kind(key.kind) {
                                    EnforcementLevel::Disabled | EnforcementLevel::Observe => {},
                                    EnforcementLevel::Warn => {
                                        warn!(
                                            provider = %provider,
                                            error_kind = %error_kind_name(key.kind),
                                            threshold = self.threshold_for_kind(key.kind),
                                            "circuit-breaker: open circuit observed"
                                        );
                                    },
                                    EnforcementLevel::Enforce => {
                                        return Ok(HookAction::Block(format!(
                                            "Circuit open for provider {provider} ({})",
                                            error_kind_name(key.kind)
                                        )));
                                    },
                                }
                            },
                            CircuitState::HalfOpen | CircuitState::Closed => {},
                        }
                    }
                }
                Ok(HookAction::Continue)
            },
            HookPayload::AfterLLMCall {
                provider,
                error,
                text,
                tool_calls,
                output_tokens,
                ..
            } => {
                let maybe_failure_kind = if let Some(raw_error) = error.as_deref() {
                    Some(classify_error_message(raw_error))
                } else if *output_tokens == 0 && text.is_none() && tool_calls.is_empty() {
                    // Legacy fallback if the caller didn't provide a concrete error string.
                    Some(ProviderErrorKind::Unknown)
                } else {
                    None
                };

                if let Some(kind) = maybe_failure_kind {
                    let key = CircuitKey::new(provider.clone(), kind);
                    let threshold = self.threshold_for_kind(kind);
                    let mut states = self.states.write().await;
                    let mut counts = self.failure_counts.write().await;
                    let current = states.get(&key).cloned().unwrap_or(CircuitState::Closed);

                    match current {
                        CircuitState::HalfOpen => {
                            counts.insert(key.clone(), threshold);
                            states.insert(
                                key,
                                CircuitState::Open {
                                    opened_at: Instant::now(),
                                },
                            );
                            info!(
                                provider = %provider,
                                error_kind = %error_kind_name(kind),
                                "circuit-breaker: half-open probe failed, circuit reopened"
                            );
                        },
                        CircuitState::Closed | CircuitState::Open { .. } => {
                            let count = counts.entry(key.clone()).or_insert(0);
                            *count += 1;
                            if *count >= threshold {
                                states.insert(
                                    key,
                                    CircuitState::Open {
                                        opened_at: Instant::now(),
                                    },
                                );

                                match self.enforcement_for_kind(kind) {
                                    EnforcementLevel::Warn | EnforcementLevel::Enforce => {
                                        warn!(
                                            provider = %provider,
                                            error_kind = %error_kind_name(kind),
                                            failures = *count,
                                            threshold,
                                            "circuit-breaker: circuit opened"
                                        );
                                    },
                                    EnforcementLevel::Disabled | EnforcementLevel::Observe => {
                                        info!(
                                            provider = %provider,
                                            error_kind = %error_kind_name(kind),
                                            failures = *count,
                                            threshold,
                                            "circuit-breaker: circuit opened"
                                        );
                                    },
                                }
                            }
                        },
                    }
                } else {
                    // Successful call: close all provider circuits and clear counts.
                    let mut counts = self.failure_counts.write().await;
                    for (key, count) in &mut *counts {
                        if key.provider == *provider {
                            *count = 0;
                        }
                    }

                    let mut states = self.states.write().await;
                    for (key, state) in &mut *states {
                        if key.provider == *provider {
                            *state = CircuitState::Closed;
                        }
                    }
                }

                Ok(HookAction::Continue)
            },
            _ => Ok(HookAction::Continue),
        }
    }
}

fn all_error_kinds() -> [ProviderErrorKind; 9] {
    [
        ProviderErrorKind::RateLimit,
        ProviderErrorKind::AuthError,
        ProviderErrorKind::ServerError,
        ProviderErrorKind::Timeout,
        ProviderErrorKind::BillingExhausted,
        ProviderErrorKind::NonRetryableRateLimit,
        ProviderErrorKind::ContextWindow,
        ProviderErrorKind::InvalidRequest,
        ProviderErrorKind::Unknown,
    ]
}

fn error_kind_name(kind: ProviderErrorKind) -> &'static str {
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
    use super::*;

    #[tokio::test]
    async fn tracks_error_classes_independently() {
        let hook = CircuitBreakerHook::new(10, 60)
            .with_threshold(ProviderErrorKind::BillingExhausted, 1)
            .with_threshold(ProviderErrorKind::ServerError, 5);

        let billing_fail = HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            error: Some("insufficient_quota".into()),
            text: None,
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 0,
            iteration: 1,
            trace_id: None,
        };
        hook.handle(HookEvent::AfterLLMCall, &billing_fail)
            .await
            .unwrap();

        let server_fail = HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            error: Some("HTTP 503 Service Unavailable".into()),
            text: None,
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 0,
            iteration: 1,
            trace_id: None,
        };
        hook.handle(HookEvent::AfterLLMCall, &server_fail)
            .await
            .unwrap();

        let snapshot = hook.snapshot().await;
        let billing = snapshot
            .iter()
            .find(|item| item.provider == "p" && item.error_kind == "billing_exhausted")
            .unwrap();
        assert_eq!(billing.failure_count, 1);
        assert_eq!(billing.threshold, 1);
        assert_eq!(billing.state, "open");

        let server = snapshot
            .iter()
            .find(|item| item.provider == "p" && item.error_kind == "server_error")
            .unwrap();
        assert_eq!(server.failure_count, 1);
        assert_eq!(server.threshold, 5);
        assert_eq!(server.state, "closed");
    }

    #[tokio::test]
    async fn observe_mode_does_not_block_open_circuit() {
        let hook = CircuitBreakerHook::new(1, 60);

        let fail = HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            error: Some("insufficient_quota".into()),
            text: None,
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 0,
            iteration: 1,
            trace_id: None,
        };
        hook.handle(HookEvent::AfterLLMCall, &fail).await.unwrap();

        let before = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 2,
            trace_id: None,
        };
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &before)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[tokio::test]
    async fn enforce_mode_blocks_open_circuit() {
        let hook = CircuitBreakerHook::new(1, 60).with_enforcement(
            ProviderErrorKind::BillingExhausted,
            EnforcementLevel::Enforce,
        );

        let fail = HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            error: Some("insufficient_quota".into()),
            text: None,
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 0,
            iteration: 1,
            trace_id: None,
        };
        hook.handle(HookEvent::AfterLLMCall, &fail).await.unwrap();

        let before = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 2,
            trace_id: None,
        };
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &before)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }

    #[tokio::test]
    async fn reset_provider_clears_all_error_class_state() {
        let hook = CircuitBreakerHook::new(1, 60);

        for msg in ["insufficient_quota", "HTTP 503 Service Unavailable"] {
            let fail = HookPayload::AfterLLMCall {
                session_key: "s1".into(),
                provider: "p".into(),
                model: "m".into(),
                error: Some(msg.to_string()),
                text: None,
                tool_calls: vec![],
                input_tokens: 1,
                output_tokens: 0,
                iteration: 1,
                trace_id: None,
            };
            hook.handle(HookEvent::AfterLLMCall, &fail).await.unwrap();
        }

        assert!(hook.reset_provider("p").await);
        assert!(hook.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn does_not_treat_zero_tokens_with_text_as_failure() {
        let hook = CircuitBreakerHook::new(1, 60);
        let payload = HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            text: Some("ok".into()),
            tool_calls: vec![],
            input_tokens: 100,
            output_tokens: 0,
            iteration: 1,
            error: None,
            trace_id: None,
        };
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();
        let snapshot = hook.snapshot().await;
        let state = snapshot.iter().find(|s| s.provider == "p").unwrap();
        assert_eq!(state.state, "closed");
        assert_eq!(state.failure_count, 0);
    }
}
