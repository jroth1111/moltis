use serde_json::Value;

/// Rebuild the persisted session history into the shape used for context
/// inspection: tool results are represented as `role: "tool"` messages.
pub(crate) fn normalize_for_context_view(history: Vec<Value>) -> Vec<Value> {
    history
        .into_iter()
        .map(|val| {
            if val.get("role").and_then(|r| r.as_str()) != Some("tool_result") {
                return val;
            }
            let tool_call_id = val
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let content = if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
                format!("Error: {err}")
            } else if let Some(res) = val.get("result") {
                res.to_string()
            } else {
                String::new()
            };
            serde_json::json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content": content,
            })
        })
        .collect()
}
