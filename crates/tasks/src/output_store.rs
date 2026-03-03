//! Shift output persistence for context injection and TTL cleanup.
//!
//! [`OutputStore`] writes one row per completed shift, capped at 64 KiB, and
//! exposes a `list_recent` query so the dispatch loop can inject the last N
//! outputs as context for the next shift.  Rows are deleted by the TTL sweep
//! after `output_retention_secs`.
//!
//! Shares the underlying [`SqlitePool`] with [`TaskStore`] so shift
//! finalization can write both task state and shift output in one `COMMIT`.

use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use time::OffsetDateTime;

use crate::errors::TransitionError;

/// Maximum byte length of a persisted shift output.
pub const OUTPUT_MAX_BYTES: usize = 65_536;

// ── Data types ────────────────────────────────────────────────────────────────

/// One shift's recorded output.
#[derive(Debug, Clone)]
pub struct ShiftOutput {
    /// The intent task this shift belongs to.
    pub intent_id: String,
    /// The shift task ID.
    pub shift_id: String,
    /// Ordinal position (1-based) of this shift within the intent.
    pub shift_num: u32,
    /// Output text (capped at [`OUTPUT_MAX_BYTES`]).
    pub output: String,
    /// Input tokens consumed by this shift.
    pub input_tokens: u64,
    /// Output tokens produced by this shift.
    pub output_tokens: u64,
    /// Unix timestamp when the output was recorded.
    pub created_at: i64,
}

// ── OutputStore ───────────────────────────────────────────────────────────────

/// SQLite store for per-shift execution outputs.
#[derive(Clone)]
pub struct OutputStore {
    pool: SqlitePool,
}

