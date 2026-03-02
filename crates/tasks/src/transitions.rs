//! Transition event enum and the state-machine apply engine.
//!
//! `apply` is the single authoritative function for evolving task state.
//! It takes (task, event) and returns an updated task on success, or a
//! `TransitionError` if the (state, event) pair is invalid.

use time::OffsetDateTime;

use crate::{
    errors::TransitionError,
    state::{RuntimeState, TerminalState},
    types::{FailureClass, HandoffContext, Task, TaskId},
};

// ── Transition events ─────────────────────────────────────────────────────────

/// Every valid transition input.
#[derive(Debug, Clone)]
pub enum TransitionEvent {
    /// An agent claims ownership of a Pending task.
    Claim {
        owner: String,
        /// Optional lease expiry for zombie detection.
        lease_duration_secs: Option<u64>,
    },

    /// Mark a Pending task as blocked on unmet dependencies.
    Block { waiting_on: Vec<TaskId> },

    /// All dependencies of a Blocked task are now Completed.
    DependenciesMet,

    /// Active task completed successfully.
    Complete,

    /// Active task failed; provide context for recovery.
    Fail {
        class: FailureClass,
        handoff: HandoffContext,
        /// If `Some`, the recovery manager will set this retry time;
        /// otherwise defaults to now + backoff.
        retry_after: Option<OffsetDateTime>,
    },

    /// Active task needs human input before it can continue.
    Escalate {
        question: String,
        handoff: HandoffContext,
    },

    /// Promote a Retrying task back to Pending (called by the recovery cron).
    PromoteRetry,

    /// A human has provided a resolution for an AwaitingHuman task.
    HumanResolve { resolution: String },

    /// Cancel a task that has not yet reached a terminal state.
    Cancel { reason: String },
}

// ── apply ─────────────────────────────────────────────────────────────────────

/// Apply `event` to `task`, returning an updated task on success.
///
/// Increments `task.runtime.version` on every successful transition so callers
/// can use it for optimistic concurrency (CAS) in the store.
///
/// # Errors
/// Returns [`TransitionError::InvalidTransition`] when the (state, event) pair
/// is not a valid edge in the state machine.
pub fn apply(mut task: Task, event: &TransitionEvent) -> Result<Task, TransitionError> {
    let new_state = dispatch(&task, event)?;

    // Update runtime fields.
    task.runtime.state = new_state;
    task.runtime.version = task.runtime.version.saturating_add(1);
    task.runtime.last_transition_at = OffsetDateTime::now_utc();

    // Track ownership changes.
    match event {
        TransitionEvent::Claim { owner, .. } => {
            task.runtime.owner = Some(owner.clone());
            task.runtime.attempt = task.runtime.attempt.saturating_add(1);
        },
        TransitionEvent::Complete
        | TransitionEvent::Fail { .. }
        | TransitionEvent::Cancel { .. } => {
            task.runtime.owner = None;
        },
        _ => {},
    }

    // Track failure context.
    if let TransitionEvent::Fail { class, handoff, .. } = event {
        task.runtime.last_failure = Some(class.clone());
        task.runtime.handoff = Some(handoff.clone());
    }

    Ok(task)
}

