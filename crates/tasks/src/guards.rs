//! Transition guard predicates.
//!
//! Guards are pure functions that determine whether a proposed transition is
//! allowed given the current task state. They are called from `transitions::apply`.

use crate::state::RuntimeState;
use crate::types::Task;

/// All declared dependencies are in a terminal-completed state.
#[must_use]
pub fn deps_satisfied(task: &Task, completed_ids: &[String]) -> bool {
    task.blocked_by
        .iter()
        .all(|dep| completed_ids.iter().any(|c| c == &dep.0))
}

/// The task is currently active (guarded `Complete` / `Fail` / `Escalate`).
#[must_use]
pub fn is_active(state: &RuntimeState) -> bool {
    state.is_active()
}

/// The task is pending or retrying — eligible for a `Claim`.
#[must_use]
pub fn is_claimable(state: &RuntimeState) -> bool {
    matches!(state, RuntimeState::Pending)
}

/// The task is blocked — eligible for `DependenciesMet`.
#[must_use]
pub fn is_blocked(state: &RuntimeState) -> bool {
    matches!(state, RuntimeState::Blocked { .. })
}

/// The task is awaiting a human — eligible for `HumanResolve`.
#[must_use]
pub fn is_awaiting_human(state: &RuntimeState) -> bool {
    state.is_awaiting_human()
}

/// The task is in the Retrying state — eligible for `PromoteRetry`.
#[must_use]
pub fn is_retrying(state: &RuntimeState) -> bool {
    matches!(state, RuntimeState::Retrying { .. })
}

/// The task has not yet reached its max_attempts budget.
#[must_use]
pub fn under_attempt_budget(attempt: u8, max_attempts: u8) -> bool {
    attempt < max_attempts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TerminalState;
    use crate::types::{FailureClass, HandoffContext, TaskId, TaskSpec};
    use time::OffsetDateTime;

    fn make_task(state: RuntimeState) -> Task {
        let spec = TaskSpec::new("test", "");
        let mut task = Task::new("default", spec);
        task.runtime.state = state;
        task
    }

    #[test]
    fn deps_satisfied_no_deps() {
        let task = make_task(RuntimeState::Pending);
        assert!(deps_satisfied(&task, &[]));
    }

    #[test]
    fn deps_satisfied_all_complete() {
        let mut task = make_task(RuntimeState::Pending);
        task.blocked_by.push(TaskId::from("1"));
        task.blocked_by.push(TaskId::from("2"));
        assert!(deps_satisfied(&task, &["1".into(), "2".into(), "3".into()]));
    }

    #[test]
    fn deps_satisfied_missing_dep() {
        let mut task = make_task(RuntimeState::Pending);
        task.blocked_by.push(TaskId::from("1"));
        task.blocked_by.push(TaskId::from("2"));
        assert!(!deps_satisfied(&task, &["1".into()]));
    }

    #[test]
    fn is_claimable_only_for_pending() {
        assert!(is_claimable(&RuntimeState::Pending));
        assert!(!is_claimable(&RuntimeState::Active {
            owner: "x".into(),
            lease_expires_at: None
        }));
        assert!(!is_claimable(&RuntimeState::Terminal(
            TerminalState::Completed
        )));
    }

    #[test]
    fn under_attempt_budget_boundary() {
        assert!(under_attempt_budget(0, 3));
        assert!(under_attempt_budget(2, 3));
        assert!(!under_attempt_budget(3, 3));
        assert!(!under_attempt_budget(10, 3));
    }

    #[test]
    fn is_retrying_state() {
        let state = RuntimeState::Retrying {
            reason: FailureClass::AgentError,
            retry_after: OffsetDateTime::now_utc(),
            handoff: HandoffContext::default(),
        };
        assert!(is_retrying(&state));
        assert!(!is_retrying(&RuntimeState::Pending));
    }
}
