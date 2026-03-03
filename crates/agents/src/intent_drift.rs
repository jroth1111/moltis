//! Intent drift detection for agent conversations.
//!
//! This module provides tools to detect when a conversation drifts away from
//! the user's original intent. This helps prevent agents from going down
//! rabbit holes or losing track of the primary goal.

use std::collections::VecDeque;

/// Maximum number of recent messages to analyze for drift detection.
const DRIFT_WINDOW_SIZE: usize = 10;

/// Threshold for semantic similarity before flagging drift.
const DRIFT_THRESHOLD: f64 = 0.3;

/// Represents the user's primary intent extracted from a conversation.
#[derive(Debug, Clone, Default)]
pub struct UserIntent {
    /// The main goal or task the user wants to accomplish.
    pub primary_goal: Option<String>,
    /// Key entities or topics mentioned in the original request.
    pub key_entities: Vec<String>,
    /// Constraints or requirements specified by the user.
    pub constraints: Vec<String>,
    /// Confidence in the intent extraction.
    pub extraction_confidence: f64,
}

/// Result of a drift detection analysis.
#[derive(Debug, Clone)]
pub struct DriftAnalysis {
    /// Whether drift was detected.
    pub drift_detected: bool,
    /// Similarity score between current context and original intent (0.0-1.0).
    pub similarity_score: f64,
    /// Reason for drift detection, if any.
    pub drift_reason: Option<String>,
    /// Suggested course correction, if drift is detected.
    pub suggested_correction: Option<String>,
}

/// Tracks intent drift over the course of a conversation.
#[derive(Debug, Clone)]
pub struct IntentDriftDetector {
    /// The original user intent.
    original_intent: UserIntent,
    /// Recent conversation topics for drift analysis.
    recent_topics: VecDeque<String>,
    /// Number of messages since the last intent check.
    messages_since_check: usize,
    /// Whether drift has been flagged.
    drift_flagged: bool,
    /// Configuration for drift detection.
    config: DriftConfig,
}

/// Configuration for drift detection behavior.
#[derive(Debug, Clone)]
pub struct DriftConfig {
    /// Number of messages between drift checks.
    pub check_interval: usize,
    /// Threshold for flagging drift (0.0-1.0, lower = more sensitive).
    pub drift_threshold: f64,
    /// Whether to suggest corrections automatically.
    pub auto_suggest_corrections: bool,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            check_interval: 5,
            drift_threshold: DRIFT_THRESHOLD,
            auto_suggest_corrections: true,
        }
    }
}

impl IntentDriftDetector {
    /// Create a new drift detector with the given original intent.
    pub fn new(intent: UserIntent) -> Self {
        Self {
            original_intent: intent,
            recent_topics: VecDeque::with_capacity(DRIFT_WINDOW_SIZE),
            messages_since_check: 0,
            drift_flagged: false,
            config: DriftConfig::default(),
        }
    }

    /// Create a detector with custom configuration.
    pub fn with_config(intent: UserIntent, config: DriftConfig) -> Self {
        Self {
            original_intent: intent,
            recent_topics: VecDeque::with_capacity(DRIFT_WINDOW_SIZE),
            messages_since_check: 0,
            drift_flagged: false,
            config,
        }
    }

    /// Extract intent from a user message.
    pub fn extract_intent(message: &str) -> UserIntent {
        let primary_goal = extract_primary_goal(message);
        let key_entities = extract_entities(message);
        let constraints = extract_constraints(message);

        let extraction_confidence = if primary_goal.is_some() {
            0.7
        } else {
            0.3
        };

        UserIntent {
            primary_goal,
            key_entities,
            constraints,
            extraction_confidence,
        }
    }

    /// Record a new message for drift analysis.
    pub fn record_message(&mut self, message: &str, _is_user: bool) {
        let topic = extract_topic(message);
        if let Some(t) = topic {
            if self.recent_topics.len() >= DRIFT_WINDOW_SIZE {
                self.recent_topics.pop_front();
            }
            self.recent_topics.push_back(t);
        }

        self.messages_since_check += 1;
    }

    /// Check for intent drift.
    pub fn check_drift(&mut self) -> DriftAnalysis {
        if self.recent_topics.is_empty() {
            return DriftAnalysis {
                drift_detected: false,
                similarity_score: 1.0,
                drift_reason: None,
                suggested_correction: None,
            };
        }

        let similarity = self.calculate_similarity();

        let drift_detected = similarity < self.config.drift_threshold;

        let drift_reason = if drift_detected {
            self.drift_flagged = true;
            Some(format!(
                "Conversation similarity ({:.2}) below threshold ({:.2})",
                similarity, self.config.drift_threshold
            ))
        } else {
            None
        };

        let suggested_correction = if drift_detected && self.config.auto_suggest_corrections {
            self.original_intent
                .primary_goal
                .as_ref()
                .map(|goal| format!("Consider refocusing on the original goal: {}", goal))
        } else {
            None
        };

        self.messages_since_check = 0;

        DriftAnalysis {
            drift_detected,
            similarity_score: similarity,
            drift_reason,
            suggested_correction,
        }
    }

    /// Check if we should perform a drift check based on message count.
    pub fn should_check(&self) -> bool {
        self.messages_since_check >= self.config.check_interval
    }

    /// Get the original intent.
    pub fn original_intent(&self) -> &UserIntent {
        &self.original_intent
    }

    /// Check if drift has been flagged.
    pub fn is_drift_flagged(&self) -> bool {
        self.drift_flagged
    }

