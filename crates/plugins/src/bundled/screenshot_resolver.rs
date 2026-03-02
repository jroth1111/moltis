//! Screenshot resolver hook.
//!
//! Fires on BeforeLLMCall and scans message history for tool results containing
//! `screenshot_path`. When found, it reads the file and injects image content
//! into the messages payload.

use std::{collections::HashSet, path::Path};

use async_trait::async_trait;
use base64::Engine;
use tracing::{debug, warn};

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
        if let HookPayload::BeforeLLMCall { messages, .. } = payload
            && let Some(msgs) = messages.as_array()
        {
            let mut changed = false;
            let mut out = Vec::with_capacity(msgs.len() + 2);
            let mut already_injected = collect_already_injected_paths(msgs);

            for msg in msgs {
                out.push(msg.clone());

                let is_tool = msg.get("role").and_then(|v| v.as_str()) == Some("tool");
                if !is_tool {
                    continue;
                }

                let Some(content) = msg.get("content").and_then(|v| v.as_str()) else {
                    continue;
                };

                for screenshot_path in extract_screenshot_paths(content) {
                    if !already_injected.insert(screenshot_path.clone()) {
                        continue;
                    }

                    match image_data_uri_from_path(&screenshot_path) {
                        Ok(data_uri) => {
                            changed = true;
                            debug!(path = %screenshot_path, "screenshot-resolver: injected image message");
                            out.push(serde_json::json!({
                                "role": "user",
                                "content": [
                                    { "type": "text", "text": format!("[screenshot_resolver] {screenshot_path}") },
                                    { "type": "image_url", "image_url": { "url": data_uri } }
                                ]
                            }));
                        },
                        Err(e) => {
                            warn!(path = %screenshot_path, error = %e, "screenshot-resolver: unable to load screenshot");
                        },
                    }
                }
            }

            if changed {
                return Ok(HookAction::ModifyPayload(serde_json::Value::Array(out)));
            }
        }

        Ok(HookAction::Continue)
    }
}

fn collect_already_injected_paths(messages: &[serde_json::Value]) -> HashSet<String> {
    let mut seen = HashSet::new();
    for msg in messages {
        if msg.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let Some(parts) = msg.get("content").and_then(|v| v.as_array()) else {
            continue;
        };
        for part in parts {
            let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(path) = text.strip_prefix("[screenshot_resolver] ") {
                seen.insert(path.to_string());
            }
        }
    }
    seen
}

fn extract_screenshot_paths(tool_content: &str) -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(tool_content) {
        collect_paths_from_json(&value, &mut paths);
    }
    paths
}

fn collect_paths_from_json(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                if k == "screenshot_path"
                    && let Some(path) = v.as_str()
                    && !path.is_empty()
                {
                    out.push(path.to_string());
                    continue;
                }
                collect_paths_from_json(v, out);
            }
        },
        serde_json::Value::Array(values) => {
            for v in values {
                collect_paths_from_json(v, out);
            }
        },
        _ => {},
    }
}

fn image_data_uri_from_path(path: &str) -> anyhow::Result<String> {
    let bytes = std::fs::read(path)?;
    let mime = guess_mime(path);
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

fn guess_mime(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[tokio::test]
    async fn injects_image_when_screenshot_path_is_present() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"png-bytes").unwrap();
        let path = file.path().display().to_string();

        let hook = ScreenshotResolverHook::new();
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            messages: serde_json::json!([
                {"role": "tool", "content": format!("{{\"result\": {{\"screenshot_path\": \"{path}\"}}}}")}
            ]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &payload)
            .await
            .unwrap();
        let HookAction::ModifyPayload(modified) = result else {
            panic!("expected ModifyPayload");
        };
        let msgs = modified.as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(
            msgs[1]["content"][0]["text"],
            format!("[screenshot_resolver] {path}")
        );
        let data_url = msgs[1]["content"][1]["image_url"]["url"].as_str().unwrap();
        assert!(data_url.starts_with("data:image/png;base64,"));
    }

    #[tokio::test]
    async fn skips_already_injected_screenshot() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"png-bytes").unwrap();
        let path = file.path().display().to_string();

        let hook = ScreenshotResolverHook::new();
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            messages: serde_json::json!([
                {"role": "tool", "content": format!("{{\"screenshot_path\": \"{path}\"}}")},
                {"role": "user", "content": [
                    {"type":"text","text": format!("[screenshot_resolver] {path}")},
                    {"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}
                ]}
            ]),
            tool_count: 0,
            iteration: 1,
        };
        let result = hook
            .handle(HookEvent::BeforeLLMCall, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }
}
