//! SQLite-backed cron store using sqlx.

use {
    async_trait::async_trait,
    sqlx::{Row, SqlitePool, sqlite::SqlitePoolOptions},
};

use crate::{
    Error, Result,
    store::CronStore,
    types::{CronJob, CronRunRecord},
};

/// SQLite-backed persistence for cron jobs and run history.
pub struct SqliteStore {
    pool: SqlitePool,
}

/// Engagement summary for delivered heartbeat runs.
#[derive(Debug, Clone, PartialEq)]
pub struct EngagementStats {
    pub sample_size: u64,
    pub response_rate: f64,
    pub average_response_minutes: Option<f64>,
}

impl SqliteStore {
    /// Create a new store with its own connection pool and run migrations.
    ///
    /// Use this for standalone cron databases. For shared pools (e.g., moltis.db),
    /// use [`SqliteStore::with_pool`] after calling [`crate::run_migrations`].
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
            .map_err(|source| Error::external("failed to connect to SQLite", source))?;

        crate::run_migrations(&pool).await?;

        Ok(Self { pool })
    }

    /// Create a store using an existing pool (migrations must already be run).
    ///
    /// Call [`crate::run_migrations`] before using this constructor.
    pub fn with_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Calculate delivery engagement stats for the most recent delivered runs.
    pub async fn engagement_stats(&self, job_id: &str, last_n: usize) -> Result<Option<EngagementStats>> {
        Self::engagement_stats_with_pool(&self.pool, job_id, last_n).await
    }

    /// Calculate delivery engagement stats using an external connection pool.
    pub async fn engagement_stats_with_pool(
        pool: &SqlitePool,
        job_id: &str,
        last_n: usize,
    ) -> Result<Option<EngagementStats>> {
        let rows = sqlx::query(
            "SELECT delivered_at_ms, user_responded, user_response_at_ms
             FROM cron_runs
             WHERE job_id = ? AND delivered_at_ms IS NOT NULL
             ORDER BY delivered_at_ms DESC
             LIMIT ?",
        )
        .bind(job_id)
        .bind(last_n as i64)
        .fetch_all(pool)
        .await?;

        if rows.is_empty() {
            return Ok(None);
        }

        let sample_size = rows.len() as u64;
        let mut responded_count = 0u64;
        let mut latencies_minutes = Vec::new();

        for row in rows {
            let responded = row
                .try_get::<Option<i64>, _>("user_responded")
                .ok()
                .flatten()
                .unwrap_or(0)
                != 0;
            if !responded {
                continue;
            }
            responded_count += 1;

            let delivered_at_ms = row
                .try_get::<Option<i64>, _>("delivered_at_ms")
                .ok()
                .flatten();
            let responded_at_ms = row
                .try_get::<Option<i64>, _>("user_response_at_ms")
                .ok()
                .flatten();
            if let (Some(delivered), Some(responded_at)) = (delivered_at_ms, responded_at_ms)
                && responded_at >= delivered
            {
                latencies_minutes.push((responded_at - delivered) as f64 / 60_000.0);
            }
        }

        let response_rate = responded_count as f64 / sample_size as f64;
        let average_response_minutes = if latencies_minutes.is_empty() {
            None
        } else {
            Some(latencies_minutes.iter().sum::<f64>() / latencies_minutes.len() as f64)
        };

        Ok(Some(EngagementStats {
            sample_size,
            response_rate,
            average_response_minutes,
        }))
    }
}

#[async_trait]
impl CronStore for SqliteStore {
    async fn load_jobs(&self) -> Result<Vec<CronJob>> {
        let rows = sqlx::query("SELECT data FROM cron_jobs")
            .fetch_all(&self.pool)
            .await?;

        let mut jobs = Vec::with_capacity(rows.len());
        for row in rows {
            let data: String = row.get("data");
            let job: CronJob = serde_json::from_str(&data)?;
            jobs.push(job);
        }
        Ok(jobs)
    }

