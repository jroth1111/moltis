use serde_json::Value;

fn truncate_at_char_boundary(text: &str, max_bytes: usize) -> &str {
    &text[..text.floor_char_boundary(max_bytes)]
}

fn tool_result_content(value: &Value) -> String {
    if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
        format!("Error: {err}")
    } else if let Some(res) = value.get("result") {
        res.to_string()
    } else {
        String::new()
    }
}

fn tool_result_to_tool_message(value: &Value, max_content_chars: Option<usize>) -> Value {
    let tool_call_id = value
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut content = tool_result_content(value);
    if let Some(max_chars) = max_content_chars
        && content.len() > max_chars
    {
        content = format!(
            "{}\n\n... [truncated — {} bytes total]",
            truncate_at_char_boundary(&content, max_chars),
            content.len()
        );
    }
    serde_json::json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": content,
    })
}

/// Rebuild the persisted session history into the shape used for context
/// inspection: tool results are represented as `role: "tool"` messages.
pub(crate) fn normalize_for_context_view(history: Vec<Value>) -> Vec<Value> {
    history
        .into_iter()
        .map(|val| {
            if val.get("role").and_then(|r| r.as_str()) != Some("tool_result") {
                return val;
            }
            tool_result_to_tool_message(&val, None)
        })
        .collect()
}

/// Rebuild persisted history into model-consumable context while preserving
/// tool call/result continuity. Only tool results that match an assistant
/// tool call ID in the same history are retained.
pub(crate) fn normalize_for_model_context(
    history: &[Value],
    max_tool_content_chars: usize,
) -> Vec<Value> {
    let valid_tool_call_ids: std::collections::HashSet<String> = history
        .iter()
        .filter_map(|msg| msg.get("tool_calls").and_then(Value::as_array))
        .flat_map(|calls| {
            calls
                .iter()
                .filter_map(|call| call.get("id").and_then(Value::as_str))
                .map(ToString::to_string)
        })
        .collect();

    history
        .iter()
        .filter_map(|val| {
            if val.get("role").and_then(|r| r.as_str()) != Some("tool_result") {
                return Some(val.clone());
            }
            let tool_call_id = val.get("tool_call_id").and_then(Value::as_str)?;
            if !valid_tool_call_ids.contains(tool_call_id) {
                return None;
            }
            Some(tool_result_to_tool_message(
                val,
                Some(max_tool_content_chars),
            ))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::{normalize_for_context_view, normalize_for_model_context};

    #[test]
    fn model_context_only_keeps_tool_results_with_matching_assistant_call() {
        let history = vec![
            json!({
                "role": "assistant",
                "content": "running",
                "tool_calls": [{ "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }}]
            }),
            json!({
                "role": "tool_result",
                "tool_call_id": "tc-1",
                "result": {"stdout": "ok"}
            }),
            json!({
                "role": "tool_result",
                "tool_call_id": "tc-orphan",
                "result": {"stdout": "orphan"}
            }),
        ];

        let normalized = normalize_for_model_context(&history, 1_000);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[1]["role"], "tool");
        assert_eq!(normalized[1]["tool_call_id"], "tc-1");
    }

    #[test]
    fn model_context_caps_large_tool_payloads() {
        let large_stdout = "x".repeat(5_000);
        let history = vec![
            json!({
                "role": "assistant",
                "content": "running",
                "tool_calls": [{ "id": "tc-1", "type": "function", "function": { "name": "exec", "arguments": "{}" }}]
            }),
            json!({
                "role": "tool_result",
                "tool_call_id": "tc-1",
                "result": {"stdout": large_stdout}
            }),
        ];

        let normalized = normalize_for_model_context(&history, 512);
        let content = normalized[1]
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(content.contains("[truncated"));
        assert!(content.len() < 1_000);
    }

    #[test]
    fn context_view_keeps_all_tool_results() {
        let history = vec![
            json!({"role": "tool_result", "tool_call_id": "a", "result": {"ok": true}}),
            json!({"role": "tool_result", "tool_call_id": "b", "result": {"ok": false}}),
        ];
        let normalized = normalize_for_context_view(history);
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0]["role"], "tool");
        assert_eq!(normalized[1]["tool_call_id"], "b");
    }
}
