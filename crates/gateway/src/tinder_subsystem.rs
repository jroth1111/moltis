//! Registration of the Moltis Tinder subsystem: tools and cron jobs.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;

use moltis_agents::tool_registry::{RateLimit, ToolRegistry};
use moltis_cron::service::CronService;
use moltis_tinder::{
    TinderBrowserTool, TinderFunnelTool,
    cron::{daily_session, ghost_recovery, hourly_replies, system_liveness},
};

/// Register Tinder cron jobs and tools at startup.
pub async fn register_tinder_subsystem(
    cron: &Arc<CronService>,
    tools: &mut ToolRegistry,
    pool: Arc<SqlitePool>,
    data_dir: &Path,
) -> Result<()> {
    // Cron jobs are idempotent because CronService::add performs an upsert by ID.
    cron.add(daily_session()).await?;
    cron.add(hourly_replies()).await?;
    cron.add(ghost_recovery()).await?;
    cron.add(system_liveness()).await?;

    tools.register_with_limit(
        Box::new(TinderBrowserTool::new(
            Arc::clone(&pool),
            data_dir.to_path_buf(),
        )),
        Some(RateLimit::new(30)),
    );
    tools.register_with_limit(
        Box::new(TinderFunnelTool::new(pool)),
        Some(RateLimit::new(120)),
    );

    tracing::info!("tinder subsystem registered");
    Ok(())
}
