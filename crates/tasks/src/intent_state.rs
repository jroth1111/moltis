//! Mutable dispatch metadata for intent tasks.
//!
//! [`IntentStore`] tracks per-intent state that changes between shifts:
//! the active shift, token ledger, structural snapshot for spin detection,
//! and a CAS version counter.
//!
//! All writes use optimistic concurrency (`UPDATE ... WHERE version = :expected`).
//! Cross-store transactions (task state + intent state in one `COMMIT`) are
//! supported via [`IntentStore::finalize_shift_tx`].

use {
    serde::{Deserialize, Serialize},
    sqlx::{Row, Sqlite, SqlitePool, Transaction},
    time::OffsetDateTime,
};

use crate::{errors::TransitionError, types::TaskId};

// ── ObjectiveSnapshot ─────────────────────────────────────────────────────────

/// Structural snapshot of intent progress — used for spin detection.
///
/// Compared between consecutive shifts: if the snapshot is identical, the
/// shift made no measurable progress (spin). Accumulated spin triggers
/// escalation to [`RuntimeState::AwaitingHuman`].
///
/// This is intentionally structural (counts and flags), not LLM narrative, so
/// comparison is deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectiveSnapshot {
    /// Number of milestones logged by the agent.
    #[serde(default)]
    pub milestone_count: u32,
    /// Number of artifacts produced (files written, API calls succeeded, etc.).
    #[serde(default)]
    pub artifact_count: u32,
    /// Child tasks in Pending state.
    #[serde(default)]
    pub child_pending: u32,
    /// Child tasks in Active state.
    #[serde(default)]
    pub child_active: u32,
    /// Child tasks in Completed state.
    #[serde(default)]
    pub child_completed: u32,
    /// Child tasks in terminal Failed/Cancelled state.
    #[serde(default)]
    pub child_failed: u32,
}

impl ObjectiveSnapshot {
    /// Whether this snapshot is structurally identical to `other`.
    /// No progress = spin candidate.
    #[must_use]
    pub fn is_same_as(&self, other: &Self) -> bool {
        self == other
    }
}

// ── IntentState ───────────────────────────────────────────────────────────────

/// Mutable dispatch state for a single intent task.
#[derive(Debug, Clone)]
pub struct IntentState {
    /// Intent task ID.
    pub intent_id: TaskId,
    /// Task list this intent belongs to.
    pub list_id: String,
    /// ID of the currently active shift task, if any.
    pub active_shift_id: Option<TaskId>,
    /// Total number of shifts dispatched.
    pub shift_count: u32,
    /// Cumulative tokens consumed across all shifts.
    pub tokens_used: u64,
    /// Maximum token budget (None = unlimited).
    pub tokens_budget: Option<u64>,
    /// Latest structural progress snapshot.
    pub snapshot: ObjectiveSnapshot,
    /// Consecutive shifts with no snapshot delta.
    pub spin_count: u32,
    /// Shifts with no delta before escalating (default 3).
    pub spin_threshold: u32,
    /// CAS version counter.
    pub version: u64,
    /// When this record was created.
    pub created_at: OffsetDateTime,
    /// When this record was last updated.
    pub updated_at: OffsetDateTime,
}

impl IntentState {
    /// Whether this intent has exceeded its token budget.
    #[must_use]
    pub fn is_over_budget(&self) -> bool {
        self.tokens_budget
            .is_some_and(|budget| self.tokens_used >= budget)
    }

    /// Whether consecutive spin count has hit the escalation threshold.
    #[must_use]
    pub fn is_spinning(&self) -> bool {
        self.spin_count >= self.spin_threshold
    }
}

// ── IntentStore ───────────────────────────────────────────────────────────────

/// SQLite store for [`IntentState`] records.
///
/// Shares the underlying pool with [`crate::store::TaskStore`] so both can
/// participate in the same SQLite transaction for atomic finalization.
#[derive(Clone)]
pub struct IntentStore {
    pool: SqlitePool,
}

