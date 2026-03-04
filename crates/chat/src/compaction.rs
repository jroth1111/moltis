use std::collections::{BTreeSet, HashMap};

use moltis_config::ChatConfig;
use serde_json::Value;

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContextCompactionConfig {
    pub(crate) enabled: bool,
    pub(crate) soft_trigger_percent: u8,
    pub(crate) hard_trigger_percent: u8,
    pub(crate) emergency_trigger_percent: u8,
    pub(crate) verbatim_turns: usize,
    pub(crate) min_verbatim_turns: usize,
    pub(crate) anchor_budget_tokens: u64,
    pub(crate) summary_budget_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextCompactionAction {
    None,
    /// Early memory flush at ~70% occupancy — no summarization, only persists
    /// important memories before they might be lost to compaction.
    PreCompact,
    SoftCompact,
    HardCompact,
    EmergencyCompact,
}

#[must_use]
pub(crate) fn context_compaction_config_from_chat(chat: &ChatConfig) -> ContextCompactionConfig {
    let compaction = &chat.compaction;
    let soft_trigger_percent = compaction.soft_trigger_percent.clamp(1, 98);
    let hard_trigger_percent = compaction
        .hard_trigger_percent
        .clamp(soft_trigger_percent.saturating_add(1), 99);
    let emergency_trigger_percent = compaction
        .emergency_trigger_percent
        .max(hard_trigger_percent.saturating_add(1))
        .min(100);
    let min_verbatim_turns = compaction.min_verbatim_turns.max(1);
    let verbatim_turns = compaction.verbatim_turns.max(min_verbatim_turns);

    ContextCompactionConfig {
        enabled: compaction.enabled,
        soft_trigger_percent,
        hard_trigger_percent,
        emergency_trigger_percent,
        verbatim_turns,
        min_verbatim_turns,
        anchor_budget_tokens: compaction.anchor_budget_tokens.max(1),
        summary_budget_tokens: compaction.summary_budget_tokens.max(64),
    }
}

#[must_use]
pub(crate) fn context_compaction_action_for_usage(
    estimated_next_input: u64,
    context_window: u64,
    config: ContextCompactionConfig,
) -> ContextCompactionAction {
    if !config.enabled || context_window == 0 {
        return ContextCompactionAction::None;
    }

    let emergency_threshold = usage_threshold(context_window, config.emergency_trigger_percent);
    let hard_threshold = usage_threshold(context_window, config.hard_trigger_percent);
    let soft_threshold = usage_threshold(context_window, config.soft_trigger_percent);
    let pre_compact_percent = config.soft_trigger_percent.saturating_sub(10).max(1);
    let pre_compact_threshold = usage_threshold(context_window, pre_compact_percent);

    if estimated_next_input >= emergency_threshold {
        ContextCompactionAction::EmergencyCompact
    } else if estimated_next_input >= hard_threshold {
        ContextCompactionAction::HardCompact
    } else if estimated_next_input >= soft_threshold {
        ContextCompactionAction::SoftCompact
    } else if estimated_next_input >= pre_compact_threshold {
        ContextCompactionAction::PreCompact
    } else {
        ContextCompactionAction::None
    }
}

#[must_use]
fn usage_threshold(context_window: u64, percent: u8) -> u64 {
    context_window.saturating_mul(percent as u64) / 100
}

#[must_use]
pub(crate) fn keep_recent_for_reduction(
    history_len: usize,
    configured_keep_recent: usize,
) -> usize {
    if history_len <= 1 {
        return history_len;
    }
    configured_keep_recent.max(1).min(history_len - 1)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Importance {
    Low,
    Normal,
    High,
    Critical,
}

impl Importance {
    fn rank(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::High => 2,
            Self::Critical => 3,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SummaryContextPlan {
    pub(crate) verbatim_turns: usize,
    pub(crate) anchor_budget_tokens: u64,
    pub(crate) summary_budget_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReductionPlan {
    pub(crate) keep_recent: usize,
    pub(crate) min_recent_to_keep: usize,
    pub(crate) anchor_budget_tokens: u64,
    pub(crate) max_anchor_messages: usize,
    pub(crate) target_total_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub(crate) struct SummaryCompactionSections {
    pub(crate) summary_source: Vec<Value>,
    pub(crate) anchors: Vec<Value>,
    pub(crate) recent: Vec<Value>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LayerStats {
    pub(crate) critical_anchor_messages: usize,
    pub(crate) working_messages: usize,
    pub(crate) background_messages: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ImportanceRetentionResult {
    pub(crate) messages: Vec<Value>,
    pub(crate) removed_messages: usize,
    pub(crate) retained_anchor_messages: usize,
    pub(crate) kept_recent_messages: usize,
}

const SUMMARY_MIN_RECENT_TURNS: usize = 4;
const REDUCTION_MAX_ANCHOR_MESSAGES: usize = 48;
const EMERGENCY_MAX_ANCHOR_MESSAGES: usize = 24;

#[must_use]
pub(crate) fn summary_context_plan(config: ContextCompactionConfig) -> SummaryContextPlan {
    let verbatim_turns = config.verbatim_turns.max(SUMMARY_MIN_RECENT_TURNS);
    SummaryContextPlan {
        verbatim_turns,
        anchor_budget_tokens: config.anchor_budget_tokens,
        summary_budget_tokens: config.summary_budget_tokens,
    }
}

#[must_use]
pub(crate) fn hard_reduction_plan(config: ContextCompactionConfig) -> ReductionPlan {
    ReductionPlan {
        keep_recent: config.min_verbatim_turns,
        min_recent_to_keep: config.min_verbatim_turns,
        anchor_budget_tokens: config.anchor_budget_tokens,
        max_anchor_messages: REDUCTION_MAX_ANCHOR_MESSAGES,
        target_total_tokens: None,
    }
}

#[must_use]
pub(crate) fn emergency_reduction_plan(
    config: ContextCompactionConfig,
    context_window: u64,
) -> ReductionPlan {
    ReductionPlan {
        keep_recent: config.min_verbatim_turns,
        min_recent_to_keep: config.min_verbatim_turns,
        anchor_budget_tokens: config.anchor_budget_tokens,
        max_anchor_messages: EMERGENCY_MAX_ANCHOR_MESSAGES,
        target_total_tokens: Some(usage_threshold(context_window, config.hard_trigger_percent)),
    }
}

#[derive(Debug, Clone)]
struct AnchorGroup {
    indices: Vec<usize>,
    importance: Importance,
    newest_index: usize,
}

#[must_use]
pub(crate) fn partition_history_for_summary(
    history: &[Value],
    plan: SummaryContextPlan,
) -> SummaryCompactionSections {
    if history.is_empty() {
        return SummaryCompactionSections {
            summary_source: Vec::new(),
            anchors: Vec::new(),
            recent: Vec::new(),
        };
    }

    let keep_recent = if history.len() <= 1 {
        history.len()
    } else {
        plan.verbatim_turns.max(1).min(history.len() - 1)
    };
    let split_at = history.len().saturating_sub(keep_recent);
    let (older, recent) = history.split_at(split_at);

    let selected_anchor_indices = select_anchor_indices(
        older,
        plan.anchor_budget_tokens,
        REDUCTION_MAX_ANCHOR_MESSAGES,
    );

    let mut summary_source = Vec::with_capacity(older.len());
    let mut anchors = Vec::new();
    for (idx, msg) in older.iter().enumerate() {
        if selected_anchor_indices.contains(&idx) {
            anchors.push(msg.clone());
        } else {
            summary_source.push(msg.clone());
        }
    }

    SummaryCompactionSections {
        summary_source,
        anchors,
        recent: recent.to_vec(),
    }
}

#[must_use]
pub(crate) fn layer_stats_for_history(history: &[Value], plan: SummaryContextPlan) -> LayerStats {
    if history.is_empty() {
        return LayerStats {
            critical_anchor_messages: 0,
            working_messages: 0,
            background_messages: 0,
        };
    }

    let keep_recent = keep_recent_for_reduction(history.len(), plan.verbatim_turns);
    let split_at = history.len().saturating_sub(keep_recent);
    let (older, recent) = history.split_at(split_at);
    let selected_anchor_indices = select_anchor_indices(
        older,
        plan.anchor_budget_tokens,
        REDUCTION_MAX_ANCHOR_MESSAGES,
    );
    let critical_anchor_messages = selected_anchor_indices.len();
    let background_messages = older.len().saturating_sub(critical_anchor_messages);

    LayerStats {
        critical_anchor_messages,
        working_messages: recent.len(),
        background_messages,
    }
}

#[must_use]
pub(crate) fn retain_with_importance(
    history: &[Value],
    plan: ReductionPlan,
) -> ImportanceRetentionResult {
    if history.is_empty() {
        return ImportanceRetentionResult {
            messages: Vec::new(),
            removed_messages: 0,
            retained_anchor_messages: 0,
            kept_recent_messages: 0,
        };
    }

    let keep_recent = keep_recent_for_reduction(history.len(), plan.keep_recent);
    let split_at = history.len().saturating_sub(keep_recent);
    let (older, recent) = history.split_at(split_at);

    let selected_anchor_indices = select_anchor_indices(
        older,
        plan.anchor_budget_tokens,
        plan.max_anchor_messages.max(1),
    );
    let anchors: Vec<Value> = older
        .iter()
        .enumerate()
        .filter(|(idx, _)| selected_anchor_indices.contains(idx))
        .map(|(_, msg)| msg.clone())
        .collect();
    let mut merged: Vec<(Value, bool)> = Vec::with_capacity(anchors.len() + recent.len());
    merged.extend(anchors.iter().cloned().map(|msg| (msg, true)));
    merged.extend(recent.iter().cloned().map(|msg| (msg, false)));

    if let Some(target_total_tokens) = plan.target_total_tokens {
        drop_low_importance_non_anchors_first(
            &mut merged,
            target_total_tokens,
            plan.min_recent_to_keep,
        );
    }

    let retained_anchor_messages = merged.iter().filter(|(_, is_anchor)| *is_anchor).count();
    let kept_recent_messages = merged.len().saturating_sub(retained_anchor_messages);
    let messages: Vec<Value> = merged.into_iter().map(|(msg, _)| msg).collect();
    let removed_messages = history.len().saturating_sub(messages.len());
    ImportanceRetentionResult {
        messages,
        removed_messages,
        retained_anchor_messages,
        kept_recent_messages,
    }
}

fn drop_low_importance_non_anchors_first(
    merged: &mut Vec<(Value, bool)>,
    target_total_tokens: u64,
    min_recent_to_keep: usize,
) {
    let mut total_tokens: u64 = merged
        .iter()
        .map(|(msg, _)| estimate_message_tokens(msg))
        .sum();
    if total_tokens <= target_total_tokens {
        return;
    }

    let mut candidates: Vec<(usize, Importance, u64)> = merged
        .iter()
        .enumerate()
        .filter(|(_, (_, is_anchor))| !*is_anchor)
        .map(|(idx, (msg, _))| (idx, classify_importance(msg), estimate_message_tokens(msg)))
        .collect();
    candidates.sort_by(|left, right| {
        left.1
            .rank()
            .cmp(&right.1.rank())
            .then_with(|| left.0.cmp(&right.0))
    });

    let mut drop_indices = BTreeSet::new();
    let mut remaining_recent = merged.iter().filter(|(_, is_anchor)| !*is_anchor).count();
    for (idx, _, tokens) in candidates {
        if total_tokens <= target_total_tokens {
            break;
        }
        if remaining_recent <= min_recent_to_keep {
            break;
        }
        drop_indices.insert(idx);
        total_tokens = total_tokens.saturating_sub(tokens);
        remaining_recent = remaining_recent.saturating_sub(1);
    }

    if drop_indices.is_empty() {
        return;
    }

    *merged = merged
        .drain(..)
        .enumerate()
        .filter(|(idx, _)| !drop_indices.contains(idx))
        .map(|(_, item)| item)
        .collect();
}

#[must_use]
pub(crate) fn parse_compaction_facts(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let candidate = trimmed
        .strip_prefix("```json")
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .or_else(|| {
            trimmed
                .strip_prefix("```")
                .and_then(|s| s.strip_suffix("```"))
        })
        .map(str::trim)
        .unwrap_or(trimmed);

    serde_json::from_str::<Vec<String>>(candidate)
        .unwrap_or_default()
        .into_iter()
        .map(|fact| fact.trim().to_string())
        .filter(|fact| !fact.is_empty())
        .collect()
}

#[must_use]
pub(crate) fn trim_summary_to_budget(summary: &str, summary_budget_tokens: u64) -> String {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if estimate_text_tokens(trimmed) <= summary_budget_tokens {
        return trimmed.to_string();
    }

    let max_chars = summary_budget_tokens.saturating_mul(4).max(64) as usize;
    truncate_chars(trimmed, max_chars)
}

#[must_use]
pub(crate) fn classify_importance(message: &Value) -> Importance {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let text = message_text(message);
    let lower = text.to_ascii_lowercase();

    if has_tool_calls(message) || matches!(role, "tool" | "tool_result") {
        return Importance::Critical;
    }

    if has_decision_markers(&lower) || has_constraint_markers(&lower) {
        return Importance::Critical;
    }

    if text.contains("```") || has_error_or_command_markers(&lower) {
        return Importance::High;
    }

    if looks_like_acknowledgement(&lower) || matches!(role, "notice") {
        return Importance::Low;
    }

    Importance::Normal
}

fn select_anchor_indices(
    history: &[Value],
    anchor_budget_tokens: u64,
    max_anchor_messages: usize,
) -> BTreeSet<usize> {
    if history.is_empty() || anchor_budget_tokens == 0 || max_anchor_messages == 0 {
        return BTreeSet::new();
    }

    let mut groups = collect_anchor_groups(history);
    groups.sort_by(|left, right| {
        right
            .importance
            .rank()
            .cmp(&left.importance.rank())
            .then_with(|| right.newest_index.cmp(&left.newest_index))
            .then_with(|| left.indices.len().cmp(&right.indices.len()))
    });

    let mut selected = BTreeSet::new();
    let mut used_budget = 0_u64;
    for group in groups {
        let new_indices: Vec<usize> = group
            .indices
            .into_iter()
            .filter(|idx| !selected.contains(idx))
            .collect();
        if new_indices.is_empty() {
            continue;
        }
        let new_tokens: u64 = new_indices
            .iter()
            .map(|idx| estimate_message_tokens(&history[*idx]))
            .sum();
        let fits_budget = used_budget.saturating_add(new_tokens) <= anchor_budget_tokens;
        let fits_count = selected.len().saturating_add(new_indices.len()) <= max_anchor_messages;
        if !fits_budget || !fits_count {
            continue;
        }

        for idx in new_indices {
            selected.insert(idx);
        }
        used_budget = used_budget.saturating_add(new_tokens);
    }

    selected
}

fn collect_anchor_groups(history: &[Value]) -> Vec<AnchorGroup> {
    let mut groups = collect_tool_chain_groups(history);
    let tool_chain_indices: BTreeSet<usize> = groups
        .iter()
        .flat_map(|group| group.indices.iter().copied())
        .collect();

    for (idx, message) in history.iter().enumerate() {
        if tool_chain_indices.contains(&idx) {
            continue;
        }
        let importance = classify_importance(message);
        if importance < Importance::High {
            continue;
        }

        let mut indices = vec![idx];
        if message_text(message).contains("```") {
            if idx > 0 {
                indices.push(idx - 1);
            }
            if idx + 1 < history.len() {
                indices.push(idx + 1);
            }
        }
        indices.sort_unstable();
        indices.dedup();

        groups.push(AnchorGroup {
            newest_index: *indices.last().unwrap_or(&idx),
            indices,
            importance,
        });
    }

    groups
}

fn collect_tool_chain_groups(history: &[Value]) -> Vec<AnchorGroup> {
    let mut assistant_by_tool_call: HashMap<String, usize> = HashMap::new();

    for (idx, message) in history.iter().enumerate() {
        if !has_tool_calls(message) {
            continue;
        }
        let ids = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| call.get("id").and_then(Value::as_str))
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            continue;
        }
        for id in &ids {
            assistant_by_tool_call.insert(id.clone(), idx);
        }
    }

    let mut results_by_tool_call: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, message) in history.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(role, "tool" | "tool_result") {
            continue;
        }
        let Some(tool_call_id) = message.get("tool_call_id").and_then(Value::as_str) else {
            continue;
        };
        results_by_tool_call
            .entry(tool_call_id.to_string())
            .or_default()
            .push(idx);
    }

    assistant_by_tool_call
        .into_iter()
        .map(|(tool_call_id, assistant_idx)| {
            let mut indices = vec![assistant_idx];
            if let Some(result_indices) = results_by_tool_call.remove(&tool_call_id) {
                indices.extend(result_indices);
            }
            indices.sort_unstable();
            indices.dedup();
            AnchorGroup {
                newest_index: *indices.last().unwrap_or(&assistant_idx),
                indices,
                importance: Importance::Critical,
            }
        })
        .collect()
}

fn has_tool_calls(message: &Value) -> bool {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty())
}

fn has_decision_markers(lower: &str) -> bool {
    const DECISION_MARKERS: &[&str] = &[
        "decision",
        "decided",
        "agreed",
        "next step",
        "action item",
        "deadline",
        "ship",
        "deploy",
        "commit to",
    ];
    DECISION_MARKERS.iter().any(|marker| lower.contains(marker))
}

fn has_constraint_markers(lower: &str) -> bool {
    const CONSTRAINT_MARKERS: &[&str] = &[
        "hard constraint",
        "must",
        "must not",
        "do not",
        "never",
        "required",
        "cannot",
    ];
    CONSTRAINT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

fn has_error_or_command_markers(lower: &str) -> bool {
    const ERROR_MARKERS: &[&str] = &[
        "error:",
        "failed",
        "stderr",
        "traceback",
        "exception",
        "exit code",
        "command output",
        "stdout",
    ];
    ERROR_MARKERS.iter().any(|marker| lower.contains(marker))
}

fn looks_like_acknowledgement(lower: &str) -> bool {
    let normalized = lower
        .trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if normalized.len() > 40 {
        return false;
    }
    const ACKS: &[&str] = &[
        "ok",
        "okay",
        "thanks",
        "thank you",
        "sounds good",
        "got it",
        "cool",
        "yep",
        "yes",
    ];
    ACKS.iter().any(|ack| normalized == *ack)
}

fn estimate_message_tokens(message: &Value) -> u64 {
    estimate_text_tokens(&message_text(message))
}

fn estimate_text_tokens(text: &str) -> u64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let bytes = trimmed.len() as u64;
    bytes.div_ceil(4).max(1)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= max_chars {
            out.push_str(" …");
            break;
        }
        out.push(ch);
    }
    out
}

fn message_text(message: &Value) -> String {
    let mut out = String::new();
    if let Some(content) = message.get("content") {
        append_content_text(content, &mut out);
    }
    if let Some(tool_name) = message.get("tool_name").and_then(Value::as_str) {
        push_with_cap(&mut out, tool_name, 120);
    }
    if let Some(arguments) = message.get("arguments") {
        push_with_cap(&mut out, &arguments.to_string(), 240);
    }
    if let Some(result) = message.get("result") {
        push_with_cap(&mut out, &result.to_string(), 360);
    }
    if let Some(error) = message.get("error").and_then(Value::as_str) {
        push_with_cap(&mut out, error, 240);
    }
    out
}

fn append_content_text(content: &Value, out: &mut String) {
    if let Some(text) = content.as_str() {
        push_with_cap(out, text, 600);
        return;
    }
    if let Some(blocks) = content.as_array() {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(Value::as_str)
            {
                push_with_cap(out, text, 300);
            }
        }
    }
}

fn push_with_cap(out: &mut String, text: &str, max_chars: usize) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&truncate_chars(trimmed, max_chars));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn classify_importance_follows_v2_tiers() {
        let tool_msg = json!({
            "role": "assistant",
            "content": "calling tool",
            "tool_calls": [{ "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }}]
        });
        assert_eq!(classify_importance(&tool_msg), Importance::Critical);

        let decision_msg = json!({
            "role": "assistant",
            "content": "Decision: ship this on Friday."
        });
        assert_eq!(classify_importance(&decision_msg), Importance::Critical);

        let constraint_msg = json!({
            "role": "system",
            "content": "Hard constraint: do not expose private keys."
        });
        assert_eq!(classify_importance(&constraint_msg), Importance::Critical);

        let code_msg = json!({
            "role": "assistant",
            "content": "```rs\nfn main() {}\n```"
        });
        assert_eq!(classify_importance(&code_msg), Importance::High);

        let diagnostics_msg = json!({
            "role": "assistant",
            "content": "stderr: command failed with exit code 1"
        });
        assert_eq!(classify_importance(&diagnostics_msg), Importance::High);

        let ack_msg = json!({"role": "user", "content": "thanks"});
        assert_eq!(classify_importance(&ack_msg), Importance::Low);
    }

    #[test]
    fn partition_history_for_summary_preserves_tool_chain_anchors() {
        let history = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": "running",
                "tool_calls": [{ "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }}]
            }),
            json!({
                "role": "tool_result",
                "tool_call_id": "tc-1",
                "tool_name": "exec",
                "success": true,
                "result": {"stdout": "ok"}
            }),
            json!({"role": "user", "content": "```rs\nfn main() {}\n```"}),
            json!({"role": "assistant", "content": "reviewed code"}),
            json!({"role": "user", "content": "recent question"}),
            json!({"role": "assistant", "content": "recent answer"}),
        ];

        let sections = partition_history_for_summary(
            &history,
            SummaryContextPlan {
                verbatim_turns: 2,
                anchor_budget_tokens: 2_048,
                summary_budget_tokens: 1_024,
            },
        );

        assert_eq!(sections.recent.len(), 2);
        assert!(
            sections
                .anchors
                .iter()
                .any(|msg| msg.get("tool_call_id").and_then(Value::as_str) == Some("tc-1")),
            "tool_result anchor should be retained"
        );
        assert!(
            sections
                .anchors
                .iter()
                .any(|msg| msg.get("tool_calls").is_some()),
            "assistant tool call anchor should be retained"
        );
    }

    #[test]
    fn partition_history_for_summary_never_keeps_tool_result_without_call() {
        let history = vec![
            json!({"role": "user", "content": "hello"}),
            json!({
                "role": "assistant",
                "content": "running",
                "tool_calls": [{ "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }}]
            }),
            json!({
                "role": "tool_result",
                "tool_call_id": "tc-1",
                "tool_name": "exec",
                "success": true,
                "result": {"stdout": "ok"}
            }),
            json!({"role": "assistant", "content": "done"}),
        ];

        let sections = partition_history_for_summary(
            &history,
            SummaryContextPlan {
                verbatim_turns: 1,
                anchor_budget_tokens: 2_048,
                summary_budget_tokens: 1_024,
            },
        );
        let anchored_tool_call_ids: BTreeSet<String> = sections
            .anchors
            .iter()
            .flat_map(|msg| {
                msg.get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flat_map(|calls| {
                        calls
                            .iter()
                            .filter_map(|call| call.get("id").and_then(Value::as_str))
                            .map(ToString::to_string)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let anchored_result_ids: Vec<String> = sections
            .anchors
            .iter()
            .filter_map(|msg| msg.get("tool_call_id").and_then(Value::as_str))
            .map(ToString::to_string)
            .collect();

        for result_id in anchored_result_ids {
            assert!(
                anchored_tool_call_ids.contains(&result_id),
                "tool_result {result_id} was anchored without matching initiating tool call"
            );
        }
    }

    #[test]
    fn retain_with_importance_prefers_decision_over_acks() {
        let history = vec![
            json!({"role": "user", "content": "ok"}),
            json!({"role": "assistant", "content": "Decision: deploy on Friday and pin rust nightly"}),
            json!({"role": "user", "content": "thanks"}),
            json!({"role": "user", "content": "what changed?"}),
            json!({"role": "assistant", "content": "latest updates"}),
        ];

        let reduced = retain_with_importance(
            &history,
            ReductionPlan {
                keep_recent: 2,
                min_recent_to_keep: 2,
                anchor_budget_tokens: 512,
                max_anchor_messages: 2,
                target_total_tokens: None,
            },
        );

        assert_eq!(reduced.kept_recent_messages, 2);
        assert_eq!(reduced.retained_anchor_messages, 1);
        assert!(
            reduced
                .messages
                .iter()
                .any(|msg| msg.get("content").and_then(Value::as_str)
                    == Some("Decision: deploy on Friday and pin rust nightly"))
        );
        assert!(
            !reduced
                .messages
                .iter()
                .any(|msg| msg.get("content").and_then(Value::as_str) == Some("ok"))
        );
    }

    #[test]
    fn trim_summary_to_budget_truncates_when_over_budget() {
        let summary = "x".repeat(5_000);
        let trimmed = trim_summary_to_budget(&summary, 128);
        assert!(trimmed.len() < summary.len());
    }

    #[test]
    fn retain_with_importance_preserves_partial_multi_tool_chain_when_budget_tight() {
        let history = vec![
            json!({
                "role": "assistant",
                "content": "running tools",
                "tool_calls": [
                    { "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }},
                    { "id": "tc-2", "type": "function", "function": { "name": "exec", "arguments": "{}" }},
                    { "id": "tc-3", "type": "function", "function": { "name": "exec", "arguments": "{}" }}
                ]
            }),
            json!({"role": "tool_result", "tool_call_id": "tc-1", "result": {"stdout": "one"}}),
            json!({"role": "tool_result", "tool_call_id": "tc-2", "result": {"stdout": "two"}}),
            json!({"role": "tool_result", "tool_call_id": "tc-3", "result": {"stdout": "three"}}),
            json!({"role": "user", "content": "latest"}),
        ];

        let reduced = retain_with_importance(
            &history,
            ReductionPlan {
                keep_recent: 1,
                min_recent_to_keep: 1,
                anchor_budget_tokens: 1_024,
                max_anchor_messages: 2,
                target_total_tokens: None,
            },
        );

        assert!(
            reduced.retained_anchor_messages >= 2,
            "at least one tool-call/result pair should survive under tight count budget"
        );
        assert!(
            reduced
                .messages
                .iter()
                .any(|msg| msg.get("role").and_then(Value::as_str) == Some("tool_result")),
            "must preserve at least one tool result anchor"
        );
    }

    #[test]
    fn anchor_selection_never_truncates_tool_chain_group_mid_span() {
        let history = vec![
            json!({
                "role": "assistant",
                "content": "running",
                "tool_calls": [{ "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }}]
            }),
            json!({"role": "tool_result", "tool_call_id": "tc-1", "result": {"stdout": "very long output"}}),
        ];
        let required_tokens: u64 = history.iter().map(estimate_message_tokens).sum();
        let selected = select_anchor_indices(&history, required_tokens.saturating_sub(1), 8);
        assert!(
            selected.is_empty(),
            "selection must skip the entire chain when budget cannot fit the full span"
        );
    }

    #[test]
    fn emergency_reduction_drops_low_importance_before_higher_importance() {
        let history = vec![
            json!({"role": "assistant", "content": "Decision: ship build 42 to staging."}),
            json!({"role": "user", "content": "ok"}),
            json!({"role": "user", "content": "Need rollback checklist and release owner."}),
            json!({"role": "assistant", "content": "Owner is Alex, checklist is in runbook."}),
        ];
        let total_tokens: u64 = history.iter().map(estimate_message_tokens).sum();
        let low_tokens = estimate_message_tokens(&history[1]);
        let target = total_tokens.saturating_sub(low_tokens.max(1));

        let reduced = retain_with_importance(
            &history,
            ReductionPlan {
                keep_recent: 3,
                min_recent_to_keep: 2,
                anchor_budget_tokens: 512,
                max_anchor_messages: 4,
                target_total_tokens: Some(target),
            },
        );

        assert!(
            !reduced
                .messages
                .iter()
                .any(|msg| msg.get("content").and_then(Value::as_str) == Some("ok"))
        );
        assert!(
            reduced.messages.iter().any(|msg| {
                msg.get("content").and_then(Value::as_str)
                    == Some("Need rollback checklist and release owner.")
            }),
            "higher-importance recent content should remain"
        );
    }
}
