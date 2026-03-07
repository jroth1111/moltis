//! Cross-session learning for agent improvement.
//!
//! This module provides mechanisms to learn from agent interactions across
//! multiple sessions, enabling the agent to improve over time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use std::sync::RwLock;
use time::OffsetDateTime;

/// Maximum number of learnings to keep per category.
const MAX_LEARNINGS_PER_CATEGORY: usize = 100;

/// Minimum confidence threshold for a learning to be applied.
const CONFIDENCE_THRESHOLD: f64 = 0.6;

/// A learned pattern from agent interactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Learning {
    /// Unique identifier for this learning.
    pub id: String,
    /// The category of learning.
    pub category: String,
    /// The actual learned content.
    pub content: String,
    /// Context in which this learning was acquired.
    pub context: LearningContext,
    /// Confidence in this learning (0.0-1.0).
    pub confidence: f64,
    /// Number of times this learning has been successfully applied.
    pub application_count: u32,
    /// When this learning was created.
    pub created_at: OffsetDateTime,
    /// When this learning was last applied.
    pub last_applied: Option<OffsetDateTime>,
}

/// Context in which a learning was acquired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningContext {
    /// The task type being performed.
    pub task_type: String,
    /// Key entities involved.
    pub entities: Vec<String>,
    /// The outcome that led to this learning.
    pub outcome: LearningOutcome,
}

/// Outcome that led to a learning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LearningOutcome {
    Success,
    FailureCorrected,
    HumanIntervention,
    PatternObserved,
}

/// Store for cross-session learnings.
#[derive(Debug, Clone)]
pub struct CrossSessionLearning {
    store_path: PathBuf,
    learnings: Arc<RwLock<HashMap<String, Vec<Learning>>>>,
    dirty: Arc<RwLock<bool>>,
}

impl CrossSessionLearning {
    /// Create a new cross-session learning store.
    pub fn new(data_dir: PathBuf) -> Self {
        let store_path = data_dir.join("agent_learnings.json");
        let learnings = Arc::new(RwLock::new(HashMap::new()));
        let dirty = Arc::new(RwLock::new(false));

        let store = Self {
            store_path,
            learnings,
            dirty,
        };

        let _ = store.load();
        store
    }

    /// Add a new learning.
    pub fn add_learning(&self, learning: Learning) {
        let mut guard = self.learnings.write().unwrap_or_else(|e| e.into_inner());
        let category_learnings = guard.entry(learning.category.clone()).or_default();

        if category_learnings
            .iter()
            .any(|l| l.content == learning.content)
        {
            return;
        }

        if category_learnings.len() >= MAX_LEARNINGS_PER_CATEGORY {
            category_learnings.sort_by(|a, b| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            category_learnings.truncate(MAX_LEARNINGS_PER_CATEGORY - 1);
        }

        category_learnings.push(learning);
        *self.dirty.write().unwrap_or_else(|e| e.into_inner()) = true;
    }

    /// Get learnings for a specific category.
    pub fn get_learnings(&self, category: &str) -> Vec<Learning> {
        let guard = self.learnings.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(category)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|l| l.confidence >= CONFIDENCE_THRESHOLD)
            .collect()
    }

    /// Get all learnings across all categories.
    pub fn get_all_learnings(&self) -> Vec<Learning> {
        let guard = self.learnings.read().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .flat_map(|v| v.iter().cloned())
            .filter(|l| l.confidence >= CONFIDENCE_THRESHOLD)
            .collect()
    }

    /// Record that a learning was applied.
    pub fn record_application(&self, learning_id: &str) {
        let mut guard = self.learnings.write().unwrap_or_else(|e| e.into_inner());
        for category_learnings in guard.values_mut() {
            for learning in category_learnings.iter_mut() {
                if learning.id == learning_id {
                    learning.application_count += 1;
                    learning.last_applied = Some(OffsetDateTime::now_utc());
                    *self.dirty.write().unwrap_or_else(|e| e.into_inner()) = true;
                    return;
                }
            }
        }
    }

    /// Boost confidence of a learning after successful application.
    pub fn boost_confidence(&self, learning_id: &str, boost: f64) {
        let mut guard = self.learnings.write().unwrap_or_else(|e| e.into_inner());
        for category_learnings in guard.values_mut() {
            for learning in category_learnings.iter_mut() {
                if learning.id == learning_id {
                    learning.confidence = (learning.confidence + boost).min(1.0);
                    *self.dirty.write().unwrap_or_else(|e| e.into_inner()) = true;
                    return;
                }
            }
        }
    }

    /// Reduce confidence of a learning after failed application.
    pub fn reduce_confidence(&self, learning_id: &str, reduction: f64) {
        let mut guard = self.learnings.write().unwrap_or_else(|e| e.into_inner());
        for category_learnings in guard.values_mut() {
            for learning in category_learnings.iter_mut() {
                if learning.id == learning_id {
                    learning.confidence = (learning.confidence - reduction).max(0.0);
                    *self.dirty.write().unwrap_or_else(|e| e.into_inner()) = true;
                    return;
                }
            }
        }
    }

