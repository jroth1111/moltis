use std::{path::PathBuf, sync::Arc, time::Duration};

use {
    async_trait::async_trait,
    base64::{Engine as _, engine::general_purpose::STANDARD as BASE64},
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
};

use crate::{
    Error,
    exec::ExecOpts,
    file_io_common::{
        DEFAULT_READ_LIMIT_BYTES, MAX_FILE_BYTES, format_with_line_numbers, normalize_sandbox_path,
        resolve_host_read_path, shell_single_quote,
    },
    params::{require_str, u64_param},
    sandbox::SandboxRouter,
};

const SANDBOX_TOO_LARGE_PREFIX: &str = "__MOLTIS_READ_FILE_TOO_LARGE__:";

#[derive(Default)]
pub struct ReadFileTool {
    sandbox_router: Option<Arc<SandboxRouter>>,
}

impl ReadFileTool {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_sandbox_router(mut self, router: Arc<SandboxRouter>) -> Self {
        self.sandbox_router = Some(router);
        self
    }

    async fn read_host_bytes(path: &str) -> crate::Result<Vec<u8>> {
        let canonical = resolve_host_read_path(path).await?;
        let metadata = tokio::fs::metadata(&canonical).await.map_err(|e| {
            Error::message(format!(
                "failed to read metadata for '{}': {e}",
                canonical.display()
            ))
        })?;

        if !metadata.is_file() {
            return Err(Error::message(format!(
                "'{}' is not a regular file",
                canonical.display()
            )));
        }

        if metadata.len() > MAX_FILE_BYTES as u64 {
            return Err(Error::message(format!(
                "file is too large ({:.1} MB) — maximum is {:.0} MB",
                metadata.len() as f64 / (1024.0 * 1024.0),
                MAX_FILE_BYTES as f64 / (1024.0 * 1024.0),
            )));
        }

        tokio::fs::read(&canonical).await.map_err(|e| {
            Error::message(format!(
                "failed to read file '{}': {e}",
                canonical.display()
            ))
        })
    }

