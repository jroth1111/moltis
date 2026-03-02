//! Event-triggered cron job matching.
//!
//! `EventMatcher` loads all event-triggered cron jobs from the store,
//! compiles their patterns into regexes, and provides fast message matching.

use std::sync::Arc;
use regex::Regex;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::types::{CronJobId, CronSchedule};

/// A compiled event trigger entry.
struct EventEntry {
    job_id: CronJobId,
    pattern: Regex,
    channel_filter: Option<String>,
}

/// Matches incoming messages against event-triggered cron jobs.
pub struct EventMatcher {
    entries: Arc<RwLock<Vec<EventEntry>>>,
}

impl EventMatcher {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Reload event entries from the given list of (job_id, schedule) pairs.
    pub async fn reload(&self, jobs: Vec<(CronJobId, CronSchedule)>) {
        let mut new_entries = Vec::new();
        for (job_id, schedule) in jobs {
            if let CronSchedule::EventTrigger { pattern, channel_filter } = schedule {
                match Regex::new(&pattern) {
                    Ok(re) => new_entries.push(EventEntry {
                        job_id,
                        pattern: re,
                        channel_filter,
                    }),
                    Err(e) => {
                        warn!(%job_id, pattern, error = %e, "invalid event trigger regex, skipping");
                    }
                }
            }
        }
        debug!(count = new_entries.len(), "event matcher reloaded");
        *self.entries.write().await = new_entries;
    }

    /// Returns the job IDs whose patterns match the given message.
    pub async fn match_message(&self, channel: Option<&str>, text: &str) -> Vec<CronJobId> {
        let entries = self.entries.read().await;
        entries
            .iter()
            .filter(|e| {
                // Check channel filter
                if let Some(ref filter) = e.channel_filter {
                    if channel.map_or(true, |ch| ch != filter) {
                        return false;
                    }
                }
                e.pattern.is_match(text)
            })
            .map(|e| e.job_id.clone())
            .collect()
    }

    /// Invalidate and rebuild from a fresh job list.
    pub async fn invalidate(&self, jobs: Vec<(CronJobId, CronSchedule)>) {
        self.reload(jobs).await;
    }
}

impl Default for EventMatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jobs(patterns: &[(&str, Option<&str>)]) -> Vec<(CronJobId, CronSchedule)> {
        patterns
            .iter()
            .enumerate()
            .map(|(i, (pat, ch))| (
                format!("job-{i}"),
                CronSchedule::EventTrigger {
                    pattern: pat.to_string(),
                    channel_filter: ch.map(str::to_string),
                },
            ))
            .collect()
    }

    #[tokio::test]
    async fn matches_simple_pattern() {
        let matcher = EventMatcher::new();
        matcher.reload(make_jobs(&[("billing", None)])).await;
        let ids = matcher.match_message(None, "I have a billing question").await;
        assert_eq!(ids, vec!["job-0"]);
    }

    #[tokio::test]
    async fn no_match() {
        let matcher = EventMatcher::new();
        matcher.reload(make_jobs(&[("billing", None)])).await;
        let ids = matcher.match_message(None, "how do I write Rust?").await;
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn channel_filter_respected() {
        let matcher = EventMatcher::new();
        matcher.reload(make_jobs(&[("hello", Some("general"))])).await;
        // Wrong channel — no match
        let ids = matcher.match_message(Some("random"), "hello world").await;
        assert!(ids.is_empty());
        // Right channel — match
        let ids = matcher.match_message(Some("general"), "hello world").await;
        assert_eq!(ids, vec!["job-0"]);
    }

    #[tokio::test]
    async fn invalid_regex_skipped() {
        let matcher = EventMatcher::new();
        // "[invalid" is not a valid regex
        let jobs = vec![("job-bad".to_string(), CronSchedule::EventTrigger {
            pattern: "[invalid".to_string(),
            channel_filter: None,
        })];
        matcher.reload(jobs).await;
        // Should have 0 entries (invalid pattern was skipped with a warning)
        let ids = matcher.match_message(None, "anything").await;
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn reload_replaces_old_entries() {
        let matcher = EventMatcher::new();
        matcher.reload(make_jobs(&[("old", None)])).await;
        matcher.reload(make_jobs(&[("new", None)])).await;
        let ids = matcher.match_message(None, "old pattern").await;
        assert!(ids.is_empty());
        let ids = matcher.match_message(None, "new pattern").await;
        assert!(!ids.is_empty());
    }
}