    /// Find relevant learnings for a given context.
    pub fn find_relevant(&self, context: &LearningContext) -> Vec<Learning> {
        let guard = self.learnings.read().unwrap_or_else(|e| e.into_inner());
        let mut relevant: Vec<Learning> = Vec::new();

        for category_learnings in guard.values() {
            for learning in category_learnings {
                if learning.confidence < CONFIDENCE_THRESHOLD {
                    continue;
                }

                let task_match = learning.context.task_type == context.task_type;
                let entity_overlap = learning
                    .context
                    .entities
                    .iter()
                    .any(|e| context.entities.contains(e));

                if task_match || entity_overlap {
                    relevant.push(learning.clone());
                }
            }
        }

        relevant.sort_by(|a, b| {
            let score_a = a.confidence * (1.0 + a.application_count as f64 * 0.1);
            let score_b = b.confidence * (1.0 + b.application_count as f64 * 0.1);
            score_b
                .partial_cmp(&score_a)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        relevant
    }

    /// Persist learnings to disk.
    pub fn save(&self) -> anyhow::Result<()> {
        if !*self.dirty.read().unwrap_or_else(|e| e.into_inner()) {
            return Ok(());
        }

        let guard = self.learnings.read().unwrap_or_else(|e| e.into_inner());
        let flat_learnings: Vec<&Learning> = guard.values().flatten().collect();

        let json = serde_json::to_string_pretty(&flat_learnings)?;
        std::fs::write(&self.store_path, json)?;

        *self.dirty.write().unwrap_or_else(|e| e.into_inner()) = false;
        Ok(())
    }

    /// Load learnings from disk.
    fn load(&self) -> anyhow::Result<()> {
        if !self.store_path.exists() {
            return Ok(());
        }

        let json = std::fs::read_to_string(&self.store_path)?;
        let flat_learnings: Vec<Learning> = serde_json::from_str(&json)?;

        let mut guard = self.learnings.write().unwrap_or_else(|e| e.into_inner());
        for learning in flat_learnings {
            guard
                .entry(learning.category.clone())
                .or_default()
                .push(learning);
        }

        Ok(())
    }
}

impl Drop for CrossSessionLearning {
    fn drop(&mut self) {
        let _ = self.save();
    }
}

impl Learning {
    /// Create a new learning.
    pub fn new(
        category: impl Into<String>,
        content: impl Into<String>,
        context: LearningContext,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            category: category.into(),
            content: content.into(),
            context,
            confidence: CONFIDENCE_THRESHOLD,
            application_count: 0,
            created_at: OffsetDateTime::now_utc(),
            last_applied: None,
        }
    }

    /// Create a learning with initial confidence.
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }
}

impl LearningContext {
    /// Create a new learning context.
    pub fn new(
        task_type: impl Into<String>,
        entities: Vec<String>,
        outcome: LearningOutcome,
    ) -> Self {
        Self {
            task_type: task_type.into(),
            entities,
            outcome,
        }
    }
}

/// Common learning categories.
pub mod categories {
    pub const ERROR_HANDLING: &str = "error_handling";
    pub const TOOL_USAGE: &str = "tool_usage";
    pub const CODE_PATTERNS: &str = "code_patterns";
    pub const USER_PREFERENCES: &str = "user_preferences";
    pub const PROJECT_STRUCTURE: &str = "project_structure";
    pub const DEBUGGING: &str = "debugging";
    pub const PERFORMANCE: &str = "performance";
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        tempfile::tempdir().unwrap().keep()
    }

    #[test]
    fn test_add_and_retrieve_learning() {
        let store = CrossSessionLearning::new(temp_dir());

        let learning = Learning::new(
            categories::ERROR_HANDLING,
            "Use .ok_or() instead of .unwrap()",
            LearningContext::new("code_review", vec![], LearningOutcome::PatternObserved),
        );

        store.add_learning(learning);

        let learnings = store.get_learnings(categories::ERROR_HANDLING);
        assert_eq!(learnings.len(), 1);
    }

    #[test]
    fn test_find_relevant_learnings() {
        let store = CrossSessionLearning::new(temp_dir());

        let learning = Learning::new(
            categories::TOOL_USAGE,
            "Prefer Edit tool over Bash",
            LearningContext::new(
                "file_editing",
                vec!["Edit".to_string()],
                LearningOutcome::Success,
            ),
        )
        .with_confidence(0.8);

        store.add_learning(learning);

        let context = LearningContext::new(
            "file_editing",
            vec!["Edit".to_string()],
            LearningOutcome::Success,
        );

        let relevant = store.find_relevant(&context);
        assert_eq!(relevant.len(), 1);
    }

    #[test]
    fn test_low_confidence_filtered() {
        let store = CrossSessionLearning::new(temp_dir());

        let learning = Learning::new(
            categories::PERFORMANCE,
            "Cache frequently accessed data",
            LearningContext::new("optimization", vec![], LearningOutcome::PatternObserved),
        )
        .with_confidence(0.3);

        store.add_learning(learning);

        let learnings = store.get_learnings(categories::PERFORMANCE);
        assert!(learnings.is_empty());
    }
}
