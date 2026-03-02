//! Append-only event ledger per task.
//!
//! `EventLog` writes one row to `task_events` for every successful transition.
//! It is a diagnostic and audit trail — it is never read back to drive behaviour.

use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

/// A single event record.
#[derive(Debug, Clone)]
pub struct TaskEvent {
    pub id: i64,
    pub task_id: String,
    pub list_id: String,
    pub event_type: String,
    pub from_state: String,
    pub to_state: String,
    pub agent_id: Option<String>,
    pub detail: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Append-only event log backed by SQLite.
#[derive(Clone)]
pub struct EventLog {
    pool: SqlitePool,
}

impl EventLog {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Append a transition event to the log.
    pub async fn append(
        &self,
        list_id: &str,
        task_id: &str,
        event_type: &str,
        from_state: &str,
        to_state: &str,
        agent_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        sqlx::query(
            "INSERT INTO task_events \
             (task_id, list_id, event_type, from_state, to_state, agent_id, detail, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(task_id)
        .bind(list_id)
        .bind(event_type)
        .bind(from_state)
        .bind(to_state)
        .bind(agent_id)
        .bind(detail)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Retrieve all events for a task, oldest first.
    pub async fn history(
        &self,
        list_id: &str,
        task_id: &str,
    ) -> Result<Vec<TaskEvent>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, task_id, list_id, event_type, from_state, to_state, \
             agent_id, detail, created_at \
             FROM task_events \
             WHERE list_id = ? AND task_id = ? \
             ORDER BY id ASC",
        )
        .bind(list_id)
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| TaskEvent {
                id: r.get::<i64, _>("id"),
                task_id: r.get::<String, _>("task_id"),
                list_id: r.get::<String, _>("list_id"),
                event_type: r.get::<String, _>("event_type"),
                from_state: r.get::<String, _>("from_state"),
                to_state: r.get::<String, _>("to_state"),
                agent_id: r.get::<Option<String>, _>("agent_id"),
                detail: r.get::<Option<String>, _>("detail"),
                created_at: OffsetDateTime::from_unix_timestamp(r.get::<i64, _>("created_at"))
                    .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            })
            .collect())
    }
}
