//! Strip internal XML tags and special tokens from LLM responses.
//!
//! Some models leak internal reasoning tags (`<thinking>`, `<reflection>`, etc.)
//! or special control tokens (`<|eot_id|>`, `<|im_end|>`, etc.) into their
//! responses. This module strips them to produce clean user-facing text.
//!
//! The stripping is done with hand-rolled string scanning (no regex) to match
//! the existing `strip_base64_blobs` pattern in `runner.rs`.

use crate::leak_detector::{LeakAction, LeakDetector};
use crate::model::ToolCall;

/// Known internal XML tags that should be stripped from LLM responses.
const INTERNAL_TAGS: &[&str] = &[
    "thinking",
    "think",
    "reflection",
    "inner_monologue",
    "scratchpad",
    "reasoning",
    "analysis",
    "self_reflection",
    "meta",
    "internal_thought",
    "function_call",
    "tool_use",
];

/// Standalone pipe tokens that should be stripped.
const STANDALONE_PIPE_TOKENS: &[&str] = &[
    "<|eot_id|>",
    "<|end|>",
    "<|im_end|>",
    "<|im_start|>",
    "<|begin_of_text|>",
    "<|end_of_text|>",
    "<|python_tag|>",
    "<|eom_id|>",
    "<|start_header_id|>",
    "<|end_header_id|>",
];

/// Tags used for tool call recovery.
const TOOL_CALL_TAGS: &[&str] = &["function_call", "tool_call"];

/// Main entry point: chain all stripping passes and trim the result.
pub fn clean_response(text: &str) -> String {
    let mut result = strip_internal_tags(text);
    result = strip_standalone_pipe_tokens(&result);
    result = strip_reasoning_patterns(&result);
    result.trim().to_string()
}

/// Strip all known internal XML tags and their content.
fn strip_internal_tags(text: &str) -> String {
    let mut result = text.to_string();
    for tag in INTERNAL_TAGS {
        result = strip_xml_tag(&result, tag);
        result = strip_pipe_tag(&result, tag);
    }
    result
}

/// Strip `<tag ...>content</tag>` pairs, handling optional attributes.
///
/// Matches opening tags with or without attributes (e.g. `<thinking>`,
/// `<thinking type="deep">`), and removes everything up to and including
/// the corresponding closing tag.
fn strip_xml_tag(text: &str, tag: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;

    let open_exact = format!("<{tag}>");
    let open_with_space = format!("<{tag} ");
    let close = format!("</{tag}>");

    loop {
        // Find the earliest opening tag variant.
        let exact_pos = rest.find(&open_exact);
        let space_pos = rest.find(&open_with_space);

        let start = match (exact_pos, space_pos) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };

        let Some(start) = start else {
            result.push_str(rest);
            break;
        };

        // Push everything before the tag.
        result.push_str(&rest[..start]);

        // Find the end of the opening tag (the `>`).
        let after_open = &rest[start..];
        let Some(gt_pos) = after_open.find('>') else {
            // Malformed tag — keep everything as-is.
            result.push_str(&rest[start..]);
            break;
        };

        // Now look for the closing tag.
        let after_open_tag = &rest[start + gt_pos + 1..];
        if let Some(close_pos) = after_open_tag.find(&close) {
            // Skip past the closing tag.
            rest = &after_open_tag[close_pos + close.len()..];
        } else {
            // No closing tag — strip everything from open tag to end
            // (the tag is likely wrapping remaining content).
            break;
        }
    }
    result
}

/// Strip `<|tag|>...<|/tag|>` pairs (pipe-delimited variant).
fn strip_pipe_tag(text: &str, tag: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;

    let open = format!("<|{tag}|>");
    let close = format!("<|/{tag}|>");

    loop {
        let Some(start) = rest.find(&open) else {
            result.push_str(rest);
            break;
        };

        result.push_str(&rest[..start]);

        let after_open = &rest[start + open.len()..];
        if let Some(close_pos) = after_open.find(&close) {
            rest = &after_open[close_pos + close.len()..];
        } else {
            // No closing tag — strip to end.
            break;
        }
    }
    result
}

/// Strip standalone pipe tokens (`<|eot_id|>`, `<|im_end|>`, etc.).
fn strip_standalone_pipe_tokens(text: &str) -> String {
    let mut result = text.to_string();
    for token in STANDALONE_PIPE_TOKENS {
        // Simple replacement — these tokens are always standalone.
        result = result.replace(token, "");
    }
    result
}

/// Strip reasoning pattern blocks: `<Thinking>...</Thinking>` and similar
/// capitalized variants that some models produce at the start of responses.
fn strip_reasoning_patterns(text: &str) -> String {
    let mut result = text.to_string();
    // Handle capitalized variants not covered by the lowercase tag list.
    for tag in &["Thinking", "Reflection", "Reasoning", "Analysis"] {
        result = strip_xml_tag(&result, tag);
    }
    result
}

