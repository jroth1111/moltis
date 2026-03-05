pub mod browser_tool;
pub mod cron;
pub mod error;
pub mod funnel;
pub mod funnel_tool;
pub mod hooks;
pub mod lock;
pub(crate) mod util;

pub use {
    browser_tool::TinderBrowserTool, funnel_tool::TinderFunnelTool, hooks::FunnelGuardHook,
    lock::SessionLock,
};

/// Run database migrations for the Tinder subsystem.
pub async fn run_migrations(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(pool)
        .await
        .map_err(|e| anyhow::anyhow!("moltis-tinder migrations failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::run_migrations;

    #[tokio::test]
    async fn run_migrations_ignores_preexisting_unknown_versions() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS _sqlx_migrations (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum BLOB NOT NULL,
                execution_time BIGINT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
             VALUES (?1, ?2, 1, ?3, 0)",
        )
        .bind(20240205100000_i64)
        .bind("legacy_removed_migration")
        .bind(vec![0_u8; 32])
        .execute(&pool)
        .await
        .unwrap();

        run_migrations(&pool).await.unwrap();

        let tables: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tinder_matches'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tables.0, 1);

        pool.close().await;
    }
}
