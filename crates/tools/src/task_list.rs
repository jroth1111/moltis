//! Shared task list tool for inter-agent task coordination.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use {
    async_trait::async_trait,
    serde::{Deserialize, Serialize},
    tokio::sync::RwLock,
};

use {
    crate::{
        Error,
        params::{require_str, str_param, str_param_any},
    },
    moltis_agents::tool_registry::AgentTool,
};

/// Status of a task in the shared list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = Error;

    fn from_str(input: &str) -> crate::Result<Self> {
        match input {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            other => Err(Error::message(format!("unknown task status: {other}"))),
        }
    }
}

/// A single task in the shared list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    #[serde(default)]
    pub description: String,
    pub status: TaskStatus,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Evidence artifact attached when a task is marked completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<String>,
    /// Epoch seconds when the task was marked completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    /// Trace ID of the last mutation (for audit traceability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trace_id: Option<String>,
}

/// File-backed store for one logical task list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskList {
    pub next_id: u64,
    pub tasks: HashMap<String, Task>,
}

impl Default for TaskList {
    fn default() -> Self {
        Self {
            next_id: 1,
            tasks: HashMap::new(),
        }
    }
}

/// Thread-safe, file-backed task store.
pub struct TaskStore {
    data_dir: PathBuf,
    lists: RwLock<HashMap<String, TaskList>>,
}

fn would_create_cycle(tasks: &HashMap<String, Task>, task_id: &str, new_deps: &[String]) -> bool {
    let mut visited = HashSet::new();
    let mut stack = new_deps.to_vec();
    while let Some(current) = stack.pop() {
        if current == task_id {
            return true;
        }
        if visited.insert(current.clone()) {
            if let Some(t) = tasks.get(&current) {
                stack.extend(t.blocked_by.iter().cloned());
            }
        }
    }
    false
}

