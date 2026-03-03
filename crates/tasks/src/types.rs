//! Core task domain types: TaskId, TaskSpec, TaskRuntime, FailureClass, HandoffContext.

use {
    serde::{Deserialize, Serialize},
    time::OffsetDateTime,
};

use crate::state::RuntimeState;

// ── Task ID ───────────────────────────────────────────────────────────────────

/// A unique, sortable task identifier (UUID v7 — time-ordered).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub String);

impl TaskId {
    /// Generate a new random task ID (UUID v7).
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for TaskId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for TaskId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

// ── Failure taxonomy ──────────────────────────────────────────────────────────

/// Typed reason why a task execution failed.
///
/// Used to drive retry policy and surfaced in the event ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// Agent loop returned an error (e.g. tool panic, unexpected agent exit).
    AgentError,
    /// LLM context window was exceeded.
    ContextOverflow,
    /// Transient provider failure (5xx, overloaded, rate-limited).
    ProviderTransient,
    /// Permanent provider failure (invalid key, billing, model not found).
    ProviderPermanent,
    /// A tool invocation failed and the agent could not continue.
    ToolError,
    /// The agent exceeded its allotted execution time.
    TimeoutExceeded,
    /// A human decision is required before the task can proceed.
    HumanBlocker,
    /// Retry budget exhausted — no further automatic retries will be attempted.
    MaxAttemptsExceeded,
}

impl FailureClass {
    /// Whether this class should trigger an automatic retry.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::AgentError
                | Self::ContextOverflow
                | Self::ProviderTransient
                | Self::ToolError
                | Self::TimeoutExceeded
        )
    }

    /// Whether a human must intervene before retrying.
    #[must_use]
    pub fn requires_human(&self) -> bool {
        matches!(self, Self::HumanBlocker | Self::ProviderPermanent)
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::AgentError => "agent_error",
            Self::ContextOverflow => "context_overflow",
            Self::ProviderTransient => "provider_transient",
            Self::ProviderPermanent => "provider_permanent",
            Self::ToolError => "tool_error",
            Self::TimeoutExceeded => "timeout_exceeded",
            Self::HumanBlocker => "human_blocker",
            Self::MaxAttemptsExceeded => "max_attempts_exceeded",
        };
        f.write_str(s)
    }
}

// ── Handoff context ───────────────────────────────────────────────────────────

/// Failure history carried from one attempt to the next.
///
/// Injected into the sub-agent system prompt so the next attempt knows what
/// has already been tried and what led to failure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HandoffContext {
    /// Human-readable description of the last action the agent completed.
    pub last_action: String,
    /// Raw error or observation that triggered the failure.
    pub observed_error: String,
    /// Approaches that are known not to work (max 50, oldest pruned first).
    pub dead_ends: Vec<String>,
    /// Agent's own suggestion for the next attempt.
    pub suggested_next_step: String,
}

impl HandoffContext {
    /// Add a dead-end entry, pruning the oldest when the cap is exceeded.
    pub fn add_dead_end(&mut self, entry: String) {
        const MAX_DEAD_ENDS: usize = 50;
        self.dead_ends.push(entry);
        if self.dead_ends.len() > MAX_DEAD_ENDS {
            self.dead_ends.remove(0);
        }
    }

    /// Merge `other`'s dead_ends into `self` without duplicates.
    pub fn merge_dead_ends(&mut self, other: &HandoffContext) {
        for entry in &other.dead_ends {
            if !self.dead_ends.contains(entry) {
                self.add_dead_end(entry.clone());
            }
        }
    }

