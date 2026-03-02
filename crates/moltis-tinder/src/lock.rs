use anyhow::Result;
use tracing::debug;

/// Time-to-live for a session lock in milliseconds (5 minutes).
const LOCK_TTL_MS: i64 = 5 * 60 * 1000;
const LOCK_NAMESPACE: &str = "tinder_lock";
const LOCK_KEY: &str = "owner";

use crate::util::now_ms;

/// Advisory lock using the session_state table to prevent concurrent
/// Tinder sessions from stepping on each other.
pub struct SessionLock {
    pool: sqlx::SqlitePool,
    session_key: String,
}

impl SessionLock {
    pub fn new(pool: sqlx::SqlitePool, session_key: String) -> Self {
        Self { pool, session_key }
    }

    /// Attempt to acquire the lock. Returns `true` if acquired, `false` if held.
    pub async fn try_acquire(&self) -> Result<bool> {
        self.try_acquire_or_expire().await
    }

    /// Attempt to acquire lock; if the current lock is expired, take it over.
    pub async fn try_acquire_or_expire(&self) -> Result<bool> {
        let now = now_ms();
        let expires = now + LOCK_TTL_MS;

        // Fast path: acquire via INSERT OR IGNORE.
        let result = sqlx::query(
            "INSERT OR IGNORE INTO session_state (session_key, namespace, key, value, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&self.session_key)
        .bind(LOCK_NAMESPACE)
        .bind(LOCK_KEY)
        .bind(expires.to_string())
        .bind(now)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() > 0 {
            debug!(session_key = %self.session_key, "tinder session lock acquired");
            return Ok(true);
        }

        // Slow path: existing lock may be stale. Use compare-and-set semantics
        // to avoid multiple contenders taking the lock simultaneously.
        let takeover = sqlx::query(
            "UPDATE session_state SET value = ?, updated_at = ? \
             WHERE session_key = ? AND namespace = ? AND key = ? \
               AND CAST(value AS INTEGER) < ?",
        )
        .bind(expires.to_string())
        .bind(now)
        .bind(&self.session_key)
        .bind(LOCK_NAMESPACE)
        .bind(LOCK_KEY)
        .bind(now)
        .execute(&self.pool)
        .await?;

        if takeover.rows_affected() > 0 {
            debug!(session_key = %self.session_key, "tinder session lock re-acquired (expired)");
            return Ok(true);
        }

        Ok(false)
    }

    /// Release the lock.
    pub async fn release(&self) -> Result<()> {
        sqlx::query(
            "DELETE FROM session_state WHERE session_key = ? AND namespace = ? AND key = ?",
        )
        .bind(&self.session_key)
        .bind(LOCK_NAMESPACE)
        .bind(LOCK_KEY)
        .execute(&self.pool)
        .await?;
        debug!(session_key = %self.session_key, "tinder session lock released");
        Ok(())
    }

    /// Refresh the lock TTL to prevent expiration during long operations.
    pub async fn refresh(&self) -> Result<()> {
        let now = now_ms();
        let expires = now + LOCK_TTL_MS;
        sqlx::query("UPDATE session_state SET value = ?, updated_at = ? WHERE session_key = ? AND namespace = ? AND key = ?")
        .bind(expires.to_string())
        .bind(now)
        .bind(&self.session_key)
        .bind(LOCK_NAMESPACE)
        .bind(LOCK_KEY)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_db() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE session_state (
                session_key TEXT NOT NULL,
                namespace   TEXT NOT NULL,
                key         TEXT NOT NULL,
                value       TEXT NOT NULL,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (session_key, namespace, key)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn acquires_once_then_blocks_until_release() {
        let pool = setup_db().await;
        let lock = SessionLock::new(pool, "s1".into());
        assert!(lock.try_acquire().await.unwrap());
        assert!(!lock.try_acquire().await.unwrap());
        lock.release().await.unwrap();
        assert!(lock.try_acquire().await.unwrap());
    }

    #[tokio::test]
    async fn acquires_expired_lock() {
        let pool = setup_db().await;
        sqlx::query(
            "INSERT INTO session_state (session_key, namespace, key, value, updated_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind("s1")
        .bind(LOCK_NAMESPACE)
        .bind(LOCK_KEY)
        .bind("1")
        .bind(1_i64)
        .execute(&pool)
        .await
        .unwrap();

        let lock = SessionLock::new(pool, "s1".into());
        assert!(lock.try_acquire_or_expire().await.unwrap());
    }
}
