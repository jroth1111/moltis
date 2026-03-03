//! Sliding-window rate limiter for outbound provider API calls.
//!
//! Keyed by `(provider_name, model_id)` to prevent flooding any single
//! provider/model endpoint. The limiter uses a per-key sliding window
//! backed by a [`VecDeque`] of timestamps.

use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;

#[cfg(feature = "metrics")]
use moltis_metrics::counter;

/// Maximum number of distinct (provider, model) keys before LRU eviction kicks in.
const MAX_KEYS: usize = 1024;

/// Sliding-window rate limiter for outbound provider calls.
///
/// Each `(provider, model)` pair gets its own window of recent call timestamps.
/// When the window is full, callers receive a `Duration` to wait before retrying.
pub struct ProviderRateLimiter {
    /// (provider, model) -> timestamps of recent calls in the window.
    windows: Arc<DashMap<(String, String), VecDeque<Instant>>>,
    /// Per-provider limits (keyed by provider name).
    limits: Arc<DashMap<String, ProviderLimit>>,
    /// Duration of the sliding window.
    window_duration: Duration,
}

/// Rate limit configuration for a single provider.
#[derive(Clone, Debug)]
pub struct ProviderLimit {
    /// Maximum requests allowed per window.
    pub requests_per_window: u32,
}

/// Result of a rate limit check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitDecision {
    /// The call is within limits; proceed immediately.
    Allowed,
    /// The call exceeds the limit; wait this long before retrying.
    Wait(Duration),
}

impl ProviderRateLimiter {
    /// Create a new rate limiter with the given sliding window duration.
    #[must_use]
    pub fn new(window_duration: Duration) -> Self {
        Self {
            windows: Arc::new(DashMap::new()),
            limits: Arc::new(DashMap::new()),
            window_duration,
        }
    }

    /// Check whether a call to `(provider, model)` is within rate limits.
    ///
    /// Returns [`RateLimitDecision::Allowed`] if the call can proceed, or
    /// [`RateLimitDecision::Wait`] with the duration to wait until a slot
    /// opens in the window.
    pub fn check(&self, provider: &str, model: &str) -> RateLimitDecision {
        self.check_at(provider, model, Instant::now())
    }

    fn check_at(&self, provider: &str, model: &str, now: Instant) -> RateLimitDecision {
        let limit = match self.limits.get(provider) {
            Some(lim) => lim.requests_per_window,
            None => return RateLimitDecision::Allowed, // No limit configured.
        };
        if limit == 0 {
            return RateLimitDecision::Wait(self.window_duration);
        }

        let key = (provider.to_string(), model.to_string());
        let mut entry = self.windows.entry(key).or_default();
        let window = entry.value_mut();

        // Evict timestamps outside the window.
        while let Some(&front) = window.front() {
            if now.duration_since(front) >= self.window_duration {
                window.pop_front();
            } else {
                break;
            }
        }

        if window.len() < limit as usize {
            RateLimitDecision::Allowed
        } else {
            // The oldest entry in the window is the one that will expire first.
            let oldest = window.front().copied().unwrap_or(now);
            let wait = self
                .window_duration
                .saturating_sub(now.duration_since(oldest));
            RateLimitDecision::Wait(wait.max(Duration::from_millis(1)))
        }
    }

    /// Record a completed call for `(provider, model)`.
    pub fn record(&self, provider: &str, model: &str) {
        self.record_at(provider, model, Instant::now());
    }

    fn record_at(&self, provider: &str, model: &str, now: Instant) {
        let key = (provider.to_string(), model.to_string());
        let mut entry = self.windows.entry(key).or_default();
        let window = entry.value_mut();
        window.push_back(now);

        // Periodic eviction of stale entries.
        if window.len() > 2048 {
            while let Some(&front) = window.front() {
                if now.duration_since(front) >= self.window_duration {
                    window.pop_front();
                } else {
                    break;
                }
            }
        }

        // LRU-style cap on the total number of tracked keys.
        drop(entry);
        self.evict_stale(now);
    }

