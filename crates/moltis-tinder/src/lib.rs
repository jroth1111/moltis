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
        .run(pool)
        .await
        .map_err(|e| anyhow::anyhow!("moltis-tinder migrations failed: {e}"))
}
