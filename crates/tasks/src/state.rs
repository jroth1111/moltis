//! Runtime state enum and terminal state variants.

use {
    serde::{Deserialize, Serialize},
    time::OffsetDateTime,
};

use crate::types::{FailureClass, HandoffContext, TaskId};

// ── Terminal state ────────────────────────────────────────────────────────────

/// Terminal states — once reached a task never transitions again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "terminal", rename_all = "snake_case")]
pub enum TerminalState {
    Completed,
    Failed { class: FailureClass },
    Canceled { reason: String },
}

impl TerminalState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::Failed { .. } => "Failed",
            Self::Canceled { .. } => "Canceled",
        }
    }
}

// ── Runtime state ─────────────────────────────────────────────────────────────

/// The full set of lifecycle states a task can occupy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeState {
    /// Waiting to be claimed; all dependencies may or may not be met.
    Pending,

    /// At least one declared dependency is not yet completed.
    Blocked { waiting_on: Vec<TaskId> },

    /// An agent has claimed the task and is executing it.
    Active {
        owner: String,
        /// Optional wall-clock deadline after which the task may be reclaimed.
        #[serde(default, with = "optional_rfc3339")]
        lease_expires_at: Option<OffsetDateTime>,
    },

    /// A previous attempt failed; the task will auto-promote back to Pending
    /// after `retry_after`.
    Retrying {
        reason: FailureClass,
        #[serde(with = "time::serde::rfc3339")]
        retry_after: OffsetDateTime,
        handoff: HandoffContext,
    },

    /// Execution is blocked pending a human response.
    AwaitingHuman {
        question: String,
        handoff: HandoffContext,
    },

    /// The task has reached a terminal outcome.
    Terminal(TerminalState),
}

// ── Serde helper for Option<OffsetDateTime> ───────────────────────────────────

mod optional_rfc3339 {
    use {
        serde::{Deserialize, Deserializer, Serializer},
        time::OffsetDateTime,
    };

    pub fn serialize<S: Serializer>(v: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(dt) => time::serde::rfc3339::serialize(dt, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|s| {
                OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

impl RuntimeState {
    /// Return a stable string name for this state (used in diagnostics / metrics).
    pub fn name(&self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Blocked { .. } => "Blocked",
            Self::Active { .. } => "Active",
            Self::Retrying { .. } => "Retrying",
            Self::AwaitingHuman { .. } => "AwaitingHuman",
            Self::Terminal(t) => t.name(),
        }
    }

    /// Whether this is a terminal state (no further transitions possible).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal(_))
    }

    /// Whether this task is currently being executed by an agent.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// Return the current owner if the task is Active.
    #[must_use]
    pub fn active_owner(&self) -> Option<&str> {
        match self {
            Self::Active { owner, .. } => Some(owner.as_str()),
            _ => None,
        }
    }

    /// Whether the task is waiting for a human decision.
    #[must_use]
    pub fn is_awaiting_human(&self) -> bool {
        matches!(self, Self::AwaitingHuman { .. })
    }

    /// Whether the task lease has expired (for zombie detection).
    #[must_use]
    pub fn is_lease_expired(&self) -> bool {
        match self {
            Self::Active {
                lease_expires_at: Some(exp),
                ..
            } => OffsetDateTime::now_utc() > *exp,
            _ => false,
        }
    }
}

impl PartialEq for RuntimeState {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_terminal() {
        assert!(RuntimeState::Terminal(TerminalState::Completed).is_terminal());
        assert!(
            RuntimeState::Terminal(TerminalState::Failed {
                class: FailureClass::AgentError
            })
            .is_terminal()
        );
        assert!(
            RuntimeState::Terminal(TerminalState::Canceled {
                reason: "manual".into()
            })
            .is_terminal()
        );
    }

    #[test]
    fn non_terminal_states_are_not_terminal() {
        assert!(!RuntimeState::Pending.is_terminal());
        assert!(!RuntimeState::Blocked { waiting_on: vec![] }.is_terminal());
        assert!(
            !RuntimeState::Active {
                owner: "agent-1".into(),
                lease_expires_at: None,
            }
            .is_terminal()
        );
    }

    #[test]
    fn active_owner_extraction() {
        let s = RuntimeState::Active {
            owner: "worker-a".into(),
            lease_expires_at: None,
        };
        assert_eq!(s.active_owner(), Some("worker-a"));
        assert_eq!(RuntimeState::Pending.active_owner(), None);
    }

    #[test]
    fn state_names() {
        assert_eq!(RuntimeState::Pending.name(), "Pending");
        assert_eq!(
            RuntimeState::Active {
                owner: "x".into(),
                lease_expires_at: None
            }
            .name(),
            "Active"
        );
        assert_eq!(
            RuntimeState::Terminal(TerminalState::Completed).name(),
            "Completed"
        );
    }

    #[test]
    fn expired_lease_detection() {
        let past = OffsetDateTime::now_utc() - time::Duration::seconds(60);
        let s = RuntimeState::Active {
            owner: "agent".into(),
            lease_expires_at: Some(past),
        };
        assert!(s.is_lease_expired());

        let future = OffsetDateTime::now_utc() + time::Duration::seconds(60);
        let s2 = RuntimeState::Active {
            owner: "agent".into(),
            lease_expires_at: Some(future),
        };
        assert!(!s2.is_lease_expired());
    }
}
