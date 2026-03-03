//! Leak detector hook: scans tool call arguments for secrets and API keys
//! before they are sent to external tools.

use {async_trait::async_trait, tracing::warn};

use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

use crate::bundled::pattern_scanner::{PatternScanner, strip_image_data_uris};

pub struct LeakDetectorHook {
    scanner: PatternScanner,
}

impl LeakDetectorHook {
    pub fn new() -> Self {
        Self {
            scanner: PatternScanner::new(&[
                "sk-",
                "ghp_",
                "xoxb-",
                "xoxp-",
                "akia",
                "-----begin",
                "aws_secret",
                "api_key",
                "api-key",
                "secret_key",
            ]),
        }
    }
}

impl Default for LeakDetectorHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HookHandler for LeakDetectorHook {
    fn name(&self) -> &str {
        "leak-detector"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::BeforeToolCall]
    }

    fn priority(&self) -> i32 {
        80
    }

    async fn handle(&self, _event: HookEvent, payload: &HookPayload) -> Result<HookAction> {
        if let HookPayload::BeforeToolCall {
            tool_name,
            arguments,
            ..
        } = payload
        {
            let Some(scan_text) = extract_scan_text(tool_name, arguments) else {
                return Ok(HookAction::Continue);
            };

            let normalized = strip_image_data_uris(&scan_text);
            if self.scanner.matches(&normalized) {
                warn!(
                    tool = %tool_name,
                    "leak-detector: potential secret detected in tool arguments"
                );
                return Ok(HookAction::Block(format!(
                    "Potential secret or credential detected in {tool_name} arguments"
                )));
            }
        }
        Ok(HookAction::Continue)
    }
}

fn extract_scan_text(tool_name: &str, arguments: &serde_json::Value) -> Option<String> {
    let command = arguments
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    // Browser screenshot/snapshot payloads can contain large base64 data; skip.
    if matches!(command, "screenshot" | "snapshot") {
        return None;
    }

    if tool_name == "tinder_browser" {
        // For tinder browser actions, only scan outbound message text.
        if !matches!(command, "type" | "fill") {
            return None;
        }
        return arguments
            .get("text")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
    }

    if tool_name == "exec" {
        return arguments
            .get("command")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
    }

    Some(arguments.to_string())
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocks_api_key_in_args() {
        let hook = LeakDetectorHook::new();
        let payload = HookPayload::BeforeToolCall {
            session_key: "s1".into(),
            tool_name: "tinder_browser".into(),
            arguments: serde_json::json!({ "command": "type", "text": "api-key: sk-12345" }),
        };
        let result = hook
            .handle(HookEvent::BeforeToolCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }

    #[tokio::test]
    async fn allows_screenshot_with_base64_payload() {
        let hook = LeakDetectorHook::new();
        let payload = HookPayload::BeforeToolCall {
            session_key: "s1".into(),
            tool_name: "tinder_browser".into(),
            arguments: serde_json::json!({
                "command": "screenshot",
                "image": "data:image/png;base64,ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/="
            }),
        };
        let result = hook
            .handle(HookEvent::BeforeToolCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[tokio::test]
    async fn blocks_exec_command_secrets() {
        let hook = LeakDetectorHook::new();
        let payload = HookPayload::BeforeToolCall {
            session_key: "s1".into(),
            tool_name: "exec".into(),
            arguments: serde_json::json!({ "command": "echo sk-secret-key-here" }),
        };
        let result = hook
            .handle(HookEvent::BeforeToolCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }
}
