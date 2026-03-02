//! Screenshot resolver hook.
//!
//! Fires on BeforeLLMCall and scans message history for tool results containing
//! `screenshot_path`. When found, it reads the file and injects image content
//! into the messages payload.
//!
//! NOTE: ContentPart::Image does not exist in the protocol crate yet.
//! This hook currently returns Continue as a no-op until image content parts
//! are added to the protocol.

use async_trait::async_trait;
use tracing::debug;

use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

pub struct ScreenshotResolverHook;

impl ScreenshotResolverHook {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ScreenshotResolverHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HookHandler for ScreenshotResolverHook {
    fn name(&self) -> &str {
        "screenshot-resolver"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::BeforeLLMCall]
    }

    fn priority(&self) -> i32 {
        50
    }

    async fn handle(&self, _event: HookEvent, payload: &HookPayload) -> Result<HookAction> {
        if let HookPayload::BeforeLLMCall { messages, .. } = payload {
            // Scan for screenshot_path references in tool result messages
            let has_screenshots = messages
                .as_array()
                .map(|msgs| {
                    msgs.iter().any(|msg| {
                        msg.get("content")
                            .and_then(|c| c.as_str())
                            .is_some_and(|s| s.contains("screenshot_path"))
                            || msg
                                .get("content")
                                .and_then(|c| c.as_array())
                                .is_some_and(|parts| {
                                    parts.iter().any(|p| {
                                        p.get("text")
                                            .and_then(|t| t.as_str())
                                            .is_some_and(|s| s.contains("screenshot_path"))
                                    })
                                })
                    })
                })
                .unwrap_or(false);

            if has_screenshots {
                debug!("screenshot-resolver: found screenshot references in messages");
                // TODO: Once ContentPart::Image is available in the protocol crate,
                // read screenshot files from the media store and inject them as image
                // content parts via HookAction::ModifyPayload.
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
    async fn continues_with_no_screenshots() {
        let hook = ScreenshotResolverHook::new();
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            messages: serde_json::json!([
                {"role": "user", "content": "hello"}
            ]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook.handle(HookEvent::BeforeLLMCall, &payload).await.unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[tokio::test]
    async fn detects_screenshot_references() {
        let hook = ScreenshotResolverHook::new();
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            messages: serde_json::json!([
                {"role": "tool", "content": "{\"screenshot_path\": \"/tmp/screen.png\"}"}
            ]),
            tool_count: 0,
            iteration: 1,
        };
        // Should still return Continue since we can't inject images yet
        let result = hook.handle(HookEvent::BeforeLLMCall, &payload).await.unwrap();
        assert!(matches!(result, HookAction::Continue));
    }
}
