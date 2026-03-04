use moltis_config::ChatConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextCompactionStrategy {
    Truncate,
    MoveToWorkspace,
}

impl ContextCompactionStrategy {
    pub(crate) fn as_config_value(self) -> &'static str {
        match self {
            Self::Truncate => "truncate",
            Self::MoveToWorkspace => "move_to_workspace",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContextCompactionConfig {
    pub(crate) strategy: ContextCompactionStrategy,
    pub(crate) keep_recent: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextCompactionAction {
    None,
    /// Early memory flush at ~70% occupancy — no summarization, only persists
    /// important memories before they might be lost to compaction.
    PreCompact,
    Compact,
    ArchiveTier,
    TruncateTier,
}

#[must_use]
pub(crate) fn context_compaction_config_from_chat(chat: &ChatConfig) -> ContextCompactionConfig {
    let strategy = match chat
        .context_compaction_strategy
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "move_to_workspace" => ContextCompactionStrategy::MoveToWorkspace,
        _ => ContextCompactionStrategy::Truncate,
    };
    ContextCompactionConfig {
        strategy,
        keep_recent: chat.context_compaction_keep_recent.max(1),
    }
}

#[must_use]
pub(crate) fn context_compaction_action_for_usage(
    estimated_next_input: u64,
    context_window: u64,
) -> ContextCompactionAction {
    let pre_compact_threshold = (context_window * 70) / 100;
    let compact_threshold = (context_window * 80) / 100;
    let archive_threshold = (context_window * 90) / 100;
    let truncate_threshold = (context_window * 95) / 100;
    if estimated_next_input >= truncate_threshold {
        ContextCompactionAction::TruncateTier
    } else if estimated_next_input >= archive_threshold {
        ContextCompactionAction::ArchiveTier
    } else if estimated_next_input >= compact_threshold {
        ContextCompactionAction::Compact
    } else if estimated_next_input >= pre_compact_threshold {
        ContextCompactionAction::PreCompact
    } else {
        ContextCompactionAction::None
    }
}

#[must_use]
pub(crate) fn archive_keep_recent_for_reduction(
    history_len: usize,
    configured_keep_recent: usize,
) -> usize {
    if history_len <= 1 {
        return history_len;
    }
    configured_keep_recent.max(1).min(history_len - 1)
}
