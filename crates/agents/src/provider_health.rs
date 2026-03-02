//! Provider health tracker with sliding-window statistics.
//!
//! Records per-call outcomes (success/error with class) and computes rolling
//! metrics: success rate, error rate by class, p50/p95/p99 latency.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// A single recorded call outcome.
#[derive(Debug, Clone)]
struct CallRecord {
    recorded_at: Instant,
    provider: String,
    model: String,
    duration_ms: u64,
    /// `None` for success, `Some(class)` for errors.
    error_class: Option<String>,
}

/// Rolling statistics for a provider+model pair.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderStats {
    pub provider: String,
    pub model: String,
    pub total_calls: usize,
    pub successes: usize,
    pub errors: usize,
    pub success_rate: f64,
    /// Error counts broken down by error class.
    pub errors_by_class: HashMap<String, usize>,
    /// Latency percentiles in milliseconds.
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
}

/// Thread-safe sliding-window health tracker for LLM providers.
///
/// Records call outcomes and computes rolling statistics over a configurable
/// window (default 5 minutes). Old entries are evicted lazily on access.
pub struct ProviderHealthTracker {
    inner: Mutex<TrackerInner>,
}

struct TrackerInner {
    records: Vec<CallRecord>,
    window: Duration,
}

impl ProviderHealthTracker {
    /// Create a new tracker with the given sliding window duration.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            inner: Mutex::new(TrackerInner {
                records: Vec::new(),
                window,
            }),
        }
    }

    /// Create a tracker with the default 5-minute window.
    #[must_use]
    pub fn default_window() -> Self {
        Self::new(Duration::from_secs(300))
    }

    /// Record a successful provider call.
    pub fn record_success(&self, provider: &str, model: &str, duration_ms: u64) {
        let record = CallRecord {
            recorded_at: Instant::now(),
            provider: provider.to_string(),
            model: model.to_string(),
            duration_ms,
            error_class: None,
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.records.push(record);
    }

    /// Record a failed provider call.
    pub fn record_error(&self, provider: &str, model: &str, duration_ms: u64, error_class: &str) {
        let record = CallRecord {
            recorded_at: Instant::now(),
            provider: provider.to_string(),
            model: model.to_string(),
            duration_ms,
            error_class: Some(error_class.to_string()),
        };
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.records.push(record);
    }

    /// Evict expired entries and return rolling stats per provider+model.
    #[must_use]
    pub fn snapshot(&self) -> Vec<ProviderStats> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let cutoff = Instant::now() - inner.window;
        inner.records.retain(|r| r.recorded_at >= cutoff);

        // Group by (provider, model).
        let mut groups: HashMap<(String, String), Vec<&CallRecord>> = HashMap::new();
        for rec in &inner.records {
            groups
                .entry((rec.provider.clone(), rec.model.clone()))
                .or_default()
                .push(rec);
        }

        groups
            .into_iter()
            .map(|((provider, model), records)| {
                let total_calls = records.len();
                let mut successes = 0usize;
                let mut errors = 0usize;
                let mut errors_by_class: HashMap<String, usize> = HashMap::new();
                let mut latencies: Vec<u64> = Vec::with_capacity(total_calls);

                for rec in &records {
                    latencies.push(rec.duration_ms);
                    if let Some(ref class) = rec.error_class {
                        errors += 1;
                        *errors_by_class.entry(class.clone()).or_default() += 1;
                    } else {
                        successes += 1;
                    }
                }

                latencies.sort_unstable();
                let p50_ms = percentile(&latencies, 50);
                let p95_ms = percentile(&latencies, 95);
                let p99_ms = percentile(&latencies, 99);

                let success_rate = if total_calls > 0 {
                    successes as f64 / total_calls as f64
                } else {
                    0.0
                };

                ProviderStats {
                    provider,
                    model,
                    total_calls,
                    successes,
                    errors,
                    success_rate,
                    errors_by_class,
                    p50_ms,
                    p95_ms,
                    p99_ms,
                }
            })
            .collect()
    }

    /// Number of records currently in the buffer (for testing).
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .records
            .len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Compute the p-th percentile from a sorted slice (nearest-rank method).
fn percentile(sorted: &[u64], p: u8) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((p as usize) * sorted.len() / 100).min(sorted.len() - 1);
    sorted[idx]
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tracker_returns_no_stats() {
        let tracker = ProviderHealthTracker::default_window();
        assert!(tracker.snapshot().is_empty());
        assert!(tracker.is_empty());
    }

    #[test]
    fn records_success_and_computes_stats() {
        let tracker = ProviderHealthTracker::default_window();
        tracker.record_success("openai", "gpt-4o", 100);
        tracker.record_success("openai", "gpt-4o", 200);
        tracker.record_success("openai", "gpt-4o", 300);

        let stats = tracker.snapshot();
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.provider, "openai");
        assert_eq!(s.model, "gpt-4o");
        assert_eq!(s.total_calls, 3);
        assert_eq!(s.successes, 3);
        assert_eq!(s.errors, 0);
        assert!((s.success_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(s.p50_ms, 200);
        assert_eq!(s.p99_ms, 300);
    }

    #[test]
    fn records_errors_with_class() {
        let tracker = ProviderHealthTracker::default_window();
        tracker.record_success("anthropic", "claude", 150);
        tracker.record_error("anthropic", "claude", 50, "RateLimit");
        tracker.record_error("anthropic", "claude", 30, "RateLimit");
        tracker.record_error("anthropic", "claude", 80, "ServerError");

        let stats = tracker.snapshot();
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.total_calls, 4);
        assert_eq!(s.successes, 1);
        assert_eq!(s.errors, 3);
        assert!((s.success_rate - 0.25).abs() < f64::EPSILON);
        assert_eq!(s.errors_by_class.get("RateLimit"), Some(&2));
        assert_eq!(s.errors_by_class.get("ServerError"), Some(&1));
    }

    #[test]
    fn multiple_providers_tracked_separately() {
        let tracker = ProviderHealthTracker::default_window();
        tracker.record_success("openai", "gpt-4o", 100);
        tracker.record_success("anthropic", "claude", 200);

        let stats = tracker.snapshot();
        assert_eq!(stats.len(), 2);
    }

    #[test]
    fn old_entries_evicted() {
        let tracker = ProviderHealthTracker::new(Duration::from_millis(50));
        tracker.record_success("p", "m", 10);

        // Wait for the window to expire.
        std::thread::sleep(Duration::from_millis(100));

        tracker.record_success("p", "m", 20);

        let stats = tracker.snapshot();
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        // Only the second call should remain.
        assert_eq!(s.total_calls, 1);
        assert_eq!(s.p50_ms, 20);
    }

    #[test]
    fn percentile_edge_cases() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 99), 42);

        let sorted = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&sorted, 50), 60);
        assert_eq!(percentile(&sorted, 95), 100);
        assert_eq!(percentile(&sorted, 99), 100);
    }

    #[test]
    fn p50_p95_p99_latency_correct() {
        let tracker = ProviderHealthTracker::default_window();
        // Insert 100 calls with latencies 1..=100
        for i in 1..=100 {
            tracker.record_success("p", "m", i);
        }

        let stats = tracker.snapshot();
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.total_calls, 100);
        assert_eq!(s.p50_ms, 51); // index 50 (0-based) in sorted 1..=100
        assert_eq!(s.p95_ms, 96); // index 95
        assert_eq!(s.p99_ms, 100); // index 99
    }
}
