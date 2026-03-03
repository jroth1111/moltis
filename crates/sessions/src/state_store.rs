//! Per-session key-value state store.
//!
//! Provides a SQLite-backed KV store scoped to `(session_key, namespace, key)`
//! so that skills and extensions can persist context across messages.

use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::Result;

/// A single state entry.
#[derive(Debug, Clone)]
pub struct StateEntry {
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub updated_at: u64,
}

#[derive(sqlx::FromRow)]
struct StateRow {
    namespace: String,
    key: String,
    value: String,
    updated_at: i64,
}

impl From<StateRow> for StateEntry {
    fn from(r: StateRow) -> Self {
        Self {
            namespace: r.namespace,
            key: r.key,
            value: r.value,
            updated_at: r.updated_at as u64,
        }
    }
}

/// Handoff context for session resumption.
///
/// Captures the essential state when a run ends so that subsequent runs
/// can quickly understand what was happening and continue effectively.
/// The context expires after 24 hours to avoid stale state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffContext {
    /// The primary goal the agent was working toward.
    pub last_goal: Option<String>,
    /// Tasks that were pending when the run ended.
    pub pending_tasks: Vec<String>,
    /// The last error encountered, if the run ended in error.
    pub last_error: Option<String>,
    /// The working directory at the end of the run.
    pub working_directory: Option<PathBuf>,
    /// Key facts learned during the run that might be useful for continuation.
    pub key_facts: Vec<String>,
    /// Unix timestamp (milliseconds) when this context was created.
    pub created_at: i64,
}

impl HandoffContext {
    /// Namespace used for storing handoff context in the state store.
    pub const NAMESPACE: &'static str = "handoff";

    /// Key used for storing handoff context in the state store.
    pub const KEY: &'static str = "context";

    /// Time-to-live for handoff context (24 hours) in milliseconds.
    pub const TTL_MS: i64 = 24 * 60 * 60 * 1000;

    /// Create a new handoff context with the current timestamp.
    pub fn new(
        last_goal: Option<String>,
        pending_tasks: Vec<String>,
        last_error: Option<String>,
        working_directory: Option<PathBuf>,
        key_facts: Vec<String>,
    ) -> Self {
        Self {
            last_goal,
            pending_tasks,
            last_error,
            working_directory,
            key_facts,
            created_at: now_ms(),
        }
    }

    /// Check if this context has expired (older than 24 hours).
    pub fn is_expired(&self) -> bool {
        let now = now_ms();
        let age_ms = now.saturating_sub(self.created_at);
        age_ms > Self::TTL_MS
    }

