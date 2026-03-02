//! `moltis-tasks` — hypervisor-grade task orchestration for multi-agent coordination.
//!
//! ## Architecture
//!
//! Tasks have a **spec** (immutable intent) and a **runtime** (mutable execution
//! state). Transitions between states are mediated by [`transitions::apply`],
//! which enforces the formal state machine and increments an optimistic
//! concurrency version counter on every change.
//!
//! Persistence is provided by [`store::TaskStore`] (SQLite, CAS writes) and
//! [`event_log::EventLog`] (append-only audit trail per task).
//!
//! Recovery from agent failures is driven by [`recovery::classify_recovery`],
//! which maps a [`types::FailureClass`] and attempt counter to a
//! [`recovery::RecoveryPhase`].

pub mod errors;
pub mod event_log;
pub mod guards;
pub mod recovery;
pub mod state;
pub mod store;
pub mod transitions;
pub mod types;

// Convenient top-level re-exports.
pub use errors::TransitionError;
pub use event_log::EventLog;
pub use recovery::{RecoveryPhase, classify_recovery};
pub use state::{RuntimeState, TerminalState};
pub use store::TaskStore;
pub use transitions::{TransitionEvent, apply};
pub use types::{FailureClass, HandoffContext, Task, TaskId, TaskRuntime, TaskSpec};
