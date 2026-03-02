use anyhow::Result;
use tracing::debug;

/// Time-to-live for a session lock in milliseconds (5 minutes).
const LOCK_TTL_MS: i64 = 5 * 60 * 1000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

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
        let now = now_ms();
        let expires = now + LOCK_TTL_MS;

        // Try to insert a new lock row. If it already exists and has not
        // expired, the INSERT OR IGNORE will be a no-op.
        let result = sqlx::query(
            "INSERT OR IGNORE INTO session_state (session_key, namespace, key, value, updated_at) \
             VALUES (?, 'tinder_lock', 'owner', ?, ?)",
        )
        .bind(&self.session_key)
        .bind(expires.to_string())
        .bind(now)
        .execute(&self.pool)
        .await?;

        if result.rows_affected() > 0 {
            debug!(session_key = %self.session_key, "tinder session lock acquired");
            return Ok(true);
        }

        // Row exists — check if it has expired.
        let existing = sqlx::query_scalar::<_, String>(
            "SELECT value FROM session_state WHERE session_key = ? AND namespace = 'tinder_lock' AND key = 'owner'",
        )
        .bind(&self.session_key)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(val) = existing {
            let expires_at: i64 = val.parse().unwrap_or(0);
            if expires_at < now {
                // Lock expired, take it over.
                sqlx::query(
                    "UPDATE session_state SET value = ?, updated_at = ? \
                     WHERE session_key = ? AND namespace = 'tinder_lock' AND key = 'owner'",
                )
                .bind((now + LOCK_TTL_MS).to_string())
                .bind(now)
                .bind(&self.session_key)
                .execute(&self.pool)
                .await?;
                debug!(session_key = %self.session_key, "tinder session lock re-acquired (expired)");
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Release the lock.
    pub async fn release(&self) -> Result<()> {
        sqlx::query(
            "DELETE FROM session_state WHERE session_key = ? AND namespace = 'tinder_lock' AND key = 'owner'",
        )
        .bind(&self.session_key)
        .execute(&self.pool)
        .await?;
        debug!(session_key = %self.session_key, "tinder session lock released");
        Ok(())
    }

    /// Refresh the lock TTL to prevent expiration during long operations.
    pub async fn refresh(&self) -> Result<()> {
        let now = now_ms();
        let expires = now + LOCK_TTL_MS;
        sqlx::query(
            "UPDATE session_state SET value = ?, updated_at = ? \
             WHERE session_key = ? AND namespace = 'tinder_lock' AND key = 'owner'",
        )
        .bind(expires.to_string())
        .bind(now)
        .bind(&self.session_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
