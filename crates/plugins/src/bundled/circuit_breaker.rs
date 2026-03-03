//! Per-error-class circuit breaker for LLM provider failover.
//!
//! Tracks failures per `(provider, ProviderErrorKind)` pair with per-class
//! thresholds. Each error class has its own independent sliding window so that,
//! e.g., billing exhaustion trips immediately while transient server errors
//! require several consecutive failures.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};

use {async_trait::async_trait, serde::Serialize, tokio::sync::RwLock, tracing::info};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels};

use {
    moltis_agents::classify::{ProviderErrorKind, classify_error},
    moltis_common::{
        Result,
        hooks::{HookAction, HookEvent, HookHandler, HookPayload},
    },
};

// ── Enforcement level ───────────────────────────────────────────────────

/// Controls whether a tripped circuit actually blocks requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum EnforcementLevel {
    /// Circuit breaker is disabled — never record, never block.
    Disabled,
    /// Record state transitions but allow all requests through.
    Observe,
    /// Log warnings when the circuit would block, but allow requests through.
    Warn,
    /// Block requests when the circuit is open.
    Enforce,
}

// ── Per-class thresholds ────────────────────────────────────────────────

/// Number of consecutive failures before the circuit opens for a given error class.
#[must_use]
pub fn threshold_for(kind: &ProviderErrorKind) -> u32 {
    match kind {
        ProviderErrorKind::BillingExhausted | ProviderErrorKind::NonRetryableRateLimit => 1,
        ProviderErrorKind::AuthError => 2,
        ProviderErrorKind::RateLimit | ProviderErrorKind::Timeout => 3,
        ProviderErrorKind::ServerError
        | ProviderErrorKind::ContextWindow
        | ProviderErrorKind::InvalidRequest
        | ProviderErrorKind::Unknown => 5,
    }
}

/// Enforcement level for a given error class.
///
/// Starts in Observe mode for all classes to collect data safely before
/// enforcing blocks.
#[must_use]
pub fn enforcement_for(_kind: &ProviderErrorKind) -> EnforcementLevel {
    EnforcementLevel::Observe
}

// ── Circuit state ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum CircuitState {
    Closed,
    Open { opened_at: Instant },
    HalfOpen,
}

/// Compound key: `(provider_name, error_class)`.
type CircuitKey = (String, ProviderErrorKind);

// ── Snapshot (serializable) ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CircuitSnapshot {
    pub provider: String,
    pub error_class: String,
    pub state: String,
    pub failure_count: u32,
    pub threshold: u32,
    pub enforcement: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_for_secs: Option<u64>,
}

// ── Hook implementation ─────────────────────────────────────────────────

pub struct CircuitBreakerHook {
    states: Arc<RwLock<HashMap<CircuitKey, CircuitState>>>,
    failure_counts: Arc<RwLock<HashMap<CircuitKey, u32>>>,
    reset_timeout: Duration,
}