/// Attempt to recover structured `ToolCall` from `<function_call>` or
/// `<tool_call>` XML blocks embedded in the response text.
///
/// Returns the cleaned text (with recovered blocks removed) and any
/// recovered tool calls.
pub fn recover_tool_calls_from_content(text: &str) -> (String, Vec<ToolCall>) {
    let mut cleaned = text.to_string();
    let mut tool_calls = Vec::new();

    for tag in TOOL_CALL_TAGS {
        let open_exact = format!("<{tag}>");
        let open_with_space = format!("<{tag} ");
        let close = format!("</{tag}>");

        loop {
            let exact_pos = cleaned.find(&open_exact);
            let space_pos = cleaned.find(&open_with_space);

            let start = match (exact_pos, space_pos) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };

            let Some(start) = start else {
                break;
            };

            let after_open = &cleaned[start..];
            let Some(gt_pos) = after_open.find('>') else {
                break;
            };

            let content_start = start + gt_pos + 1;
            let after_content = &cleaned[content_start..];

            let Some(close_pos) = after_content.find(&close) else {
                break;
            };

            let xml_content = &cleaned[content_start..content_start + close_pos].trim();

            // Try to parse the content as JSON to extract tool call info.
            if let Some(tc) = parse_tool_call_json(xml_content) {
                tool_calls.push(tc);
            }

            // Remove the entire block from cleaned text.
            let end = content_start + close_pos + close.len();
            cleaned = format!("{}{}", &cleaned[..start], &cleaned[end..]);
        }
    }

    (cleaned.trim().to_string(), tool_calls)
}

/// Parse JSON content from an XML tool call block into a `ToolCall`.
///
/// Accepts formats like:
/// ```json
/// {"name": "exec", "arguments": {"command": "ls"}}
/// ```
/// or:
/// ```json
/// {"tool": "exec", "arguments": {"command": "ls"}}
/// ```
fn parse_tool_call_json(content: &str) -> Option<ToolCall> {
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
    let name = parsed
        .get("name")
        .or_else(|| parsed.get("tool"))
        .and_then(|v| v.as_str())?
        .to_string();
    let arguments = parsed
        .get("arguments")
        .or_else(|| parsed.get("parameters"))
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let id = format!("xml-{}", uuid::Uuid::new_v4());
    Some(ToolCall {
        id,
        name,
        arguments,
    })
}