/// Pure state-machine dispatch — returns the next [`RuntimeState`] or an error.
fn dispatch(task: &Task, event: &TransitionEvent) -> Result<RuntimeState, TransitionError> {
    match (&task.runtime.state, event) {
        // ── Pending ────────────────────────────────────────────────────────

        // Pending → Active (agent claims the task)
        (
            RuntimeState::Pending,
            TransitionEvent::Claim {
                owner,
                lease_duration_secs,
            },
        ) => {
            let lease_expires_at = lease_duration_secs
                .map(|secs| OffsetDateTime::now_utc() + time::Duration::seconds(secs as i64));
            Ok(RuntimeState::Active {
                owner: owner.clone(),
                lease_expires_at,
            })
        },

        // Pending → Blocked (dependency tracking)
        (RuntimeState::Pending, TransitionEvent::Block { waiting_on }) => {
            Ok(RuntimeState::Blocked {
                waiting_on: waiting_on.clone(),
            })
        },

        // Pending → Terminal(Canceled)
        (RuntimeState::Pending, TransitionEvent::Cancel { reason }) => {
            Ok(RuntimeState::Terminal(TerminalState::Canceled {
                reason: reason.clone(),
            }))
        },

        // ── Blocked ────────────────────────────────────────────────────────

        // Blocked → Pending (all deps satisfied)
        (RuntimeState::Blocked { .. }, TransitionEvent::DependenciesMet) => {
            Ok(RuntimeState::Pending)
        },

        // Blocked → Terminal(Canceled)
        (RuntimeState::Blocked { .. }, TransitionEvent::Cancel { reason }) => {
            Ok(RuntimeState::Terminal(TerminalState::Canceled {
                reason: reason.clone(),
            }))
        },

        // ── Active ─────────────────────────────────────────────────────────

        // Active → Terminal(Completed)
        (RuntimeState::Active { .. }, TransitionEvent::Complete) => {
            Ok(RuntimeState::Terminal(TerminalState::Completed))
        },

        // Active → Retrying (recoverable failure, budget not exhausted)
        (
            RuntimeState::Active { .. },
            TransitionEvent::Fail {
                class,
                handoff,
                retry_after,
            },
        ) => {
            let attempt = task.runtime.attempt;
            let max = task.spec.max_attempts;

            // Human-required failures escalate regardless of budget.
            if class.requires_human() {
                return Ok(RuntimeState::AwaitingHuman {
                    question: format!("Task failed: {}. Human input required.", class),
                    handoff: handoff.clone(),
                });
            }

            // Budget exhausted or non-retryable → terminal failure.
            if attempt >= max || !class.is_retryable() {
                let final_class = if attempt >= max {
                    FailureClass::MaxAttemptsExceeded
                } else {
                    class.clone()
                };
                return Ok(RuntimeState::Terminal(TerminalState::Failed {
                    class: final_class,
                }));
            }

            let effective_retry_after = retry_after
                .unwrap_or_else(|| OffsetDateTime::now_utc() + time::Duration::seconds(5));

            Ok(RuntimeState::Retrying {
                reason: class.clone(),
                retry_after: effective_retry_after,
                handoff: handoff.clone(),
            })
        },

        // Active → AwaitingHuman (explicit escalation)
        (RuntimeState::Active { .. }, TransitionEvent::Escalate { question, handoff }) => {
            Ok(RuntimeState::AwaitingHuman {
                question: question.clone(),
                handoff: handoff.clone(),
            })
        },

        // Active → Terminal(Canceled)
        (RuntimeState::Active { .. }, TransitionEvent::Cancel { reason }) => {
            Ok(RuntimeState::Terminal(TerminalState::Canceled {
                reason: reason.clone(),
            }))
        },

        // ── Retrying ───────────────────────────────────────────────────────

        // Retrying → Pending (promoted by recovery cron)
        (RuntimeState::Retrying { .. }, TransitionEvent::PromoteRetry) => Ok(RuntimeState::Pending),

        // Retrying → Terminal(Canceled)
        (RuntimeState::Retrying { .. }, TransitionEvent::Cancel { reason }) => {
            Ok(RuntimeState::Terminal(TerminalState::Canceled {
                reason: reason.clone(),
            }))
        },

        // ── AwaitingHuman ──────────────────────────────────────────────────

        // AwaitingHuman → Pending (human provided resolution → ready to retry)
        (RuntimeState::AwaitingHuman { .. }, TransitionEvent::HumanResolve { .. }) => {
            Ok(RuntimeState::Pending)
        },

        // AwaitingHuman → Terminal(Canceled)
        (RuntimeState::AwaitingHuman { .. }, TransitionEvent::Cancel { reason }) => {
            Ok(RuntimeState::Terminal(TerminalState::Canceled {
                reason: reason.clone(),
            }))
        },

        // ── Terminal ───────────────────────────────────────────────────────

        // No transitions out of Terminal.
        (RuntimeState::Terminal(_), _) => Err(TransitionError::invalid(&task.runtime.state, event)),

        // ── Catch-all invalid transitions ──────────────────────────────────
        _ => Err(TransitionError::invalid(&task.runtime.state, event)),
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TaskSpec;

    fn pending_task() -> Task {
        Task::new("default", TaskSpec::new("work", ""))
    }

    fn active_task(owner: &str) -> Task {
        let t = pending_task();
        apply(
            t,
            &TransitionEvent::Claim {
                owner: owner.to_string(),
                lease_duration_secs: None,
            },
        )
        .expect("claim")
    }

    // ── Pending transitions ───────────────────────────────────────────────

    #[test]
    fn pending_claim_goes_active() {
        let task = pending_task();
        let result = apply(
            task,
            &TransitionEvent::Claim {
                owner: "agent-1".into(),
                lease_duration_secs: None,
            },
        )
        .expect("claim should succeed");
        assert!(result.runtime.state.is_active());
        assert_eq!(result.runtime.owner.as_deref(), Some("agent-1"));
        assert_eq!(result.runtime.attempt, 1);
        assert_eq!(result.runtime.version, 1);
    }

    #[test]
    fn pending_block_goes_blocked() {
        let task = pending_task();
        let dep = TaskId::from("dep-1");
        let result = apply(
            task,
            &TransitionEvent::Block {
                waiting_on: vec![dep.clone()],
            },
        )
        .expect("block should succeed");
        assert!(matches!(
            result.runtime.state,
            RuntimeState::Blocked { waiting_on } if waiting_on == vec![dep]
        ));
    }

    #[test]
    fn pending_cancel() {
        let task = pending_task();
        let result = apply(
            task,
            &TransitionEvent::Cancel {
                reason: "not needed".into(),
            },
        )
        .expect("cancel should succeed");
        assert!(matches!(
            result.runtime.state,
            RuntimeState::Terminal(TerminalState::Canceled { reason }) if reason == "not needed"
        ));
    }

    // ── Blocked transitions ───────────────────────────────────────────────

    #[test]
    fn blocked_deps_met_goes_pending() {
        let t = pending_task();
        let blocked = apply(
            t,
            &TransitionEvent::Block {
                waiting_on: vec![TaskId::from("x")],
            },
        )
        .expect("block");
        let pending = apply(blocked, &TransitionEvent::DependenciesMet).expect("deps met");
        assert_eq!(pending.runtime.state, RuntimeState::Pending);
    }

    // ── Active transitions ────────────────────────────────────────────────

    #[test]
    fn active_complete_goes_terminal_completed() {
        let task = active_task("agent");
        let result = apply(task, &TransitionEvent::Complete).expect("complete");
        assert!(matches!(
            result.runtime.state,
            RuntimeState::Terminal(TerminalState::Completed)
        ));
        assert!(result.runtime.owner.is_none());
    }

    #[test]
    fn active_fail_retryable_goes_retrying() {
        let task = active_task("agent");
        let result = apply(
            task,
            &TransitionEvent::Fail {
                class: FailureClass::ProviderTransient,
                handoff: HandoffContext::default(),
                retry_after: None,
            },
        )
        .expect("fail retryable");
        assert!(matches!(
            result.runtime.state,
            RuntimeState::Retrying { .. }
        ));
        assert_eq!(
            result.runtime.last_failure,
            Some(FailureClass::ProviderTransient)
        );
    }

    #[test]
    fn active_fail_exhausts_budget_goes_terminal_failed() {
        let mut task = active_task("agent");
        task.runtime.attempt = 3; // already at max
        task.spec.max_attempts = 3;

        let result = apply(
            task,
            &TransitionEvent::Fail {
                class: FailureClass::AgentError,
                handoff: HandoffContext::default(),
                retry_after: None,
            },
        )
        .expect("fail should succeed");
        assert!(matches!(
            result.runtime.state,
            RuntimeState::Terminal(TerminalState::Failed {
                class: FailureClass::MaxAttemptsExceeded
            })
        ));
    }

    #[test]
    fn active_fail_human_required_goes_awaiting_human() {
        let task = active_task("agent");
        let result = apply(
            task,
            &TransitionEvent::Fail {
                class: FailureClass::ProviderPermanent,
                handoff: HandoffContext::default(),
                retry_after: None,
            },
        )
        .expect("fail human-required");
        // ProviderPermanent.requires_human() → AwaitingHuman (not terminal)
        assert!(matches!(
            result.runtime.state,
            RuntimeState::AwaitingHuman { .. }
        ));
    }

    #[test]
    fn active_escalate_goes_awaiting_human() {
        let task = active_task("agent");
        let result = apply(
            task,
            &TransitionEvent::Escalate {
                question: "which env?".into(),
                handoff: HandoffContext::default(),
            },
        )
        .expect("escalate");
        assert!(matches!(
            result.runtime.state,
            RuntimeState::AwaitingHuman { .. }
        ));
    }

    // ── Retrying transitions ───────────────────────────────────────────────

    #[test]
    fn retrying_promote_goes_pending() {
        let task = active_task("agent");
        let retrying = apply(
            task,
            &TransitionEvent::Fail {
                class: FailureClass::AgentError,
                handoff: HandoffContext::default(),
                retry_after: None,
            },
        )
        .expect("fail");
        let promoted = apply(retrying, &TransitionEvent::PromoteRetry).expect("promote");
        assert_eq!(promoted.runtime.state, RuntimeState::Pending);
    }

    // ── AwaitingHuman transitions ─────────────────────────────────────────

    #[test]
    fn awaiting_human_resolve_goes_pending() {
        let task = active_task("agent");
        let awaiting = apply(
            task,
            &TransitionEvent::Escalate {
                question: "question".into(),
                handoff: HandoffContext::default(),
            },
        )
        .expect("escalate");
        let resolved = apply(
            awaiting,
            &TransitionEvent::HumanResolve {
                resolution: "use prod".into(),
            },
        )
        .expect("resolve");
        assert_eq!(resolved.runtime.state, RuntimeState::Pending);
    }

    // ── Terminal is final ─────────────────────────────────────────────────

    #[test]
    fn terminal_rejects_all_transitions() {
        let task = active_task("agent");
        let done = apply(task, &TransitionEvent::Complete).expect("complete");
        assert!(done.runtime.state.is_terminal());

        let err = apply(done.clone(), &TransitionEvent::Complete).unwrap_err();
        assert!(matches!(err, TransitionError::InvalidTransition { .. }));

        let err2 = apply(
            done,
            &TransitionEvent::Cancel {
                reason: "late".into(),
            },
        )
        .unwrap_err();
        assert!(matches!(err2, TransitionError::InvalidTransition { .. }));
    }

    // ── Version increments ────────────────────────────────────────────────

    #[test]
    fn version_increments_each_transition() {
        let t0 = pending_task();
        assert_eq!(t0.runtime.version, 0);

        let t1 = apply(
            t0,
            &TransitionEvent::Claim {
                owner: "a".into(),
                lease_duration_secs: None,
            },
        )
        .expect("claim");
        assert_eq!(t1.runtime.version, 1);

        let t2 = apply(t1, &TransitionEvent::Complete).expect("complete");
        assert_eq!(t2.runtime.version, 2);
    }

    // ── Invalid transitions ───────────────────────────────────────────────

    #[test]
    fn invalid_transitions_return_error() {
        // Cannot complete a Pending task (must claim first)
        let task = pending_task();
        assert!(matches!(
            apply(task, &TransitionEvent::Complete),
            Err(TransitionError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn claim_with_lease_sets_expiry() {
        let task = pending_task();
        let claimed = apply(
            task,
            &TransitionEvent::Claim {
                owner: "agent".into(),
                lease_duration_secs: Some(300),
            },
        )
        .expect("claim");
        if let RuntimeState::Active {
            lease_expires_at: Some(exp),
            ..
        } = claimed.runtime.state
        {
            let now = OffsetDateTime::now_utc();
            assert!(exp > now);
            assert!(exp < now + time::Duration::seconds(400));
        } else {
            panic!("expected Active state with lease");
        }
    }
}