impl CircuitBreakerHook {
    pub fn new(_failure_threshold: u32, reset_timeout_secs: u64) -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            failure_counts: Arc::new(RwLock::new(HashMap::new())),
            reset_timeout: Duration::from_secs(reset_timeout_secs),
        }
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
            a.0.cmp(&b.0)
                .then_with(|| format!("{:?}", a.1).cmp(&format!("{:?}", b.1)))
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
                    provider: key.0,
                    error_class: format!("{:?}", key.1),
                    state: state_name,
                    failure_count,
                    threshold: threshold_for(&key.1),
                    enforcement: format!("{:?}", enforcement_for(&key.1)),
                    open_for_secs,
                }
            })
            .collect()
    }

    pub async fn reset_provider(&self, provider: &str) -> bool {
        let mut states = self.states.write().await;
        let mut counts = self.failure_counts.write().await;
        let state_keys: Vec<CircuitKey> = states
            .keys()
            .filter(|(p, _)| p == provider)
            .cloned()
            .collect();
        let count_keys: Vec<CircuitKey> = counts
            .keys()
            .filter(|(p, _)| p == provider)
            .cloned()
            .collect();
        let had_state = !state_keys.is_empty();
        let had_count = !count_keys.is_empty();
        for k in state_keys {
            states.remove(&k);
        }
        for k in count_keys {
            counts.remove(&k);
        }
        had_state || had_count
    }

    pub async fn reset_all(&self) {
        self.states.write().await.clear();
        self.failure_counts.write().await.clear();
    }

    /// Check whether any circuit for the given provider is open.
    async fn any_circuit_open(
        &self,
        provider: &str,
    ) -> Option<(ProviderErrorKind, EnforcementLevel)> {
        let states = self.states.read().await;
        for ((p, kind), state) in states.iter() {
            if p != provider {
                continue;
            }
            if let CircuitState::Open { opened_at } = state
                && opened_at.elapsed() < self.reset_timeout
            {
                return Some((*kind, enforcement_for(kind)));
            }
        }
        None
    }

    /// Transition timed-out open circuits to half-open for a provider.
    async fn transition_to_half_open(&self, provider: &str) {
        let mut states = self.states.write().await;
        let keys: Vec<CircuitKey> = states
            .keys()
            .filter(|(p, _)| p == provider)
            .cloned()
            .collect();
        for key in keys {
            if let Some(CircuitState::Open { opened_at }) = states.get(&key)
                && opened_at.elapsed() >= self.reset_timeout
            {
                info!(
                    provider = %key.0,
                    error_class = ?key.1,
                    "circuit-breaker: timeout elapsed, entering half-open"
                );
                #[cfg(feature = "metrics")]
                counter!(
                    "moltis_circuit_breaker_state_transitions_total",
                    labels::PROVIDER => key.0.clone(),
                    labels::ERROR_TYPE => format!("{:?}", key.1),
                    "to_state" => "half_open"
                )
                .increment(1);
                states.insert(key, CircuitState::HalfOpen);
            }
        }
    }
}

