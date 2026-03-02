//! SQLite-backed task store with optimistic concurrency (CAS).
//!
//! Every write uses `UPDATE ... WHERE version = :expected` to detect concurrent
//! modifications without holding a lock across async boundaries.

use std::path::Path;

use sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions};
use time::OffsetDateTime;

use crate::{
    errors::TransitionError,
    event_log::EventLog,
    state::RuntimeState,
    transitions::{TransitionEvent, apply},
    types::{Task, TaskId, TaskRuntime, TaskSpec},
};

/// SQLite-backed store for tasks across multiple named lists.
#[derive(Clone)]
pub struct TaskStore {
    pool: SqlitePool,
    log: EventLog,
}

impl TaskStore {
    /// Open (or create) the SQLite database at `db_path` and run migrations.
    pub async fn open(db_path: &Path) -> Result<Self, TransitionError> {
        if let Some(parent) = db_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| TransitionError::Other(e.to_string()))?;
        }

        let url = format!("sqlite://{}?mode=rwc", db_path.to_string_lossy());
        let opts = url
            .parse::<SqliteConnectOptions>()
            .map_err(|e| TransitionError::Other(e.to_string()))?
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .foreign_keys(true);

        let pool = SqlitePool::connect_with(opts)
            .await
            .map_err(TransitionError::Storage)?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| TransitionError::Other(e.to_string()))?;

        let log = EventLog::new(pool.clone());
        Ok(Self { pool, log })
    }

    /// Create a task from a [`TaskSpec`] in the given list.
    pub async fn create(
        &self,
        list_id: &str,
        spec: TaskSpec,
        blocked_by: Vec<TaskId>,
    ) -> Result<Task, TransitionError> {
        let task = Task {
            id: TaskId::new(),
            list_id: list_id.to_string(),
            spec,
            runtime: TaskRuntime::default(),
            blocked_by,
        };
        self.insert(&task).await?;
        Ok(task)
    }

    /// Insert a fully-constructed task (used by `create` and migrations).
    async fn insert(&self, task: &Task) -> Result<(), TransitionError> {
        let spec_json =
            serde_json::to_string(&task.spec).map_err(|e| TransitionError::Other(e.to_string()))?;
        let runtime_json = serde_json::to_string(&task.runtime)
            .map_err(|e| TransitionError::Other(e.to_string()))?;
        let blocked_by_json = serde_json::to_string(&task.blocked_by)
            .map_err(|e| TransitionError::Other(e.to_string()))?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let version = task.runtime.version as i64;

        sqlx::query(
            "INSERT INTO tasks (id, list_id, spec_json, runtime_json, blocked_by, version, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&task.id.0)
        .bind(&task.list_id)
        .bind(&spec_json)
        .bind(&runtime_json)
        .bind(&blocked_by_json)
        .bind(version)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        self.log
            .append(
                &task.list_id,
                &task.id.0,
                "create",
                "—",
                task.runtime.state.name(),
                task.runtime.owner.as_deref(),
                None,
            )
            .await
            .map_err(TransitionError::Storage)?;

        Ok(())
    }

    /// Read a single task.
    pub async fn get(&self, list_id: &str, task_id: &str) -> Result<Option<Task>, TransitionError> {
        let row = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks WHERE list_id = ? AND id = ?",
        )
        .bind(list_id)
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        row.map(|r| {
            Self::row_to_task(
                r.get::<String, _>("id"),
                r.get::<String, _>("list_id"),
                r.get::<String, _>("spec_json"),
                r.get::<String, _>("runtime_json"),
                r.get::<String, _>("blocked_by"),
                r.get::<i64, _>("version"),
            )
        })
        .transpose()
    }

    /// List tasks in a list, with optional state filter.
    pub async fn list(
        &self,
        list_id: &str,
        state_filter: Option<&str>,
    ) -> Result<Vec<Task>, TransitionError> {
        let rows = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks WHERE list_id = ? ORDER BY updated_at DESC",
        )
        .bind(list_id)
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        let mut tasks = Vec::with_capacity(rows.len());
        for r in rows {
            let task = Self::row_to_task(
                r.get::<String, _>("id"),
                r.get::<String, _>("list_id"),
                r.get::<String, _>("spec_json"),
                r.get::<String, _>("runtime_json"),
                r.get::<String, _>("blocked_by"),
                r.get::<i64, _>("version"),
            )?;
            if let Some(filter) = state_filter {
                if task.runtime.state.name().to_lowercase() != filter.to_lowercase() {
                    continue;
                }
            }
            tasks.push(task);
        }
        Ok(tasks)
    }

    /// Apply a transition event with optimistic concurrency control.
    ///
    /// If `expected_version` is `None`, the current version is read and used
    /// automatically (single-writer optimistic path).
    ///
    /// # Errors
    /// - [`TransitionError::NotFound`] if the task doesn't exist.
    /// - [`TransitionError::VersionConflict`] if the version has changed since read.
    /// - [`TransitionError::InvalidTransition`] if the (state, event) is invalid.
    pub async fn apply_transition(
        &self,
        list_id: &str,
        task_id: &str,
        expected_version: Option<u64>,
        event: &TransitionEvent,
    ) -> Result<Task, TransitionError> {
        let current = self
            .get(list_id, task_id)
            .await?
            .ok_or_else(|| TransitionError::NotFound(task_id.to_string()))?;

        // Verify optimistic concurrency.
        if let Some(expected) = expected_version {
            if current.runtime.version != expected {
                return Err(TransitionError::VersionConflict {
                    expected,
                    actual: current.runtime.version,
                });
            }
        }

        let from_state = current.runtime.state.name();
        let from_version = current.runtime.version;

        // Apply the transition in-memory.
        let updated = apply(current, event)?;

        // Persist with CAS: UPDATE ... WHERE version = from_version.
        let runtime_json = serde_json::to_string(&updated.runtime)
            .map_err(|e| TransitionError::Other(e.to_string()))?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let new_version = updated.runtime.version as i64;
        let old_version = from_version as i64;

        let rows_affected = sqlx::query(
            "UPDATE tasks SET runtime_json = ?, version = ?, updated_at = ? \
             WHERE list_id = ? AND id = ? AND version = ?",
        )
        .bind(&runtime_json)
        .bind(new_version)
        .bind(now)
        .bind(&updated.list_id)
        .bind(&updated.id.0)
        .bind(old_version)
        .execute(&self.pool)
        .await
        .map_err(TransitionError::Storage)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(TransitionError::VersionConflict {
                expected: from_version,
                actual: from_version + 1,
            });
        }

        // Append to event log.
        let to_state = updated.runtime.state.name();
        let event_type = event_type_name(event);
        self.log
            .append(
                &updated.list_id,
                &updated.id.0,
                event_type,
                from_state,
                to_state,
                updated.runtime.owner.as_deref(),
                None,
            )
            .await
            .map_err(TransitionError::Storage)?;

        #[cfg(feature = "tracing")]
        tracing::debug!(
            list_id = %updated.list_id,
            task_id = %updated.id.0,
            from = from_state,
            to = to_state,
            event = event_type,
            version = new_version,
            "task transition"
        );

        #[cfg(feature = "metrics")]
        {
            moltis_metrics::counter!("task_transitions_total",
                "from_state" => from_state,
                "to_state" => to_state,
                "event" => event_type,
            )
            .increment(1);
        }

        Ok(updated)
    }

    /// Update task metadata (subject, description, blocked_by) without a
    /// state-machine transition. Does NOT increment the version counter.
    pub async fn update_metadata(
        &self,
        list_id: &str,
        task_id: &str,
        subject: Option<&str>,
        description: Option<&str>,
        blocked_by: Option<&[TaskId]>,
    ) -> Result<Task, TransitionError> {
        let mut task = self
            .get(list_id, task_id)
            .await?
            .ok_or_else(|| TransitionError::NotFound(task_id.to_string()))?;

        if let Some(s) = subject {
            task.spec.subject = s.to_string();
        }
        if let Some(d) = description {
            task.spec.description = d.to_string();
        }
        if let Some(by) = blocked_by {
            task.blocked_by = by.to_vec();
        }

        let spec_json =
            serde_json::to_string(&task.spec).map_err(|e| TransitionError::Other(e.to_string()))?;
        let blocked_by_json = serde_json::to_string(&task.blocked_by)
            .map_err(|e| TransitionError::Other(e.to_string()))?;
        let now = OffsetDateTime::now_utc().unix_timestamp();

        sqlx::query(
            "UPDATE tasks SET spec_json = ?, blocked_by = ?, updated_at = ? \
             WHERE list_id = ? AND id = ?",
        )
        .bind(&spec_json)
        .bind(&blocked_by_json)
        .bind(now)
        .bind(&task.list_id)
        .bind(&task.id.0)
        .execute(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        Ok(task)
    }

    /// Return the event log accessor.
    pub fn event_log(&self) -> &EventLog {
        &self.log
    }

    /// List tasks in `Retrying` state whose `retry_after` has passed.
    pub async fn due_retries(&self, list_id: &str) -> Result<Vec<Task>, TransitionError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let rows = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks WHERE list_id = ?",
        )
        .bind(list_id)
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        let mut due = Vec::new();
        for r in rows {
            let task = Self::row_to_task(
                r.get::<String, _>("id"),
                r.get::<String, _>("list_id"),
                r.get::<String, _>("spec_json"),
                r.get::<String, _>("runtime_json"),
                r.get::<String, _>("blocked_by"),
                r.get::<i64, _>("version"),
            )?;
            if let RuntimeState::Retrying { retry_after, .. } = &task.runtime.state {
                if retry_after.unix_timestamp() <= now {
                    due.push(task);
                }
            }
        }
        Ok(due)
    }

    /// List tasks in `Active` state with an expired lease.
    pub async fn expired_leases(&self, list_id: &str) -> Result<Vec<Task>, TransitionError> {
        let rows = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks WHERE list_id = ?",
        )
        .bind(list_id)
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Self::row_to_task(
                    r.get::<String, _>("id"),
                    r.get::<String, _>("list_id"),
                    r.get::<String, _>("spec_json"),
                    r.get::<String, _>("runtime_json"),
                    r.get::<String, _>("blocked_by"),
                    r.get::<i64, _>("version"),
                )
                .ok()
                .filter(|t| t.runtime.state.is_lease_expired())
            })
            .collect())
    }

    /// Sweep all lists for overdue `Retrying` tasks and promote them to `Pending`.
    ///
    /// Called by the background retry-promotion poll. Returns the count of tasks
    /// that were successfully promoted.
    pub async fn promote_due_retries_all(&self) -> Result<usize, TransitionError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();

        let rows = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        let mut due: Vec<Task> = rows
            .into_iter()
            .filter_map(|r| {
                Self::row_to_task(
                    r.get::<String, _>("id"),
                    r.get::<String, _>("list_id"),
                    r.get::<String, _>("spec_json"),
                    r.get::<String, _>("runtime_json"),
                    r.get::<String, _>("blocked_by"),
                    r.get::<i64, _>("version"),
                )
                .ok()
            })
            .filter(|t| {
                matches!(&t.runtime.state, RuntimeState::Retrying { retry_after, .. } if retry_after.unix_timestamp() <= now)
            })
            .collect();

        let mut promoted = 0usize;
        for task in due.drain(..) {
            let list_id = task.list_id.clone();
            let task_id = task.id.0.clone();
            let version = task.runtime.version;
            match self
                .apply_transition(
                    &list_id,
                    &task_id,
                    Some(version),
                    &TransitionEvent::PromoteRetry,
                )
                .await
            {
                Ok(_) => promoted += 1,
                Err(TransitionError::VersionConflict { .. }) => {
                    // Another writer raced us; skip — will be picked up next sweep.
                },
                Err(e) => {
                    #[cfg(feature = "tracing")]
                    tracing::warn!(
                        list_id = %list_id,
                        task_id = %task_id,
                        error = %e,
                        "retry promotion failed"
                    );
                    let _ = e;
                },
            }
        }

        Ok(promoted)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn row_to_task(
        id: String,
        list_id: String,
        spec_json: String,
        runtime_json: String,
        blocked_by: String,
        version: i64,
    ) -> Result<Task, TransitionError> {
        let spec: TaskSpec = serde_json::from_str(&spec_json)
            .map_err(|e| TransitionError::Other(format!("spec deserialize: {e}")))?;
        let mut runtime: TaskRuntime = serde_json::from_str(&runtime_json)
            .map_err(|e| TransitionError::Other(format!("runtime deserialize: {e}")))?;
        let blocked_by_ids: Vec<TaskId> = serde_json::from_str(&blocked_by)
            .map_err(|e| TransitionError::Other(format!("blocked_by deserialize: {e}")))?;

        // Authoritative version comes from the DB column, not the JSON.
        runtime.version = version as u64;

        Ok(Task {
            id: TaskId(id),
            list_id,
            spec,
            runtime,
            blocked_by: blocked_by_ids,
        })
    }
}

