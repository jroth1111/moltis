//! Registration of the moltis-tinder subsystem: tools, hooks, and cron jobs.
//!
//! Called once at server startup from `prepare_gateway`. All types referenced
//! here are provided by the `moltis-tinder` crate (merged via
//! feature/moltis-tinder). Until that branch lands, the imports below are
//! commented out with `// TODO: uncomment after merge of feature/moltis-tinder`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;

// TODO: uncomment after merge of feature/moltis-tinder
// use moltis_tinder::{TinderFunnelTool, TinderBrowserTool, FunnelGuardHook};
// use moltis_tinder::cron::{daily_session, hourly_replies, ghost_recovery, system_liveness};

use moltis_agents::tool_registry::ToolRegistry;
use moltis_common::hooks::HookRegistry;
use moltis_cron::service::CronService;

/// Register the Tinder subsystem (cron jobs, tools, hooks) at startup.
///
/// Must be called after the `ToolRegistry`, `HookRegistry`, and `CronService`
/// are fully constructed, and after all database migrations have run.
///
/// # Errors
///
/// Returns an error if any cron job fails to register.
pub async fn register_tinder_subsystem(
    cron: &Arc<CronService>,
    tools: &mut ToolRegistry,
    hooks: &mut HookRegistry,
    pool: Arc<SqlitePool>,
    data_dir: &Path,
) -> Result<()> {
    // ── Cron jobs ──────────────────────────────────────────────────────────
    // TODO: uncomment after merge of feature/moltis-tinder
    // cron.add(daily_session()).await?;
    // cron.add(hourly_replies()).await?;
    // cron.add(ghost_recovery()).await?;
    // cron.add(system_liveness()).await?;

    // Suppress unused-variable warnings until the TODO lines are uncommented.
    let _ = cron;

    // ── Tools ──────────────────────────────────────────────────────────────
    // TODO: uncomment after merge of feature/moltis-tinder
    // tools.register(Box::new(TinderBrowserTool::new(data_dir, Arc::clone(&pool))));
    // tools.register(Box::new(TinderFunnelTool::new(Arc::clone(&pool))));

    let _ = tools;
    let _ = &pool;
    let _ = data_dir;

    // ── Hooks ──────────────────────────────────────────────────────────────
    // TODO: uncomment after merge of feature/moltis-tinder
    // hooks.register(Arc::new(FunnelGuardHook::new(Arc::clone(&pool))));

    let _ = hooks;

    tracing::info!("tinder subsystem registered (tools/hooks/cron pending feature/moltis-tinder merge)");

    Ok(())
}
