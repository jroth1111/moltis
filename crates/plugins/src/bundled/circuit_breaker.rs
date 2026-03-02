//! Circuit breaker hook for LLM provider failover.
//!
//! Tracks consecutive failures per provider and opens a circuit to block calls
//! when a provider is unreliable, allowing it to recover.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use {async_trait::async_trait, tokio::sync::RwLock, tracing::info};

use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

#[derive(Debug, Clone)]
enum CircuitState {
    Closed,
    Open { opened_at: Instant },
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
                let states = self.states.read().await;
                if let Some(CircuitState::Open { opened_at }) = states.get(provider) {
                    if opened_at.elapsed() < self.reset_timeout {
                        return Ok(HookAction::Block(format!(
                            "Circuit open for provider {provider}"
                        )));
                    }
                    // Timeout elapsed — allow through (half-open)
                    drop(states);
                    let mut states = self.states.write().await;
                    states.insert(provider.clone(), CircuitState::Closed);
                    info!(provider = %provider, "circuit-breaker: half-open, allowing call");
                }
                Ok(HookAction::Continue)
            },
            HookPayload::AfterLLMCall {
                provider,
                output_tokens,
                ..
            } => {
                if *output_tokens == 0 {
                    // Treat zero output tokens as a failure
                    let mut counts = self.failure_counts.write().await;
                    let count = counts.entry(provider.clone()).or_insert(0);
                    *count += 1;
                    if *count >= self.failure_threshold {
                        let mut states = self.states.write().await;
                        states.insert(
                            provider.clone(),
                            CircuitState::Open {
                                opened_at: Instant::now(),
                            },
                        );
                        info!(
                            provider = %provider,
                            failures = *count,
                            "circuit-breaker: circuit opened for provider"
                        );
                    }
                } else {
                    // Success — reset failure count and close circuit
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

        // Two failures
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
            hook.handle(HookEvent::AfterLLMCall, &payload).await.unwrap();
        }

        // Now BeforeLLMCall should be blocked
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "bad-provider".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook.handle(HookEvent::BeforeLLMCall, &payload).await.unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }

    #[tokio::test]
    async fn allows_healthy_provider() {
        let hook = CircuitBreakerHook::new(3, 60);

        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "good-provider".into(),
            model: "m".into(),
            messages: serde_json::json!([]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook.handle(HookEvent::BeforeLLMCall, &payload).await.unwrap();
        assert!(matches!(result, HookAction::Continue));
    }
}
