//! Intent drift detection for agent goal alignment.
//!
//! This module provides functionality to track when an agent's current actions
//! drift away from the original user intent, enabling early intervention.

use std::collections::HashSet;
use tracing::{debug, warn};

/// Tracks the original user intent and detects drift from current actions.
#[derive(Debug, Clone)]
pub struct IntentTracker {
    /// The original user intent/goal.
    original_intent: String,
    /// Keywords extracted from the original intent.
    original_keywords: HashSet<String>,
    /// Number of turns since last drift check.
    turns_since_check: usize,
    /// Threshold for drift detection (0.0 to 1.0).
    drift_threshold: f64,
}

impl IntentTracker {
    /// Create a new intent tracker with the original user intent.
    pub fn new(original_intent: &str) -> Self {
        let keywords = extract_keywords(original_intent);
        debug!(
            keywords = ?keywords,
            intent = %original_intent,
            "IntentTracker initialized"
        );
        Self {
            original_intent: original_intent.to_lowercase(),
            original_keywords: keywords,
            turns_since_check: 0,
            drift_threshold: 0.7,
        }
    }

    /// Set a custom drift threshold (0.0 to 1.0).
    pub fn with_drift_threshold(mut self, threshold: f64) -> Self {
        self.drift_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    /// Check the current intent against the original and return a drift score.
    /// Returns (drift_score, is_drifted) where drift_score is 0.0-1.0.
    pub fn check_drift(&mut self, current_intent: &str, trace_id: Option<&str>) -> (f64, bool) {
        self.turns_since_check += 1;

        let current_keywords = extract_keywords(current_intent);
        let drift_score = compute_drift_score(&self.original_keywords, &current_keywords);
        let is_drifted = drift_score > self.drift_threshold;

        if is_drifted {
            warn!(
                drift_score,
                threshold = self.drift_threshold,
                trace_id = trace_id.unwrap_or(""),
                original = %self.original_intent,
                current = %current_intent,
                "Intent drift detected"
            );
        } else {
            debug!(
                drift_score,
                trace_id = trace_id.unwrap_or(""),
                "Intent alignment check"
            );
        }

        (drift_score, is_drifted)
    }

    /// Get the original intent.
    pub fn original_intent(&self) -> &str {
        &self.original_intent
    }

    /// Get the current drift threshold.
    pub fn drift_threshold(&self) -> f64 {
        self.drift_threshold
    }
}

/// Extract significant keywords from text.
/// Filters out common stop words and returns unique lowercase keywords.
fn extract_keywords(text: &str) -> HashSet<String> {
    // Common English stop words to filter out
    const STOP_WORDS: &[&str] = &[
        "a", "an", "the", "and", "or", "but", "is", "are", "was", "were", "be", "been", "being",
        "have", "has", "had", "do", "does", "did", "will", "would", "could", "should", "may",
        "might", "must", "shall", "can", "need", "dare", "ought", "used", "to", "of", "in", "for",
        "on", "with", "at", "by", "from", "as", "into", "through", "during", "before", "after",
        "above", "below", "between", "under", "again", "further", "then", "once", "here", "there",
        "when", "where", "why", "how", "all", "each", "few", "more", "most", "other", "some",
        "such", "no", "nor", "not", "only", "own", "same", "so", "than", "too", "very", "just",
        "also", "now", "that", "this", "these", "those", "i", "you", "he", "she", "it", "we",
        "they", "what", "which", "who", "me", "him", "her", "us", "them", "my", "your", "his",
        "its", "our", "their", "please", "help", "want", "let", "get", "make", "go", "see", "know",
        "take", "come", "think", "look", "use", "find", "give", "tell", "work", "call", "try",
        "ask", "need", "feel", "become", "leave", "put", "means", "any", "if", "about", "up",
        "out", "over", "down", "off",
    ];

    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| {
            let s = *s;
            s.len() > 2 && !STOP_WORDS.contains(&s)
        })
        .map(String::from)
        .collect()
}

/// Compute drift score between original and current keyword sets.
/// Returns 0.0 when identical, 1.0 when completely different.
fn compute_drift_score(original: &HashSet<String>, current: &HashSet<String>) -> f64 {
    if original.is_empty() && current.is_empty() {
        return 0.0;
    }
    if original.is_empty() || current.is_empty() {
        return 1.0;
    }

    // Jaccard similarity: |intersection| / |union|
    let intersection = original.intersection(current).count();
    let union = original.union(current).count();

    if union == 0 {
        return 1.0;
    }

    let similarity = intersection as f64 / union as f64;
    // Convert similarity (0-1) to drift (0-1): drift = 1 - similarity
    1.0 - similarity
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_keywords_filters_stop_words() {
        let keywords = extract_keywords("Please help me to write a function in Rust");
        assert!(!keywords.contains("please"));
        assert!(!keywords.contains("help"));
        assert!(!keywords.contains("to"));
        assert!(!keywords.contains("a"));
        assert!(!keywords.contains("in"));
        assert!(keywords.contains("write"));
        assert!(keywords.contains("function"));
        assert!(keywords.contains("rust"));
    }

    #[test]
    fn test_extract_keywords_lowercase() {
        let keywords = extract_keywords("Write CODE in Rust");
        assert!(keywords.contains("write"));
        assert!(keywords.contains("code"));
        assert!(keywords.contains("rust"));
    }

    #[test]
    fn test_compute_drift_score_identical() {
        let a: HashSet<String> = ["write", "function", "rust"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: HashSet<String> = ["write", "function", "rust"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let score = compute_drift_score(&a, &b);
        assert!((score - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_compute_drift_score_completely_different() {
        let a: HashSet<String> = ["write", "function"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: HashSet<String> = ["delete", "file"].iter().map(|s| s.to_string()).collect();
        let score = compute_drift_score(&a, &b);
        assert!((score - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_compute_drift_score_partial_overlap() {
        let a: HashSet<String> = ["write", "function", "rust"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: HashSet<String> = ["write", "function", "python"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let score = compute_drift_score(&a, &b);
        // intersection: write, function (2)
        // union: write, function, rust, python (4)
        // similarity: 0.5, drift: 0.5
        assert!((score - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_intent_tracker_no_drift() {
        let mut tracker = IntentTracker::new("Write a Rust function to parse JSON");
        let (score, is_drifted) =
            tracker.check_drift("Create a Rust function that can parse JSON data", None);
        assert!(score < 0.7);
        assert!(!is_drifted);
    }

    #[test]
    fn test_intent_tracker_with_drift() {
        let mut tracker = IntentTracker::new("Write a Rust function to parse JSON");
        let (score, is_drifted) = tracker.check_drift("Delete all files in the directory", None);
        assert!(score > 0.7);
        assert!(is_drifted);
    }

    #[test]
    fn test_intent_tracker_custom_threshold() {
        let mut tracker = IntentTracker::new("Write a Rust function").with_drift_threshold(0.3);
        let (score, is_drifted) = tracker.check_drift("Write a Python function", None);
        // Partial overlap but below 0.3 drift threshold
        assert!(score > 0.3);
        assert!(is_drifted);
    }

    #[test]
    fn test_empty_intents() {
        let mut tracker = IntentTracker::new("");
        let (score, _) = tracker.check_drift("", None);
        assert!((score - 0.0).abs() < 0.001);
    }
}
