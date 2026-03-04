use std::collections::{BTreeSet, HashMap};

use moltis_config::ChatConfig;
use serde_json::Value;

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
    pub(crate) anchor_budget_tokens: u64,
    pub(crate) max_anchor_messages: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SummaryCompactionSections {
    pub(crate) summary_source: Vec<Value>,
    pub(crate) anchors: Vec<Value>,
    pub(crate) recent: Vec<Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct ImportanceRetentionResult {
    pub(crate) messages: Vec<Value>,
    pub(crate) removed_messages: usize,
    pub(crate) retained_anchor_messages: usize,
    pub(crate) kept_recent_messages: usize,
}

const SUMMARY_MIN_RECENT_TURNS: usize = 4;
const SUMMARY_MAX_RECENT_TURNS: usize = 12;
const SUMMARY_DEFAULT_RECENT_TURNS: usize = 10;
const SUMMARY_DEFAULT_ANCHOR_BUDGET_TOKENS: u64 = 2_048;
const SUMMARY_DEFAULT_BUDGET_TOKENS: u64 = 1_200;
const ARCHIVE_DEFAULT_ANCHOR_BUDGET_TOKENS: u64 = 1_024;
const ARCHIVE_DEFAULT_MAX_ANCHOR_MESSAGES: usize = 12;
const TRUNCATE_DEFAULT_ANCHOR_BUDGET_TOKENS: u64 = 384;
const TRUNCATE_DEFAULT_MAX_ANCHOR_MESSAGES: usize = 4;

#[must_use]
pub(crate) fn summary_context_plan(configured_keep_recent: usize) -> SummaryContextPlan {
    let verbatim_turns = configured_keep_recent
        .max(SUMMARY_DEFAULT_RECENT_TURNS)
        .clamp(SUMMARY_MIN_RECENT_TURNS, SUMMARY_MAX_RECENT_TURNS);
    SummaryContextPlan {
        verbatim_turns,
        anchor_budget_tokens: SUMMARY_DEFAULT_ANCHOR_BUDGET_TOKENS,
        summary_budget_tokens: SUMMARY_DEFAULT_BUDGET_TOKENS,
    }
}

#[must_use]
pub(crate) fn archive_reduction_plan(configured_keep_recent: usize) -> ReductionPlan {
    ReductionPlan {
        keep_recent: configured_keep_recent,
        anchor_budget_tokens: ARCHIVE_DEFAULT_ANCHOR_BUDGET_TOKENS,
        max_anchor_messages: ARCHIVE_DEFAULT_MAX_ANCHOR_MESSAGES,
    }
}

#[must_use]
pub(crate) fn truncate_reduction_plan(keep_recent: usize) -> ReductionPlan {
    ReductionPlan {
        keep_recent,
        anchor_budget_tokens: TRUNCATE_DEFAULT_ANCHOR_BUDGET_TOKENS,
        max_anchor_messages: TRUNCATE_DEFAULT_MAX_ANCHOR_MESSAGES,
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
        ARCHIVE_DEFAULT_MAX_ANCHOR_MESSAGES,
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

    let keep_recent = archive_keep_recent_for_reduction(history.len(), plan.keep_recent);
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

    let mut merged = Vec::with_capacity(anchors.len() + recent.len());
    merged.extend(anchors.clone());
    merged.extend(recent.iter().cloned());

    let removed_messages = history.len().saturating_sub(merged.len());
    ImportanceRetentionResult {
        messages: merged,
        removed_messages,
        retained_anchor_messages: anchors.len(),
        kept_recent_messages: recent.len(),
    }
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

    if has_tool_calls(message) || matches!(role, "tool" | "tool_result") || text.contains("```") {
        return Importance::Critical;
    }

    if has_decision_markers(&lower) {
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

    for (idx, message) in history.iter().enumerate() {
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
    fn classify_importance_prioritizes_tool_and_code_context() {
        let tool_msg = json!({
            "role": "assistant",
            "content": "calling tool",
            "tool_calls": [{ "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }}]
        });
        assert_eq!(classify_importance(&tool_msg), Importance::Critical);

        let code_msg = json!({
            "role": "assistant",
            "content": "```rs\nfn main() {}\n```"
        });
        assert_eq!(classify_importance(&code_msg), Importance::Critical);

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
                anchor_budget_tokens: 512,
                max_anchor_messages: 2,
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
                anchor_budget_tokens: 1_024,
                max_anchor_messages: 2,
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
}
