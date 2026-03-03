//! Append-only event ledger per task.
//!
//! `EventLog` writes one row to `task_events` for every successful transition.
//! It is a diagnostic and audit trail — it is never read back to drive behaviour.

use {
    sqlx::{Row, SqlitePool},
    time::OffsetDateTime,
};

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

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    async fn test_log() -> EventLog {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS task_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                task_id     TEXT NOT NULL,
                list_id     TEXT NOT NULL,
                event_type  TEXT NOT NULL,
                from_state  TEXT NOT NULL,
                to_state    TEXT NOT NULL,
                agent_id    TEXT,
                detail      TEXT,
                created_at  INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        EventLog::new(pool)
    }

    #[tokio::test]
    async fn append_and_history_oldest_first() {
        let log = test_log().await;
        log.append("list1", "task-1", "Create", "", "Pending", None, None)
            .await
            .unwrap();
        log.append(
            "list1",
            "task-1",
            "Claim",
            "Pending",
            "Active",
            Some("bot"),
            None,
        )
        .await
        .unwrap();
        log.append(
            "list1",
            "task-1",
            "Complete",
            "Active",
            "Terminal",
            None,
            Some("done"),
        )
        .await
        .unwrap();

        let history = log.history("list1", "task-1").await.unwrap();
        assert_eq!(history.len(), 3);
        // Oldest first — IDs must be strictly ascending.
        assert!(history[0].id < history[1].id);
        assert!(history[1].id < history[2].id);
        assert_eq!(history[0].event_type, "Create");
        assert_eq!(history[2].event_type, "Complete");
    }

    #[tokio::test]
    async fn history_is_scoped_to_task_and_list() {
        let log = test_log().await;
        log.append("list1", "t1", "Create", "", "Pending", None, None)
            .await
            .unwrap();
        log.append("list2", "t1", "Create", "", "Pending", None, None)
            .await
            .unwrap();
        log.append("list1", "t2", "Create", "", "Pending", None, None)
            .await
            .unwrap();

        let h = log.history("list1", "t1").await.unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].list_id, "list1");
        assert_eq!(h[0].task_id, "t1");
    }

    #[tokio::test]
    async fn optional_fields_round_trip() {
        let log = test_log().await;
        log.append(
            "l",
            "t",
            "Claim",
            "Pending",
            "Active",
            Some("agent-x"),
            Some("detail text"),
        )
        .await
        .unwrap();

        let h = log.history("l", "t").await.unwrap();
        assert_eq!(h[0].agent_id.as_deref(), Some("agent-x"));
        assert_eq!(h[0].detail.as_deref(), Some("detail text"));
    }

    #[tokio::test]
    async fn empty_history_for_unknown_task() {
        let log = test_log().await;
        let h = log.history("no-list", "no-task").await.unwrap();
        assert!(h.is_empty());
    }
}
