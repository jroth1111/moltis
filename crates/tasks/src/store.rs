//! SQLite-backed task store with optimistic concurrency (CAS).
//!
//! Every write uses `UPDATE ... WHERE version = :expected` to detect concurrent
//! modifications without holding a lock across async boundaries.

use std::path::Path;

use {
    sqlx::{Row, Sqlite, SqlitePool, Transaction, sqlite::SqliteConnectOptions},
    time::OffsetDateTime,
};

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
        let is_intent: i64 = if task.spec.is_intent { 1 } else { 0 };
        let parent_task = task.spec.parent_task.as_ref().map(|id| id.0.as_str());
        let principal_json = task
            .spec
            .principal
            .as_ref()
            .map(|p| serde_json::to_string(p).map_err(|e| TransitionError::Other(e.to_string())))
            .transpose()?;
        let state_name = task.runtime.state.name();

        sqlx::query(
            "INSERT INTO tasks (id, list_id, spec_json, runtime_json, blocked_by, version, \
             created_at, updated_at, is_intent, parent_task, principal_json, state_name) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&task.id.0)
        .bind(&task.list_id)
        .bind(&spec_json)
        .bind(&runtime_json)
        .bind(&blocked_by_json)
        .bind(version)
        .bind(now)
        .bind(now)
        .bind(is_intent)
        .bind(parent_task)
        .bind(principal_json.as_deref())
        .bind(state_name)
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
            if let Some(filter) = state_filter
                && task.runtime.state.name().to_lowercase() != filter.to_lowercase()
            {
                continue;
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
        if let Some(expected) = expected_version
            && current.runtime.version != expected
        {
            return Err(TransitionError::VersionConflict {
                expected,
                actual: current.runtime.version,
            });
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
        let state_name = updated.runtime.state.name();

        let rows_affected = sqlx::query(
            "UPDATE tasks SET runtime_json = ?, version = ?, updated_at = ?, state_name = ? \
             WHERE list_id = ? AND id = ? AND version = ?",
        )
        .bind(&runtime_json)
        .bind(new_version)
        .bind(now)
        .bind(state_name)
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

    /// Return the underlying SQLite pool.
    ///
    /// Used by the dispatch layer for cross-store transactional finalization
    /// (intent_state + task_outputs share the same pool).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Begin an explicit transaction on the shared pool.
    pub async fn begin_tx(&self) -> Result<Transaction<'_, Sqlite>, TransitionError> {
        self.pool.begin().await.map_err(TransitionError::Storage)
    }

    /// Read a single task using an existing transaction.
    pub async fn get_tx(
        tx: &mut Transaction<'_, Sqlite>,
        list_id: &str,
        task_id: &str,
    ) -> Result<Option<Task>, TransitionError> {
        let row = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks WHERE list_id = ? AND id = ?",
        )
        .bind(list_id)
        .bind(task_id)
        .fetch_optional(&mut **tx)
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

    /// Apply a transition within an existing transaction (no auto-commit).
    ///
    /// The caller is responsible for committing the transaction. This enables
    /// atomic multi-table operations (e.g. finalize shift + update intent state).
    pub async fn apply_transition_tx(
        tx: &mut Transaction<'_, Sqlite>,
        list_id: &str,
        task_id: &str,
        expected_version: Option<u64>,
        event: &TransitionEvent,
    ) -> Result<Task, TransitionError> {
        let current = Self::get_tx(tx, list_id, task_id)
            .await?
            .ok_or_else(|| TransitionError::NotFound(task_id.to_string()))?;

        if let Some(expected) = expected_version
            && current.runtime.version != expected
        {
            return Err(TransitionError::VersionConflict {
                expected,
                actual: current.runtime.version,
            });
        }

        let from_state = current.runtime.state.name();
        let from_version = current.runtime.version;
        let updated = apply(current, event)?;

        let runtime_json = serde_json::to_string(&updated.runtime)
            .map_err(|e| TransitionError::Other(e.to_string()))?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let new_version = updated.runtime.version as i64;
        let old_version = from_version as i64;
        let to_state = updated.runtime.state.name();

        let rows_affected = sqlx::query(
            "UPDATE tasks SET runtime_json = ?, version = ?, updated_at = ?, state_name = ? \
             WHERE list_id = ? AND id = ? AND version = ?",
        )
        .bind(&runtime_json)
        .bind(new_version)
        .bind(now)
        .bind(to_state)
        .bind(&updated.list_id)
        .bind(&updated.id.0)
        .bind(old_version)
        .execute(&mut **tx)
        .await
        .map_err(TransitionError::Storage)?
        .rows_affected();

        if rows_affected == 0 {
            return Err(TransitionError::VersionConflict {
                expected: from_version,
                actual: from_version + 1,
            });
        }

        let event_type = event_type_name(event);
        sqlx::query(
            "INSERT INTO task_events (task_id, list_id, event_type, from_state, to_state, agent_id, detail, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&updated.id.0)
        .bind(&updated.list_id)
        .bind(event_type)
        .bind(from_state)
        .bind(to_state)
        .bind(updated.runtime.owner.as_deref())
        .bind(None::<&str>)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(TransitionError::Storage)?;

        Ok(updated)
    }

    /// Check whether any child task (parent_task = intent_id) is in a
    /// non-terminal state. Used as a guard before creating new shifts.
    pub async fn has_non_terminal_child(
        &self,
        intent_id: &str,
    ) -> Result<bool, TransitionError> {
        let row = sqlx::query(
            "SELECT COUNT(*) as cnt FROM tasks \
             WHERE parent_task = ? \
             AND state_name NOT IN ('Completed', 'Failed', 'Cancelled')",
        )
        .bind(intent_id)
        .fetch_one(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        let count: i64 = row.get("cnt");
        Ok(count > 0)
    }

    /// List intent tasks in actionable states (Pending or Active).
    ///
    /// Used by the dispatch loop to find intents that need a new shift or
    /// are currently being executed.
    pub async fn list_actionable_intents(&self) -> Result<Vec<Task>, TransitionError> {
        let rows = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks \
             WHERE is_intent = 1 AND state_name IN ('Pending', 'Active') \
             ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        rows.into_iter()
            .map(|r| {
                Self::row_to_task(
                    r.get::<String, _>("id"),
                    r.get::<String, _>("list_id"),
                    r.get::<String, _>("spec_json"),
                    r.get::<String, _>("runtime_json"),
                    r.get::<String, _>("blocked_by"),
                    r.get::<i64, _>("version"),
                )
            })
            .collect()
    }

    /// List child shift tasks for a given intent.
    pub async fn list_shifts_for_intent(
        &self,
        intent_id: &str,
    ) -> Result<Vec<Task>, TransitionError> {
        let rows = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks \
             WHERE parent_task = ? \
             ORDER BY created_at ASC",
        )
        .bind(intent_id)
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        rows.into_iter()
            .map(|r| {
                Self::row_to_task(
                    r.get::<String, _>("id"),
                    r.get::<String, _>("list_id"),
                    r.get::<String, _>("spec_json"),
                    r.get::<String, _>("runtime_json"),
                    r.get::<String, _>("blocked_by"),
                    r.get::<i64, _>("version"),
                )
            })
            .collect()
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
            if let RuntimeState::Retrying { retry_after, .. } = &task.runtime.state
                && retry_after.unix_timestamp() <= now
            {
                due.push(task);
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

    /// Sweep all lists for Active tasks with an expired lease and fail them as
    /// [`FailureClass::TimeoutExceeded`].
    ///
    /// Called by the background zombie-reclamation poll. Returns the count of
    /// tasks that were successfully transitioned. `VersionConflict` is silently
    /// skipped (another writer already changed the task); other errors are logged
    /// and skipped.
    pub async fn expire_zombie_leases_all(&self) -> Result<usize, TransitionError> {
        use crate::types::{FailureClass, HandoffContext};

        let rows = sqlx::query(
            "SELECT id, list_id, spec_json, runtime_json, blocked_by, version \
             FROM tasks",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        let mut expired: Vec<Task> = rows
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
            .filter(|t| t.runtime.state.is_lease_expired())
            .collect();

        let mut reclaimed = 0usize;
        for task in expired.drain(..) {
            let list_id = task.list_id.clone();
            let task_id = task.id.0.clone();
            let version = task.runtime.version;
            match self
                .apply_transition(
                    &list_id,
                    &task_id,
                    Some(version),
                    &TransitionEvent::Fail {
                        class: FailureClass::TimeoutExceeded,
                        handoff: HandoffContext::default(),
                        retry_after: None,
                    },
                )
                .await
            {
                Ok(_) => reclaimed += 1,
                Err(TransitionError::VersionConflict { .. }) => {
                    // Another writer raced us; skip — will be picked up next sweep.
                },
                Err(e) => {
                    #[cfg(feature = "tracing")]
                    tracing::warn!(
                        list_id = %list_id,
                        task_id = %task_id,
                        error = %e,
                        "zombie lease reclamation failed"
                    );
                    let _ = e;
                },
            }
        }

        Ok(reclaimed)
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
        TransitionEvent::RenewLease { .. } => "RenewLease",
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::types::{FailureClass, HandoffContext},
    };

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

    #[tokio::test]
    async fn update_metadata_changes_spec_without_bumping_version() {
        let (store, _dir) = test_store().await;
        let task = store
            .create("list1", TaskSpec::new("original", "desc"), vec![])
            .await
            .expect("create");
        let v0 = task.runtime.version;

        let updated = store
            .update_metadata("list1", &task.id.0, Some("renamed"), Some("new desc"), None)
            .await
            .expect("update_metadata");

        assert_eq!(updated.spec.subject, "renamed");
        assert_eq!(updated.spec.description, "new desc");
        // Version must NOT be incremented by metadata-only update.
        assert_eq!(updated.runtime.version, v0);
    }

    #[tokio::test]
    async fn update_metadata_none_fields_unchanged() {
        let (store, _dir) = test_store().await;
        let task = store
            .create("list1", TaskSpec::new("subject", "desc"), vec![])
            .await
            .expect("create");

        // Pass None for subject and description — only blocked_by changes.
        let updated = store
            .update_metadata("list1", &task.id.0, None, None, Some(&[]))
            .await
            .expect("update_metadata");

        assert_eq!(updated.spec.subject, "subject");
        assert_eq!(updated.spec.description, "desc");
        assert!(updated.blocked_by.is_empty());
    }

    #[tokio::test]
    async fn expired_leases_returns_only_expired() {
        let (store, _dir) = test_store().await;

        // Task claimed with an already-expired lease (1 second in the past).
        let t = store
            .create("list1", TaskSpec::new("exp", ""), vec![])
            .await
            .expect("create");
        let claimed = store
            .apply_transition(
                "list1",
                &t.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: Some(1),
                },
            )
            .await
            .expect("claim");

        // Task claimed without a lease — should NOT appear in expired_leases.
        let t2 = store
            .create("list1", TaskSpec::new("no-lease", ""), vec![])
            .await
            .expect("create2");
        store
            .apply_transition(
                "list1",
                &t2.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim2");

        // Force the first task's lease into the past by patching the JSON.
        let past = OffsetDateTime::now_utc() - time::Duration::seconds(60);
        let mut rt = claimed.runtime.clone();
        rt.state = RuntimeState::Active {
            owner: "agent".into(),
            lease_expires_at: Some(past),
        };
        let rt_json = serde_json::to_string(&rt).expect("serialize");
        sqlx::query("UPDATE tasks SET runtime_json = ? WHERE list_id = ? AND id = ?")
            .bind(&rt_json)
            .bind("list1")
            .bind(&claimed.id.0)
            .execute(store.pool())
            .await
            .expect("patch runtime");

        let expired = store.expired_leases("list1").await.expect("expired_leases");
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, claimed.id);
    }

    #[tokio::test]
    async fn expire_zombie_leases_all_transitions_expired_tasks() {
        let (store, _dir) = test_store().await;

        // Task claimed with an active lease — will be patched to expired.
        let t = store
            .create("z", TaskSpec::new("zombie", ""), vec![])
            .await
            .expect("create");
        let claimed = store
            .apply_transition(
                "z",
                &t.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: Some(3600),
                },
            )
            .await
            .expect("claim");

        // Task without a lease — should NOT be affected.
        let t2 = store
            .create("z", TaskSpec::new("no-lease", ""), vec![])
            .await
            .expect("create2");
        store
            .apply_transition(
                "z",
                &t2.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim2");

        // Patch the first task's lease 60 seconds into the past.
        let past = OffsetDateTime::now_utc() - time::Duration::seconds(60);
        let mut rt = claimed.runtime.clone();
        rt.state = RuntimeState::Active {
            owner: "agent".into(),
            lease_expires_at: Some(past),
        };
        let rt_json = serde_json::to_string(&rt).expect("serialize");
        sqlx::query("UPDATE tasks SET runtime_json = ? WHERE list_id = ? AND id = ?")
            .bind(&rt_json)
            .bind("z")
            .bind(&claimed.id.0)
            .execute(store.pool())
            .await
            .expect("patch runtime");

        let count = store.expire_zombie_leases_all().await.expect("sweep");
        assert_eq!(count, 1, "only the expired-lease task should be reclaimed");

        // The expired task must no longer be Active.
        let reclaimed = store
            .get("z", &claimed.id.0)
            .await
            .expect("get")
            .expect("exists");
        assert!(
            !reclaimed.runtime.state.is_active(),
            "reclaimed task should not be Active; got {:?}",
            reclaimed.runtime.state
        );
    }

    #[tokio::test]
    async fn integration_create_claim_fail_promote() {
        let (store, _dir) = test_store().await;

        let task = store
            .create("wf", TaskSpec::new("e2e", ""), vec![])
            .await
            .expect("create");
        assert_eq!(task.runtime.state, RuntimeState::Pending);

        // Claim
        let task = store
            .apply_transition(
                "wf",
                &task.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "bot".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim");
        assert!(task.runtime.state.is_active());

        // Fail with overdue retry_after
        let past = OffsetDateTime::now_utc() - time::Duration::seconds(5);
        let task = store
            .apply_transition(
                "wf",
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
        assert!(matches!(task.runtime.state, RuntimeState::Retrying { .. }));

        // promote_due_retries_all should pick it up
        let n = store.promote_due_retries_all().await.expect("promote");
        assert_eq!(n, 1);

        let final_task = store
            .get("wf", &task.id.0)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(final_task.runtime.state, RuntimeState::Pending);
    }

    // ── Dispatch column tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn insert_writes_denormalized_columns() {
        use crate::types::TaskPrincipal;

        let (store, _dir) = test_store().await;
        let mut spec = TaskSpec::new("intent-task", "find restaurants");
        spec.is_intent = true;
        spec.principal = Some(TaskPrincipal {
            channel: "whatsapp".into(),
            sender: "+15551234567".into(),
            account_id: "biz-1".into(),
        });

        let task = store.create("list1", spec, vec![]).await.expect("create");

        // Verify denormalized columns directly via SQL.
        let row = sqlx::query(
            "SELECT is_intent, parent_task, principal_json, state_name \
             FROM tasks WHERE list_id = ? AND id = ?",
        )
        .bind("list1")
        .bind(&task.id.0)
        .fetch_one(store.pool())
        .await
        .expect("fetch row");

        let is_intent: i64 = row.get("is_intent");
        let parent_task: Option<String> = row.get("parent_task");
        let principal_json: Option<String> = row.get("principal_json");
        let state_name: String = row.get("state_name");

        assert_eq!(is_intent, 1);
        assert!(parent_task.is_none());
        assert!(principal_json.is_some());
        assert_eq!(state_name, "Pending");

        // Verify principal round-trips.
        let p: TaskPrincipal =
            serde_json::from_str(principal_json.as_ref().unwrap()).expect("deserialize");
        assert_eq!(p.channel, "whatsapp");
    }

    #[tokio::test]
    async fn insert_non_intent_defaults() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("normal task", "");
        let task = store.create("list1", spec, vec![]).await.expect("create");

        let row = sqlx::query(
            "SELECT is_intent, parent_task, principal_json, state_name \
             FROM tasks WHERE list_id = ? AND id = ?",
        )
        .bind("list1")
        .bind(&task.id.0)
        .fetch_one(store.pool())
        .await
        .expect("fetch row");

        let is_intent: i64 = row.get("is_intent");
        let parent_task: Option<String> = row.get("parent_task");
        let principal_json: Option<String> = row.get("principal_json");

        assert_eq!(is_intent, 0);
        assert!(parent_task.is_none());
        assert!(principal_json.is_none());
    }

    #[tokio::test]
    async fn transition_updates_state_name_column() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("track-state", "");
        let task = store.create("list1", spec, vec![]).await.expect("create");

        // Verify initial state_name.
        let row = sqlx::query("SELECT state_name FROM tasks WHERE list_id = ? AND id = ?")
            .bind("list1")
            .bind(&task.id.0)
            .fetch_one(store.pool())
            .await
            .expect("fetch");
        let state_name: String = row.get("state_name");
        assert_eq!(state_name, "Pending");

        // Claim → Active.
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

        let row = sqlx::query("SELECT state_name FROM tasks WHERE list_id = ? AND id = ?")
            .bind("list1")
            .bind(&task.id.0)
            .fetch_one(store.pool())
            .await
            .expect("fetch");
        let state_name: String = row.get("state_name");
        assert_eq!(state_name, "Active");

        // Complete → Terminal(Completed).
        store
            .apply_transition("list1", &task.id.0, None, &TransitionEvent::Complete)
            .await
            .expect("complete");

        let row = sqlx::query("SELECT state_name FROM tasks WHERE list_id = ? AND id = ?")
            .bind("list1")
            .bind(&task.id.0)
            .fetch_one(store.pool())
            .await
            .expect("fetch");
        let state_name: String = row.get("state_name");
        assert_eq!(state_name, "Completed");
    }

    #[tokio::test]
    async fn insert_with_parent_task() {
        use crate::types::TaskId;

        let (store, _dir) = test_store().await;
        let mut spec = TaskSpec::new("shift-1", "");
        let parent_id = TaskId::new();
        spec.parent_task = Some(parent_id.clone());

        let task = store.create("list1", spec, vec![]).await.expect("create");

        let row = sqlx::query("SELECT parent_task FROM tasks WHERE list_id = ? AND id = ?")
            .bind("list1")
            .bind(&task.id.0)
            .fetch_one(store.pool())
            .await
            .expect("fetch");
        let pt: Option<String> = row.get("parent_task");
        assert_eq!(pt, Some(parent_id.0));
    }

    // ── Dispatch query tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn list_actionable_intents_filters_correctly() {
        let (store, _dir) = test_store().await;

        // Normal task (not intent) — should not appear.
        store
            .create("list1", TaskSpec::new("normal", ""), vec![])
            .await
            .expect("create normal");

        // Intent task (Pending) — should appear.
        let mut spec = TaskSpec::new("intent-1", "");
        spec.is_intent = true;
        store
            .create("list1", spec, vec![])
            .await
            .expect("create intent-1");

        // Intent task (Active) — should appear.
        let mut spec = TaskSpec::new("intent-2", "");
        spec.is_intent = true;
        let t = store
            .create("list1", spec, vec![])
            .await
            .expect("create intent-2");
        store
            .apply_transition(
                "list1",
                &t.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim intent-2");

        // Intent task (Completed) — should NOT appear.
        let mut spec = TaskSpec::new("intent-done", "");
        spec.is_intent = true;
        let t = store
            .create("list1", spec, vec![])
            .await
            .expect("create intent-done");
        store
            .apply_transition(
                "list1",
                &t.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim intent-done");
        store
            .apply_transition("list1", &t.id.0, None, &TransitionEvent::Complete)
            .await
            .expect("complete intent-done");

        let intents = store.list_actionable_intents().await.expect("list intents");
        assert_eq!(intents.len(), 2);
        let subjects: Vec<&str> = intents.iter().map(|t| t.spec.subject.as_str()).collect();
        assert!(subjects.contains(&"intent-1"));
        assert!(subjects.contains(&"intent-2"));
    }

    #[tokio::test]
    async fn has_non_terminal_child_detects_active_shift() {
        let (store, _dir) = test_store().await;

        // Create intent.
        let mut intent_spec = TaskSpec::new("intent", "");
        intent_spec.is_intent = true;
        let intent = store
            .create("list1", intent_spec, vec![])
            .await
            .expect("create intent");

        // No children yet.
        assert!(
            !store
                .has_non_terminal_child(&intent.id.0)
                .await
                .expect("check")
        );

        // Create a shift child in Pending state.
        let mut shift_spec = TaskSpec::new("shift-1", "");
        shift_spec.parent_task = Some(intent.id.clone());
        let shift = store
            .create("list1", shift_spec, vec![])
            .await
            .expect("create shift");

        // Now has a non-terminal child.
        assert!(
            store
                .has_non_terminal_child(&intent.id.0)
                .await
                .expect("check")
        );

        // Claim → Active.
        store
            .apply_transition(
                "list1",
                &shift.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "agent".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim shift");

        assert!(
            store
                .has_non_terminal_child(&intent.id.0)
                .await
                .expect("check active")
        );

        // Complete the shift.
        store
            .apply_transition("list1", &shift.id.0, None, &TransitionEvent::Complete)
            .await
            .expect("complete shift");

        // All children terminal — guard should pass.
        assert!(
            !store
                .has_non_terminal_child(&intent.id.0)
                .await
                .expect("check completed")
        );
    }

    #[tokio::test]
    async fn list_shifts_for_intent_returns_children() {
        let (store, _dir) = test_store().await;

        let mut intent_spec = TaskSpec::new("intent", "");
        intent_spec.is_intent = true;
        let intent = store
            .create("list1", intent_spec, vec![])
            .await
            .expect("create intent");

        // Create two child shifts.
        for i in 0..2 {
            let mut spec = TaskSpec::new(format!("shift-{i}"), "");
            spec.parent_task = Some(intent.id.clone());
            store
                .create("list1", spec, vec![])
                .await
                .expect("create shift");
        }

        // Unrelated task (no parent).
        store
            .create("list1", TaskSpec::new("unrelated", ""), vec![])
            .await
            .expect("create unrelated");

        let shifts = store
            .list_shifts_for_intent(&intent.id.0)
            .await
            .expect("list shifts");
        assert_eq!(shifts.len(), 2);
        assert!(shifts.iter().all(|s| s.spec.parent_task.as_ref().unwrap() == &intent.id));
    }

    #[tokio::test]
    async fn apply_transition_tx_within_transaction() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("tx-test", "");
        let task = store.create("list1", spec, vec![]).await.expect("create");

        // Begin a transaction and apply Claim within it.
        let mut tx = store.begin_tx().await.expect("begin tx");

        let claimed = TaskStore::apply_transition_tx(
            &mut tx,
            "list1",
            &task.id.0,
            None,
            &TransitionEvent::Claim {
                owner: "tx-agent".into(),
                lease_duration_secs: None,
            },
        )
        .await
        .expect("claim in tx");

        assert!(claimed.runtime.state.is_active());
        assert_eq!(claimed.runtime.version, 1);

        // Before commit, the non-transactional read should still see Pending.
        let before_commit = store
            .get("list1", &task.id.0)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(
            before_commit.runtime.state,
            RuntimeState::Pending,
            "uncommitted tx should not be visible outside"
        );

        // Commit.
        tx.commit().await.expect("commit");

        // After commit, the change is visible.
        let after_commit = store
            .get("list1", &task.id.0)
            .await
            .expect("get")
            .expect("exists");
        assert!(after_commit.runtime.state.is_active());

        // state_name column should also be updated.
        let row = sqlx::query("SELECT state_name FROM tasks WHERE list_id = ? AND id = ?")
            .bind("list1")
            .bind(&task.id.0)
            .fetch_one(store.pool())
            .await
            .expect("fetch");
        let state_name: String = row.get("state_name");
        assert_eq!(state_name, "Active");
    }

    #[tokio::test]
    async fn apply_transition_tx_rollback_on_drop() {
        let (store, _dir) = test_store().await;
        let spec = TaskSpec::new("rollback-test", "");
        let task = store.create("list1", spec, vec![]).await.expect("create");

        {
            let mut tx = store.begin_tx().await.expect("begin tx");
            TaskStore::apply_transition_tx(
                &mut tx,
                "list1",
                &task.id.0,
                None,
                &TransitionEvent::Claim {
                    owner: "doomed".into(),
                    lease_duration_secs: None,
                },
            )
            .await
            .expect("claim in tx");

            // Drop tx without committing — implicit rollback.
        }

        let task = store
            .get("list1", &task.id.0)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(
            task.runtime.state,
            RuntimeState::Pending,
            "rolled-back tx should leave task unchanged"
        );
    }
}