impl OutputStore {
    /// Wrap an existing pool (should be the same pool as [`crate::store::TaskStore`]).
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a shift output within an existing transaction.
    ///
    /// Silently truncates `output` to [`OUTPUT_MAX_BYTES`] on write.
    pub async fn insert_tx(
        tx: &mut Transaction<'_, Sqlite>,
        intent_id: &str,
        shift_id: &str,
        list_id: &str,
        shift_num: u32,
        output: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Result<(), TransitionError> {
        let capped: &str = if output.len() > OUTPUT_MAX_BYTES {
            // Truncate at a char boundary within the limit.
            let boundary = output
                .char_indices()
                .take_while(|(i, _)| *i < OUTPUT_MAX_BYTES)
                .last()
                .map_or(0, |(i, c)| i + c.len_utf8());
            &output[..boundary]
        } else {
            output
        };

        let now = OffsetDateTime::now_utc().unix_timestamp();

        sqlx::query(
            "INSERT INTO task_outputs \
             (intent_id, shift_id, list_id, shift_num, output, input_tokens, output_tokens, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(intent_id)
        .bind(shift_id)
        .bind(list_id)
        .bind(shift_num as i64)
        .bind(capped)
        .bind(input_tokens as i64)
        .bind(output_tokens as i64)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(TransitionError::Storage)?;

        Ok(())
    }

    /// Return the `limit` most-recent shift outputs for `intent_id`.
    ///
    /// Ordered descending by shift_num so the caller can take the first N for
    /// context injection (most recent last).
    pub async fn list_recent(
        &self,
        intent_id: &str,
        limit: u32,
    ) -> Result<Vec<ShiftOutput>, TransitionError> {
        let rows = sqlx::query(
            "SELECT intent_id, shift_id, shift_num, output, input_tokens, output_tokens, created_at \
             FROM task_outputs \
             WHERE intent_id = ? \
             ORDER BY shift_num DESC \
             LIMIT ?",
        )
        .bind(intent_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        let mut outputs: Vec<ShiftOutput> = rows
            .into_iter()
            .map(|r| ShiftOutput {
                intent_id: r.get("intent_id"),
                shift_id: r.get("shift_id"),
                shift_num: r.get::<i64, _>("shift_num") as u32,
                output: r.get("output"),
                input_tokens: r.get::<i64, _>("input_tokens") as u64,
                output_tokens: r.get::<i64, _>("output_tokens") as u64,
                created_at: r.get("created_at"),
            })
            .collect();

        // Re-order ascending (oldest first) for chronological injection.
        outputs.reverse();
        Ok(outputs)
    }

    /// Delete all outputs older than `cutoff_unix_ts`. Returns the number of
    /// rows removed.
    pub async fn delete_older_than(&self, cutoff_unix_ts: i64) -> Result<u64, TransitionError> {
        let result = sqlx::query("DELETE FROM task_outputs WHERE created_at < ?")
            .bind(cutoff_unix_ts)
            .execute(&self.pool)
            .await
            .map_err(TransitionError::Storage)?;

        Ok(result.rows_affected())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskStore;

    async fn make_store() -> (TaskStore, OutputStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("tasks.db");
        let task_store = TaskStore::open(&db_path).await.expect("open");
        let output_store = OutputStore::from_pool(task_store.pool().clone());
        (task_store, output_store, dir)
    }

    #[tokio::test]
    async fn insert_and_list_recent() {
        let (task_store, output_store, _dir) = make_store().await;

        let mut tx = task_store.begin_tx().await.expect("begin");
        OutputStore::insert_tx(&mut tx, "i1", "s1", "default", 1, "output one", 10, 20)
            .await
            .expect("insert 1");
        OutputStore::insert_tx(&mut tx, "i1", "s2", "default", 2, "output two", 30, 40)
            .await
            .expect("insert 2");
        tx.commit().await.expect("commit");

        let recent = output_store.list_recent("i1", 10).await.expect("list");
        assert_eq!(recent.len(), 2);
        // Ascending order after reversal.
        assert_eq!(recent[0].shift_num, 1);
        assert_eq!(recent[1].shift_num, 2);
        assert_eq!(recent[0].output, "output one");
    }

    #[tokio::test]
    async fn list_recent_honours_limit() {
        let (task_store, output_store, _dir) = make_store().await;
        let mut tx = task_store.begin_tx().await.expect("begin");
        for n in 1u32..=5 {
            OutputStore::insert_tx(&mut tx, "i2", &format!("s{n}"), "default", n, "x", 0, 0)
                .await
                .expect("insert");
        }
        tx.commit().await.expect("commit");

        let recent = output_store.list_recent("i2", 3).await.expect("list");
        assert_eq!(recent.len(), 3);
        // limit=3 DESC → shifts 5,4,3; reversed → 3,4,5
        assert_eq!(recent[0].shift_num, 3);
        assert_eq!(recent[2].shift_num, 5);
    }

    #[tokio::test]
    async fn output_capped_at_max_bytes() {
        let (task_store, output_store, _dir) = make_store().await;
        let big = "a".repeat(OUTPUT_MAX_BYTES + 1000);

        let mut tx = task_store.begin_tx().await.expect("begin");
        OutputStore::insert_tx(&mut tx, "i3", "s1", "default", 1, &big, 0, 0)
            .await
            .expect("insert");
        tx.commit().await.expect("commit");

        let recent = output_store.list_recent("i3", 1).await.expect("list");
        assert_eq!(recent.len(), 1);
        assert!(recent[0].output.len() <= OUTPUT_MAX_BYTES);
    }

    #[tokio::test]
    async fn delete_older_than_removes_stale() {
        let (task_store, output_store, _dir) = make_store().await;
        // Insert rows via direct SQL with old timestamps.
        let old_ts: i64 = 1_000_000;
        let new_ts = OffsetDateTime::now_utc().unix_timestamp();

        sqlx::query(
            "INSERT INTO task_outputs \
             (intent_id, shift_id, list_id, shift_num, output, input_tokens, output_tokens, created_at) \
             VALUES ('i4', 's_old', 'default', 1, 'old', 0, 0, ?)",
        )
        .bind(old_ts)
        .execute(task_store.pool())
        .await
        .expect("insert old");

        sqlx::query(
            "INSERT INTO task_outputs \
             (intent_id, shift_id, list_id, shift_num, output, input_tokens, output_tokens, created_at) \
             VALUES ('i4', 's_new', 'default', 2, 'new', 0, 0, ?)",
        )
        .bind(new_ts)
        .execute(task_store.pool())
        .await
        .expect("insert new");

        let cutoff = new_ts - 1;
        let deleted = output_store
            .delete_older_than(cutoff)
            .await
            .expect("delete");
        assert_eq!(deleted, 1);

        let remaining = output_store.list_recent("i4", 10).await.expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].shift_id, "s_new");
    }
}