impl IntentStore {
    /// Wrap an existing pool (should be the same pool as [`TaskStore`]).
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Create a new intent state record for the given intent task.
    ///
    /// Called once when an intent task is created. Idempotent: if a record
    /// already exists for `intent_id`, returns it without writing.
    pub async fn create(
        &self,
        intent_id: &str,
        list_id: &str,
        tokens_budget: Option<u64>,
        spin_threshold: Option<u32>,
    ) -> Result<IntentState, TransitionError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let snapshot_json = serde_json::to_string(&ObjectiveSnapshot::default())
            .map_err(|e| TransitionError::Other(e.to_string()))?;
        let threshold = spin_threshold.unwrap_or(3) as i64;
        let budget = tokens_budget.map(|b| b as i64);

        sqlx::query(
            "INSERT OR IGNORE INTO intent_state \
             (intent_id, list_id, active_shift_id, shift_count, tokens_used, tokens_budget, \
              snapshot_json, spin_count, spin_threshold, version, created_at, updated_at) \
             VALUES (?, ?, NULL, 0, 0, ?, ?, 0, ?, 0, ?, ?)",
        )
        .bind(intent_id)
        .bind(list_id)
        .bind(budget)
        .bind(&snapshot_json)
        .bind(threshold)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        self.get(intent_id)
            .await?
            .ok_or_else(|| TransitionError::NotFound(intent_id.to_string()))
    }

    /// Read the intent state for the given intent task ID.
    pub async fn get(&self, intent_id: &str) -> Result<Option<IntentState>, TransitionError> {
        let row = sqlx::query(
            "SELECT intent_id, list_id, active_shift_id, shift_count, tokens_used, \
             tokens_budget, snapshot_json, spin_count, spin_threshold, version, \
             created_at, updated_at \
             FROM intent_state WHERE intent_id = ?",
        )
        .bind(intent_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(TransitionError::Storage)?;

        row.map(Self::row_to_state).transpose()
    }

    /// Set the active shift and increment shift_count with CAS.
    ///
    /// Called just before dispatching a new shift — atomically records which
    /// shift is running and bumps the counter.
    pub async fn set_active_shift(
        &self,
        intent_id: &str,
        shift_id: &str,
        expected_version: u64,
    ) -> Result<IntentState, TransitionError> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let ev = expected_version as i64;
        let nv = ev + 1;

        let rows = sqlx::query(
            "UPDATE intent_state \
             SET active_shift_id = ?, shift_count = shift_count + 1, \
                 version = ?, updated_at = ? \
             WHERE intent_id = ? AND version = ?",
        )
        .bind(shift_id)
        .bind(nv)
        .bind(now)
        .bind(intent_id)
        .bind(ev)
        .execute(&self.pool)
        .await
        .map_err(TransitionError::Storage)?
        .rows_affected();

        if rows == 0 {
            return Err(TransitionError::VersionConflict {
                expected: expected_version,
                actual: expected_version + 1,
            });
        }

        self.get(intent_id)
            .await?
            .ok_or_else(|| TransitionError::NotFound(intent_id.to_string()))
    }

    /// Finalize a completed shift within an existing transaction.
    ///
    /// - Clears `active_shift_id`.
    /// - Adds `tokens_delta` to `tokens_used`.
    /// - Compares `new_snapshot` to the stored one; if identical, increments
    ///   `spin_count`, otherwise resets it to 0.
    /// - Bumps `version`.
    ///
    /// Returns `(updated_state, is_spinning)` where `is_spinning` is true when
    /// the new `spin_count` meets or exceeds `spin_threshold`.
    pub async fn finalize_shift_tx(
        tx: &mut Transaction<'_, Sqlite>,
        intent_id: &str,
        new_snapshot: ObjectiveSnapshot,
        tokens_delta: u64,
        expected_version: u64,
    ) -> Result<(IntentState, bool), TransitionError> {
        // Read current state within the transaction.
        let current = Self::get_tx(tx, intent_id)
            .await?
            .ok_or_else(|| TransitionError::NotFound(intent_id.to_string()))?;

        if current.version != expected_version {
            return Err(TransitionError::VersionConflict {
                expected: expected_version,
                actual: current.version,
            });
        }

        // Determine spin delta.
        let new_spin_count = if new_snapshot.is_same_as(&current.snapshot) {
            current.spin_count.saturating_add(1)
        } else {
            0
        };
        let is_spinning = new_spin_count >= current.spin_threshold;

        let new_snapshot_json = serde_json::to_string(&new_snapshot)
            .map_err(|e| TransitionError::Other(e.to_string()))?;
        let new_tokens = current.tokens_used.saturating_add(tokens_delta) as i64;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let new_version = (current.version + 1) as i64;
        let ev = expected_version as i64;

        let rows = sqlx::query(
            "UPDATE intent_state \
             SET active_shift_id = NULL, tokens_used = ?, snapshot_json = ?, \
                 spin_count = ?, version = ?, updated_at = ? \
             WHERE intent_id = ? AND version = ?",
        )
        .bind(new_tokens)
        .bind(&new_snapshot_json)
        .bind(new_spin_count as i64)
        .bind(new_version)
        .bind(now)
        .bind(intent_id)
        .bind(ev)
        .execute(&mut **tx)
        .await
        .map_err(TransitionError::Storage)?
        .rows_affected();

        if rows == 0 {
            return Err(TransitionError::VersionConflict {
                expected: expected_version,
                actual: expected_version + 1,
            });
        }

        // Construct updated state in-memory (avoid a second SQL round-trip).
        let updated = IntentState {
            intent_id: current.intent_id,
            list_id: current.list_id,
            active_shift_id: None,
            shift_count: current.shift_count,
            tokens_used: current.tokens_used.saturating_add(tokens_delta),
            tokens_budget: current.tokens_budget,
            snapshot: new_snapshot,
            spin_count: new_spin_count,
            spin_threshold: current.spin_threshold,
            version: current.version + 1,
            created_at: current.created_at,
            updated_at: OffsetDateTime::now_utc(),
        };

        Ok((updated, is_spinning))
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Read intent state using an existing transaction.
    async fn get_tx(
        tx: &mut Transaction<'_, Sqlite>,
        intent_id: &str,
    ) -> Result<Option<IntentState>, TransitionError> {
        let row = sqlx::query(
            "SELECT intent_id, list_id, active_shift_id, shift_count, tokens_used, \
             tokens_budget, snapshot_json, spin_count, spin_threshold, version, \
             created_at, updated_at \
             FROM intent_state WHERE intent_id = ?",
        )
        .bind(intent_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(TransitionError::Storage)?;

        row.map(Self::row_to_state).transpose()
    }

    fn row_to_state(row: sqlx::sqlite::SqliteRow) -> Result<IntentState, TransitionError> {
        let snapshot_json: String = row.get("snapshot_json");
        let snapshot: ObjectiveSnapshot = serde_json::from_str(&snapshot_json)
            .map_err(|e| TransitionError::Other(format!("snapshot deserialize: {e}")))?;

        let created_at_unix: i64 = row.get("created_at");
        let updated_at_unix: i64 = row.get("updated_at");
        let created_at = OffsetDateTime::from_unix_timestamp(created_at_unix)
            .map_err(|e| TransitionError::Other(format!("created_at: {e}")))?;
        let updated_at = OffsetDateTime::from_unix_timestamp(updated_at_unix)
            .map_err(|e| TransitionError::Other(format!("updated_at: {e}")))?;

        let active_shift_id: Option<String> = row.get("active_shift_id");
        let tokens_budget: Option<i64> = row.get("tokens_budget");

        Ok(IntentState {
            intent_id: TaskId(row.get::<String, _>("intent_id")),
            list_id: row.get("list_id"),
            active_shift_id: active_shift_id.map(TaskId),
            shift_count: row.get::<i64, _>("shift_count") as u32,
            tokens_used: row.get::<i64, _>("tokens_used") as u64,
            tokens_budget: tokens_budget.map(|b| b as u64),
            snapshot,
            spin_count: row.get::<i64, _>("spin_count") as u32,
            spin_threshold: row.get::<i64, _>("spin_threshold") as u32,
            version: row.get::<i64, _>("version") as u64,
            created_at,
            updated_at,
        })
    }
}

#[allow(clippy::expect_used, clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskStore;

    async fn test_stores() -> (TaskStore, IntentStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("tasks.db");
        let store = TaskStore::open(&db_path).await.expect("open store");
        let intent = IntentStore::from_pool(store.pool().clone());
        (store, intent, dir)
    }

    // ── ObjectiveSnapshot ─────────────────────────────────────────────────────

    #[test]
    fn snapshot_default_is_zero() {
        let s = ObjectiveSnapshot::default();
        assert_eq!(s.milestone_count, 0);
        assert_eq!(s.artifact_count, 0);
    }

    #[test]
    fn snapshot_is_same_as_identical() {
        let a = ObjectiveSnapshot {
            milestone_count: 2,
            artifact_count: 1,
            ..Default::default()
        };
        let b = a.clone();
        assert!(a.is_same_as(&b));
    }

    #[test]
    fn snapshot_is_same_as_different() {
        let a = ObjectiveSnapshot {
            milestone_count: 1,
            ..Default::default()
        };
        let b = ObjectiveSnapshot {
            milestone_count: 2,
            ..Default::default()
        };
        assert!(!a.is_same_as(&b));
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let s = ObjectiveSnapshot {
            milestone_count: 3,
            artifact_count: 2,
            child_completed: 5,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ObjectiveSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // ── IntentState helpers ───────────────────────────────────────────────────

    #[test]
    fn is_over_budget_no_budget() {
        let state = IntentState {
            intent_id: TaskId::new(),
            list_id: "l".into(),
            active_shift_id: None,
            shift_count: 0,
            tokens_used: 100_000,
            tokens_budget: None,
            snapshot: ObjectiveSnapshot::default(),
            spin_count: 0,
            spin_threshold: 3,
            version: 0,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(!state.is_over_budget());
    }

    #[test]
    fn is_over_budget_within_budget() {
        let state = IntentState {
            tokens_used: 50_000,
            tokens_budget: Some(100_000),
            intent_id: TaskId::new(),
            list_id: "l".into(),
            active_shift_id: None,
            shift_count: 0,
            snapshot: ObjectiveSnapshot::default(),
            spin_count: 0,
            spin_threshold: 3,
            version: 0,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(!state.is_over_budget());
    }

    #[test]
    fn is_over_budget_at_limit() {
        let state = IntentState {
            tokens_used: 100_000,
            tokens_budget: Some(100_000),
            intent_id: TaskId::new(),
            list_id: "l".into(),
            active_shift_id: None,
            shift_count: 0,
            snapshot: ObjectiveSnapshot::default(),
            spin_count: 0,
            spin_threshold: 3,
            version: 0,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(state.is_over_budget());
    }

    #[test]
    fn is_spinning_below_threshold() {
        let state = IntentState {
            spin_count: 2,
            spin_threshold: 3,
            intent_id: TaskId::new(),
            list_id: "l".into(),
            active_shift_id: None,
            shift_count: 0,
            tokens_used: 0,
            tokens_budget: None,
            snapshot: ObjectiveSnapshot::default(),
            version: 0,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(!state.is_spinning());
    }

    #[test]
    fn is_spinning_at_threshold() {
        let state = IntentState {
            spin_count: 3,
            spin_threshold: 3,
            intent_id: TaskId::new(),
            list_id: "l".into(),
            active_shift_id: None,
            shift_count: 0,
            tokens_used: 0,
            tokens_budget: None,
            snapshot: ObjectiveSnapshot::default(),
            version: 0,
            created_at: OffsetDateTime::now_utc(),
            updated_at: OffsetDateTime::now_utc(),
        };
        assert!(state.is_spinning());
    }

    // ── IntentStore CRUD ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_and_get() {
        let (_task_store, intent_store, _dir) = test_stores().await;
        let id = TaskId::new();

        let state = intent_store
            .create(&id.0, "list1", None, None)
            .await
            .expect("create");

        assert_eq!(state.intent_id.0, id.0);
        assert_eq!(state.list_id, "list1");
        assert!(state.active_shift_id.is_none());
        assert_eq!(state.shift_count, 0);
        assert_eq!(state.tokens_used, 0);
        assert_eq!(state.spin_count, 0);
        assert_eq!(state.spin_threshold, 3); // default
        assert_eq!(state.version, 0);

        let fetched = intent_store
            .get(&id.0)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(fetched.intent_id, state.intent_id);
    }

    #[tokio::test]
    async fn create_is_idempotent() {
        let (_ts, is, _dir) = test_stores().await;
        let id = TaskId::new();

        intent_store_create(&is, &id).await;
        // Second create should not error and should return the existing row.
        let second = is.create(&id.0, "list1", None, None).await.expect("second create");
        assert_eq!(second.version, 0); // Not bumped.
    }

    #[tokio::test]
    async fn create_with_budget_and_threshold() {
        let (_ts, is, _dir) = test_stores().await;
        let id = TaskId::new();

        let state = is
            .create(&id.0, "list1", Some(50_000), Some(5))
            .await
            .expect("create");

        assert_eq!(state.tokens_budget, Some(50_000));
        assert_eq!(state.spin_threshold, 5);
    }

    #[tokio::test]
    async fn get_returns_none_for_unknown() {
        let (_ts, is, _dir) = test_stores().await;
        let result = is.get("nonexistent").await.expect("get");
        assert!(result.is_none());
    }

    // ── set_active_shift ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_active_shift_increments_count() {
        let (_ts, is, _dir) = test_stores().await;
        let id = TaskId::new();
        let shift_id = TaskId::new();

        intent_store_create(&is, &id).await;
        let updated = is
            .set_active_shift(&id.0, &shift_id.0, 0)
            .await
            .expect("set_active_shift");

        assert_eq!(updated.active_shift_id, Some(shift_id.clone()));
        assert_eq!(updated.shift_count, 1);
        assert_eq!(updated.version, 1);
    }

    #[tokio::test]
    async fn set_active_shift_cas_conflict() {
        let (_ts, is, _dir) = test_stores().await;
        let id = TaskId::new();
        let shift_id = TaskId::new();

        intent_store_create(&is, &id).await;

        let result = is.set_active_shift(&id.0, &shift_id.0, 99).await;
        assert!(
            matches!(result, Err(TransitionError::VersionConflict { .. })),
            "expected VersionConflict"
        );
    }

    // ── finalize_shift_tx ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn finalize_shift_clears_active_and_accumulates_tokens() {
        let (task_store, is, _dir) = test_stores().await;
        let id = TaskId::new();
        let shift_id = TaskId::new();

        intent_store_create(&is, &id).await;
        is.set_active_shift(&id.0, &shift_id.0, 0)
            .await
            .expect("set active");

        let new_snapshot = ObjectiveSnapshot {
            milestone_count: 1,
            ..Default::default()
        };

        let mut tx = task_store.begin_tx().await.expect("begin tx");
        let (updated, is_spinning) = IntentStore::finalize_shift_tx(
            &mut tx,
            &id.0,
            new_snapshot.clone(),
            1500,
            1, // version after set_active_shift
        )
        .await
        .expect("finalize");
        tx.commit().await.expect("commit");

        assert!(!is_spinning);
        assert!(updated.active_shift_id.is_none());
        assert_eq!(updated.tokens_used, 1500);
        assert_eq!(updated.spin_count, 0); // snapshot changed from default
        assert_eq!(updated.version, 2);
        assert_eq!(updated.snapshot, new_snapshot);
    }

    #[tokio::test]
    async fn finalize_shift_increments_spin_count_on_no_progress() {
        let (task_store, is, _dir) = test_stores().await;
        let id = TaskId::new();

        intent_store_create(&is, &id).await;
        is.set_active_shift(&id.0, "shift-1", 0)
            .await
            .expect("set active");

        // Finalize with same (default) snapshot — spin.
        let mut tx = task_store.begin_tx().await.expect("begin");
        let (s1, spinning) = IntentStore::finalize_shift_tx(
            &mut tx,
            &id.0,
            ObjectiveSnapshot::default(),
            0,
            1,
        )
        .await
        .expect("finalize 1");
        tx.commit().await.expect("commit 1");

        assert!(!spinning);
        assert_eq!(s1.spin_count, 1);

        // Second shift: same snapshot again.
        is.set_active_shift(&id.0, "shift-2", 2)
            .await
            .expect("set active 2");

        let mut tx = task_store.begin_tx().await.expect("begin 2");
        let (s2, spinning) = IntentStore::finalize_shift_tx(
            &mut tx,
            &id.0,
            ObjectiveSnapshot::default(),
            0,
            3,
        )
        .await
        .expect("finalize 2");
        tx.commit().await.expect("commit 2");

        assert!(!spinning);
        assert_eq!(s2.spin_count, 2);

        // Third shift: hit threshold (3).
        is.set_active_shift(&id.0, "shift-3", 4)
            .await
            .expect("set active 3");

        let mut tx = task_store.begin_tx().await.expect("begin 3");
        let (s3, spinning) = IntentStore::finalize_shift_tx(
            &mut tx,
            &id.0,
            ObjectiveSnapshot::default(),
            0,
            5,
        )
        .await
        .expect("finalize 3");
        tx.commit().await.expect("commit 3");

        assert!(spinning, "spin_count == spin_threshold should be detected");
        assert_eq!(s3.spin_count, 3);
    }

    #[tokio::test]
    async fn finalize_shift_resets_spin_on_progress() {
        let (task_store, is, _dir) = test_stores().await;
        let id = TaskId::new();

        // version=0 → create
        intent_store_create(&is, &id).await;

        // Shift 1: spin (no progress).
        is.set_active_shift(&id.0, "s1", 0).await.expect("set s1"); // version=1
        let mut tx = task_store.begin_tx().await.expect("tx1");
        let (s1, _) = IntentStore::finalize_shift_tx(
            &mut tx, &id.0, ObjectiveSnapshot::default(), 0, 1,
        )
        .await
        .expect("finalize s1"); // version=2
        tx.commit().await.expect("commit1");
        assert_eq!(s1.spin_count, 1);

        // Shift 2: spin (no progress).
        is.set_active_shift(&id.0, "s2", 2).await.expect("set s2"); // version=3
        let mut tx = task_store.begin_tx().await.expect("tx2");
        let (s2, _) = IntentStore::finalize_shift_tx(
            &mut tx, &id.0, ObjectiveSnapshot::default(), 0, 3,
        )
        .await
        .expect("finalize s2"); // version=4
        tx.commit().await.expect("commit2");
        assert_eq!(s2.spin_count, 2);

        // Shift 3: progress — snapshot has a milestone.
        is.set_active_shift(&id.0, "s3", 4).await.expect("set s3"); // version=5
        let progress = ObjectiveSnapshot { milestone_count: 1, ..Default::default() };
        let mut tx = task_store.begin_tx().await.expect("tx3");
        let (s3, spinning) = IntentStore::finalize_shift_tx(
            &mut tx, &id.0, progress, 100, 5,
        )
        .await
        .expect("finalize s3"); // version=6
        tx.commit().await.expect("commit3");

        assert!(!spinning, "progress should not trigger spin escalation");
        assert_eq!(s3.spin_count, 0, "progress resets spin_count");
        assert_eq!(s3.tokens_used, 100);
    }

    #[tokio::test]
    async fn finalize_shift_tx_cas_conflict() {
        let (task_store, is, _dir) = test_stores().await;
        let id = TaskId::new();

        intent_store_create(&is, &id).await;

        let mut tx = task_store.begin_tx().await.expect("tx");
        let result = IntentStore::finalize_shift_tx(
            &mut tx,
            &id.0,
            ObjectiveSnapshot::default(),
            0,
            99, // stale
        )
        .await;

        assert!(matches!(result, Err(TransitionError::VersionConflict { .. })));
    }

    // ── Helper ────────────────────────────────────────────────────────────────

    async fn intent_store_create(is: &IntentStore, id: &TaskId) -> IntentState {
        is.create(&id.0, "list1", None, None)
            .await
            .expect("create intent state")
    }
}