    async fn read_sandbox_bytes(
        router: &SandboxRouter,
        session_key: &str,
        path: &str,
    ) -> crate::Result<Vec<u8>> {
        let logical_path = normalize_sandbox_path(path)?;
        let sandbox_id = router.sandbox_id_for(session_key);
        let image = router.resolve_image(session_key, None).await;
        let backend = router.backend();
        backend.ensure_ready(&sandbox_id, Some(&image)).await?;

        let quoted_path = shell_single_quote(&logical_path);
        let command = format!(
            "if [ ! -f {quoted_path} ]; then \\
                 echo \"path is not a regular file\" >&2; \\
                 exit 2; \\
             fi; \\
             size=$(wc -c < {quoted_path}); \\
             if [ \"$size\" -gt {MAX_FILE_BYTES} ]; then \\
                 echo \"{SANDBOX_TOO_LARGE_PREFIX}$size\" >&2; \\
                 exit 3; \\
             fi; \\
             base64 < {quoted_path} | tr -d '\\n'"
        );

        let result = backend
            .exec(
                &sandbox_id,
                &command,
                &ExecOpts {
                    timeout: Duration::from_secs(30),
                    max_output_bytes: MAX_FILE_BYTES * 2,
                    working_dir: Some(PathBuf::from("/home/sandbox")),
                    env: Vec::new(),
                },
            )
            .await?;

        if result.exit_code != 0 {
            if let Some(size_str) = result
                .stderr
                .lines()
                .find_map(|line| line.strip_prefix(SANDBOX_TOO_LARGE_PREFIX))
                && let Ok(size) = size_str.trim().parse::<u64>()
            {
                return Err(Error::message(format!(
                    "file is too large ({:.1} MB) — maximum is {:.0} MB",
                    size as f64 / (1024.0 * 1024.0),
                    MAX_FILE_BYTES as f64 / (1024.0 * 1024.0),
                )));
            }

            let detail = if !result.stderr.trim().is_empty() {
                result.stderr.trim().to_string()
            } else if !result.stdout.trim().is_empty() {
                result.stdout.trim().to_string()
            } else {
                format!("sandbox command failed with exit code {}", result.exit_code)
            };
            return Err(Error::message(format!(
                "failed to read sandbox file '{}': {detail}",
                logical_path
            )));
        }

        let bytes = BASE64
            .decode(result.stdout.trim())
            .map_err(|e| Error::message(format!("failed to decode sandbox file payload: {e}")))?;

        if bytes.len() > MAX_FILE_BYTES {
            return Err(Error::message(format!(
                "file is too large ({:.1} MB) — maximum is {:.0} MB",
                bytes.len() as f64 / (1024.0 * 1024.0),
                MAX_FILE_BYTES as f64 / (1024.0 * 1024.0),
            )));
        }

        Ok(bytes)
    }

    async fn read_bytes_for_session(&self, session_key: &str, path: &str) -> crate::Result<Vec<u8>> {
        if let Some(ref router) = self.sandbox_router {
            let has_container_backend = !matches!(router.backend_name(), "none" | "restricted-host");
            if has_container_backend && router.is_sandboxed(session_key).await {
                return Self::read_sandbox_bytes(router, session_key, path).await;
            }
        }

        Self::read_host_bytes(path).await
    }
}

#[async_trait]
impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file from the local workspace. Returns numbered lines for easier references."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative path to the file"
                },
                "offset": {
                    "type": "integer",
                    "description": "Byte offset to start reading from",
                    "default": 0
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum bytes to include in output (default 65536, max 1048576)",
                    "default": 65536
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let path = require_str(&params, "path")?;
        let session_key = params
            .get("_session_key")
            .and_then(Value::as_str)
            .unwrap_or("main");
        let offset = u64_param(&params, "offset", 0) as usize;
        let limit = u64_param(&params, "limit", DEFAULT_READ_LIMIT_BYTES as u64)
            .clamp(1, MAX_FILE_BYTES as u64) as usize;

        let bytes = self.read_bytes_for_session(session_key, path).await?;
        let total_size = bytes.len();
        let bounded_offset = offset.min(total_size);
        let end = bounded_offset.saturating_add(limit).min(total_size);
        let slice = &bytes[bounded_offset..end];

        let start_line = bytes[..bounded_offset]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1;
        let content = String::from_utf8_lossy(slice).into_owned();
        let numbered = format_with_line_numbers(&content, start_line);

        Ok(json!({
            "path": path,
            "offset": bounded_offset,
            "limit": limit,
            "size": total_size,
            "bytes_read": slice.len(),
            "truncated": end < total_size,
            "start_line": start_line,
            "content": numbered,
        }))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serde_json::json,
        std::{
            fs,
            io::Write,
            sync::{Mutex, OnceLock},
        },
        tempfile::tempdir,
    };

    fn data_dir_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn read_file_rejects_parent_traversal() {
        let tool = ReadFileTool::new();
        let err = tool.execute(json!({"path": "../secret.txt"})).await.unwrap_err();
        assert!(err.to_string().contains("path traversal"));
    }

    #[tokio::test]
    async fn read_file_returns_numbered_content() {
        let _guard = data_dir_lock().lock().unwrap_or_else(|e| e.into_inner());
        let previous_data_dir = moltis_config::data_dir();
        let temp = tempdir().unwrap();
        moltis_config::set_data_dir(temp.path().to_path_buf());
        let file_path = temp.path().join("notes.txt");
        let mut file = fs::File::create(&file_path).unwrap();
        writeln!(file, "first").unwrap();
        writeln!(file, "second").unwrap();
        writeln!(file, "third").unwrap();

        let tool = ReadFileTool::new();
        let payload = tool
            .execute(json!({
                "path": file_path.to_string_lossy(),
                "offset": 6,
                "limit": 32
            }))
            .await
            .unwrap();

        let content = payload.get("content").and_then(Value::as_str).unwrap();
        assert!(content.contains("2 | second"));
        assert!(content.contains("3 | third"));
        assert_eq!(payload.get("start_line").and_then(Value::as_u64), Some(2));

        moltis_config::set_data_dir(previous_data_dir);
    }
}