    /// Return a compact string suitable for injecting into a system prompt.
    #[must_use]
    pub fn as_prompt_context(&self) -> String {
        let mut parts = Vec::new();

        if !self.last_action.is_empty() {
            parts.push(format!(
                "Previous attempt last action: {}",
                self.last_action
            ));
        }
        if !self.observed_error.is_empty() {
            parts.push(format!("Previous failure: {}", self.observed_error));
        }
        if !self.dead_ends.is_empty() {
            parts.push(format!(
                "Known dead-ends to avoid:\n{}",
                self.dead_ends
                    .iter()
                    .map(|e| format!("- {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if !self.suggested_next_step.is_empty() {
            parts.push(format!("Suggested next step: {}", self.suggested_next_step));
        }

        parts.join("\n\n")
    }
}

// ── Task Spec (immutable) ─────────────────────────────────────────────────────

/// Immutable task intent — set at creation time, never changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    /// Short imperative title (e.g. "Fix authentication bug").
    pub subject: String,
    /// Detailed description of what needs to be done.
    #[serde(default)]
    pub description: String,
    /// Scheduling priority: 0 = lowest, 4 = critical. Default: 2.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// Maximum automatic retry attempts before terminal failure. Default: 3.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
    /// When this task was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

fn default_priority() -> u8 {
    2
}

fn default_max_attempts() -> u8 {
    3
}

impl TaskSpec {
    pub fn new(subject: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            subject: subject.into(),
            description: description.into(),
            priority: default_priority(),
            max_attempts: default_max_attempts(),
            created_at: OffsetDateTime::now_utc(),
        }
    }
}

// ── Task Runtime (mutable) ────────────────────────────────────────────────────

/// Mutable execution state — updated on every transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRuntime {
    /// Current lifecycle state.
    pub state: RuntimeState,
    /// Number of execution attempts so far (starts at 0, incremented on Claim).
    #[serde(default)]
    pub attempt: u8,
    /// Optimistic concurrency counter — incremented on every successful transition.
    #[serde(default)]
    pub version: u64,
    /// Agent identifier currently owning this task (set on Claim).
    #[serde(default)]
    pub owner: Option<String>,
    /// When the last transition occurred.
    #[serde(with = "time::serde::rfc3339")]
    pub last_transition_at: OffsetDateTime,
    /// Failure context carried from the last failed attempt.
    #[serde(default)]
    pub handoff: Option<HandoffContext>,
    /// Classification of the most recent failure.
    #[serde(default)]
    pub last_failure: Option<FailureClass>,
}

impl Default for TaskRuntime {
    fn default() -> Self {
        Self {
            state: RuntimeState::Pending,
            attempt: 0,
            version: 0,
            owner: None,
            last_transition_at: OffsetDateTime::now_utc(),
            handoff: None,
            last_failure: None,
        }
    }
}

// ── Task (full record) ────────────────────────────────────────────────────────

/// A complete task record combining spec + runtime + dependency list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    /// Which task list this belongs to (namespacing, e.g. team name).
    pub list_id: String,
    pub spec: TaskSpec,
    pub runtime: TaskRuntime,
    /// IDs of tasks that must reach [`TerminalState::Completed`] before this
    /// task can be claimed.
    #[serde(default)]
    pub blocked_by: Vec<TaskId>,
}

impl Task {
    pub fn new(list_id: impl Into<String>, spec: TaskSpec) -> Self {
        Self {
            id: TaskId::new(),
            list_id: list_id.into(),
            spec,
            runtime: TaskRuntime::default(),
            blocked_by: Vec::new(),
        }
    }

    /// Convenience: current state name as a static str.
    #[must_use]
    pub fn state_name(&self) -> &'static str {
        self.runtime.state.name()
    }

    /// Whether this task is in a terminal state.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.runtime.state.is_terminal()
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_unique() {
        let a = TaskId::new();
        let b = TaskId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn failure_class_retryable() {
        assert!(FailureClass::ProviderTransient.is_retryable());
        assert!(FailureClass::AgentError.is_retryable());
        assert!(!FailureClass::ProviderPermanent.is_retryable());
        assert!(!FailureClass::MaxAttemptsExceeded.is_retryable());
    }

    #[test]
    fn failure_class_requires_human() {
        assert!(FailureClass::HumanBlocker.requires_human());
        assert!(FailureClass::ProviderPermanent.requires_human());
        assert!(!FailureClass::AgentError.requires_human());
    }

    #[test]
    fn handoff_dead_end_cap() {
        let mut h = HandoffContext::default();
        for i in 0..55usize {
            h.add_dead_end(format!("entry-{i}"));
        }
        assert_eq!(h.dead_ends.len(), 50);
        // Oldest entries pruned.
        assert!(!h.dead_ends.iter().any(|e| e == "entry-0"));
        assert!(h.dead_ends.iter().any(|e| e == "entry-54"));
    }

    #[test]
    fn handoff_as_prompt_context_empty() {
        let h = HandoffContext::default();
        assert!(h.as_prompt_context().is_empty());
    }

    #[test]
    fn handoff_as_prompt_context_fields() {
        let h = HandoffContext {
            last_action: "searched for X".into(),
            observed_error: "timeout".into(),
            dead_ends: vec!["approach A".into()],
            suggested_next_step: "try approach B".into(),
        };
        let ctx = h.as_prompt_context();
        assert!(ctx.contains("searched for X"));
        assert!(ctx.contains("timeout"));
        assert!(ctx.contains("approach A"));
        assert!(ctx.contains("approach B"));
    }

    #[test]
    fn task_new_has_pending_state() {
        let spec = TaskSpec::new("test", "desc");
        let task = Task::new("default", spec);
        assert_eq!(task.state_name(), "Pending");
        assert!(!task.is_terminal());
    }
}
