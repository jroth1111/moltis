use std::{path::PathBuf, sync::Arc, time::Duration};

use {
    async_trait::async_trait,
    base64::{Engine as _, engine::general_purpose::STANDARD as BASE64},
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    tokio::io::AsyncWriteExt,
    tracing::info,
};

use crate::{
    Error,
    approval::{ApprovalDecision, ApprovalManager, cleanup_approval, persist_approval},
    exec::{ApprovalBroadcaster, ExecOpts},
    file_io_common::{
        MAX_FILE_BYTES, is_memory_scoped_host_path, is_memory_scoped_sandbox_path,
        normalize_sandbox_path, resolve_host_write_path, shell_single_quote,
    },
    params::{bool_param, require_str},
    sandbox::SandboxRouter,
};

#[derive(Default)]
pub struct WriteFileTool {
    sandbox_router: Option<Arc<SandboxRouter>>,
    approval_manager: Option<Arc<ApprovalManager>>,
    approval_store: Option<Arc<sqlx::SqlitePool>>,
    broadcaster: Option<Arc<dyn ApprovalBroadcaster>>,
}

impl WriteFileTool {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_sandbox_router(mut self, router: Arc<SandboxRouter>) -> Self {
        self.sandbox_router = Some(router);
        self
    }

    pub fn with_approval(
        mut self,
        manager: Arc<ApprovalManager>,
        broadcaster: Arc<dyn ApprovalBroadcaster>,
    ) -> Self {
        self.approval_manager = Some(manager);
        self.broadcaster = Some(broadcaster);
        self
    }

    pub fn with_approval_store(mut self, pool: Arc<sqlx::SqlitePool>) -> Self {
        self.approval_store = Some(pool);
        self
    }

    async fn request_write_approval(
        &self,
        session_key: &str,
        target: &str,
        append: bool,
        bytes: usize,
    ) -> crate::Result<()> {
        let Some(ref manager) = self.approval_manager else {
            return Ok(());
        };

        let command = format!("write_file path={target} append={append} bytes={bytes}");
        let (request_id, rx) = manager.create_request(&command).await;

        if let Some(ref pool) = self.approval_store
            && let Err(error) = persist_approval(
                pool.as_ref(),
                &request_id,
                session_key,
                "write_file",
                &command,
            )
            .await
        {
            info!(
                request_id,
                session_key,
                error = %error,
                "failed to persist write_file approval"
            );
        }

        if let Some(ref broadcaster) = self.broadcaster {
            broadcaster.broadcast_request(&request_id, &command).await?;
        }

        let decision = manager.wait_for_decision(rx).await;

        if let Some(ref pool) = self.approval_store {
            let _ = cleanup_approval(pool.as_ref(), &request_id).await;
        }

        match decision {
            ApprovalDecision::Approved => Ok(()),
            ApprovalDecision::Denied => Err(Error::message("write_file denied by user")),
            ApprovalDecision::Timeout => Err(Error::message("write_file approval timed out")),
        }
    }

    async fn write_host_file(path: &PathBuf, content: &str, append: bool) -> crate::Result<usize> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                Error::message(format!(
                    "failed to create parent directory '{}': {e}",
                    parent.display()
                ))
            })?;
        }

        if append {
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(|e| {
                    Error::message(format!("failed to open file '{}': {e}", path.display()))
                })?;
            file.write_all(content.as_bytes()).await.map_err(|e| {
                Error::message(format!("failed to append file '{}': {e}", path.display()))
            })?;
            file.flush().await.map_err(|e| {
                Error::message(format!("failed to flush file '{}': {e}", path.display()))
            })?;
            return Ok(content.len());
        }

        tokio::fs::write(path, content).await.map_err(|e| {
            Error::message(format!("failed to write file '{}': {e}", path.display()))
        })?;
        Ok(content.len())
    }

    async fn write_sandbox_file(
        router: &SandboxRouter,
        session_key: &str,
        path: &str,
        content: &str,
        append: bool,
    ) -> crate::Result<usize> {
        let logical_path = normalize_sandbox_path(path)?;
        let sandbox_id = router.sandbox_id_for(session_key);
        let image = router.resolve_image(session_key, None).await;
        let backend = router.backend();
        backend.ensure_ready(&sandbox_id, Some(&image)).await?;

        let encoded = BASE64.encode(content.as_bytes());
        let redirection = if append {
            ">>"
        } else {
            ">"
        };
        let quoted_path = shell_single_quote(&logical_path);
        let quoted_payload = shell_single_quote(&encoded);
        let command = format!(
            "if [ -d {quoted_path} ]; then \\
                 echo \"path is a directory\" >&2; \\
                 exit 2; \\
             fi; \\
             dir=$(dirname {quoted_path}); \\
             mkdir -p \"$dir\"; \\
             printf %s {quoted_payload} | base64 -d {redirection} {quoted_path}"
        );

        let result = backend
            .exec(
                &sandbox_id,
                &command,
                &ExecOpts {
                    timeout: Duration::from_secs(30),
                    max_output_bytes: 16 * 1024,
                    working_dir: Some(PathBuf::from("/home/sandbox")),
                    env: Vec::new(),
                },
            )
            .await?;

        if result.exit_code != 0 {
            let detail = if !result.stderr.trim().is_empty() {
                result.stderr.trim().to_string()
            } else if !result.stdout.trim().is_empty() {
                result.stdout.trim().to_string()
            } else {
                format!("sandbox command failed with exit code {}", result.exit_code)
            };
            return Err(Error::message(format!(
                "failed to write sandbox file '{}': {detail}",
                logical_path
            )));
        }

        Ok(content.len())
    }
}

