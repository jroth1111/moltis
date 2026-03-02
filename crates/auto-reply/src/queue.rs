/// Per-session followup queue for inbound messages.
///
/// Modes: per-message, batch, debounce.
/// Drop policies: oldest, newest, none.

#[derive(Debug, Clone)]
pub enum QueueMode {
    /// Each inbound message triggers a separate agent run.
    PerMessage,
    /// Accumulate multiple inbound messages into a single agent run.
    Batch,
    /// Wait for an idle period before invoking the agent.
    Debounce { idle_ms: u64 },
}

#[derive(Debug, Clone)]
pub enum DropPolicy {
    Oldest,
    Newest,
    None,
}

/// Default message priority (normal).
pub const DEFAULT_PRIORITY: i32 = 0;

/// Well-known priority levels.
pub mod priority {
    /// System / control-plane messages.
    pub const SYSTEM: i32 = 100;
    /// Urgent user-initiated messages.
    pub const HIGH: i32 = 50;
    /// Normal messages.
    pub const NORMAL: i32 = 0;
    /// Low-priority background / batch messages.
    pub const LOW: i32 = -50;
}
