//! Task board tool: a simple task management tool backed by the session state store.
//!
//! **Deprecated.** Use the `task_list` tool (backed by `moltis-tasks` with a formal
//! state machine, CAS writes, and event logging) instead.
//! This module will be removed in a future release.

use std::sync::Arc;

use {
    async_trait::async_trait,
    serde_json::{Value, json},
};

use moltis_sessions::state_store::SessionStateStore;

/// Agent tool for managing a shared task board via the session state store.
#[deprecated(
    since = "0.1.0",
    note = "Use the `task_list` tool (backed by `moltis-tasks`) instead. \
            TaskBoardTool will be removed in a future release."
)]
pub struct TaskBoardTool {
    state_store: Arc<SessionStateStore>,
}

const NAMESPACE: &str = "task_board";

impl TaskBoardTool {
    pub fn new(state_store: Arc<SessionStateStore>) -> Self {
        Self { state_store }
    }

    async fn create_task(&self, session_key: &str, subject: &str) -> anyhow::Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let task = json!({
            "id": id,
            "subject": subject,
            "status": "pending",
            "created_at": now,
        });
        self.state_store
            .set(session_key, NAMESPACE, &id, &task.to_string())
            .await?;
        Ok(task)
    }

    async fn list_tasks(&self, session_key: &str) -> anyhow::Result<Value> {
        let entries = self.state_store.list(session_key, NAMESPACE).await?;
        let tasks: Vec<Value> = entries
            .into_iter()
            .filter_map(|e| serde_json::from_str(&e.value).ok())
            .collect();
        Ok(json!({ "tasks": tasks }))
    }

    async fn update_task(
        &self,
        session_key: &str,
        id: &str,
        status: &str,
    ) -> anyhow::Result<Value> {
        let existing = self.state_store.get(session_key, NAMESPACE, id).await?;
        let Some(raw) = existing else {
            return Ok(json!({ "error": "task not found" }));
        };
        let mut task: Value = serde_json::from_str(&raw)?;
        task["status"] = json!(status);
        self.state_store
            .set(session_key, NAMESPACE, id, &task.to_string())
            .await?;
        Ok(task)
    }

    async fn delete_task(&self, session_key: &str, id: &str) -> anyhow::Result<Value> {
        let deleted = self.state_store.delete(session_key, NAMESPACE, id).await?;
        Ok(json!({ "deleted": deleted }))
    }
}

#[async_trait]
impl moltis_agents::tool_registry::AgentTool for TaskBoardTool {
    fn name(&self) -> &str {
        "task_board"
    }

    fn description(&self) -> &str {
        "Manage a shared task board. Operations: create (subject), list, update (id, status), delete (id)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "update", "delete"],
                    "description": "The action to perform"
                },
                "subject": {
                    "type": "string",
                    "description": "Subject for new task (required for create)"
                },
                "id": {
                    "type": "string",
                    "description": "Task ID (required for update, delete)"
                },
                "status": {
                    "type": "string",
                    "description": "New status (required for update)"
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let session_key = params
            .get("_session_key")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'action' parameter"))?;

        match action {
            "create" => {
                let subject = params
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'create' requires 'subject'"))?;
                self.create_task(session_key, subject).await
            },
            "list" => self.list_tasks(session_key).await,
            "update" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'update' requires 'id'"))?;
                let status = params
                    .get("status")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'update' requires 'status'"))?;
                self.update_task(session_key, id, status).await
            },
            "delete" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("'delete' requires 'id'"))?;
                self.delete_task(session_key, id).await
            },
            _ => Ok(json!({ "error": format!("unknown action: {action}") })),
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used, deprecated)]
#[cfg(test)]
mod tests {
    use super::*;
    use moltis_agents::tool_registry::AgentTool;

    async fn test_store() -> Arc<SessionStateStore> {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS session_state (
                session_key TEXT NOT NULL,
                namespace   TEXT NOT NULL,
                key         TEXT NOT NULL,
                value       TEXT NOT NULL,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (session_key, namespace, key)
            )"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        Arc::new(SessionStateStore::new(pool))
    }

    #[tokio::test]
    async fn create_and_list_tasks() {
        let tool = TaskBoardTool::new(test_store().await);

        let created = tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "Ship fix",
            }))
            .await
            .unwrap();
        assert_eq!(created["status"], "pending");

        let listed = tool.execute(serde_json::json!({ "action": "list" })).await.unwrap();
        let tasks = listed["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["subject"], "Ship fix");
    }

    #[tokio::test]
    async fn update_and_delete_task() {
        let tool = TaskBoardTool::new(test_store().await);

        let created = tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "Review PR",
            }))
            .await
            .unwrap();
        let id = created["id"].as_str().expect("id").to_string();

        let updated = tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "status": "done",
            }))
            .await
            .unwrap();
        assert_eq!(updated["status"], "done");

        let deleted = tool
            .execute(serde_json::json!({
                "action": "delete",
                "id": updated["id"],
            }))
            .await
            .unwrap();
        assert_eq!(deleted["deleted"], true);
    }
}
