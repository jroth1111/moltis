//! Typed error for task state-machine violations.

use crate::state::RuntimeState;
use crate::transitions::TransitionEvent;

/// An invalid state-machine transition.
#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("cannot apply {event:?} to a task in state {state}")]
    InvalidTransition {
        state: &'static str,
        event: Box<TransitionEvent>,
    },

    #[error("optimistic concurrency conflict: expected version {expected}, found {actual}")]
    VersionConflict { expected: u64, actual: u64 },

    #[error("task not found: {0}")]
    NotFound(String),

    #[error("storage error: {0}")]
    Storage(#[from] sqlx::Error),

    #[error("{0}")]
    Other(String),
}

impl TransitionError {
    pub(crate) fn invalid(state: &RuntimeState, event: &TransitionEvent) -> Self {
        Self::InvalidTransition {
            state: state.name(),
            event: Box::new(event.clone()),
        }
    }
}