    async fn save_job(&self, job: &CronJob) -> Result<()> {
        let data = serde_json::to_string(job)?;
        sqlx::query(
            "INSERT INTO cron_jobs (id, data) VALUES (?, ?)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        )
        .bind(&job.id)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_job(&self, id: &str) -> Result<()> {
        let result = sqlx::query("DELETE FROM cron_jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(Error::job_not_found(id));
        }
        Ok(())
    }

    async fn update_job(&self, job: &CronJob) -> Result<()> {
        let data = serde_json::to_string(job)?;
        let result = sqlx::query("UPDATE cron_jobs SET data = ? WHERE id = ?")
            .bind(&data)
            .bind(&job.id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(Error::job_not_found(job.id.clone()));
        }
        Ok(())
    }

    async fn append_run(&self, job_id: &str, run: &CronRunRecord) -> Result<()> {
        let status = serde_json::to_string(&run.status)?;
        sqlx::query(
            "INSERT INTO cron_runs (
                job_id, started_at_ms, finished_at_ms, status, error, duration_ms, output,
                input_tokens, output_tokens, delivery_channel, delivery_to, delivered_at_ms,
                user_responded, user_response_at_ms
             )
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(job_id)
        .bind(run.started_at_ms as i64)
        .bind(run.finished_at_ms as i64)
        .bind(&status)
        .bind(&run.error)
        .bind(run.duration_ms as i64)
        .bind(&run.output)
        .bind(run.input_tokens.map(|v| v as i64))
        .bind(run.output_tokens.map(|v| v as i64))
        .bind(&run.delivery_channel)
        .bind(&run.delivery_to)
        .bind(run.delivered_at_ms.map(|v| v as i64))
        .bind(if run.user_responded { 1i64 } else { 0i64 })
        .bind(run.user_response_at_ms.map(|v| v as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_runs(&self, job_id: &str, limit: usize) -> Result<Vec<CronRunRecord>> {
        let rows = sqlx::query(
            "SELECT job_id, started_at_ms, finished_at_ms, status, error, duration_ms, output,
                    input_tokens, output_tokens, delivery_channel, delivery_to, delivered_at_ms,
                    user_responded, user_response_at_ms
             FROM cron_runs
             WHERE job_id = ?
             ORDER BY started_at_ms DESC
             LIMIT ?",
        )
        .bind(job_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut runs = Vec::with_capacity(rows.len());
        for row in rows {
            let status_str: String = row.get("status");
            let status = serde_json::from_str(&status_str)?;
            runs.push(CronRunRecord {
                job_id: row.get("job_id"),
                started_at_ms: row.get::<i64, _>("started_at_ms") as u64,
                finished_at_ms: row.get::<i64, _>("finished_at_ms") as u64,
                status,
                error: row.get("error"),
                duration_ms: row.get::<i64, _>("duration_ms") as u64,
                output: row.get("output"),
                input_tokens: row
                    .try_get::<Option<i64>, _>("input_tokens")
                    .ok()
                    .flatten()
                    .map(|v| v as u64),
                output_tokens: row
                    .try_get::<Option<i64>, _>("output_tokens")
                    .ok()
                    .flatten()
                    .map(|v| v as u64),
                delivery_channel: row.try_get::<Option<String>, _>("delivery_channel").ok().flatten(),
                delivery_to: row.try_get::<Option<String>, _>("delivery_to").ok().flatten(),
                delivered_at_ms: row
                    .try_get::<Option<i64>, _>("delivered_at_ms")
                    .ok()
                    .flatten()
                    .map(|v| v as u64),
                user_responded: row
                    .try_get::<Option<i64>, _>("user_responded")
                    .ok()
                    .flatten()
                    .unwrap_or(0)
                    != 0,
                user_response_at_ms: row
                    .try_get::<Option<i64>, _>("user_response_at_ms")
                    .ok()
                    .flatten()
                    .map(|v| v as u64),
            });
        }
        // Reverse so oldest first (consistent with other stores).
        runs.reverse();
        Ok(runs)
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {super::*, crate::types::*};

    async fn make_store() -> SqliteStore {
        SqliteStore::new("sqlite::memory:").await.unwrap()
    }

    fn make_job(id: &str) -> CronJob {
        CronJob {
            id: id.into(),
            name: format!("job-{id}"),
            enabled: true,
            delete_after_run: false,
            schedule: CronSchedule::At { at_ms: 1000 },
            payload: CronPayload::SystemEvent { text: "hi".into() },
            session_target: SessionTarget::Main,
            state: CronJobState::default(),
            sandbox: CronSandboxConfig::default(),
            wake_mode: CronWakeMode::default(),
            system: false,
            created_at_ms: 1000,
            updated_at_ms: 1000,
        }
    }

    #[tokio::test]
    async fn test_sqlite_roundtrip() {
        let store = make_store().await;
        store.save_job(&make_job("1")).await.unwrap();
        store.save_job(&make_job("2")).await.unwrap();

        let jobs = store.load_jobs().await.unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[tokio::test]
    async fn test_sqlite_upsert() {
        let store = make_store().await;
        store.save_job(&make_job("1")).await.unwrap();

        let mut job = make_job("1");
        job.name = "updated".into();
        store.save_job(&job).await.unwrap();

        let jobs = store.load_jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "updated");
    }

    #[tokio::test]
    async fn test_sqlite_delete() {
        let store = make_store().await;
        store.save_job(&make_job("1")).await.unwrap();
        store.delete_job("1").await.unwrap();
        assert!(store.load_jobs().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_sqlite_delete_not_found() {
        let store = make_store().await;
        assert!(store.delete_job("nope").await.is_err());
    }

    #[tokio::test]
    async fn test_sqlite_update() {
        let store = make_store().await;
        store.save_job(&make_job("1")).await.unwrap();

        let mut job = make_job("1");
        job.name = "patched".into();
        store.update_job(&job).await.unwrap();

        let jobs = store.load_jobs().await.unwrap();
        assert_eq!(jobs[0].name, "patched");
    }

    #[tokio::test]
    async fn test_sqlite_runs() {
        let store = make_store().await;
        store.save_job(&make_job("j1")).await.unwrap();

        for i in 0..5 {
            let run = CronRunRecord {
                job_id: "j1".into(),
                started_at_ms: i * 1000,
                finished_at_ms: i * 1000 + 500,
                status: RunStatus::Ok,
                error: None,
                duration_ms: 500,
                output: None,
                input_tokens: None,
                output_tokens: None,
                delivery_channel: None,
                delivery_to: None,
                delivered_at_ms: None,
                user_responded: false,
                user_response_at_ms: None,
            };
            store.append_run("j1", &run).await.unwrap();
        }

        let runs = store.get_runs("j1", 3).await.unwrap();
        assert_eq!(runs.len(), 3);
        // Should be the last 3, in chronological order
        assert_eq!(runs[0].started_at_ms, 2000);
        assert_eq!(runs[2].started_at_ms, 4000);
    }

    #[tokio::test]
    async fn test_sqlite_runs_empty() {
        let store = make_store().await;
        let runs = store.get_runs("none", 10).await.unwrap();
        assert!(runs.is_empty());
    }

    #[tokio::test]
    async fn test_engagement_stats() {
        let store = make_store().await;
        store.save_job(&make_job("j1")).await.unwrap();

        let runs = vec![
            CronRunRecord {
                job_id: "j1".into(),
                started_at_ms: 1_000,
                finished_at_ms: 1_200,
                status: RunStatus::Ok,
                error: None,
                duration_ms: 200,
                output: Some("first".into()),
                input_tokens: None,
                output_tokens: None,
                delivery_channel: Some("telegram:bot".into()),
                delivery_to: Some("chat-1".into()),
                delivered_at_ms: Some(1_200),
                user_responded: true,
                user_response_at_ms: Some(1_200 + 5 * 60_000),
            },
            CronRunRecord {
                job_id: "j1".into(),
                started_at_ms: 2_000,
                finished_at_ms: 2_200,
                status: RunStatus::Ok,
                error: None,
                duration_ms: 200,
                output: Some("second".into()),
                input_tokens: None,
                output_tokens: None,
                delivery_channel: Some("telegram:bot".into()),
                delivery_to: Some("chat-1".into()),
                delivered_at_ms: Some(2_200),
                user_responded: false,
                user_response_at_ms: None,
            },
            CronRunRecord {
                job_id: "j1".into(),
                started_at_ms: 3_000,
                finished_at_ms: 3_200,
                status: RunStatus::Ok,
                error: None,
                duration_ms: 200,
                output: Some("third".into()),
                input_tokens: None,
                output_tokens: None,
                delivery_channel: Some("telegram:bot".into()),
                delivery_to: Some("chat-1".into()),
                delivered_at_ms: Some(3_200),
                user_responded: true,
                user_response_at_ms: Some(3_200 + 15 * 60_000),
            },
        ];
        for run in &runs {
            store.append_run("j1", run).await.unwrap();
        }

        let stats = store.engagement_stats("j1", 10).await.unwrap().unwrap();
        assert_eq!(stats.sample_size, 3);
        assert!((stats.response_rate - (2.0 / 3.0)).abs() < 0.0001);
        assert_eq!(stats.average_response_minutes, Some(10.0));
    }

    #[tokio::test]
    async fn test_engagement_stats_none_when_no_deliveries() {
        let store = make_store().await;
        store.save_job(&make_job("j1")).await.unwrap();
        assert!(store.engagement_stats("j1", 10).await.unwrap().is_none());
    }
}