impl TaskStore {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            data_dir: base_dir.join("tasks"),
            lists: RwLock::new(HashMap::new()),
        }
    }

    fn file_path(&self, list_id: &str) -> PathBuf {
        self.data_dir.join(format!("{list_id}.json"))
    }

    async fn ensure_list(&self, list_id: &str) -> crate::Result<()> {
        let mut lists = self.lists.write().await;
        if lists.contains_key(list_id) {
            return Ok(());
        }

        let path = self.file_path(list_id);
        let list = if path.exists() {
            let data = tokio::fs::read_to_string(&path).await.map_err(|e| {
                Error::message(format!("failed to read task list '{list_id}': {e}"))
            })?;
            serde_json::from_str::<TaskList>(&data).map_err(|e| {
                Error::message(format!("failed to parse task list '{list_id}' JSON: {e}"))
            })?
        } else {
            TaskList::default()
        };
        lists.insert(list_id.to_string(), list);
        Ok(())
    }

    async fn persist(&self, list_id: &str) -> crate::Result<()> {
        let lists = self.lists.read().await;
        let Some(list) = lists.get(list_id) else {
            return Ok(());
        };
        tokio::fs::create_dir_all(&self.data_dir)
            .await
            .map_err(|e| Error::message(format!("failed to create task dir: {e}")))?;
        let payload = serde_json::to_string_pretty(list).map_err(|e| {
            Error::message(format!("failed to serialize task list '{list_id}': {e}"))
        })?;
        tokio::fs::write(self.file_path(list_id), payload)
            .await
            .map_err(|e| Error::message(format!("failed to write task list '{list_id}': {e}")))?;
        Ok(())
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub async fn create(
        &self,
        list_id: &str,
        subject: String,
        description: String,
    ) -> crate::Result<Task> {
        self.ensure_list(list_id).await?;
        let mut lists = self.lists.write().await;
        let list = lists
            .get_mut(list_id)
            .ok_or_else(|| Error::message(format!("missing task list: {list_id}")))?;

        let id = list.next_id.to_string();
        list.next_id = list.next_id.saturating_add(1);
        let now = Self::now();
        let task = Task {
            id: id.clone(),
            subject,
            description,
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: Vec::new(),
            created_at: now,
            updated_at: now,
            proof: None,
            completed_at: None,
            last_trace_id: None,
        };
        list.tasks.insert(id, task.clone());
        drop(lists);
        self.persist(list_id).await?;
        Ok(task)
    }

    pub async fn list_tasks(
        &self,
        list_id: &str,
        status_filter: Option<&TaskStatus>,
    ) -> crate::Result<Vec<Task>> {
        self.ensure_list(list_id).await?;
        let lists = self.lists.read().await;
        let list = lists
            .get(list_id)
            .ok_or_else(|| Error::message(format!("missing task list: {list_id}")))?;

        let mut tasks: Vec<Task> = list
            .tasks
            .values()
            .filter(|t| status_filter.is_none_or(|s| &t.status == s))
            .cloned()
            .collect();
        tasks.sort_by_key(|t| t.id.parse::<u64>().unwrap_or(0));
        Ok(tasks)
    }

    pub async fn get(&self, list_id: &str, task_id: &str) -> crate::Result<Option<Task>> {
        self.ensure_list(list_id).await?;
        let lists = self.lists.read().await;
        let list = lists
            .get(list_id)
            .ok_or_else(|| Error::message(format!("missing task list: {list_id}")))?;
        Ok(list.tasks.get(task_id).cloned())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        list_id: &str,
        task_id: &str,
        status: Option<TaskStatus>,
        subject: Option<String>,
        description: Option<String>,
        owner: Option<String>,
        blocked_by: Option<Vec<String>>,
        proof: Option<String>,
        caller_identity: Option<&str>,
        trace_id: Option<&str>,
        force: bool,
    ) -> crate::Result<Task> {
        self.ensure_list(list_id).await?;
        let mut lists = self.lists.write().await;
        let list = lists
            .get_mut(list_id)
            .ok_or_else(|| Error::message(format!("missing task list: {list_id}")))?;

        // Validate before mutating.
        {
            let task = list
                .tasks
                .get(task_id)
                .ok_or_else(|| Error::message(format!("task not found: {task_id}")))?;

            // Ownership enforcement: non-owners cannot modify status or owner
            // unless `force` is set.
            if !force {
                if let (Some(current_owner), Some(caller)) = (&task.owner, caller_identity) {
                    let is_owner_mutation = status.is_some() || owner.is_some();
                    if is_owner_mutation && current_owner != caller {
                        return Err(Error::message(format!(
                            "task {task_id} is owned by '{current_owner}'; \
                             caller '{caller}' cannot modify status or owner"
                        )));
                    }
                }
            }

            // Status transition: Pending → Completed is forbidden (must pass through InProgress).
            if let Some(TaskStatus::Completed) = &status {
                if task.status == TaskStatus::Pending {
                    return Err(Error::message(format!(
                        "task {task_id} cannot transition from pending to completed directly; \
                         set status to in_progress first"
                    )));
                }
            }

            // Validate new blocked_by list.
            if let Some(ref deps) = blocked_by {
                // Self-reference check.
                if deps.contains(&task_id.to_string()) {
                    return Err(Error::message(format!(
                        "task {task_id} cannot block itself"
                    )));
                }
                // Existence check.
                for dep_id in deps {
                    if !list.tasks.contains_key(dep_id.as_str()) {
                        return Err(Error::message(format!(
                            "blocked_by refers to nonexistent task: {dep_id}"
                        )));
                    }
                }
                // Cycle check.
                if would_create_cycle(&list.tasks, task_id, deps) {
                    return Err(Error::message(format!(
                        "task {task_id}: setting blocked_by would create a dependency cycle"
                    )));
                }
            }

            // When transitioning to InProgress, check all current (or new) deps are completed.
            if let Some(TaskStatus::InProgress) = &status {
                let effective_deps = blocked_by.as_deref().unwrap_or(&task.blocked_by);
                let blocked: Vec<String> = effective_deps
                    .iter()
                    .filter(|dep_id| {
                        list.tasks
                            .get(dep_id.as_str())
                            .is_some_and(|dep| dep.status != TaskStatus::Completed)
                    })
                    .cloned()
                    .collect();
                if !blocked.is_empty() {
                    return Err(Error::message(format!(
                        "task {task_id} is blocked by incomplete tasks: {}",
                        blocked.join(", ")
                    )));
                }
            }
        }

        let completing = matches!(&status, Some(TaskStatus::Completed));

        let task = list
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| Error::message(format!("task not found: {task_id}")))?;

        if let Some(status) = status {
            task.status = status;
        }
        if let Some(subject) = subject {
            task.subject = subject;
        }
        if let Some(description) = description {
            task.description = description;
        }
        if let Some(owner) = owner {
            task.owner = Some(owner);
        }
        if let Some(blocked_by) = blocked_by {
            task.blocked_by = blocked_by;
        }
        task.updated_at = Self::now();

        // Completion tracking.
        if completing {
            task.completed_at = Some(Self::now());
            if let Some(proof) = proof {
                task.proof = Some(proof);
            } else {
                tracing::debug!(task_id, "task completed without proof artifact");
            }
        }

        // Trace ID audit trail.
        if let Some(tid) = trace_id {
            task.last_trace_id = Some(tid.to_string());
        }

        let updated = task.clone();
        drop(lists);
        self.persist(list_id).await?;
        Ok(updated)
    }

    /// Atomically claim a pending task and set it to in-progress.
    pub async fn claim(
        &self,
        list_id: &str,
        task_id: &str,
        owner: &str,
        trace_id: Option<&str>,
    ) -> crate::Result<Task> {
        self.ensure_list(list_id).await?;
        let mut lists = self.lists.write().await;
        let list = lists
            .get_mut(list_id)
            .ok_or_else(|| Error::message(format!("missing task list: {list_id}")))?;

        let (status, deps) = {
            let task = list
                .tasks
                .get(task_id)
                .ok_or_else(|| Error::message(format!("task not found: {task_id}")))?;
            (task.status.clone(), task.blocked_by.clone())
        };

        if status != TaskStatus::Pending {
            return Err(Error::message(format!(
                "task {task_id} cannot be claimed: current status is {}",
                status.as_str()
            )));
        }

        let blocked: Vec<String> = deps
            .iter()
            .filter(|dep_id| {
                list.tasks
                    .get(dep_id.as_str())
                    .is_some_and(|dep| dep.status != TaskStatus::Completed)
            })
            .cloned()
            .collect();
        if !blocked.is_empty() {
            return Err(Error::message(format!(
                "task {task_id} is blocked by incomplete tasks: {}",
                blocked.join(", ")
            )));
        }

        let task = list
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| Error::message(format!("task not found: {task_id}")))?;
        task.owner = Some(owner.to_string());
        task.status = TaskStatus::InProgress;
        task.updated_at = Self::now();
        if let Some(tid) = trace_id {
            task.last_trace_id = Some(tid.to_string());
        }

        let claimed = task.clone();
        drop(lists);
        self.persist(list_id).await?;
        Ok(claimed)
    }
}

