//! Recovery manager: maps failure class + attempt count to a recovery phase.
//!
//! The recovery cron job polls `Retrying` tasks whose `retry_after` timestamp
//! has passed and promotes them back to `Pending` via `PromoteRetry`.

use time::OffsetDateTime;

use crate::types::FailureClass;

/// The next recovery action for a failed task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryPhase {
    /// Retry immediately (or after a very short delay).
    ImmediateRetry,
    /// Retry after a calculated backoff.
    DeferredRetry { retry_after: OffsetDateTime },
    /// A human must intervene before the task can retry.
    HumanEscalation,
    /// No further automatic attempts — task should be marked terminal.
    TerminalFailure,
}

/// Determine the recovery phase for a failed task.
///
/// `attempt` is the attempt that just failed (1-indexed after the first Claim),
/// `max_attempts` is the configured budget from `TaskSpec`.
#[must_use]
pub fn classify_recovery(
    class: &FailureClass,
    attempt: u8,
    max_attempts: u8,
) -> RecoveryPhase {
    // Budget exhausted or permanently non-retryable.
    if attempt >= max_attempts {
        return RecoveryPhase::TerminalFailure;
    }

    if class.requires_human() {
        return RecoveryPhase::HumanEscalation;
    }

    if !class.is_retryable() {
        return RecoveryPhase::TerminalFailure;
    }

    // Exponential backoff: 5s, 20s, 60s, 180s, 300s cap.
    let backoff_secs = backoff_seconds(attempt);
    let retry_after = OffsetDateTime::now_utc() + time::Duration::seconds(backoff_secs);

    if backoff_secs <= 10 {
        RecoveryPhase::ImmediateRetry
    } else {
        RecoveryPhase::DeferredRetry { retry_after }
    }
}

/// Compute backoff in seconds for attempt `n` (1-indexed).
///
/// Returns:
/// - attempt 1: 5s  (immediate)
/// - attempt 2: 20s
/// - attempt 3: 60s
/// - attempt 4: 180s
/// - attempt 5+: 300s (capped)
fn backoff_seconds(attempt: u8) -> i64 {
    match attempt {
        0 | 1 => 5,
        2 => 20,
        3 => 60,
        4 => 180,
        _ => 300,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_exhausted_is_terminal() {
        assert_eq!(
            classify_recovery(&FailureClass::AgentError, 3, 3),
            RecoveryPhase::TerminalFailure
        );
    }

    #[test]
    fn permanent_failure_is_terminal() {
        // ProviderPermanent.requires_human() == true → HumanEscalation
        assert_eq!(
            classify_recovery(&FailureClass::ProviderPermanent, 1, 3),
            RecoveryPhase::HumanEscalation
        );
    }

    #[test]
    fn human_blocker_is_escalation() {
        assert_eq!(
            classify_recovery(&FailureClass::HumanBlocker, 1, 3),
            RecoveryPhase::HumanEscalation
        );
    }

    #[test]
    fn first_attempt_is_immediate() {
        assert_eq!(
            classify_recovery(&FailureClass::AgentError, 1, 3),
            RecoveryPhase::ImmediateRetry
        );
    }

    #[test]
    fn second_attempt_is_deferred() {
        let phase = classify_recovery(&FailureClass::ProviderTransient, 2, 5);
        assert!(matches!(phase, RecoveryPhase::DeferredRetry { .. }));
    }

    #[test]
    fn context_overflow_is_retryable_then_exhausted() {
        // First two attempts: retryable
        assert_ne!(
            classify_recovery(&FailureClass::ContextOverflow, 1, 3),
            RecoveryPhase::TerminalFailure
        );
        // Last attempt exhausted
        assert_eq!(
            classify_recovery(&FailureClass::ContextOverflow, 3, 3),
            RecoveryPhase::TerminalFailure
        );
    }
}