#[async_trait]
impl AgentTool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write text to a local file. Non-memory targets require explicit approval."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute or relative file path to write"
                },
                "content": {
                    "type": "string",
                    "description": "Text content to write"
                },
                "append": {
                    "type": "boolean",
                    "description": "Append to an existing file instead of replacing it",
                    "default": false
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let path = require_str(&params, "path")?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing required parameter: content"))?;
        if content.len() > MAX_FILE_BYTES {
            return Err(Error::message(format!(
                "content is too large ({:.1} MB) — maximum is {:.0} MB",
                content.len() as f64 / (1024.0 * 1024.0),
                MAX_FILE_BYTES as f64 / (1024.0 * 1024.0),
            ))
            .into());
        }

        let append = bool_param(&params, "append", false);
        let session_key = params
            .get("_session_key")
            .and_then(Value::as_str)
            .unwrap_or("main");

        if let Some(ref router) = self.sandbox_router {
            let has_container_backend =
                !matches!(router.backend_name(), "none" | "restricted-host");
            if has_container_backend && router.is_sandboxed(session_key).await {
                let sandbox_path = normalize_sandbox_path(path)?;
                if !is_memory_scoped_sandbox_path(&sandbox_path) {
                    self.request_write_approval(session_key, &sandbox_path, append, content.len())
                        .await?;
                }

                let bytes_written =
                    Self::write_sandbox_file(router, session_key, path, content, append).await?;
                return Ok(json!({
                    "path": sandbox_path,
                    "append": append,
                    "bytes_written": bytes_written,
                    "sandboxed": true,
                }));
            }
        }

        let canonical_path = resolve_host_write_path(path).await?;
        if !is_memory_scoped_host_path(&canonical_path) {
            self.request_write_approval(
                session_key,
                &canonical_path.display().to_string(),
                append,
                content.len(),
            )
            .await?;
        }

        let bytes_written = Self::write_host_file(&canonical_path, content, append).await?;
        Ok(json!({
            "path": canonical_path.to_string_lossy(),
            "append": append,
            "bytes_written": bytes_written,
            "sandboxed": false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        serde_json::json,
        std::sync::{Mutex, OnceLock},
        tempfile::tempdir,
        tokio::time::{Duration, sleep},
    };

    fn data_dir_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct NoopBroadcaster;

    #[async_trait]
    impl ApprovalBroadcaster for NoopBroadcaster {
        async fn broadcast_request(&self, _request_id: &str, _command: &str) -> crate::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn write_file_requires_approval_outside_memory_scope() {
        let _guard = data_dir_lock().lock().unwrap_or_else(|e| e.into_inner());
        let previous_data_dir = moltis_config::data_dir();
        let temp = tempdir().unwrap();
        moltis_config::set_data_dir(temp.path().to_path_buf());

        let manager = Arc::new(ApprovalManager::default());
        let tool = WriteFileTool::new().with_approval(manager.clone(), Arc::new(NoopBroadcaster));

        let target = temp.path().join("notes").join("todo.txt");
        let params = json!({
            "path": target.to_string_lossy(),
            "content": "ship it",
            "_session_key": "main"
        });

        let manager_for_task = manager.clone();
        let approve_task = tokio::spawn(async move {
            for _ in 0..20 {
                let ids = manager_for_task.pending_ids().await;
                if let Some(id) = ids.first() {
                    manager_for_task
                        .resolve(id, ApprovalDecision::Approved, Some("write_file"))
                        .await;
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        });

        let result = tool.execute(params).await.unwrap();
        approve_task.await.unwrap();

        assert_eq!(result.get("bytes_written").and_then(Value::as_u64), Some(7));
        let written = tokio::fs::read_to_string(target).await.unwrap();
        assert_eq!(written, "ship it");

        moltis_config::set_data_dir(previous_data_dir);
    }

    #[tokio::test]
    async fn write_file_auto_approves_memory_scope() {
        let _guard = data_dir_lock().lock().unwrap_or_else(|e| e.into_inner());
        let previous_data_dir = moltis_config::data_dir();
        let temp = tempdir().unwrap();
        moltis_config::set_data_dir(temp.path().to_path_buf());

        let manager = Arc::new(ApprovalManager::default());
        let tool = WriteFileTool::new().with_approval(manager.clone(), Arc::new(NoopBroadcaster));

        let memory_dir = temp.path().join("memory");
        tokio::fs::create_dir_all(&memory_dir).await.unwrap();
        let target = memory_dir.join("daily.md");

        tool.execute(json!({
            "path": target.to_string_lossy(),
            "content": "remember this",
            "_session_key": "main"
        }))
        .await
        .unwrap();

        let pending = manager.pending_ids().await;
        assert!(pending.is_empty());
        let written = tokio::fs::read_to_string(target).await.unwrap();
        assert_eq!(written, "remember this");

        moltis_config::set_data_dir(previous_data_dir);
    }
}