/// Scan content for credential leaks and sanitize/block as needed.
///
/// This intentionally does not call [`clean_response`], because tool output may
/// legitimately contain XML-like tags that should not be stripped.
///
/// - `Warn` detections are logged but the content is still returned.
/// - `Redact` detections are replaced in-place.
/// - `Block` detections return `"[BLOCKED: potential credential leak]"`.
#[must_use]
pub fn sanitize_with_leak_detection(text: &str, sensitivity: f64) -> String {
    let detector = LeakDetector::new(sensitivity);
    let mut warn_patterns: Vec<&'static str> = Vec::new();
    for leak_match in detector.scan(text) {
        if leak_match.action == LeakAction::Warn
            && !warn_patterns.contains(&leak_match.pattern_name)
        {
            warn_patterns.push(leak_match.pattern_name);
        }
    }

    match detector.apply(text) {
        Ok(s) => {
            for pattern in warn_patterns {
                tracing::warn!(
                    pattern = %pattern,
                    "potential credential leak detected (warn-only)"
                );
            }
            s
        },
        Err(pattern) => {
            tracing::warn!(
                pattern = %pattern,
                "blocked content due to potential credential leak"
            );
            "[BLOCKED: potential credential leak]".to_string()
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_xml_tag ──────────────────────────────────────────────

    #[test]
    fn strip_simple_thinking_tag() {
        let input = "Hello <thinking>internal thought</thinking> world";
        assert_eq!(strip_xml_tag(input, "thinking"), "Hello  world");
    }

    #[test]
    fn strip_tag_with_attributes() {
        let input = "Start <thinking type=\"deep\">reasoning here</thinking> end";
        assert_eq!(strip_xml_tag(input, "thinking"), "Start  end");
    }

    #[test]
    fn strip_multiple_tags() {
        let input = "<think>a</think>text<think>b</think>";
        assert_eq!(strip_xml_tag(input, "think"), "text");
    }

    #[test]
    fn no_matching_tag_unchanged() {
        let input = "Hello world";
        assert_eq!(strip_xml_tag(input, "thinking"), "Hello world");
    }

    #[test]
    fn unclosed_tag_strips_to_end() {
        let input = "Hello <thinking>unfinished";
        // No closing tag — everything from open tag to end is stripped.
        assert_eq!(strip_xml_tag(input, "thinking"), "Hello ");
    }

    #[test]
    fn nested_content_preserved_outside() {
        let input = "before<reflection>some analysis</reflection>after";
        assert_eq!(strip_xml_tag(input, "reflection"), "beforeafter");
    }

    // ── strip_pipe_tag ─────────────────────────────────────────────

    #[test]
    fn strip_pipe_tag_basic() {
        let input = "Hello <|thinking|>internal<|/thinking|> world";
        assert_eq!(strip_pipe_tag(input, "thinking"), "Hello  world");
    }

    #[test]
    fn strip_pipe_tag_no_close() {
        let input = "Hello <|thinking|>unfinished";
        assert_eq!(strip_pipe_tag(input, "thinking"), "Hello ");
    }

    // ── strip_standalone_pipe_tokens ───────────────────────────────

    #[test]
    fn strip_eot_tokens() {
        let input = "Hello world<|eot_id|>";
        assert_eq!(strip_standalone_pipe_tokens(input), "Hello world");
    }

    #[test]
    fn strip_multiple_standalone_tokens() {
        let input = "<|begin_of_text|>Hello<|im_end|> world<|end|>";
        assert_eq!(strip_standalone_pipe_tokens(input), "Hello world");
    }

    // ── strip_reasoning_patterns ───────────────────────────────────

    #[test]
    fn strip_capitalized_thinking() {
        let input = "<Thinking>Let me reason about this...</Thinking>Here is my answer.";
        assert_eq!(strip_reasoning_patterns(input), "Here is my answer.");
    }

    // ── clean_response (integration) ───────────────────────────────

    #[test]
    fn clean_response_strips_all_tag_types() {
        let input = "<thinking>reasoning</thinking>Answer here<|eot_id|><|im_end|>";
        assert_eq!(clean_response(input), "Answer here");
    }

    #[test]
    fn clean_response_preserves_normal_text() {
        let input = "This is a normal response with no tags.";
        assert_eq!(
            clean_response(input),
            "This is a normal response with no tags."
        );
    }

    #[test]
    fn clean_response_trims_whitespace() {
        let input = "  <thinking>x</thinking>  Hello  ";
        assert_eq!(clean_response(input), "Hello");
    }

    #[test]
    fn clean_response_complex_mixed() {
        let input = "<Thinking>Step 1: analyze</Thinking>\n\nThe answer is 42.<|end|>\n<reflection>Was I right?</reflection>";
        assert_eq!(clean_response(input), "The answer is 42.");
    }

    // ── recover_tool_calls_from_content ────────────────────────────

    #[test]
    fn recover_tool_call_from_function_call_block() {
        let input = r#"Some text <function_call>{"name": "exec", "arguments": {"command": "ls"}}</function_call> more text"#;
        let (cleaned, calls) = recover_tool_calls_from_content(input);
        assert_eq!(cleaned, "Some text  more text");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].arguments, serde_json::json!({"command": "ls"}));
    }

    #[test]
    fn recover_tool_call_with_tool_key() {
        let input =
            r#"<tool_call>{"tool": "web_search", "arguments": {"query": "rust"}}</tool_call>"#;
        let (cleaned, calls) = recover_tool_calls_from_content(input);
        assert_eq!(cleaned, "");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
    }

    #[test]
    fn recover_no_tool_calls_returns_empty() {
        let input = "Just normal text with no tool calls.";
        let (cleaned, calls) = recover_tool_calls_from_content(input);
        assert_eq!(cleaned, "Just normal text with no tool calls.");
        assert!(calls.is_empty());
    }

    #[test]
    fn recover_malformed_json_skipped() {
        let input = "<function_call>not json</function_call>rest";
        let (cleaned, calls) = recover_tool_calls_from_content(input);
        assert_eq!(cleaned, "rest");
        assert!(calls.is_empty());
    }

    #[test]
    fn recover_multiple_tool_calls() {
        let input = r#"<tool_call>{"name": "a", "arguments": {}}</tool_call>text<tool_call>{"name": "b", "arguments": {}}</tool_call>"#;
        let (cleaned, calls) = recover_tool_calls_from_content(input);
        assert_eq!(cleaned, "text");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
    }

    // ── sanitize_with_leak_detection ───────────────────────────────

    #[test]
    fn sanitize_with_leak_detection_preserves_tool_tag_content() {
        let input = r#"{"result":"<thinking>model diagnostics</thinking>","status":"ok"}"#;
        let output = sanitize_with_leak_detection(input, 0.0);
        assert_eq!(output, input);
    }

    #[test]
    fn sanitize_with_leak_detection_warn_does_not_block_content() {
        let input = "-----BEGIN CERTIFICATE-----\nMIIBxTCCAW...";
        let output = sanitize_with_leak_detection(input, 1.0);
        assert_eq!(output, input);
    }

    #[test]
    fn sanitize_with_leak_detection_blocks_block_patterns() {
        let input = "creds: AKIAIOSFODNN7EXAMPLE";
        let output = sanitize_with_leak_detection(input, 1.0);
        assert_eq!(output, "[BLOCKED: potential credential leak]");
    }
}