impl fmt::Debug for CircuitBreakerHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreakerHook")
            .field("reset_timeout", &self.reset_timeout)
            .finish()
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
                // First, transition any timed-out circuits to half-open.
                self.transition_to_half_open(provider).await;

                // Check if any circuit for this provider is open.
                if let Some((kind, enforcement)) = self.any_circuit_open(provider).await {
                    match enforcement {
                        EnforcementLevel::Disabled => {},
                        EnforcementLevel::Observe => {
                            info!(
                                provider = %provider,
                                error_class = ?kind,
                                "circuit-breaker: would block (observe mode)"
                            );
                            #[cfg(feature = "metrics")]
                            counter!(
                                "moltis_circuit_breaker_observed_blocks_total",
                                labels::PROVIDER => provider.clone(),
                                labels::ERROR_TYPE => format!("{kind:?}")
                            )
                            .increment(1);
                        },
                        EnforcementLevel::Warn => {
                            tracing::warn!(
                                provider = %provider,
                                error_class = ?kind,
                                "circuit-breaker: circuit open (warn mode, allowing through)"
                            );
                            #[cfg(feature = "metrics")]
                            counter!(
                                "moltis_circuit_breaker_observed_blocks_total",
                                labels::PROVIDER => provider.clone(),
                                labels::ERROR_TYPE => format!("{kind:?}")
                            )
                            .increment(1);
                        },
                        EnforcementLevel::Enforce => {
                            #[cfg(feature = "metrics")]
                            counter!(
                                "moltis_circuit_breaker_enforced_blocks_total",
                                labels::PROVIDER => provider.clone(),
                                labels::ERROR_TYPE => format!("{kind:?}")
                            )
                            .increment(1);
                            return Ok(HookAction::Block(format!(
                                "Circuit open for provider {provider} (error class: {kind:?})"
                            )));
                        },
                    }
                }
                Ok(HookAction::Continue)
            },
            HookPayload::AfterLLMCall {
                provider,
                text,
                tool_calls,
                output_tokens,
                error_message,
                ..
            } => {
                let failed = *output_tokens == 0 && text.is_none() && tool_calls.is_empty();

                if failed {
                    // Classify the error. Use error_message if available, fall back
                    // to Unknown if no message was provided.
                    let error_kind = error_message
                        .as_deref()
                        .map(|msg| classify_error(None, msg))
                        .unwrap_or(ProviderErrorKind::Unknown);

                    let key: CircuitKey = (provider.clone(), error_kind);
                    let class_threshold = threshold_for(&error_kind);

                    #[cfg(feature = "metrics")]
                    counter!(
                        "moltis_circuit_breaker_failures_total",
                        labels::PROVIDER => provider.clone(),
                        labels::ERROR_TYPE => format!("{error_kind:?}")
                    )
                    .increment(1);

                    let mut states = self.states.write().await;
                    let mut counts = self.failure_counts.write().await;
                    let state = states.get(&key).cloned().unwrap_or(CircuitState::Closed);

                    match state {
                        CircuitState::HalfOpen => {
                            counts.insert(key.clone(), class_threshold);
                            states.insert(key.clone(), CircuitState::Open {
                                opened_at: Instant::now(),
                            });
                            info!(
                                provider = %provider,
                                error_class = ?error_kind,
                                "circuit-breaker: half-open probe failed, circuit reopened"
                            );
                            #[cfg(feature = "metrics")]
                            counter!(
                                "moltis_circuit_breaker_state_transitions_total",
                                labels::PROVIDER => provider.clone(),
                                labels::ERROR_TYPE => format!("{error_kind:?}"),
                                "to_state" => "open"
                            )
                            .increment(1);
                        },
                        CircuitState::Closed | CircuitState::Open { .. } => {
                            let count = counts.entry(key.clone()).or_insert(0);
                            *count += 1;
                            if *count >= class_threshold {
                                states.insert(key.clone(), CircuitState::Open {
                                    opened_at: Instant::now(),
                                });
                                info!(
                                    provider = %provider,
                                    error_class = ?error_kind,
                                    failures = *count,
                                    threshold = class_threshold,
                                    "circuit-breaker: circuit opened for provider"
                                );
                                #[cfg(feature = "metrics")]
                                counter!(
                                    "moltis_circuit_breaker_state_transitions_total",
                                    labels::PROVIDER => provider.clone(),
                                    labels::ERROR_TYPE => format!("{error_kind:?}"),
                                    "to_state" => "open"
                                )
                                .increment(1);
                            }
                        },
                    }
                } else {
                    // Success — reset all failure counts for this provider.
                    let mut counts = self.failure_counts.write().await;
                    let mut states = self.states.write().await;
                    let keys_to_reset: Vec<CircuitKey> = counts
                        .keys()
                        .filter(|(p, _)| p == provider)
                        .cloned()
                        .collect();
                    for key in &keys_to_reset {
                        counts.insert(key.clone(), 0);
                        states.insert(key.clone(), CircuitState::Closed);
                    }
                    #[cfg(feature = "metrics")]
                    if !keys_to_reset.is_empty() {
                        counter!(
                            "moltis_circuit_breaker_resets_total",
                            labels::PROVIDER => provider.clone()
                        )
                        .increment(1);
                    }
                }
                Ok(HookAction::Continue)
            },
            _ => Ok(HookAction::Continue),
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn make_after_payload(provider: &str, error_message: Option<&str>) -> HookPayload {
        HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: provider.into(),
            model: "m".into(),
            text: None,
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            iteration: 1,
            trace_id: None,
            error_message: error_message.map(String::from),
        }
    }

    fn make_success_payload(provider: &str) -> HookPayload {
        HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: provider.into(),
            model: "m".into(),
            text: Some("ok".into()),
            tool_calls: vec![],
            input_tokens: 100,
            output_tokens: 50,
            iteration: 1,
            trace_id: None,
            error_message: None,
        }
    }

    fn make_before_payload(provider: &str) -> HookPayload {
        HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: provider.into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 1,
            trace_id: None,
        }
    }

    #[tokio::test]
    async fn billing_error_trips_after_one_failure() {
        let hook = CircuitBreakerHook::new(5, 60);

        // Single billing error should trip (threshold=1).
        let payload = make_after_payload("p", Some("insufficient_quota: billing limit reached"));
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.provider == "p" && s.error_class == "BillingExhausted")
            .unwrap();
        assert_eq!(entry.state, "open");
        assert_eq!(entry.failure_count, 1);
        assert_eq!(entry.threshold, 1);
    }

    #[tokio::test]
    async fn server_error_requires_five_failures() {
        let hook = CircuitBreakerHook::new(5, 60);

        // Four failures should not trip.
        for _ in 0..4 {
            let payload = make_after_payload("p", Some("500 internal server error"));
            hook.handle(HookEvent::AfterLLMCall, &payload)
                .await
                .unwrap();
        }

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.provider == "p" && s.error_class == "ServerError")
            .unwrap();
        assert_eq!(entry.state, "closed");
        assert_eq!(entry.failure_count, 4);

        // Fifth failure trips.
        let payload = make_after_payload("p", Some("500 internal server error"));
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.provider == "p" && s.error_class == "ServerError")
            .unwrap();
        assert_eq!(entry.state, "open");
    }

    #[tokio::test]
    async fn rate_limit_requires_three_failures() {
        let hook = CircuitBreakerHook::new(5, 60);

        for _ in 0..2 {
            let payload = make_after_payload("p", Some("429 too many requests"));
            hook.handle(HookEvent::AfterLLMCall, &payload)
                .await
                .unwrap();
        }

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.provider == "p" && s.error_class == "RateLimit")
            .unwrap();
        assert_eq!(entry.state, "closed");

        // Third failure trips.
        let payload = make_after_payload("p", Some("429 too many requests"));
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.provider == "p" && s.error_class == "RateLimit")
            .unwrap();
        assert_eq!(entry.state, "open");
    }

    #[tokio::test]
    async fn different_error_classes_have_independent_circuits() {
        let hook = CircuitBreakerHook::new(5, 60);

        // Trip billing circuit.
        let billing = make_after_payload("p", Some("insufficient_quota"));
        hook.handle(HookEvent::AfterLLMCall, &billing)
            .await
            .unwrap();

        // Record server error (not enough to trip).
        let server = make_after_payload("p", Some("500 internal server error"));
        hook.handle(HookEvent::AfterLLMCall, &server).await.unwrap();

        let snapshot = hook.snapshot().await;
        let billing_entry = snapshot
            .iter()
            .find(|s| s.error_class == "BillingExhausted")
            .unwrap();
        let server_entry = snapshot
            .iter()
            .find(|s| s.error_class == "ServerError")
            .unwrap();

        assert_eq!(billing_entry.state, "open");
        assert_eq!(server_entry.state, "closed");
    }

    #[tokio::test]
    async fn observe_mode_does_not_block() {
        let hook = CircuitBreakerHook::new(5, 60);

        // Trip the circuit.
        let payload = make_after_payload("p", Some("insufficient_quota"));
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();

        // In Observe mode (default), BeforeLLMCall should Continue.
        let before = make_before_payload("p");
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &before)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[tokio::test]
    async fn half_open_failure_reopens() {
        let hook = CircuitBreakerHook::new(5, 60);
        let kind = ProviderErrorKind::ServerError;
        let key: CircuitKey = ("p".into(), kind);

        // Force half-open state.
        {
            let mut states = hook.states.write().await;
            states.insert(key.clone(), CircuitState::HalfOpen);
        }

        // Half-open probe fails — reopens.
        let fail = make_after_payload("p", Some("500 internal server error"));
        hook.handle(HookEvent::AfterLLMCall, &fail).await.unwrap();

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.provider == "p" && s.error_class == "ServerError")
            .unwrap();
        assert_eq!(entry.state, "open");
        assert_eq!(entry.failure_count, threshold_for(&kind));
    }

    #[tokio::test]
    async fn success_resets_all_failure_counts() {
        let hook = CircuitBreakerHook::new(5, 60);

        // Record some failures (but not enough to trip).
        for _ in 0..2 {
            let payload = make_after_payload("p", Some("429 too many requests"));
            hook.handle(HookEvent::AfterLLMCall, &payload)
                .await
                .unwrap();
        }
        let payload = make_after_payload("p", Some("500 internal server error"));
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();

        // Success resets everything.
        let success = make_success_payload("p");
        hook.handle(HookEvent::AfterLLMCall, &success)
            .await
            .unwrap();

        let snapshot = hook.snapshot().await;
        for entry in &snapshot {
            if entry.provider == "p" {
                assert_eq!(entry.state, "closed");
                assert_eq!(entry.failure_count, 0);
            }
        }
    }

    #[tokio::test]
    async fn reset_provider_clears_all_classes() {
        let hook = CircuitBreakerHook::new(5, 60);

        let billing = make_after_payload("p", Some("insufficient_quota"));
        hook.handle(HookEvent::AfterLLMCall, &billing)
            .await
            .unwrap();

        let server = make_after_payload("p", Some("500 internal server error"));
        hook.handle(HookEvent::AfterLLMCall, &server).await.unwrap();

        assert!(hook.reset_provider("p").await);
        let snapshot = hook.snapshot().await;
        assert!(
            snapshot.iter().all(|s| s.provider != "p"),
            "all entries for provider 'p' should be cleared"
        );
    }

    #[tokio::test]
    async fn reset_all_clears_everything() {
        let hook = CircuitBreakerHook::new(5, 60);

        let a = make_after_payload("a", Some("insufficient_quota"));
        hook.handle(HookEvent::AfterLLMCall, &a).await.unwrap();
        let b = make_after_payload("b", Some("429 rate limit"));
        hook.handle(HookEvent::AfterLLMCall, &b).await.unwrap();

        hook.reset_all().await;
        assert!(hook.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn unknown_error_when_no_error_message() {
        let hook = CircuitBreakerHook::new(5, 60);

        // No error_message => classified as Unknown (threshold=5).
        for _ in 0..5 {
            let payload = make_after_payload("p", None);
            hook.handle(HookEvent::AfterLLMCall, &payload)
                .await
                .unwrap();
        }

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.provider == "p" && s.error_class == "Unknown")
            .unwrap();
        assert_eq!(entry.state, "open");
    }

    #[tokio::test]
    async fn does_not_treat_text_response_as_failure() {
        let hook = CircuitBreakerHook::new(5, 60);
        let payload = make_success_payload("p");
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();

        let snapshot = hook.snapshot().await;
        assert!(
            snapshot.is_empty() || snapshot.iter().all(|s| s.state == "closed"),
            "success should not create open circuits"
        );
    }

    #[tokio::test]
    async fn enforcement_level_defaults_to_observe() {
        let kinds = [
            ProviderErrorKind::RateLimit,
            ProviderErrorKind::AuthError,
            ProviderErrorKind::ServerError,
            ProviderErrorKind::Timeout,
            ProviderErrorKind::BillingExhausted,
            ProviderErrorKind::NonRetryableRateLimit,
            ProviderErrorKind::ContextWindow,
            ProviderErrorKind::InvalidRequest,
            ProviderErrorKind::Unknown,
        ];
        for kind in &kinds {
            assert_eq!(
                enforcement_for(kind),
                EnforcementLevel::Observe,
                "enforcement_for({kind:?}) should be Observe"
            );
        }
    }

    #[tokio::test]
    async fn snapshot_includes_threshold_and_enforcement() {
        let hook = CircuitBreakerHook::new(5, 60);

        let payload = make_after_payload("p", Some("insufficient_quota"));
        hook.handle(HookEvent::AfterLLMCall, &payload)
            .await
            .unwrap();

        let snapshot = hook.snapshot().await;
        let entry = snapshot
            .iter()
            .find(|s| s.error_class == "BillingExhausted")
            .unwrap();
        assert_eq!(entry.threshold, 1);
        assert_eq!(entry.enforcement, "Observe");
    }
}