    /// Apply a `Retry-After` hint from the provider by temporarily reducing
    /// the effective limit. This is done by filling the window with synthetic
    /// timestamps that expire after `wait` duration.
    pub fn apply_retry_after(&self, provider: &str, model: &str, wait: Duration) {
        let now = Instant::now();
        let limit = match self.limits.get(provider) {
            Some(lim) => lim.requests_per_window,
            None => return,
        };

        let key = (provider.to_string(), model.to_string());
        let mut entry = self.windows.entry(key).or_default();
        let window = entry.value_mut();

        // Clear and fill with synthetic timestamps that won't expire until
        // `now + wait`, effectively blocking new requests for that duration.
        window.clear();
        // Place `limit` synthetic entries at a time offset so they expire
        // at `now + wait`.
        let synthetic_time = now + wait.min(self.window_duration);
        let offset = self
            .window_duration
            .saturating_sub(Duration::from_millis(1));
        let stamp = synthetic_time.checked_sub(offset).unwrap_or(now);
        for _ in 0..limit {
            window.push_back(stamp);
        }

        #[cfg(feature = "metrics")]
        counter!("moltis_provider_rate_limit_retry_after").increment(1);
    }

    /// Set the rate limit for a provider.
    pub fn set_limit(&self, provider: &str, limit: ProviderLimit) {
        self.limits.insert(provider.to_string(), limit);
    }

