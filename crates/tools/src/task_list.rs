//! Shared task list tool for inter-agent task coordination.
//!
//! This is a thin adapter over [`moltis_tasks::TaskStore`] that preserves the
//! original tool interface while gaining formal state-machine enforcement,
//! optimistic concurrency, event logging, failure taxonomy, and retry support.
//!
//! ## Backward-compatible status mapping
//!
//! | RuntimeState        | Tool status string    |
//! |---------------------|-----------------------|
//! | Pending             | "pending"             |
//! | Blocked { .. }      | "pending" + blocked_by|
//! | Active              | "in_progress"         |
//! | Retrying { .. }     | "pending"             |
//! | AwaitingHuman       | "awaiting_human"      |
//! | Terminal(Completed) | "completed"           |
//! | Terminal(Failed)    | "failed"              |
//! | Terminal(Canceled)  | "canceled"            |

use std::{path::Path, sync::Arc};

use {async_trait::async_trait, serde_json::json};

use {
    crate::{
        Error,
        params::{require_str, str_param, str_param_any},
    },
    moltis_agents::tool_registry::AgentTool,
    moltis_tasks::{
        AutonomyTier, FailureClass, HandoffContext, RuntimeState, Task, TaskId, TaskSpec,
        TaskPrincipal, TaskStore, TerminalState, TransitionEvent,
    },
};

// ── Tool wrapper ──────────────────────────────────────────────────────────────

/// Tool wrapper around [`moltis_tasks::TaskStore`].
pub struct TaskListTool {
    store: Arc<TaskStore>,
    /// Global max-attempts override from `TasksConfig`. When set, overrides the
    /// per-task default on every newly created task.
    max_attempts_override: Option<u8>,
}

impl TaskListTool {
    pub fn new(store: Arc<TaskStore>) -> Self {
        Self {
            store,
            max_attempts_override: None,
        }
    }

    /// Return the underlying `Arc<TaskStore>` for sharing with other tools.
    pub fn store(&self) -> Arc<TaskStore> {
        Arc::clone(&self.store)
    }

    /// Apply a global max-attempts override from `TasksConfig`.
    #[must_use]
    pub fn with_max_attempts_override(mut self, override_val: Option<u8>) -> Self {
        self.max_attempts_override = override_val;
        self
    }
}

// ── View helpers ──────────────────────────────────────────────────────────────

/// Build the JSON view of a task — backward-compatible shape with new fields.
fn task_view(task: &Task) -> serde_json::Value {
    let status = runtime_state_to_status(&task.runtime.state);
    let blocked_by: Vec<&str> = task.blocked_by.iter().map(|id| id.0.as_str()).collect();

    let mut v = json!({
        "id":           task.id.0,
        "list_id":      task.list_id,
        "subject":      task.spec.subject,
        "description":  task.spec.description,
        "status":       status,
        "owner":        task.runtime.owner,
        "blocked_by":   blocked_by,
        "attempt":      task.runtime.attempt,
        "max_attempts": task.spec.max_attempts,
        "version":      task.runtime.version,
        "created_at":   task.spec.created_at.unix_timestamp(),
        "updated_at":   task.runtime.last_transition_at.unix_timestamp(),
    });

    // Include failure context if present.
    if let Some(ref failure) = task.runtime.last_failure {
        v["failure_class"] = json!(failure.to_string());
    }
    if let Some(ref handoff) = task.runtime.handoff {
        v["handoff"] = json!({
            "last_action":       handoff.last_action,
            "observed_error":    handoff.observed_error,
            "dead_ends":         handoff.dead_ends,
            "suggested_next_step": handoff.suggested_next_step,
        });
    }

    // Include retry_after for Retrying state.
    if let RuntimeState::Retrying { retry_after, .. } = &task.runtime.state {
        v["retry_after"] = json!(retry_after.unix_timestamp());
    }

    // Include escalation question for AwaitingHuman state.
    if let RuntimeState::AwaitingHuman { question, .. } = &task.runtime.state {
        v["question"] = json!(question);
    }

    v
}

