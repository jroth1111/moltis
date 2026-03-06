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
    /// Maintenance / scheduled background tasks (lowest tier).
    pub const MAINTENANCE: i32 = -100;
}

/// Typed priority class for messages entering the queue.
///
/// Maps to `_priority` integer values used by the chat queue:
/// - `Interactive`  → 0   (user-initiated chat)
/// - `Background`   → -50 (inter-session agent sends)
/// - `Maintenance`  → -100 (cron / scheduled jobs)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessagePriority {
    /// Direct user interaction — highest tier.
    Interactive,
    /// Background inter-session message from another agent.
    Background,
    /// Scheduled/cron maintenance task — lowest tier.
    Maintenance,
}

impl MessagePriority {
    /// Convert to the integer `_priority` value recognised by the chat queue.
    #[must_use]
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Interactive => priority::NORMAL,
            Self::Background => priority::LOW,
            Self::Maintenance => priority::MAINTENANCE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_priority_as_i32_values() {
        assert_eq!(MessagePriority::Interactive.as_i32(), 0);
        assert_eq!(MessagePriority::Background.as_i32(), -50);
        assert_eq!(MessagePriority::Maintenance.as_i32(), -100);
    }

    #[test]
    fn priority_ordering() {
        let interactive = MessagePriority::Interactive.as_i32();
        let background = MessagePriority::Background.as_i32();
        let maintenance = MessagePriority::Maintenance.as_i32();
        assert!(
            interactive > background,
            "Interactive must outrank Background"
        );
        assert!(
            background > maintenance,
            "Background must outrank Maintenance"
        );
    }
}