    /// Evict keys whose windows are completely stale.
    fn evict_stale(&self, now: Instant) {
        if self.windows.len() <= MAX_KEYS {
            return;
        }
        self.windows.retain(|_, window| {
            window
                .back()
                .is_some_and(|&t| now.duration_since(t) < self.window_duration)
        });
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limit_always_allows() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(60));
        // No limit set for "openai" — everything should be allowed.
        assert_eq!(rl.check("openai", "gpt-4"), RateLimitDecision::Allowed);
        rl.record("openai", "gpt-4");
        assert_eq!(rl.check("openai", "gpt-4"), RateLimitDecision::Allowed);
    }

    #[test]
    fn respects_limit() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(60));
        rl.set_limit(
            "openai",
            ProviderLimit {
                requests_per_window: 2,
            },
        );

        let now = Instant::now();
        assert_eq!(
            rl.check_at("openai", "gpt-4", now),
            RateLimitDecision::Allowed
        );
        rl.record_at("openai", "gpt-4", now);

        assert_eq!(
            rl.check_at("openai", "gpt-4", now),
            RateLimitDecision::Allowed
        );
        rl.record_at("openai", "gpt-4", now);

        // Third request should be rate limited.
        match rl.check_at("openai", "gpt-4", now) {
            RateLimitDecision::Wait(d) => {
                assert!(d > Duration::ZERO);
                assert!(d <= Duration::from_secs(60));
            },
            RateLimitDecision::Allowed => panic!("expected rate limit"),
        }
    }

    #[test]
    fn window_slides() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(10));
        rl.set_limit(
            "anthropic",
            ProviderLimit {
                requests_per_window: 1,
            },
        );

        let t0 = Instant::now();
        rl.record_at("anthropic", "claude", t0);

        // Immediately after: blocked.
        assert!(matches!(
            rl.check_at("anthropic", "claude", t0 + Duration::from_secs(1)),
            RateLimitDecision::Wait(_)
        ));

        // After window expires: allowed.
        assert_eq!(
            rl.check_at("anthropic", "claude", t0 + Duration::from_secs(11)),
            RateLimitDecision::Allowed
        );
    }

    #[test]
    fn different_models_independent() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(60));
        rl.set_limit(
            "openai",
            ProviderLimit {
                requests_per_window: 1,
            },
        );

        let now = Instant::now();
        rl.record_at("openai", "gpt-4", now);

        // gpt-4 is now blocked.
        assert!(matches!(
            rl.check_at("openai", "gpt-4", now),
            RateLimitDecision::Wait(_)
        ));

        // gpt-3.5 is still allowed.
        assert_eq!(
            rl.check_at("openai", "gpt-3.5", now),
            RateLimitDecision::Allowed
        );
    }

    #[test]
    fn different_providers_independent() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(60));
        rl.set_limit(
            "openai",
            ProviderLimit {
                requests_per_window: 1,
            },
        );

        let now = Instant::now();
        rl.record_at("openai", "gpt-4", now);

        // openai is blocked; anthropic has no limit so allowed.
        assert!(matches!(
            rl.check_at("openai", "gpt-4", now),
            RateLimitDecision::Wait(_)
        ));
        assert_eq!(
            rl.check_at("anthropic", "claude", now),
            RateLimitDecision::Allowed
        );
    }

    #[test]
    fn apply_retry_after_blocks_requests() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(60));
        rl.set_limit(
            "openai",
            ProviderLimit {
                requests_per_window: 10,
            },
        );

        let now = Instant::now();
        // Should be allowed before retry-after.
        assert_eq!(
            rl.check_at("openai", "gpt-4", now),
            RateLimitDecision::Allowed
        );

        rl.apply_retry_after("openai", "gpt-4", Duration::from_secs(30));

        // Should be blocked after applying retry-after.
        match rl.check("openai", "gpt-4") {
            RateLimitDecision::Wait(d) => {
                assert!(d > Duration::ZERO);
            },
            RateLimitDecision::Allowed => panic!("expected rate limit after retry-after"),
        }
    }

    #[test]
    fn zero_limit_blocks_everything() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(60));
        rl.set_limit(
            "openai",
            ProviderLimit {
                requests_per_window: 0,
            },
        );

        match rl.check("openai", "gpt-4") {
            RateLimitDecision::Wait(d) => {
                assert_eq!(d, Duration::from_secs(60));
            },
            RateLimitDecision::Allowed => panic!("expected blocked with zero limit"),
        }
    }

    #[test]
    fn evict_stale_removes_old_keys() {
        let rl = ProviderRateLimiter::new(Duration::from_millis(10));
        rl.set_limit(
            "test",
            ProviderLimit {
                requests_per_window: 100,
            },
        );

        let t0 = Instant::now();
        for i in 0..10 {
            rl.record_at("test", &format!("model-{i}"), t0);
        }

        // After the window expires, eviction should remove all.
        let t1 = t0 + Duration::from_millis(20);
        rl.record_at("test", "fresh", t1);
        rl.evict_stale(t1);

        // The fresh key should remain.
        assert!(
            rl.windows
                .contains_key(&("test".to_string(), "fresh".to_string()))
        );
    }

    #[test]
    fn wait_duration_accuracy() {
        let rl = ProviderRateLimiter::new(Duration::from_secs(10));
        rl.set_limit(
            "p",
            ProviderLimit {
                requests_per_window: 2,
            },
        );

        let t0 = Instant::now();
        rl.record_at("p", "m", t0);
        rl.record_at("p", "m", t0 + Duration::from_secs(3));

        // At t0+5, we have 2 entries in window. The oldest expires at t0+10.
        match rl.check_at("p", "m", t0 + Duration::from_secs(5)) {
            RateLimitDecision::Wait(d) => {
                // Should wait until the oldest (t0) expires at t0+10.
                // At t0+5, that's 5 seconds away.
                assert!(d >= Duration::from_secs(4));
                assert!(d <= Duration::from_secs(6));
            },
            RateLimitDecision::Allowed => panic!("expected wait"),
        }

        // At t0+11, the first entry has expired. Only 1 in window.
        assert_eq!(
            rl.check_at("p", "m", t0 + Duration::from_secs(11)),
            RateLimitDecision::Allowed
        );
    }
}
