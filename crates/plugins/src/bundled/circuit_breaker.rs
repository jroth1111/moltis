//! Circuit breaker hook for LLM provider failover.
//!
//! Tracks consecutive failures per provider and opens a circuit to block calls
//! when a provider is unreliable, allowing it to recover.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use {async_trait::async_trait, serde::Serialize, tokio::sync::RwLock, tracing::info};

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

#[derive(Debug, Clone, Serialize)]
pub struct CircuitSnapshot {
    pub provider: String,
    pub state: String,
    pub failure_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_for_secs: Option<u64>,
}

pub struct CircuitBreakerHook {
    states: Arc<RwLock<HashMap<String, CircuitState>>>,
    failure_counts: Arc<RwLock<HashMap<String, u32>>>,
    failure_threshold: u32,
    reset_timeout: Duration,
}

impl CircuitBreakerHook {
    pub fn new(failure_threshold: u32, reset_timeout_secs: u64) -> Self {
        Self {
            states: Arc::new(RwLock::new(HashMap::new())),
            failure_counts: Arc::new(RwLock::new(HashMap::new())),
            failure_threshold,
            reset_timeout: Duration::from_secs(reset_timeout_secs),
        }
    }

    pub async fn snapshot(&self) -> Vec<CircuitSnapshot> {
        let states = self.states.read().await;
        let counts = self.failure_counts.read().await;

        let mut providers: Vec<String> = states
            .keys()
            .chain(counts.keys())
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        providers.sort();

        providers
            .into_iter()
            .map(|provider| {
                let state = states
                    .get(&provider)
                    .cloned()
                    .unwrap_or(CircuitState::Closed);
                let failure_count = *counts.get(&provider).unwrap_or(&0);
                let (state_name, open_for_secs) = match state {
                    CircuitState::Closed => ("closed".to_string(), None),
                    CircuitState::HalfOpen => ("half_open".to_string(), None),
                    CircuitState::Open { opened_at } => {
                        ("open".to_string(), Some(opened_at.elapsed().as_secs()))
                    },
                };
                CircuitSnapshot {
                    provider,
                    state: state_name,
                    failure_count,
                    open_for_secs,
                }
            })
            .collect()
    }

    pub async fn reset_provider(&self, provider: &str) -> bool {
        let mut states = self.states.write().await;
        let mut counts = self.failure_counts.write().await;
        let had_state = states.remove(provider).is_some();
        let had_count = counts.remove(provider).is_some();
        had_state || had_count
    }

    pub async fn reset_all(&self) {
        self.states.write().await.clear();
        self.failure_counts.write().await.clear();
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
                if let Some(state) = states.get(provider).cloned() {
                    match state {
                        CircuitState::Open { opened_at } => {
                            if opened_at.elapsed() < self.reset_timeout {
                                return Ok(HookAction::Block(format!(
                                    "Circuit open for provider {provider}"
                                )));
                            }
                            states.insert(provider.clone(), CircuitState::HalfOpen);
                            info!(
                                provider = %provider,
                                "circuit-breaker: timeout elapsed, entering half-open"
                            );
                        },
                        CircuitState::HalfOpen | CircuitState::Closed => {},
                    }
                }
                Ok(HookAction::Continue)
            },
            HookPayload::AfterLLMCall {
                provider,
                text,
                tool_calls,
                output_tokens,
                ..
            } => {
                let failed = *output_tokens == 0 && text.is_none() && tool_calls.is_empty();
                if failed {
                    let mut states = self.states.write().await;
                    let mut counts = self.failure_counts.write().await;
                    let state = states
                        .get(provider)
                        .cloned()
                        .unwrap_or(CircuitState::Closed);

                    match state {
                        // Probe failed — reopen immediately.
                        CircuitState::HalfOpen => {
                            counts.insert(provider.clone(), self.failure_threshold);
                            states.insert(provider.clone(), CircuitState::Open {
                                opened_at: Instant::now(),
                            });
                            info!(
                                provider = %provider,
                                "circuit-breaker: half-open probe failed, circuit reopened"
                            );
                        },
                        CircuitState::Closed | CircuitState::Open { .. } => {
                            let count = counts.entry(provider.clone()).or_insert(0);
                            *count += 1;
                            if *count >= self.failure_threshold {
                                states.insert(provider.clone(), CircuitState::Open {
                                    opened_at: Instant::now(),
                                });
                                info!(
                                    provider = %provider,
                                    failures = *count,
                                    "circuit-breaker: circuit opened for provider"
                                );
                            }
                        },
                    }
                } else {
                    // Success — reset failure count and close circuit.
                    let mut counts = self.failure_counts.write().await;
                    counts.insert(provider.clone(), 0);
                    let mut states = self.states.write().await;
                    states.insert(provider.clone(), CircuitState::Closed);
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

    #[tokio::test]
    async fn opens_circuit_after_threshold() {
        let hook = CircuitBreakerHook::new(2, 60);

        // Two failures.
        for _ in 0..2 {
            let payload = HookPayload::AfterLLMCall {
                session_key: "s1".into(),
                provider: "bad-provider".into(),
                model: "m".into(),
                text: None,
                tool_calls: vec![],
                input_tokens: 100,
                output_tokens: 0,
                iteration: 1,
            };
            hook.handle(HookEvent::AfterLLMCall, &payload)
                .await
                .unwrap();
        }

        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "bad-provider".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }

    #[tokio::test]
    async fn half_open_failure_reopens_immediately() {
        let hook = CircuitBreakerHook::new(2, 60);

        // Force a half-open probe state.
        {
            let mut states = hook.states.write().await;
            states.insert("p".into(), CircuitState::HalfOpen);
        }

        let before = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 2,
        };
        let allow = hook
            .handle(HookEvent::BeforeLLMCall, &before)
            .await
            .unwrap();
        assert!(matches!(allow, HookAction::Continue));

        // Probe fails => reopen immediately and block new attempts until timeout.
        let fail = HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            text: None,
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 0,
            iteration: 2,
        };
        hook.handle(HookEvent::AfterLLMCall, &fail).await.unwrap();

        let snapshot = hook.snapshot().await;
        let provider = snapshot.iter().find(|s| s.provider == "p").unwrap();
        assert_eq!(provider.state, "open");
        assert_eq!(provider.failure_count, 2);

        let blocked = hook
            .handle(HookEvent::BeforeLLMCall, &before)
            .await
            .unwrap();
        assert!(matches!(blocked, HookAction::Block(_)));
    }

    #[tokio::test]
    async fn reset_provider_clears_state() {
        let hook = CircuitBreakerHook::new(1, 60);
        let fail = HookPayload::AfterLLMCall {
            session_key: "s1".into(),
            provider: "p".into(),
            model: "m".into(),
            text: None,
            tool_calls: vec![],
            input_tokens: 1,
            output_tokens: 0,
            iteration: 1,
        };
        hook.handle(HookEvent::AfterLLMCall, &fail).await.unwrap();

        assert!(hook.reset_provider("p").await);
        let snapshot = hook.snapshot().await;
        assert!(snapshot.is_empty());
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
