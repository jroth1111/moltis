//! Leak detector hook: scans tool call arguments for secrets and API keys
//! before they are sent to external tools.

use {async_trait::async_trait, tracing::warn};

use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

use crate::bundled::pattern_scanner::PatternScanner;

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
            let args_str = arguments.to_string();
            if self.scanner.matches(&args_str) {
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

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocks_api_key_in_args() {
        let hook = LeakDetectorHook::new();
        let payload = HookPayload::BeforeToolCall {
            session_key: "s1".into(),
            tool_name: "exec".into(),
            arguments: serde_json::json!({ "command": "curl -H 'api-key: sk-12345'" }),
        };
        let result = hook
            .handle(HookEvent::BeforeToolCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }

    #[tokio::test]
    async fn allows_safe_args() {
        let hook = LeakDetectorHook::new();
        let payload = HookPayload::BeforeToolCall {
            session_key: "s1".into(),
            tool_name: "exec".into(),
            arguments: serde_json::json!({ "command": "echo hello" }),
        };
        let result = hook
            .handle(HookEvent::BeforeToolCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[tokio::test]
    async fn skips_non_external_tools() {
        let hook = LeakDetectorHook::new();
        let payload = HookPayload::BeforeToolCall {
            session_key: "s1".into(),
            tool_name: "session_state".into(),
            arguments: serde_json::json!({ "key": "sk-secret-key-here" }),
        };
        let result = hook
            .handle(HookEvent::BeforeToolCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }
}