/// Map a [`RuntimeState`] to a backward-compatible status string.
fn runtime_state_to_status(state: &RuntimeState) -> &'static str {
    match state {
        RuntimeState::Pending => "pending",
        RuntimeState::Blocked { .. } => "pending",
        RuntimeState::Active { .. } => "in_progress",
        RuntimeState::Retrying { .. } => "pending",
        RuntimeState::AwaitingHuman { .. } => "awaiting_human",
        RuntimeState::Terminal(TerminalState::Completed) => "completed",
        RuntimeState::Terminal(TerminalState::Failed { .. }) => "failed",
        RuntimeState::Terminal(TerminalState::Canceled { .. }) => "canceled",
    }
}

/// Parse a status string for list filtering.  Accepts both new and legacy values.
fn parse_status_filter(s: &str) -> Option<&'static str> {
    match s {
        "pending" => Some("Pending"),
        "in_progress" => Some("Active"),
        "completed" => Some("Completed"),
        "failed" => Some("Failed"),
        "canceled" => Some("Canceled"),
        "awaiting_human" => Some("AwaitingHuman"),
        "retrying" => Some("Retrying"),
        _ => None,
    }
}

/// Parse a `FailureClass` from a tool parameter string.
fn parse_failure_class(s: &str) -> crate::Result<FailureClass> {
    match s {
        "agent_error" => Ok(FailureClass::AgentError),
        "context_overflow" => Ok(FailureClass::ContextOverflow),
        "provider_transient" => Ok(FailureClass::ProviderTransient),
        "provider_permanent" => Ok(FailureClass::ProviderPermanent),
        "tool_error" => Ok(FailureClass::ToolError),
        "timeout_exceeded" => Ok(FailureClass::TimeoutExceeded),
        "human_blocker" => Ok(FailureClass::HumanBlocker),
        "max_attempts_exceeded" => Ok(FailureClass::MaxAttemptsExceeded),
        other => Err(Error::message(format!("unknown failure_class: {other}"))),
    }
}

/// Parse an `AutonomyTier` from a tool parameter string.
fn parse_autonomy_tier(s: &str) -> crate::Result<AutonomyTier> {
    match s {
        "auto" => Ok(AutonomyTier::Auto),
        "confirm" => Ok(AutonomyTier::Confirm),
        "approve" => Ok(AutonomyTier::Approve),
        other => Err(Error::message(format!("unknown autonomy_tier: {other}"))),
    }
}

