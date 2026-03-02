//! Outbound provider rate limiter.
//!
//! Applies per-(provider, model) sliding-window limits before outbound LLM calls.
//! Supports provider-specific overrides, Retry-After backpressure, and bounded
//! memory via simple LRU eviction.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use dashmap::{DashMap, mapref::entry::Entry};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels, llm as llm_metrics};

/// Sliding-window limit for a provider/model.
#[derive(Debug, Clone, Copy)]
pub struct ProviderLimit {
    pub window: Duration,
    pub max_requests: u32,
}

impl ProviderLimit {
    #[must_use]
    pub fn from_config(cfg: &moltis_config::schema::ProviderRateLimitWindowConfig) -> Self {
        Self {
            window: Duration::from_secs(cfg.window_secs.max(1)),
            max_requests: cfg.max_requests_per_window,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RateKey {
    provider: String,
    model: String,
}

#[derive(Debug, Clone, Copy)]
struct WindowState {
    window_started: Instant,
    count: u32,
    blocked_until: Option<Instant>,
}

/// Rate limiter decision before issuing a provider call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitDecision {
    Allowed,
    Wait(Duration),
    Rejected(Duration),
}

/// Per-provider outbound rate limiter.
pub struct ProviderRateLimiter {
    default_limit: ProviderLimit,
    provider_limits: HashMap<String, ProviderLimit>,
    wait_on_limit: bool,
    max_tracked_keys: usize,
    windows: Arc<DashMap<RateKey, WindowState>>,
    lru: Mutex<VecDeque<RateKey>>,
}

impl ProviderRateLimiter {
    /// Build from config. Returns `None` when disabled.
    #[must_use]
    pub fn from_config(cfg: &moltis_config::schema::ProviderRateLimitConfig) -> Option<Arc<Self>> {
        if !cfg.enabled {
            return None;
        }
        let provider_limits = cfg
            .providers
            .iter()
            .map(|(k, v)| (k.clone(), ProviderLimit::from_config(v)))
            .collect();
        Some(Arc::new(Self {
            default_limit: ProviderLimit::from_config(&cfg.defaults),
            provider_limits,
            wait_on_limit: cfg.wait_on_limit,
            max_tracked_keys: cfg.max_tracked_keys.max(1),
            windows: Arc::new(DashMap::new()),
            lru: Mutex::new(VecDeque::new()),
        }))
    }

    fn key(provider: &str, model: &str) -> RateKey {
        RateKey {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    fn limit_for(&self, provider: &str) -> ProviderLimit {
        self.provider_limits
            .get(provider)
            .copied()
            .unwrap_or(self.default_limit)
    }

    fn touch_lru(&self, key: RateKey) {
        let mut lru = self.lru.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = lru.iter().position(|k| k == &key) {
            lru.remove(pos);
        }
        lru.push_back(key);

        while lru.len() > self.max_tracked_keys {
            if let Some(evicted) = lru.pop_front() {
                self.windows.remove(&evicted);
            }
        }
    }

    /// Acquire budget for the next outbound provider call.
    #[must_use]
    pub fn acquire(&self, provider: &str, model: &str) -> RateLimitDecision {
        let key = Self::key(provider, model);
        let limit = self.limit_for(provider);
        let now = Instant::now();

        if limit.max_requests == 0 {
            let decision = if self.wait_on_limit {
                RateLimitDecision::Wait(limit.window)
            } else {
                RateLimitDecision::Rejected(limit.window)
            };
            #[cfg(feature = "metrics")]
            match decision {
                RateLimitDecision::Wait(_) => {
                    counter!(
                        llm_metrics::PROVIDER_RATE_LIMIT_QUEUED,
                        labels::PROVIDER => provider.to_string(),
                        labels::MODEL => model.to_string()
                    )
                    .increment(1);
                },
                RateLimitDecision::Rejected(_) => {
                    counter!(
                        llm_metrics::PROVIDER_RATE_LIMIT_REJECTED,
                        labels::PROVIDER => provider.to_string(),
                        labels::MODEL => model.to_string()
                    )
                    .increment(1);
                },
                RateLimitDecision::Allowed => {},
            }
            return decision;
        }

        let decision = match self.windows.entry(key.clone()) {
            Entry::Occupied(mut occupied) => {
                let state = occupied.get_mut();

                if let Some(until) = state.blocked_until
                    && until > now
                {
                    let wait = until.duration_since(now);
                    if self.wait_on_limit {
                        RateLimitDecision::Wait(wait)
                    } else {
                        RateLimitDecision::Rejected(wait)
                    }
                } else {
                    let elapsed = now.duration_since(state.window_started);
                    if elapsed >= limit.window {
                        state.window_started = now;
                        state.count = 1;
                        state.blocked_until = None;
                        RateLimitDecision::Allowed
                    } else if state.count < limit.max_requests {
                        state.count += 1;
                        RateLimitDecision::Allowed
                    } else {
                        let retry_after = limit.window.saturating_sub(elapsed);
                        state.blocked_until = Some(now + retry_after);
                        if self.wait_on_limit {
                            RateLimitDecision::Wait(retry_after)
                        } else {
                            RateLimitDecision::Rejected(retry_after)
                        }
                    }
                }
            },
            Entry::Vacant(vacant) => {
                vacant.insert(WindowState {
                    window_started: now,
                    count: 1,
                    blocked_until: None,
                });
                RateLimitDecision::Allowed
            },
        };

        self.touch_lru(key);

        #[cfg(feature = "metrics")]
        match decision {
            RateLimitDecision::Wait(_) => {
                counter!(
                    llm_metrics::PROVIDER_RATE_LIMIT_QUEUED,
                    labels::PROVIDER => provider.to_string(),
                    labels::MODEL => model.to_string()
                )
                .increment(1);
            },
            RateLimitDecision::Rejected(_) => {
                counter!(
                    llm_metrics::PROVIDER_RATE_LIMIT_REJECTED,
                    labels::PROVIDER => provider.to_string(),
                    labels::MODEL => model.to_string()
                )
                .increment(1);
            },
            RateLimitDecision::Allowed => {},
        }

        decision
    }

    /// Record a Retry-After hint from a previous provider 429 response.
    pub fn note_retry_after_ms(&self, provider: &str, model: &str, retry_after_ms: u64) {
        let key = Self::key(provider, model);
        let now = Instant::now();
        let retry_after = Duration::from_millis(retry_after_ms.max(1));

        match self.windows.entry(key.clone()) {
            Entry::Occupied(mut occupied) => {
                occupied.get_mut().blocked_until = Some(now + retry_after);
            },
            Entry::Vacant(vacant) => {
                vacant.insert(WindowState {
                    window_started: now,
                    count: 0,
                    blocked_until: Some(now + retry_after),
                });
            },
        }

        self.touch_lru(key);
    }

    #[cfg(test)]
    fn tracked_len(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn test_limiter(
        max_requests: u32,
        window: Duration,
        wait_on_limit: bool,
        max_keys: usize,
    ) -> ProviderRateLimiter {
        ProviderRateLimiter {
            default_limit: ProviderLimit {
                window,
                max_requests,
            },
            provider_limits: HashMap::new(),
            wait_on_limit,
            max_tracked_keys: max_keys,
            windows: Arc::new(DashMap::new()),
            lru: Mutex::new(VecDeque::new()),
        }
    }

    #[test]
    fn allows_until_limit_then_rejects() {
        let limiter = test_limiter(2, Duration::from_secs(60), false, 128);
        assert_eq!(limiter.acquire("openai", "gpt"), RateLimitDecision::Allowed);
        assert_eq!(limiter.acquire("openai", "gpt"), RateLimitDecision::Allowed);
        assert!(matches!(
            limiter.acquire("openai", "gpt"),
            RateLimitDecision::Rejected(_)
        ));
    }

    #[test]
    fn returns_wait_when_configured_to_wait() {
        let limiter = test_limiter(1, Duration::from_secs(60), true, 128);
        assert_eq!(limiter.acquire("openai", "gpt"), RateLimitDecision::Allowed);
        assert!(matches!(
            limiter.acquire("openai", "gpt"),
            RateLimitDecision::Wait(_)
        ));
    }

    #[test]
    fn note_retry_after_blocks_subsequent_requests() {
        let limiter = test_limiter(10, Duration::from_secs(60), false, 128);
        limiter.note_retry_after_ms("openai", "gpt", 5_000);
        assert!(matches!(
            limiter.acquire("openai", "gpt"),
            RateLimitDecision::Rejected(_)
        ));
    }

    #[test]
    fn evicts_lru_entries_when_capacity_exceeded() {
        let limiter = test_limiter(10, Duration::from_secs(60), false, 2);
        assert_eq!(limiter.acquire("p1", "m1"), RateLimitDecision::Allowed);
        assert_eq!(limiter.acquire("p2", "m2"), RateLimitDecision::Allowed);
        assert_eq!(limiter.acquire("p3", "m3"), RateLimitDecision::Allowed);
        assert!(limiter.tracked_len() <= 2);
    }

    #[test]
    fn window_resets_after_expiry() {
        let limiter = test_limiter(1, Duration::from_millis(20), false, 128);
        assert_eq!(limiter.acquire("openai", "gpt"), RateLimitDecision::Allowed);
        assert!(matches!(
            limiter.acquire("openai", "gpt"),
            RateLimitDecision::Rejected(_)
        ));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(limiter.acquire("openai", "gpt"), RateLimitDecision::Allowed);
    }
}