fn event_type_name(event: &TransitionEvent) -> &'static str {
    match event {
        TransitionEvent::Claim { .. } => "Claim",
        TransitionEvent::Block { .. } => "Block",
        TransitionEvent::DependenciesMet => "DependenciesMet",
        TransitionEvent::Complete => "Complete",
        TransitionEvent::Fail { .. } => "Fail",
        TransitionEvent::Escalate { .. } => "Escalate",
        TransitionEvent::PromoteRetry => "PromoteRetry",
        TransitionEvent::HumanResolve { .. } => "HumanResolve",
        TransitionEvent::Cancel { .. } => "Cancel",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FailureClass, HandoffContext};

    async fn test_store() -> (TaskStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("tasks.db");
        let store = TaskStore::open(&db_path).await.expect("open store");
        (store, dir)
    }

    #[tokio::test]
    async fn create_and_get() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("test task", "do something");
        let task = store.create("list1", spec, vec![]).await.expect("create");
        assert_eq!(task.runtime.state, RuntimeState::Pending);

        let fetched = store
            .get("list1", &task.id.0)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(fetched.id, task.id);
        assert_eq!(fetched.spec.subject, "test task");
    }

    #[tokio::test]
    async fn apply_claim_transition() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("work", "");
        let task = store.create("default", spec, vec![]).await.expect("create");

        let claimed = store
            .apply_transition(
                "default",
                &task.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent-1".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim");

        assert!(claimed.runtime.state.is_active());
        assert_eq!(claimed.runtime.version, 1);
        assert_eq!(claimed.runtime.attempt, 1);
    }

    #[tokio::test]
    async fn cas_version_conflict() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("work", "");
        let task = store.create("default", spec, vec![]).await.expect("create");

        // Claim once to bump version.
        store
            .apply_transition(
                "default",
                &task.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent-a".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("first claim");

        // Try to apply Complete with stale version 0 (task is now version 1).
        // The state machine will reject Complete on Active (need a different test),
        // so use Cancel which is valid from Active, but with wrong version.
        let result = store
            .apply_transition(
                "default",
                &task.id.0,
                Some(0), // stale
                &TransitionEvent::Cancel {
                    reason: "test".into(),
                },
            )
            .await;

        assert!(
            matches!(result, Err(TransitionError::VersionConflict { .. })),
            "expected VersionConflict, got {result:?}"
        );
    }

    #[tokio::test]
    async fn list_and_filter() {
        let (store, _dir) = test_store().await;
        store
            .create("list1", TaskSpec::new("a", ""), vec![])
            .await
            .expect("a");
        store
            .create("list1", TaskSpec::new("b", ""), vec![])
            .await
            .expect("b");

        let all = store.list("list1", None).await.expect("list all");
        assert_eq!(all.len(), 2);

        let pending = store
            .list("list1", Some("Pending"))
            .await
            .expect("list pending");
        assert_eq!(pending.len(), 2);

        let active = store
            .list("list1", Some("Active"))
            .await
            .expect("list active");
        assert_eq!(active.len(), 0);
    }

    #[tokio::test]
    async fn event_log_records_transitions() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("logged", "");
        let task = store.create("list1", spec, vec![]).await.expect("create");

        store
            .apply_transition(
                "list1",
                &task.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim");

        let history = store
            .event_log()
            .history("list1", &task.id.0)
            .await
            .expect("history");

        // create event + Claim event
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].event_type, "Claim");
        assert_eq!(history[1].from_state, "Pending");
        assert_eq!(history[1].to_state, "Active");
    }

    #[tokio::test]
    async fn due_retries_only_past_deadline() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("retry-test", "");
        let task = store.create("r", spec, vec![]).await.expect("create");

        // Claim then fail with a past retry_after.
        store
            .apply_transition(
                "r",
                &task.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim");

        let past = OffsetDateTime::now_utc() - time::Duration::seconds(60);
        store
            .apply_transition(
                "r",
                &task.id.0,
                None,
                &TransitionEvent::Fail {
                    class: FailureClass::AgentError,
                    handoff: HandoffContext::default(),
                    retry_after: Some(past),
                },
            )
            .await
            .expect("fail");

        let due = store.due_retries("r").await.expect("due_retries");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, task.id);
    }

    #[tokio::test]
    async fn promote_due_retries_all_across_lists() {
        let (store, _dir) = test_store().await;

        // Two tasks in different lists, both overdue.
        let past = OffsetDateTime::now_utc() - time::Duration::seconds(30);
        for list in ["list-a", "list-b"] {
            let spec = TaskSpec::new("retry-me", "");
            let t = store.create(list, spec, vec![]).await.expect("create");
            // Claim → Active
            let t = store
                .apply_transition(
                    list,
                    &t.id.0,
                    None,
                    &TransitionEvent::Claim {
                        owner: "agent".into(),
                        lease_duration_secs: None,
                    },
                )
                .await
                .expect("claim");
            // Fail → Retrying (with past retry_after)
            store
                .apply_transition(
                    list,
                    &t.id.0,
                    None,
                    &TransitionEvent::Fail {
                        class: FailureClass::AgentError,
                        handoff: HandoffContext::default(),
                        retry_after: Some(past),
                    },
                )
                .await
                .expect("fail");
        }

        // One task with a future retry_after — should NOT be promoted.
        let future = OffsetDateTime::now_utc() + time::Duration::seconds(300);
        let spec = TaskSpec::new("not-yet", "");
        let t = store
            .create("list-a", spec, vec![])
            .await
            .expect("create future");
        let t = store
            .apply_transition(
                "list-a",
                &t.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim future");
        store
            .apply_transition(
                "list-a",
                &t.id.0,
                None,
                &TransitionEvent::Fail {
                    class: FailureClass::AgentError,
                    handoff: HandoffContext::default(),
                    retry_after: Some(future),
                },
            )
            .await
            .expect("fail future");

        let promoted = store.promote_due_retries_all().await.expect("promote");
        assert_eq!(promoted, 2, "only the two overdue tasks should be promoted");

        // Verify both overdue tasks are now Pending.
        for list in ["list-a", "list-b"] {
            let pending = store.list(list, Some("Pending")).await.expect("list");
            assert!(
                pending.iter().any(|t| t.spec.subject == "retry-me"),
                "{list}: expected retry-me to be Pending"
            );
        }

        // Verify the not-yet task is still Retrying.
        let retrying_a = store
            .list("list-a", Some("Retrying"))
            .await
            .expect("retrying");
        assert_eq!(retrying_a.len(), 1);
        assert_eq!(retrying_a[0].spec.subject, "not-yet");
    }
}
