use std::{path::PathBuf, sync::Arc};

use {
    anyhow::Result,
    async_trait::async_trait,
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    tracing::{debug, warn},
};

use crate::{SessionLock, funnel};

pub struct TinderBrowserTool {
    pool: Arc<sqlx::SqlitePool>,
    data_dir: PathBuf,
}

const DEFAULT_SESSION_ID: &str = "tinder-default";

impl TinderBrowserTool {
    pub fn new(pool: Arc<sqlx::SqlitePool>, data_dir: PathBuf) -> Self {
        Self { pool, data_dir }
    }
}

#[async_trait]
impl AgentTool for TinderBrowserTool {
    fn name(&self) -> &str {
        "tinder_browser"
    }

    fn description(&self) -> &str {
        "Browser automation for Tinder: navigate, type, click, screenshot with session persistence."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["navigate", "type", "click", "screenshot", "scroll"],
                    "description": "Browser command to execute"
                },
                "url": { "type": "string", "description": "URL for navigate" },
                "selector": { "type": "string", "description": "CSS selector for type/click" },
                "text": { "type": "string", "description": "Text to type" },
                "match_id": { "type": "string", "description": "Match ID for exchange tracking" },
                "session_id": { "type": "string", "description": "Browser session ID" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, params: Value) -> Result<Value> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing command"))?;

        let raw_session_id = params["session_id"].as_str().unwrap_or(DEFAULT_SESSION_ID);
        let session_id = sanitize_session_id(raw_session_id)?;

        // Guard browser state and tinder DB writes from concurrent runs.
        let lock = SessionLock::new(self.pool.as_ref().clone(), session_id.clone());
        if !lock.try_acquire().await? {
            return Ok(json!({
                "status": "skipped",
                "error": format!("session lock held for {session_id}")
            }));
        }

        let result = self.execute_with_lock(command, &session_id, &params).await;

        if let Err(e) = lock.release().await {
            warn!(session = %session_id, error = %e, "failed to release tinder session lock");
        }

        result
    }
}

fn sanitize_session_id(session_id: &str) -> Result<String> {
    if session_id.is_empty() {
        anyhow::bail!("session_id cannot be empty");
    }
    if session_id.len() > 64 {
        anyhow::bail!("session_id too long (max 64)");
    }
    if !session_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        anyhow::bail!("session_id contains invalid characters");
    }
    Ok(session_id.to_string())
}

impl TinderBrowserTool {
    async fn execute_with_lock(
        &self,
        command: &str,
        session_id: &str,
        params: &Value,
    ) -> Result<Value> {
        let state_dir = self.data_dir.join("browser_state");
        tokio::fs::create_dir_all(&state_dir).await?;
        let state_path = state_dir.join(format!("{session_id}.json"));

        // Build the moltis-browser CLI arguments.
        let mut args: Vec<String> = vec![
            command.to_string(),
            "--state-load".to_string(),
            state_path.display().to_string(),
            "--state-save".to_string(),
            state_path.display().to_string(),
        ];

        if let Some(url) = params["url"].as_str() {
            args.push("--url".to_string());
            args.push(url.to_string());
        }
        if let Some(selector) = params["selector"].as_str() {
            args.push("--selector".to_string());
            args.push(selector.to_string());
        }
        if let Some(text) = params["text"].as_str() {
            args.push("--text".to_string());
            args.push(text.to_string());
        }

        debug!(command = %command, session_id = %session_id, "spawning browser subprocess");

        let output = tokio::process::Command::new("moltis-browser")
            .args(&args)
            .output()
            .await;

        let output = match output {
            Ok(o) => o,
            Err(e) => {
                return Ok(json!({
                    "status": "error",
                    "error": format!("failed to spawn moltis-browser: {e}")
                }));
            },
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            warn!(command = %command, stderr = %stderr, "browser command failed");
            return Ok(json!({
                "status": "error",
                "error": stderr,
                "stdout": stdout
            }));
        }

        // On successful "type" command, increment exchange count.
        let mut exchange_warning: Option<String> = None;
        let exchange_synced = if command == "type" {
            if let Some(match_id) = params["match_id"].as_str() {
                match funnel::increment_exchange(&self.pool, match_id).await {
                    Ok(_) => true,
                    Err(e) => {
                        warn!(match_id = %match_id, error = %e, "failed to increment exchange count");
                        exchange_warning = Some(format!(
                            "message sent but exchange_count update failed for match_id={match_id}: {e}"
                        ));
                        false
                    },
                }
            } else {
                true
            }
        } else {
            true
        };

        // Handle screenshots: detect base64 image data in output.
        let mut screenshot_path: Option<String> = None;
        if command == "screenshot"
            && let Some(b64_data) = extract_base64_image(&stdout)
        {
            match decode_and_save_screenshot(b64_data, &self.data_dir).await {
                Ok(path) => screenshot_path = Some(path),
                Err(e) => warn!(error = %e, "failed to process screenshot"),
            }
        }

        let mut result = json!({
            "status": "ok",
            "output": stdout,
            "exchange_count_synced": exchange_synced
        });
        if let Some(warning) = exchange_warning {
            result["warning"] = json!(warning);
        }
        if let Some(path) = screenshot_path {
            result["screenshot_path"] = json!(path);
        }

        Ok(result)
    }
}

/// Try to extract a base64-encoded image from the browser output.
fn extract_base64_image(output: &str) -> Option<&str> {
    // Look for a data URL or raw base64 block.
    if let Some(idx) = output.find("data:image/png;base64,") {
        let start = idx + "data:image/png;base64,".len();
        let end = output[start..]
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .map(|i| start + i)
            .unwrap_or(output.len());
        return Some(&output[start..end]);
    }
    None
}

/// Decode base64 image, resize to max 768px, and save to the media directory.
async fn decode_and_save_screenshot(b64: &str, data_dir: &std::path::Path) -> Result<String> {
    use {base64::Engine, image::GenericImageView};

    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
    let img =
        image::load_from_memory(&bytes).map_err(|e| anyhow::anyhow!("image decode error: {e}"))?;

    let (w, h) = img.dimensions();
    let max_dim = 768u32;
    let resized = if w > max_dim || h > max_dim {
        let scale = max_dim as f64 / w.max(h) as f64;
        let nw = (w as f64 * scale) as u32;
        let nh = (h as f64 * scale) as u32;
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3)
    };

    let media_dir = data_dir.join("media");
    tokio::fs::create_dir_all(&media_dir).await?;
    let filename = format!("screenshot-{}.png", uuid::Uuid::new_v4());
    let filepath = media_dir.join(&filename);

    resized.save(&filepath)?;
    Ok(filepath.display().to_string())
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::sanitize_session_id;

    #[test]
    fn accepts_safe_session_id() {
        let id = sanitize_session_id("session-123_abc").expect("valid id");
        assert_eq!(id, "session-123_abc");
    }

    #[test]
    fn rejects_traversal_session_id() {
        assert!(sanitize_session_id("../escape").is_err());
        assert!(sanitize_session_id("/abs/path").is_err());
        assert!(sanitize_session_id("name with spaces").is_err());
    }
}
