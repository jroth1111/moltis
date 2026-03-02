use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

#[derive(Debug, Clone)]
struct ProviderHealthSample {
    timestamp_ms: u64,
    provider: String,
    model: String,
    duration_ms: u64,
    error_class: Option<String>,
}

#[derive(Debug)]
pub struct ProviderHealthTracker {
    window: Duration,
    max_samples: usize,
    samples: Mutex<VecDeque<ProviderHealthSample>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderHealthSnapshot {
    pub generated_at_ms: u64,
    pub window_secs: u64,
    pub sample_count: usize,
    pub providers: Vec<ProviderModelHealth>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModelHealth {
    pub provider: String,
    pub model: String,
    pub total_requests: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub success_rate: f64,
    pub error_rate: f64,
    pub error_rate_by_class: HashMap<String, f64>,
    pub p50_latency_ms: Option<f64>,
    pub p95_latency_ms: Option<f64>,
    pub p99_latency_ms: Option<f64>,
}

impl ProviderHealthTracker {
    pub fn new(window: Duration, max_samples: usize) -> Self {
        Self {
            window,
            max_samples: max_samples.max(1),
            samples: Mutex::new(VecDeque::with_capacity(max_samples.max(1))),
        }
    }

    pub fn default_window() -> Duration {
        Duration::from_secs(5 * 60)
    }

    pub fn default_max_samples() -> usize {
        10_000
    }

    fn prune_locked(samples: &mut VecDeque<ProviderHealthSample>, cutoff_ms: u64) {
        while samples
            .front()
            .is_some_and(|sample| sample.timestamp_ms < cutoff_ms)
        {
            samples.pop_front();
        }
    }

    fn record(
        &self,
        provider: &str,
        model: &str,
        duration_ms: u64,
        error_class: Option<String>,
    ) {
        let now = now_ms();
        let cutoff = now.saturating_sub(self.window.as_millis() as u64);
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune_locked(&mut samples, cutoff);
        if samples.len() >= self.max_samples {
            samples.pop_front();
        }
        samples.push_back(ProviderHealthSample {
            timestamp_ms: now,
            provider: provider.to_string(),
            model: model.to_string(),
            duration_ms,
            error_class,
        });
    }

    pub fn record_success(&self, provider: &str, model: &str, duration_ms: u64) {
        self.record(provider, model, duration_ms, None);
    }

    pub fn record_failure(
        &self,
        provider: &str,
        model: &str,
        duration_ms: u64,
        error_class: impl Into<String>,
    ) {
        self.record(provider, model, duration_ms, Some(error_class.into()));
    }

    pub fn snapshot(&self) -> ProviderHealthSnapshot {
        let now = now_ms();
        let cutoff = now.saturating_sub(self.window.as_millis() as u64);
        let mut samples = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune_locked(&mut samples, cutoff);

        let mut grouped: BTreeMap<(String, String), Vec<&ProviderHealthSample>> = BTreeMap::new();
        for sample in samples.iter() {
            grouped
                .entry((sample.provider.clone(), sample.model.clone()))
                .or_default()
                .push(sample);
        }

        let providers = grouped
            .into_iter()
            .map(|((provider, model), group)| {
                let total = group.len() as u64;
                let mut success_count = 0_u64;
                let mut error_counts: HashMap<String, u64> = HashMap::new();
                let mut latencies: Vec<u64> = Vec::with_capacity(group.len());

                for sample in group {
                    latencies.push(sample.duration_ms);
                    if let Some(class) = &sample.error_class {
                        *error_counts.entry(class.clone()).or_insert(0) += 1;
                    } else {
                        success_count += 1;
                    }
                }

                latencies.sort_unstable();
                let error_count = total.saturating_sub(success_count);
                let success_rate = if total == 0 {
                    0.0
                } else {
                    success_count as f64 / total as f64
                };
                let error_rate = if total == 0 {
                    0.0
                } else {
                    error_count as f64 / total as f64
                };
                let mut error_rate_by_class = HashMap::new();
                if total > 0 {
                    for (class, count) in error_counts {
                        error_rate_by_class.insert(class, count as f64 / total as f64);
                    }
                }

                ProviderModelHealth {
                    provider,
                    model,
                    total_requests: total,
                    success_count,
                    error_count,
                    success_rate,
                    error_rate,
                    error_rate_by_class,
                    p50_latency_ms: percentile(&latencies, 0.50),
                    p95_latency_ms: percentile(&latencies, 0.95),
                    p99_latency_ms: percentile(&latencies, 0.99),
                }
            })
            .collect();

        ProviderHealthSnapshot {
            generated_at_ms: now,
            window_secs: self.window.as_secs(),
            sample_count: samples.len(),
            providers,
        }
    }
}

impl Default for ProviderHealthTracker {
    fn default() -> Self {
        Self::new(Self::default_window(), Self::default_max_samples())
    }
}

pub fn global_tracker() -> Arc<ProviderHealthTracker> {
    static TRACKER: OnceLock<Arc<ProviderHealthTracker>> = OnceLock::new();
    Arc::clone(TRACKER.get_or_init(|| Arc::new(ProviderHealthTracker::default())))
}

pub fn global_snapshot() -> ProviderHealthSnapshot {
    global_tracker().snapshot()
}

fn percentile(sorted: &[u64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let clamped = p.clamp(0.0, 1.0);
    let idx = ((sorted.len() - 1) as f64 * clamped).round() as usize;
    sorted.get(idx).map(|v| *v as f64)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn computes_success_and_error_rates() {
        let tracker = ProviderHealthTracker::new(Duration::from_secs(60), 100);
        tracker.record_success("p1", "m1", 100);
        tracker.record_success("p1", "m1", 200);
        tracker.record_failure("p1", "m1", 300, "rate_limit");

        let snapshot = tracker.snapshot();
        let stats = snapshot
            .providers
            .iter()
            .find(|item| item.provider == "p1" && item.model == "m1")
            .unwrap();
        assert_eq!(stats.total_requests, 3);
        assert_eq!(stats.success_count, 2);
        assert_eq!(stats.error_count, 1);
        assert!((stats.success_rate - (2.0 / 3.0)).abs() < 0.001);
        assert!((stats.error_rate - (1.0 / 3.0)).abs() < 0.001);
        assert!((stats.error_rate_by_class["rate_limit"] - (1.0 / 3.0)).abs() < 0.001);
    }

    #[test]
    fn computes_latency_percentiles() {
        let tracker = ProviderHealthTracker::new(Duration::from_secs(60), 100);
        for latency in [10_u64, 20, 30, 40, 50] {
            tracker.record_success("p2", "m2", latency);
        }

        let snapshot = tracker.snapshot();
        let stats = snapshot
            .providers
            .iter()
            .find(|item| item.provider == "p2" && item.model == "m2")
            .unwrap();
        assert_eq!(stats.p50_latency_ms, Some(30.0));
        assert_eq!(stats.p95_latency_ms, Some(50.0));
        assert_eq!(stats.p99_latency_ms, Some(50.0));
    }

    #[test]
    fn evicts_when_max_samples_reached() {
        let tracker = ProviderHealthTracker::new(Duration::from_secs(60), 2);
        tracker.record_success("p", "m", 10);
        tracker.record_success("p", "m", 20);
        tracker.record_success("p", "m", 30);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.sample_count, 2);
    }
}