    /// Clear the drift flag.
    pub fn clear_drift_flag(&mut self) {
        self.drift_flagged = false;
    }

    /// Calculate semantic similarity between recent topics and original intent.
    fn calculate_similarity(&self) -> f64 {
        let Some(goal) = self.original_intent.primary_goal.as_ref() else {
            return 1.0;
        };
        let goal_lower = goal.to_lowercase();
        let goal_words: std::collections::HashSet<&str> = goal_lower
            .split_whitespace()
            .filter(|w| w.len() > 3)
            .collect();

        if goal_words.is_empty() {
            return 1.0;
        }

        let mut match_count = 0;
        let mut total_checks = 0;

        for topic in &self.recent_topics {
            let topic_lower = topic.to_lowercase();
            let topic_words: std::collections::HashSet<&str> = topic_lower
                .split_whitespace()
                .filter(|w| w.len() > 3)
                .collect();

            let intersection = goal_words.intersection(&topic_words).count();
            if intersection > 0 {
                match_count += 1;
            }
            total_checks += 1;
        }

        if total_checks == 0 {
            return 1.0;
        }

        match_count as f64 / total_checks as f64
    }
}

/// Extract the primary goal from a message.
fn extract_primary_goal(message: &str) -> Option<String> {
    let action_words = [
        "create",
        "build",
        "fix",
        "implement",
        "add",
        "remove",
        "update",
        "refactor",
        "test",
        "debug",
        "deploy",
        "write",
        "read",
        "analyze",
        "help",
        "explain",
        "show",
        "find",
        "search",
        "convert",
        "optimize",
    ];

    let lower = message.to_lowercase();
    for word in action_words {
        if lower.contains(word) {
            for sentence in message.split('.') {
                if sentence.to_lowercase().contains(word) {
                    return Some(sentence.trim().to_string());
                }
            }
        }
    }

    message.split('.').next().map(|s| s.trim().to_string())
}

/// Extract key entities from a message.
fn extract_entities(message: &str) -> Vec<String> {
    let mut entities = Vec::new();

    let mut in_quotes = false;
    let mut current_entity = String::new();
    let mut quote_char = ' ';

    for ch in message.chars() {
        if (ch == '"' || ch == '\'') && !in_quotes {
            in_quotes = true;
            quote_char = ch;
            current_entity.clear();
        } else if ch == quote_char && in_quotes {
            in_quotes = false;
            if !current_entity.is_empty() {
                entities.push(current_entity.clone());
            }
        } else if in_quotes {
            current_entity.push(ch);
        }
    }

    for word in message.split_whitespace() {
        if word.contains('.') || word.contains('/') || word.contains('_') {
            let cleaned: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '.' || *c == '/' || *c == '_' || *c == '-')
                .collect();
            if cleaned.len() > 3 {
                entities.push(cleaned);
            }
        }
    }

    entities
}

/// Extract constraints from a message.
fn extract_constraints(message: &str) -> Vec<String> {
    let mut constraints = Vec::new();

    let constraint_patterns = [
        ("must ", "must"),
        ("should ", "should"),
        ("need to ", "need to"),
        ("don't ", "don't"),
        ("cannot ", "cannot"),
        ("without ", "without"),
        ("using ", "using"),
        ("with ", "with"),
    ];

    let lower = message.to_lowercase();
    for (pattern, _) in constraint_patterns {
        if lower.contains(pattern) {
            if let Some(pos) = lower.find(pattern) {
                let end = message[pos..]
                    .find('.')
                    .unwrap_or(message[pos..].len().min(50))
                    + pos;
                constraints.push(message[pos..end].trim().to_string());
            }
        }
    }

    constraints
}

/// Extract the main topic from a message.
fn extract_topic(message: &str) -> Option<String> {
    let words: Vec<&str> = message
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .take(10)
        .collect();

    if words.is_empty() {
        None
    } else {
        Some(words.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_intent() {
        let intent = IntentDriftDetector::extract_intent(
            "Create a new function called `process_data` that handles JSON input",
        );

        assert!(intent.primary_goal.is_some());
        assert!(intent.key_entities.contains(&"process_data".to_string()));
    }

    #[test]
    fn test_no_drift_initially() {
        let intent = UserIntent {
            primary_goal: Some("Fix the bug in authentication".to_string()),
            key_entities: vec!["authentication".to_string()],
            constraints: vec![],
            extraction_confidence: 0.8,
        };

        let detector = IntentDriftDetector::new(intent);
        let analysis = detector.clone().check_drift();

        assert!(!analysis.drift_detected);
    }

    #[test]
    fn test_drift_detection() {
        let intent = UserIntent {
            primary_goal: Some("Fix the authentication bug in login.rs".to_string()),
            key_entities: vec!["authentication".to_string(), "login.rs".to_string()],
            constraints: vec![],
            extraction_confidence: 0.8,
        };

        let mut detector = IntentDriftDetector::new(intent);

        for _ in 0..5 {
            detector.record_message("Let's discuss the weather and climate patterns", true);
        }

        let analysis = detector.check_drift();
        assert!(analysis.drift_detected);
        assert!(analysis.suggested_correction.is_some());
    }

    #[test]
    fn test_should_check_interval() {
        let intent = UserIntent::default();
        let mut detector = IntentDriftDetector::new(intent);

        assert!(!detector.should_check());

        for _ in 0..5 {
            detector.record_message("test", true);
        }

        assert!(detector.should_check());
    }
}
