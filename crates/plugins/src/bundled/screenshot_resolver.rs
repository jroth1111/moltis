//! Screenshot resolver hook.
//!
//! Fires on BeforeLLMCall and scans message history for tool results containing
//! `screenshot_path`. When found, it reads the file and injects image content
//! into the messages payload.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use {
    async_trait::async_trait,
    base64::Engine,
    tracing::{debug, warn},
};

use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

pub struct ScreenshotResolverHook;
const MAX_SCREENSHOT_BYTES: u64 = 8 * 1024 * 1024;

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

                    match image_data_uri_from_path(&screenshot_path).await {
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

async fn image_data_uri_from_path(path: &str) -> anyhow::Result<String> {
    let candidate = resolve_media_path(path)?;
    let metadata = tokio::fs::metadata(&candidate).await?;
    if metadata.len() > MAX_SCREENSHOT_BYTES {
        anyhow::bail!("screenshot too large: {} bytes", metadata.len());
    }

    let bytes = tokio::fs::read(&candidate).await?;
    let mime = guess_mime(candidate.to_string_lossy().as_ref());
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

fn resolve_media_path(path: &str) -> anyhow::Result<PathBuf> {
    let candidate = std::fs::canonicalize(path)?;
    let media_root = std::fs::canonicalize(moltis_config::data_dir().join("media"))?;
    if !candidate.starts_with(&media_root) {
        anyhow::bail!("screenshot path outside media root");
    }
    Ok(candidate)
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
    use {
        super::*,
        std::{
            io::Write,
            path::Path,
            sync::{Mutex, OnceLock},
        },
    };

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct DataDirOverride;

    impl DataDirOverride {
        fn new(path: &Path) -> Self {
            moltis_config::set_data_dir(path.to_path_buf());
            Self
        }
    }

    impl Drop for DataDirOverride {
        fn drop(&mut self) {
            moltis_config::clear_data_dir();
        }
    }

    fn prepare_media_file() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).expect("media dir");
        let path = media.join("screenshot-test.png");
        let mut file = std::fs::File::create(&path).expect("file create");
        file.write_all(b"png-bytes").expect("file write");
        (dir, path.display().to_string())
    }

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
        let _guard = env_lock().lock().expect("env lock");
        let (dir, path) = prepare_media_file();
        let _data_dir = DataDirOverride::new(dir.path());

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
        let _guard = env_lock().lock().expect("env lock");
        let (dir, path) = prepare_media_file();
        let _data_dir = DataDirOverride::new(dir.path());

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

    #[tokio::test]
    async fn rejects_paths_outside_media_root() {
        let _guard = env_lock().lock().expect("env lock");
        let dir = tempfile::tempdir().expect("tempdir");
        let media = dir.path().join("media");
        std::fs::create_dir_all(&media).expect("media dir");
        let _data_dir = DataDirOverride::new(dir.path());

        let outside = tempfile::NamedTempFile::new().expect("outside file");
        let outside_path = outside.path().display().to_string();

        let hook = ScreenshotResolverHook::new();
        let payload = HookPayload::BeforeLLMCall {
            session_key: "s1".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            messages: serde_json::json!([
                {"role": "tool", "content": format!("{{\"screenshot_path\": \"{outside_path}\"}}")}
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