/// Tool wrapper around [`TaskStore`].
pub struct TaskListTool {
    store: Arc<TaskStore>,
}

impl TaskListTool {
    pub fn new(base_dir: &Path) -> Self {
        Self {
            store: Arc::new(TaskStore::new(base_dir)),
        }
    }
}

#[async_trait]
impl AgentTool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }

    fn description(&self) -> &str {
        "Manage a shared task list for coordinated multi-agent execution. \
         Actions: create, list, get, update, claim."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "get", "update", "claim"],
                    "description": "Task list action to perform."
                },
                "list_id": {
                    "type": "string",
                    "description": "Task list identifier (default: default)."
                },
                "id": {
                    "type": "string",
                    "description": "Task ID for get/update/claim."
                },
                "subject": {
                    "type": "string",
                    "description": "Task subject for create/update."
                },
                "description": {
                    "type": "string",
                    "description": "Task description for create/update."
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "completed"],
                    "description": "Task status for list/update."
                },
                "owner": {
                    "type": "string",
                    "description": "Task owner for update/claim."
                },
                "blocked_by": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "List of task IDs that block this task."
                },
                "proof": {
                    "type": "string",
                    "description": "Evidence artifact to attach when completing a task."
                },
                "force": {
                    "type": "boolean",
                    "description": "Override ownership enforcement for update."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let action = require_str(&params, "action")?;
        let list_id = str_param_any(&params, &["list_id", "listId"]).unwrap_or("default");

        match action {
            "create" => {
                let subject = require_str(&params, "subject")?.to_string();
                let description = str_param(&params, "description").unwrap_or("").to_string();
                let task = self.store.create(list_id, subject, description).await?;
                Ok(serde_json::json!({
                    "ok": true,
                    "task": task,
                }))
            },
            "list" => {
                let status = str_param(&params, "status")
                    .map(str::parse::<TaskStatus>)
                    .transpose()?;
                let tasks = self.store.list_tasks(list_id, status.as_ref()).await?;
                Ok(serde_json::json!({
                    "ok": true,
                    "tasks": tasks,
                    "count": tasks.len(),
                }))
            },
            "get" => {
                let id = require_str(&params, "id")?;
                let task = self.store.get(list_id, id).await?;
                Ok(serde_json::json!({
                    "ok": task.is_some(),
                    "task": task,
                }))
            },
            "update" => {
                let id = require_str(&params, "id")?;
                let status = str_param(&params, "status")
                    .map(str::parse::<TaskStatus>)
                    .transpose()?;
                let subject = str_param(&params, "subject").map(String::from);
                let description = str_param(&params, "description").map(String::from);
                let owner = str_param(&params, "owner").map(String::from);
                let blocked_by = params
                    .get("blocked_by")
                    .and_then(serde_json::Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(String::from)
                            .collect::<Vec<_>>()
                    });
                let proof = str_param(&params, "proof").map(String::from);
                let caller_identity = str_param(&params, "_session_key");
                let trace_id = str_param(&params, "_trace_id");
                let force = params
                    .get("force")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let task = self
                    .store
                    .update(
                        list_id,
                        id,
                        status,
                        subject,
                        description,
                        owner,
                        blocked_by,
                        proof,
                        caller_identity,
                        trace_id,
                        force,
                    )
                    .await?;
                Ok(serde_json::json!({
                    "ok": true,
                    "task": task,
                }))
            },
            "claim" => {
                let id = require_str(&params, "id")?;
                let owner = str_param_any(&params, &["owner", "_session_key"])
                    .unwrap_or("agent")
                    .to_string();
                let trace_id = str_param(&params, "_trace_id");
                let task = self.store.claim(list_id, id, &owner, trace_id).await?;
                Ok(serde_json::json!({
                    "ok": true,
                    "task": task,
                }))
            },
            _ => Err(Error::message(format!("unknown task_list action: {action}")).into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

    fn tool(tmp: &tempfile::TempDir) -> TaskListTool {
        TaskListTool::new(tmp.path())
    }

    #[tokio::test]
    async fn create_and_list_tasks() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "first",
                "description": "desc"
            }))
            .await?;

        let result = task_tool
            .execute(serde_json::json!({
                "action": "list"
            }))
            .await?;
        assert_eq!(result["count"], 1);
        assert_eq!(result["tasks"][0]["subject"], "first");
        assert_eq!(result["tasks"][0]["status"], "pending");
        Ok(())
    }

    #[tokio::test]
    async fn claim_moves_task_to_in_progress() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "work"
            }))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing task id"))?;

        let claimed = task_tool
            .execute(serde_json::json!({
                "action": "claim",
                "id": id,
                "owner": "worker-a"
            }))
            .await?;
        assert_eq!(claimed["task"]["status"], "in_progress");
        assert_eq!(claimed["task"]["owner"], "worker-a");
        Ok(())
    }

    #[tokio::test]
    async fn claim_rejects_non_pending_task() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "work"
            }))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing task id"))?;

        // First set to in_progress
        task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "status": "in_progress"
            }))
            .await?;

        // Then set to completed
        task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "status": "completed"
            }))
            .await?;

        let result = task_tool
            .execute(serde_json::json!({
                "action": "claim",
                "id": id,
                "owner": "worker-a"
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected claim failure"))?;
        assert!(err.to_string().contains("cannot be claimed"));
        Ok(())
    }

    #[tokio::test]
    async fn claim_rejects_when_blocked_dependencies_incomplete() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let dep = task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "dep"
            }))
            .await?;
        let dep_id = dep["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing dep id"))?;

        let main = task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "main"
            }))
            .await?;
        let main_id = main["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing main id"))?;

        task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": main_id,
                "blocked_by": [dep_id]
            }))
            .await?;

        let result = task_tool
            .execute(serde_json::json!({
                "action": "claim",
                "id": main_id
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected blocked claim failure"))?;
        assert!(err.to_string().contains("blocked by incomplete tasks"));
        Ok(())
    }

    #[tokio::test]
    async fn update_rejects_pending_to_completed() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "work"
            }))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing task id"))?;

        let result = task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "status": "completed"
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected transition error"))?;
        assert!(err.to_string().contains("in_progress first"));
        Ok(())
    }

    #[tokio::test]
    async fn update_rejects_self_block() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "work"
            }))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing task id"))?;

        let result = task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "blocked_by": [id]
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected self-block error"))?;
        assert!(err.to_string().contains("cannot block itself"));
        Ok(())
    }

    #[tokio::test]
    async fn update_rejects_nonexistent_dep() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({
                "action": "create",
                "subject": "work"
            }))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing task id"))?;

        let result = task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "blocked_by": ["999"]
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected nonexistent dep error"))?;
        assert!(err.to_string().contains("nonexistent task"));
        Ok(())
    }

    #[tokio::test]
    async fn update_rejects_cycle() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);

        // Create task A and B
        let a = task_tool
            .execute(serde_json::json!({"action": "create", "subject": "A"}))
            .await?;
        let a_id = a["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing id"))?;

        let b = task_tool
            .execute(serde_json::json!({"action": "create", "subject": "B"}))
            .await?;
        let b_id = b["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing id"))?;

        // Set A blocked_by B (valid)
        task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": a_id,
                "blocked_by": [b_id]
            }))
            .await?;

        // Now try to set B blocked_by A — this creates a cycle
        let result = task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": b_id,
                "blocked_by": [a_id]
            }))
            .await;
        let err = result
            .err()
            .ok_or_else(|| std::io::Error::other("expected cycle error"))?;
        assert!(err.to_string().contains("cycle"));
        Ok(())
    }

    #[tokio::test]
    async fn update_allows_valid_transitions() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({"action": "create", "subject": "work"}))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing task id"))?;

        // Pending → InProgress is valid
        let result = task_tool
            .execute(serde_json::json!({"action": "update", "id": id, "status": "in_progress"}))
            .await?;
        assert_eq!(result["task"]["status"], "in_progress");

        // InProgress → Completed is valid
        let result = task_tool
            .execute(serde_json::json!({"action": "update", "id": id, "status": "completed"}))
            .await?;
        assert_eq!(result["task"]["status"], "completed");
        Ok(())
    }

    // ── Branch 7: Ownership enforcement ──────────────────────────────────

    #[tokio::test]
    async fn update_rejects_foreign_owner_status_change() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        // Claim it as agent-a.
        store.claim("l", &task.id, "agent-a", None).await?;
        // agent-b tries to change status → rejected.
        let err = store
            .update(
                "l",
                &task.id,
                Some(TaskStatus::Completed),
                None,
                None,
                None,
                None,
                None,
                Some("agent-b"),
                None,
                false,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot modify status or owner"));
        Ok(())
    }

    #[tokio::test]
    async fn update_allows_owner_status_change() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        store.claim("l", &task.id, "agent-a", None).await?;
        // agent-a completes their own task → allowed.
        let updated = store
            .update(
                "l",
                &task.id,
                Some(TaskStatus::Completed),
                None,
                None,
                None,
                None,
                Some("all tests pass".into()),
                Some("agent-a"),
                None,
                false,
            )
            .await?;
        assert_eq!(updated.status, TaskStatus::Completed);
        assert_eq!(updated.proof.as_deref(), Some("all tests pass"));
        Ok(())
    }

    #[tokio::test]
    async fn update_allows_metadata_from_non_owner() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        store.claim("l", &task.id, "agent-a", None).await?;
        // agent-b updates subject (not status/owner) → allowed.
        let updated = store
            .update(
                "l",
                &task.id,
                None,
                Some("updated subject".into()),
                None,
                None,
                None,
                None,
                Some("agent-b"),
                None,
                false,
            )
            .await?;
        assert_eq!(updated.subject, "updated subject");
        Ok(())
    }

    #[tokio::test]
    async fn update_unrestricted_for_unowned_tasks() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        // No owner set — any caller can change status.
        let updated = store
            .update(
                "l",
                &task.id,
                Some(TaskStatus::InProgress),
                None,
                None,
                None,
                None,
                None,
                Some("agent-b"),
                None,
                false,
            )
            .await?;
        assert_eq!(updated.status, TaskStatus::InProgress);
        Ok(())
    }

    #[tokio::test]
    async fn update_force_overrides_ownership() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        store.claim("l", &task.id, "agent-a", None).await?;
        // agent-b with force=true can change status.
        let updated = store
            .update(
                "l",
                &task.id,
                Some(TaskStatus::Completed),
                None,
                None,
                None,
                None,
                None,
                Some("agent-b"),
                None,
                true,
            )
            .await?;
        assert_eq!(updated.status, TaskStatus::Completed);
        Ok(())
    }

    // ── Branch 8: Completion proof ───────────────────────────────────────

    #[tokio::test]
    async fn completion_stores_proof_and_completed_at() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        store.claim("l", &task.id, "me", None).await?;
        let updated = store
            .update(
                "l",
                &task.id,
                Some(TaskStatus::Completed),
                None,
                None,
                None,
                None,
                Some("cargo test -- ok".into()),
                Some("me"),
                None,
                false,
            )
            .await?;
        assert_eq!(updated.proof.as_deref(), Some("cargo test -- ok"));
        assert!(updated.completed_at.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn completion_without_proof_still_allowed() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        store.claim("l", &task.id, "me", None).await?;
        let updated = store
            .update(
                "l",
                &task.id,
                Some(TaskStatus::Completed),
                None,
                None,
                None,
                None,
                None,
                Some("me"),
                None,
                false,
            )
            .await?;
        assert_eq!(updated.status, TaskStatus::Completed);
        assert!(updated.proof.is_none());
        assert!(updated.completed_at.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn proof_visible_in_get_response() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({"action": "create", "subject": "work"}))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing id"))?;
        // Move to in_progress, then complete with proof via tool params.
        task_tool
            .execute(serde_json::json!({"action": "update", "id": id, "status": "in_progress"}))
            .await?;
        task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "status": "completed",
                "proof": "screenshot.png"
            }))
            .await?;
        let got = task_tool
            .execute(serde_json::json!({"action": "get", "id": id}))
            .await?;
        assert_eq!(got["task"]["proof"], "screenshot.png");
        assert!(got["task"]["completed_at"].as_u64().is_some());
        Ok(())
    }

    // ── Branch 3: Trace ID on task mutations ─────────────────────────────

    #[tokio::test]
    async fn update_stores_trace_id() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        let updated = store
            .update(
                "l",
                &task.id,
                Some(TaskStatus::InProgress),
                None,
                None,
                None,
                None,
                None,
                None,
                Some("trace-abc-123"),
                false,
            )
            .await?;
        assert_eq!(updated.last_trace_id.as_deref(), Some("trace-abc-123"));
        Ok(())
    }

    #[tokio::test]
    async fn claim_stores_trace_id() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let store = TaskStore::new(tmp.path());
        let task = store.create("l", "work".into(), "".into()).await?;
        let claimed = store
            .claim("l", &task.id, "agent-a", Some("trace-xyz"))
            .await?;
        assert_eq!(claimed.last_trace_id.as_deref(), Some("trace-xyz"));
        Ok(())
    }

    #[tokio::test]
    async fn trace_id_threaded_via_tool_params() -> TestResult<()> {
        let tmp = tempfile::tempdir()?;
        let task_tool = tool(&tmp);
        let created = task_tool
            .execute(serde_json::json!({"action": "create", "subject": "work"}))
            .await?;
        let id = created["task"]["id"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing id"))?;
        let updated = task_tool
            .execute(serde_json::json!({
                "action": "update",
                "id": id,
                "status": "in_progress",
                "_trace_id": "tid-001"
            }))
            .await?;
        assert_eq!(updated["task"]["last_trace_id"], "tid-001");
        Ok(())
    }
}