/// Parse a `HandoffContext` from JSON parameter.
fn parse_handoff(params: &serde_json::Value) -> HandoffContext {
    let h = params.get("handoff");
    HandoffContext {
        last_action: h
            .and_then(|v| v.get("last_action"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        observed_error: h
            .and_then(|v| v.get("observed_error"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        dead_ends: h
            .and_then(|v| v.get("dead_ends"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        suggested_next_step: h
            .and_then(|v| v.get("suggested_next_step"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

// ── AgentTool impl ────────────────────────────────────────────────────────────

#[async_trait]
impl AgentTool for TaskListTool {
    fn name(&self) -> &str {
        crate::tool_names::TASK_LIST
    }

    fn categories(&self) -> &'static [&'static str] {
        &["orchestration"]
    }

    fn description(&self) -> &str {
        "Manage a shared task list for coordinated multi-agent execution. \
         Actions: create, list, get, update, claim, fail, escalate, resolve, retry, history."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "get", "update", "claim",
                             "fail", "escalate", "resolve", "retry", "history"],
                    "description": "Task list action to perform."
                },
                "list_id": {
                    "type": "string",
                    "description": "Task list identifier (default: default)."
                },
                "id": {
                    "type": "string",
                    "description": "Task ID for get/update/claim/fail/escalate/resolve/retry/history."
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
                    "description": "Task status for list filter or legacy update."
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
                "failure_class": {
                    "type": "string",
                    "enum": ["agent_error", "context_overflow", "provider_transient",
                             "provider_permanent", "tool_error", "timeout_exceeded",
                             "human_blocker", "max_attempts_exceeded"],
                    "description": "Failure classification for fail action."
                },
                "handoff": {
                    "type": "object",
                    "description": "Handoff context for fail/escalate actions.",
                    "properties": {
                        "last_action":         { "type": "string" },
                        "observed_error":      { "type": "string" },
                        "dead_ends":           { "type": "array", "items": { "type": "string" } },
                        "suggested_next_step": { "type": "string" }
                    }
                },
                "question": {
                    "type": "string",
                    "description": "Question for escalate action."
                },
                "resolution": {
                    "type": "string",
                    "description": "Human resolution for resolve action."
                },
                "expected_version": {
                    "type": "integer",
                    "description": "Optimistic concurrency version for CAS writes (optional)."
                },
                "active_form": {
                    "type": "string",
                    "description": "Present continuous form shown in spinner (e.g. 'Running tests')."
                },
                "is_intent": {
                    "type": "boolean",
                    "description": "Mark this as a dispatch-managed intent task (long-running, multi-shift). Default: false."
                },
                "autonomy_tier": {
                    "type": "string",
                    "enum": ["auto", "confirm", "approve"],
                    "description": "Maximum autonomy tier for shift agents. Only applies when is_intent is true. Default: auto."
                },
                "parent_task": {
                    "type": "string",
                    "description": "Optional parent intent task ID for shift tasks."
                },
                "principal": {
                    "type": "object",
                    "description": "Optional execution identity for autonomous dispatch.",
                    "properties": {
                        "channel": { "type": "string" },
                        "sender": { "type": "string" },
                        "account_id": { "type": "string" }
                    },
                    "required": ["channel", "sender"]
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let action = require_str(&params, "action")?;
        let list_id_input = str_param_any(&params, &["list_id", "listId"]).map(str::to_string);
        let list_id = list_id_input.as_deref().unwrap_or("default");
        let expected_version = params.get("expected_version").and_then(|v| v.as_u64());

        match action {
            // ── create ────────────────────────────────────────────────────
            "create" => {
                let subject = require_str(&params, "subject")?.to_string();
                let description = str_param(&params, "description").unwrap_or("").to_string();
                let active_form = str_param(&params, "active_form").map(String::from);
                let blocked_by: Vec<TaskId> = params
                    .get("blocked_by")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(TaskId::from)
                            .collect()
                    })
                    .unwrap_or_default();

                let is_intent = params
                    .get("is_intent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let autonomy_tier = str_param(&params, "autonomy_tier")
                    .map(parse_autonomy_tier)
                    .transpose()?
                    .unwrap_or_default();
                let dispatch_autonomy_tier = str_param(&params, "_dispatch_autonomy_tier")
                    .map(parse_autonomy_tier)
                    .transpose()?;
                if is_intent
                    && let Some(caller_tier) = dispatch_autonomy_tier
                    && autonomy_tier > caller_tier
                {
                    return Err(Error::message(format!(
                        "autonomy_tier `{autonomy_tier}` exceeds dispatch caller tier `{caller_tier}`"
                    ))
                    .into());
                }
                let parent_task = str_param_any(&params, &["parent_task", "parentTask"])
                    .map(TaskId::from);
                let principal = params
                    .get("principal")
                    .cloned()
                    .map(serde_json::from_value::<TaskPrincipal>)
                    .transpose()
                    .map_err(|e| Error::message(format!("invalid principal: {e}")))?;

                let mut spec = TaskSpec::new(subject, description);
                spec.is_intent = is_intent;
                spec.autonomy_tier = autonomy_tier;
                spec.parent_task = parent_task;
                spec.principal = principal.clone();
                if let Some(af) = active_form {
                    // active_form is a UI hint; not stored in the spec currently.
                    let _ = af;
                }
                if let Some(override_max) = self.max_attempts_override.filter(|&v| v > 0) {
                    spec.max_attempts = override_max;
                }

                let resolved_list_id = if list_id_input.as_deref().is_none()
                    || list_id_input.as_deref() == Some("default")
                {
                    principal
                        .as_ref()
                        .map(TaskPrincipal::canonical_list_id)
                        .unwrap_or_else(|| list_id.to_string())
                } else {
                    list_id.to_string()
                };

                let task = self
                    .store
                    .create(&resolved_list_id, spec, blocked_by)
                    .await
                    .map_err(anyhow::Error::from)?;
                Ok(json!({ "ok": true, "task": task_view(&task) }))
            },

            // ── list ──────────────────────────────────────────────────────
            "list" => {
                let filter = str_param(&params, "status").and_then(parse_status_filter);
                let tasks = self
                    .store
                    .list(list_id, filter)
                    .await
                    .map_err(anyhow::Error::from)?;
                let views: Vec<_> = tasks.iter().map(task_view).collect();
                Ok(json!({ "ok": true, "tasks": views, "count": views.len() }))
            },

            // ── get ───────────────────────────────────────────────────────
            "get" => {
                let id = require_str(&params, "id")?;
                let task = self
                    .store
                    .get(list_id, id)
                    .await
                    .map_err(anyhow::Error::from)?;
                Ok(json!({
                    "ok": task.is_some(),
                    "task": task.as_ref().map(task_view),
                }))
            },

            // ── update ────────────────────────────────────────────────────
            // Legacy: update status to pending/in_progress/completed via the
            // new state machine transitions. Metadata changes go through
            // update_metadata.
            "update" => {
                let id = require_str(&params, "id")?;
                let status = str_param(&params, "status");
                let subject = str_param(&params, "subject");
                let description = str_param(&params, "description");
                let owner = str_param(&params, "owner");
                let new_blocked_by =
                    params
                        .get("blocked_by")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .map(TaskId::from)
                                .collect::<Vec<_>>()
                        });

                // Apply metadata update first if any.
                if subject.is_some() || description.is_some() || new_blocked_by.is_some() {
                    self.store
                        .update_metadata(
                            list_id,
                            id,
                            subject,
                            description,
                            new_blocked_by.as_deref(),
                        )
                        .await
                        .map_err(anyhow::Error::from)?;
                }

                // Apply state transition if status changed.
                let task = if let Some(status) = status {
                    let task_before = self
                        .store
                        .get(list_id, id)
                        .await
                        .map_err(anyhow::Error::from)?
                        .ok_or_else(|| Error::message(format!("task not found: {id}")))?;

                    let event = match status {
                        "completed" => TransitionEvent::Complete,
                        "in_progress" => TransitionEvent::Claim {
                            owner: owner
                                .or(task_before.runtime.owner.as_deref())
                                .unwrap_or("agent")
                                .to_string(),
                            lease_duration_secs: None,
                        },
                        "pending" => {
                            // Map update to pending back → depends on current state.
                            match &task_before.runtime.state {
                                RuntimeState::Retrying { .. } => TransitionEvent::PromoteRetry,
                                RuntimeState::AwaitingHuman { .. } => {
                                    TransitionEvent::HumanResolve {
                                        resolution: "manual reset to pending".into(),
                                    }
                                },
                                _ => {
                                    // Already pending or invalid; skip transition.
                                    return Ok(
                                        json!({ "ok": true, "task": task_view(&task_before) }),
                                    );
                                },
                            }
                        },
                        other => {
                            return Err(Error::message(format!(
                                "unknown status for update: {other}"
                            ))
                            .into());
                        },
                    };

                    self.store
                        .apply_transition(list_id, id, expected_version, &event)
                        .await
                        .map_err(anyhow::Error::from)?
                } else {
                    self.store
                        .get(list_id, id)
                        .await
                        .map_err(anyhow::Error::from)?
                        .ok_or_else(|| Error::message(format!("task not found: {id}")))?
                };

                Ok(json!({ "ok": true, "task": task_view(&task) }))
            },

            // ── claim ─────────────────────────────────────────────────────
            "claim" => {
                let id = require_str(&params, "id")?;
                let owner = str_param_any(&params, &["owner", "_session_key"])
                    .unwrap_or("agent")
                    .to_string();

                // Guard: check blocked_by dependencies are completed.
                let task_before = self
                    .store
                    .get(list_id, id)
                    .await
                    .map_err(anyhow::Error::from)?
                    .ok_or_else(|| Error::message(format!("task not found: {id}")))?;

                let incomplete_deps = self.incomplete_deps(&task_before).await?;
                if !incomplete_deps.is_empty() {
                    return Err(Error::message(format!(
                        "task {id} is blocked by incomplete tasks: {}",
                        incomplete_deps.join(", ")
                    ))
                    .into());
                }

                let task = self
                    .store
                    .apply_transition(
                        list_id,
                        id,
                        expected_version,
                        &TransitionEvent::Claim {
                            owner,
                            lease_duration_secs: None,
                        },
                    )
                    .await
                    .map_err(anyhow::Error::from)?;

                Ok(json!({ "ok": true, "task": task_view(&task) }))
            },

            // ── fail (new) ────────────────────────────────────────────────
            "fail" => {
                let id = require_str(&params, "id")?;
                let class_str = str_param(&params, "failure_class").unwrap_or("agent_error");
                let class = parse_failure_class(class_str)?;
                let handoff = parse_handoff(&params);

                let task = self
                    .store
                    .apply_transition(
                        list_id,
                        id,
                        expected_version,
                        &TransitionEvent::Fail {
                            class,
                            handoff,
                            retry_after: None,
                        },
                    )
                    .await
                    .map_err(anyhow::Error::from)?;

                Ok(json!({ "ok": true, "task": task_view(&task) }))
            },

            // ── escalate (new) ────────────────────────────────────────────
            "escalate" => {
                let id = require_str(&params, "id")?;
                let question = str_param(&params, "question")
                    .unwrap_or("Human input required.")
                    .to_string();
                let handoff = parse_handoff(&params);

                let task = self
                    .store
                    .apply_transition(
                        list_id,
                        id,
                        expected_version,
                        &TransitionEvent::Escalate { question, handoff },
                    )
                    .await
                    .map_err(anyhow::Error::from)?;

                Ok(json!({ "ok": true, "task": task_view(&task) }))
            },

            // ── resolve (new) ─────────────────────────────────────────────
            "resolve" => {
                let id = require_str(&params, "id")?;
                let resolution = str_param(&params, "resolution").unwrap_or("").to_string();

                let task = self
                    .store
                    .apply_transition(
                        list_id,
                        id,
                        expected_version,
                        &TransitionEvent::HumanResolve { resolution },
                    )
                    .await
                    .map_err(anyhow::Error::from)?;

                Ok(json!({ "ok": true, "task": task_view(&task) }))
            },

            // ── retry (new) ───────────────────────────────────────────────
            "retry" => {
                let id = require_str(&params, "id")?;

                let task = self
                    .store
                    .apply_transition(
                        list_id,
                        id,
                        expected_version,
                        &TransitionEvent::PromoteRetry,
                    )
                    .await
                    .map_err(anyhow::Error::from)?;

                Ok(json!({ "ok": true, "task": task_view(&task) }))
            },

            // ── history (new) ─────────────────────────────────────────────
            "history" => {
                let id = require_str(&params, "id")?;
                let events = self
                    .store
                    .event_log()
                    .history(list_id, id)
                    .await
                    .map_err(anyhow::Error::from)?;

                let views: Vec<_> = events
                    .iter()
                    .map(|e| {
                        json!({
                            "id":         e.id,
                            "event_type": e.event_type,
                            "from_state": e.from_state,
                            "to_state":   e.to_state,
                            "agent_id":   e.agent_id,
                            "detail":     e.detail,
                            "created_at": e.created_at.unix_timestamp(),
                        })
                    })
                    .collect();

                Ok(json!({ "ok": true, "task_id": id, "events": views }))
            },

            _ => Err(Error::message(format!("unknown task_list action: {action}")).into()),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl TaskListTool {
    /// Returns task IDs that are declared as dependencies but not yet completed.
    async fn incomplete_deps(&self, task: &Task) -> crate::Result<Vec<String>> {
        let mut incomplete = Vec::new();
        for dep_id in &task.blocked_by {
            match self.store.get(&task.list_id, &dep_id.0).await {
                Ok(Some(dep))
                    if !dep.is_terminal()
                        || dep.runtime.state
                            != RuntimeState::Terminal(TerminalState::Completed) =>
                {
                    incomplete.push(dep_id.0.clone());
                },
                Ok(None) => {
                    // Missing dep → treat as incomplete.
                    incomplete.push(dep_id.0.clone());
                },
                _ => {},
            }
        }
        Ok(incomplete)
    }
}

// ── Constructor helpers ───────────────────────────────────────────────────────

impl TaskListTool {
    /// Create a new `TaskListTool` opening the SQLite database at `base_dir/tasks.db`.
    pub async fn from_data_dir(base_dir: &Path) -> anyhow::Result<Self> {
        let db_path = base_dir.join("tasks").join("tasks.db");
        let store = TaskStore::open(&db_path)
            .await
            .map_err(anyhow::Error::from)?;
        Ok(Self::new(Arc::new(store)))
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use {super::*, tempfile::TempDir};

    async fn tool(dir: &TempDir) -> TaskListTool {
        TaskListTool::from_data_dir(dir.path())
            .await
            .expect("init tool")
    }

    async fn tool_with_max_attempts(dir: &TempDir, override_val: u8) -> TaskListTool {
        TaskListTool::from_data_dir(dir.path())
            .await
            .expect("init tool")
            .with_max_attempts_override(Some(override_val))
    }

    #[tokio::test]
    async fn max_attempts_override_applied_on_create() {
        let tmp = TempDir::new().unwrap();
        let t = tool_with_max_attempts(&tmp, 7).await;
        let result = t
            .execute(json!({ "action": "create", "subject": "limited" }))
            .await
            .unwrap();
        let id = result["task"]["id"].as_str().unwrap().to_string();

        // Retrieve the stored spec and check max_attempts was overridden.
        let got = t
            .execute(json!({ "action": "get", "id": id }))
            .await
            .unwrap();
        assert_eq!(got["task"]["max_attempts"], 7);
    }

    #[tokio::test]
    async fn create_and_list_tasks() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        t.execute(json!({ "action": "create", "subject": "first", "description": "desc" }))
            .await
            .unwrap();

        let result = t.execute(json!({ "action": "list" })).await.unwrap();
        assert_eq!(result["count"], 1);
        assert_eq!(result["tasks"][0]["subject"], "first");
        assert_eq!(result["tasks"][0]["status"], "pending");
    }

    #[tokio::test]
    async fn claim_moves_task_to_in_progress() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let created = t
            .execute(json!({ "action": "create", "subject": "work" }))
            .await
            .unwrap();
        let id = created["task"]["id"].as_str().unwrap().to_string();

        let claimed = t
            .execute(json!({ "action": "claim", "id": id, "owner": "worker-a" }))
            .await
            .unwrap();
        assert_eq!(claimed["task"]["status"], "in_progress");
        assert_eq!(claimed["task"]["owner"], "worker-a");
    }

    #[tokio::test]
    async fn claim_rejects_non_pending_task() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let created = t
            .execute(json!({ "action": "create", "subject": "work" }))
            .await
            .unwrap();
        let id = created["task"]["id"].as_str().unwrap().to_string();

        // Mark completed via claim then complete.
        t.execute(json!({ "action": "claim", "id": id, "owner": "a" }))
            .await
            .unwrap();
        t.execute(json!({ "action": "update", "id": id, "status": "completed" }))
            .await
            .unwrap();

        let result = t
            .execute(json!({ "action": "claim", "id": id, "owner": "b" }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // State-machine rejects Claim on a completed task.
        assert!(
            err.contains("cannot be claimed")
                || err.contains("InvalidTransition")
                || err.contains("cannot apply"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn claim_rejects_when_blocked_dependencies_incomplete() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let dep = t
            .execute(json!({ "action": "create", "subject": "dep" }))
            .await
            .unwrap();
        let dep_id = dep["task"]["id"].as_str().unwrap().to_string();

        let main = t
            .execute(json!({ "action": "create", "subject": "main" }))
            .await
            .unwrap();
        let main_id = main["task"]["id"].as_str().unwrap().to_string();

        t.execute(json!({ "action": "update", "id": main_id, "blocked_by": [dep_id] }))
            .await
            .unwrap();

        let result = t.execute(json!({ "action": "claim", "id": main_id })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blocked"));
    }

    #[tokio::test]
    async fn fail_and_history() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let created = t
            .execute(json!({ "action": "create", "subject": "retryable" }))
            .await
            .unwrap();
        let id = created["task"]["id"].as_str().unwrap().to_string();

        t.execute(json!({ "action": "claim", "id": id, "owner": "agent" }))
            .await
            .unwrap();

        let failed = t
            .execute(json!({
                "action": "fail",
                "id": id,
                "failure_class": "agent_error",
                "handoff": {
                    "last_action": "searched for X",
                    "observed_error": "timeout",
                    "dead_ends": ["approach A"],
                    "suggested_next_step": "try B"
                }
            }))
            .await
            .unwrap();
        assert_eq!(failed["task"]["failure_class"], "agent_error");

        let history = t
            .execute(json!({ "action": "history", "id": id }))
            .await
            .unwrap();
        assert!(history["events"].as_array().unwrap().len() >= 2);
    }

    #[tokio::test]
    async fn create_intent_task_sets_is_intent_and_tier() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let result = t
            .execute(json!({
                "action": "create",
                "subject": "intent task",
                "description": "do something",
                "is_intent": true,
                "autonomy_tier": "confirm"
            }))
            .await
            .unwrap();
        let id = result["task"]["id"].as_str().unwrap().to_string();

        // Fetch the stored task and inspect the spec via the store directly.
        let store = t.store();
        let task = store.get("default", &id).await.unwrap().unwrap();
        assert!(task.spec.is_intent);
        assert_eq!(task.spec.autonomy_tier, AutonomyTier::Confirm);
    }

    #[tokio::test]
    async fn create_task_without_intent_flag_defaults_to_non_intent() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let result = t
            .execute(json!({ "action": "create", "subject": "plain task" }))
            .await
            .unwrap();
        let id = result["task"]["id"].as_str().unwrap().to_string();

        let store = t.store();
        let task = store.get("default", &id).await.unwrap().unwrap();
        assert!(!task.spec.is_intent);
        assert_eq!(task.spec.autonomy_tier, AutonomyTier::Auto);
    }

    #[tokio::test]
    async fn create_with_principal_derives_canonical_list_id() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let result = t
            .execute(json!({
                "action": "create",
                "subject": "principal task",
                "principal": {
                    "channel": "whatsapp",
                    "sender": "+15551234567",
                    "account_id": "biz-1"
                }
            }))
            .await
            .unwrap();

        let list_id = result["task"]["list_id"].as_str().unwrap();
        assert!(list_id.starts_with("v1:whatsapp:"));

        let id = result["task"]["id"].as_str().unwrap().to_string();
        let store = t.store();
        let task = store.get(list_id, &id).await.unwrap().unwrap();
        let principal = task.spec.principal.expect("principal must be set");
        assert_eq!(principal.channel, "whatsapp");
        assert_eq!(principal.sender, "+15551234567");
        assert_eq!(principal.account_id, "biz-1");
    }

    #[tokio::test]
    async fn create_with_parent_and_principal_persists_relationship() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;

        let parent = t
            .execute(json!({
                "action": "create",
                "subject": "intent root",
                "list_id": "custom-list",
                "is_intent": true
            }))
            .await
            .unwrap();
        let parent_id = parent["task"]["id"].as_str().unwrap().to_string();

        let child = t
            .execute(json!({
                "action": "create",
                "subject": "shift child",
                "list_id": "custom-list",
                "parent_task": parent_id,
                "principal": {
                    "channel": "web",
                    "sender": "alice"
                }
            }))
            .await
            .unwrap();
        let child_id = child["task"]["id"].as_str().unwrap().to_string();

        let store = t.store();
        let task = store.get("custom-list", &child_id).await.unwrap().unwrap();
        assert_eq!(task.spec.parent_task.as_ref().map(|id| id.0.as_str()), Some(parent_id.as_str()));
        assert_eq!(
            task.spec.principal.as_ref().map(|p| p.channel.as_str()),
            Some("web")
        );
    }

    #[tokio::test]
    async fn create_task_rejects_unknown_autonomy_tier() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let result = t
            .execute(json!({
                "action": "create",
                "subject": "bad tier",
                "autonomy_tier": "supercharge"
            }))
            .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown autonomy_tier")
        );
    }

    #[tokio::test]
    async fn create_intent_rejects_tier_above_dispatch_caller_tier() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let result = t
            .execute(json!({
                "action": "create",
                "subject": "escalated intent",
                "is_intent": true,
                "autonomy_tier": "approve",
                "_dispatch_autonomy_tier": "auto",
            }))
            .await;
        assert!(result.is_err(), "create should fail when requested tier exceeds caller tier");
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("exceeds dispatch caller tier")
        );
    }

    #[tokio::test]
    async fn escalate_and_resolve() {
        let tmp = TempDir::new().unwrap();
        let t = tool(&tmp).await;
        let created = t
            .execute(json!({ "action": "create", "subject": "escalation test" }))
            .await
            .unwrap();
        let id = created["task"]["id"].as_str().unwrap().to_string();

        t.execute(json!({ "action": "claim", "id": id, "owner": "agent" }))
            .await
            .unwrap();

        let escalated = t
            .execute(json!({ "action": "escalate", "id": id, "question": "which env?" }))
            .await
            .unwrap();
        assert_eq!(escalated["task"]["status"], "awaiting_human");

        let resolved = t
            .execute(json!({ "action": "resolve", "id": id, "resolution": "use staging" }))
            .await
            .unwrap();
        assert_eq!(resolved["task"]["status"], "pending");
    }
}