    /// Format the handoff context as a human-readable summary for injection into system prompts.
    pub fn to_prompt_summary(&self) -> String {
        if self.is_expired() {
            return String::new();
        }

        let mut parts = Vec::new();

        if let Some(ref goal) = self.last_goal {
            parts.push(format!("Previous goal: {}", goal));
        }

        if !self.pending_tasks.is_empty() {
            parts.push(format!(
                "Pending tasks:\n{}",
                self.pending_tasks
                    .iter()
                    .map(|t| format!("  - {}", t))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if let Some(ref error) = self.last_error {
            parts.push(format!("Last error: {}", error));
        }

        if let Some(ref dir) = self.working_directory {
            parts.push(format!("Working directory: {}", dir.display()));
        }

        if !self.key_facts.is_empty() {
            parts.push(format!(
                "Key facts:\n{}",
                self.key_facts
                    .iter()
                    .map(|f| format!("  - {}", f))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!(
                "## Session Context (from previous run)\n\n{}\n",
                parts.join("\n\n")
            )
        }
    }
}

/// SQLite-backed per-session state store.
pub struct SessionStateStore {
    pool: sqlx::SqlitePool,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

impl SessionStateStore {
    pub fn new(pool: sqlx::SqlitePool) -> Self {
        Self { pool }
    }

    /// Get a value by session key, namespace, and key.
    pub async fn get(
        &self,
        session_key: &str,
        namespace: &str,
        key: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query_scalar::<_, String>(
            "SELECT value FROM session_state WHERE session_key = ? AND namespace = ? AND key = ?",
        )
        .bind(session_key)
        .bind(namespace)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Set a value. Inserts or updates the entry.
    pub async fn set(
        &self,
        session_key: &str,
        namespace: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            r#"INSERT INTO session_state (session_key, namespace, key, value, updated_at)
               VALUES (?, ?, ?, ?, ?)
               ON CONFLICT(session_key, namespace, key) DO UPDATE SET
                 value = excluded.value,
                 updated_at = excluded.updated_at"#,
        )
        .bind(session_key)
        .bind(namespace)
        .bind(key)
        .bind(value)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a single key.
    pub async fn delete(&self, session_key: &str, namespace: &str, key: &str) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM session_state WHERE session_key = ? AND namespace = ? AND key = ?",
        )
        .bind(session_key)
        .bind(namespace)
        .bind(key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// List all entries in a namespace for a session.
    pub async fn list(&self, session_key: &str, namespace: &str) -> Result<Vec<StateEntry>> {
        let rows = sqlx::query_as::<_, StateRow>(
            "SELECT namespace, key, value, updated_at FROM session_state \
             WHERE session_key = ? AND namespace = ? ORDER BY key",
        )
        .bind(session_key)
        .bind(namespace)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Delete all entries in a namespace for a session.
    pub async fn delete_all(&self, session_key: &str, namespace: &str) -> Result<u64> {
        let result =
            sqlx::query("DELETE FROM session_state WHERE session_key = ? AND namespace = ?")
                .bind(session_key)
                .bind(namespace)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }

    /// Delete all state for a session (cascade on session delete).
    pub async fn delete_session(&self, session_key: &str) -> Result<u64> {
        let result = sqlx::query("DELETE FROM session_state WHERE session_key = ?")
            .bind(session_key)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// List all session keys that have an entry in the given namespace and key.
    ///
    /// Used by the self-repair scanner to find sessions with `running_since` set.
    pub async fn list_sessions_with_key(&self, namespace: &str, key: &str) -> Result<Vec<String>> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT session_key FROM session_state WHERE namespace = ? AND key = ?",
        )
        .bind(namespace)
        .bind(key)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// List all sessions that are currently marked as running (have `running_since` set).
    ///
    /// Convenience wrapper for `list_sessions_with_key("self_repair", "running_since")`.
    pub async fn list_running_sessions(&self) -> Result<Vec<String>> {
        self.list_sessions_with_key("self_repair", "running_since")
            .await
    }

    /// Save handoff context for a session.
    ///
    /// This should be called at the end of a run (success or error) to capture
    /// the essential state for resumption.
    pub async fn set_handoff(&self, session_key: &str, ctx: &HandoffContext) -> Result<()> {
        let value = serde_json::to_string(ctx)?;
        self.set(session_key, HandoffContext::NAMESPACE, HandoffContext::KEY, &value)
            .await
    }

    /// Get handoff context for a session.
    ///
    /// Returns `None` if no handoff exists or if it has expired (older than 24 hours).
    pub async fn get_handoff(&self, session_key: &str) -> Result<Option<HandoffContext>> {
        let Some(value) = self
            .get(session_key, HandoffContext::NAMESPACE, HandoffContext::KEY)
            .await?
        else {
            return Ok(None);
        };

        let ctx: HandoffContext = serde_json::from_str(&value)?;
        if ctx.is_expired() {
            // Clean up expired context
            let _ = self
                .delete(session_key, HandoffContext::NAMESPACE, HandoffContext::KEY)
                .await;
            return Ok(None);
        }

        Ok(Some(ctx))
    }

    /// Clear handoff context for a session.
    pub async fn clear_handoff(&self, session_key: &str) -> Result<bool> {
        self.delete(session_key, HandoffContext::NAMESPACE, HandoffContext::KEY)
            .await
    }

    /// Run migrations for tests (creates the session_state table in an in-memory DB).
    #[cfg(test)]
    pub async fn run_migrations_for_tests(pool: &sqlx::SqlitePool) -> Result<()> {
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
        .execute(pool)
        .await?;
        Ok(())
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> sqlx::SqlitePool {
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
        pool
    }

    #[tokio::test]
    async fn test_set_and_get() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store
            .set("session:1", "my-skill", "count", "42")
            .await
            .unwrap();
        let val = store.get("session:1", "my-skill", "count").await.unwrap();
        assert_eq!(val.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn test_get_missing() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        let val = store.get("session:1", "ns", "missing").await.unwrap();
        assert!(val.is_none());
    }

    #[tokio::test]
    async fn test_set_overwrites() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store.set("s1", "ns", "k", "v1").await.unwrap();
        store.set("s1", "ns", "k", "v2").await.unwrap();
        let val = store.get("s1", "ns", "k").await.unwrap();
        assert_eq!(val.as_deref(), Some("v2"));
    }

    #[tokio::test]
    async fn test_delete() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store.set("s1", "ns", "k", "v").await.unwrap();
        let deleted = store.delete("s1", "ns", "k").await.unwrap();
        assert!(deleted);
        assert!(store.get("s1", "ns", "k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete_missing() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        let deleted = store.delete("s1", "ns", "k").await.unwrap();
        assert!(!deleted);
    }

    #[tokio::test]
    async fn test_list() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store.set("s1", "ns", "a", "1").await.unwrap();
        store.set("s1", "ns", "b", "2").await.unwrap();
        store.set("s1", "other", "c", "3").await.unwrap();

        let entries = store.list("s1", "ns").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[1].key, "b");
    }

    #[tokio::test]
    async fn test_delete_all() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store.set("s1", "ns", "a", "1").await.unwrap();
        store.set("s1", "ns", "b", "2").await.unwrap();
        store.set("s1", "other", "c", "3").await.unwrap();

        let count = store.delete_all("s1", "ns").await.unwrap();
        assert_eq!(count, 2);
        // "other" namespace untouched.
        assert!(store.get("s1", "other", "c").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_delete_session() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store.set("s1", "ns1", "a", "1").await.unwrap();
        store.set("s1", "ns2", "b", "2").await.unwrap();
        store.set("s2", "ns1", "a", "3").await.unwrap();

        let count = store.delete_session("s1").await.unwrap();
        assert_eq!(count, 2);
        // s2 untouched.
        assert!(store.get("s2", "ns1", "a").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_namespace_isolation() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store.set("s1", "ns-a", "key", "val-a").await.unwrap();
        store.set("s1", "ns-b", "key", "val-b").await.unwrap();

        assert_eq!(
            store.get("s1", "ns-a", "key").await.unwrap().as_deref(),
            Some("val-a")
        );
        assert_eq!(
            store.get("s1", "ns-b", "key").await.unwrap().as_deref(),
            Some("val-b")
        );
    }

    #[tokio::test]
    async fn test_list_sessions_with_key() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store
            .set("session:1", "self_repair", "running_since", "123")
            .await
            .unwrap();
        store
            .set("session:2", "self_repair", "running_since", "456")
            .await
            .unwrap();
        store
            .set("session:2", "self_repair", "repair_attempts", "1")
            .await
            .unwrap();
        store
            .set("session:3", "other", "running_since", "789")
            .await
            .unwrap();

        let mut sessions = store
            .list_sessions_with_key("self_repair", "running_since")
            .await
            .unwrap();
        sessions.sort();
        assert_eq!(sessions, vec!["session:1", "session:2"]);
    }

    #[tokio::test]
    async fn test_list_running_sessions() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        store
            .set("session:running", "self_repair", "running_since", "111")
            .await
            .unwrap();
        store
            .set("session:idle", "self_repair", "repair_attempts", "2")
            .await
            .unwrap();

        let sessions = store.list_running_sessions().await.unwrap();
        assert_eq!(sessions, vec!["session:running"]);
    }

    // ── Handoff context tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_handoff_save_and_retrieve() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        let ctx = HandoffContext::new(
            Some("Fix the bug in auth module".to_string()),
            vec!["Add tests".to_string(), "Update docs".to_string()],
            None,
            Some(PathBuf::from("/home/user/project")),
            vec!["Bug is in line 42".to_string()],
        );

        store.set_handoff("session:1", &ctx).await.unwrap();

        let retrieved = store.get_handoff("session:1").await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.last_goal, Some("Fix the bug in auth module".to_string()));
        assert_eq!(retrieved.pending_tasks.len(), 2);
        assert_eq!(retrieved.working_directory, Some(PathBuf::from("/home/user/project")));
    }

    #[tokio::test]
    async fn test_handoff_with_error() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        let ctx = HandoffContext::new(
            Some("Deploy to production".to_string()),
            vec![],
            Some("Permission denied".to_string()),
            None,
            vec![],
        );

        store.set_handoff("session:error", &ctx).await.unwrap();

        let retrieved = store.get_handoff("session:error").await.unwrap().unwrap();
        assert_eq!(retrieved.last_error, Some("Permission denied".to_string()));
    }

    #[tokio::test]
    async fn test_handoff_missing() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        let retrieved = store.get_handoff("session:missing").await.unwrap();
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_handoff_expired() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        // Create an expired context by setting created_at to 25 hours ago
        let mut ctx = HandoffContext::new(
            Some("Old task".to_string()),
            vec![],
            None,
            None,
            vec![],
        );
        // 25 hours ago in milliseconds
        ctx.created_at = now_ms().saturating_sub(25 * 60 * 60 * 1000);

        // Manually store it
        let value = serde_json::to_string(&ctx).unwrap();
        store
            .set("session:expired", HandoffContext::NAMESPACE, HandoffContext::KEY, &value)
            .await
            .unwrap();

        // Should return None and clean up
        let retrieved = store.get_handoff("session:expired").await.unwrap();
        assert!(retrieved.is_none());

        // Verify it was deleted
        let raw = store
            .get("session:expired", HandoffContext::NAMESPACE, HandoffContext::KEY)
            .await
            .unwrap();
        assert!(raw.is_none());
    }

    #[tokio::test]
    async fn test_handoff_clear() {
        let pool = test_pool().await;
        let store = SessionStateStore::new(pool);

        let ctx = HandoffContext::new(
            Some("Task".to_string()),
            vec![],
            None,
            None,
            vec![],
        );

        store.set_handoff("session:clear", &ctx).await.unwrap();
        assert!(store.get_handoff("session:clear").await.unwrap().is_some());

        let cleared = store.clear_handoff("session:clear").await.unwrap();
        assert!(cleared);
        assert!(store.get_handoff("session:clear").await.unwrap().is_none());
    }

    #[test]
    fn test_handoff_prompt_summary() {
        let ctx = HandoffContext::new(
            Some("Fix the bug".to_string()),
            vec!["Add tests".to_string()],
            None,
            Some(PathBuf::from("/home/user/project")),
            vec!["Bug is in auth".to_string()],
        );

        let summary = ctx.to_prompt_summary();
        assert!(summary.contains("Previous goal: Fix the bug"));
        assert!(summary.contains("Pending tasks"));
        assert!(summary.contains("- Add tests"));
        assert!(summary.contains("Working directory: /home/user/project"));
        assert!(summary.contains("Key facts"));
        assert!(summary.contains("- Bug is in auth"));
    }

    #[test]
    fn test_handoff_prompt_summary_empty() {
        let ctx = HandoffContext::new(None, vec![], None, None, vec![]);
        let summary = ctx.to_prompt_summary();
        assert!(summary.is_empty());
    }

    #[test]
    fn test_handoff_is_expired() {
        let fresh_ctx = HandoffContext::new(
            Some("Task".to_string()),
            vec![],
            None,
            None,
            vec![],
        );
        assert!(!fresh_ctx.is_expired());

        let mut old_ctx = HandoffContext::new(
            Some("Task".to_string()),
            vec![],
            None,
            None,
            vec![],
        );
        old_ctx.created_at = now_ms().saturating_sub(25 * 60 * 60 * 1000);
        assert!(old_ctx.is_expired());
    }
}
