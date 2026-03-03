//! Cost tracking and budget guard hooks.
//!
//! - `CostTrackerHook`: records per-call spend into the `agent_spend` table after each LLM call.
//! - `CostGuardHook`: blocks agent starts when the daily spend limit has been reached.

use std::sync::Arc;

use {async_trait::async_trait, tracing::warn};

use moltis_common::{
    Result,
    hooks::{HookAction, HookEvent, HookHandler, HookPayload},
};

// ── CostTrackerHook ─────────────────────────────────────────────────────────

/// Records LLM call costs to the `agent_spend` SQLite table.
pub struct CostTrackerHook {
    db: Arc<sqlx::SqlitePool>,
}

impl CostTrackerHook {
    pub fn new(db: Arc<sqlx::SqlitePool>) -> Self {
        Self { db }
    }

    fn compute_cost(model: &str, input_tokens: u32, output_tokens: u32) -> f64 {
        let (input_rate, output_rate) = if model.starts_with("claude") {
            (0.000015, 0.000075) // per token
        } else if model.starts_with("gpt-4") {
            (0.00003, 0.00006)
        } else {
            (0.000001, 0.000001)
        };
        input_tokens as f64 * input_rate + output_tokens as f64 * output_rate
    }
}

#[async_trait]
impl HookHandler for CostTrackerHook {
    fn name(&self) -> &str {
        "cost-tracker"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::AfterLLMCall]
    }

    fn priority(&self) -> i32 {
        10
    }

    async fn handle(&self, _event: HookEvent, payload: &HookPayload) -> Result<HookAction> {
        if let HookPayload::AfterLLMCall {
            model,
            input_tokens,
            output_tokens,
            ..
        } = payload
        {
            let cost = Self::compute_cost(model, *input_tokens, *output_tokens);
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let date = time::OffsetDateTime::now_utc().date().to_string();

            let result =
                sqlx::query("INSERT INTO agent_spend (date, model, cost, ts) VALUES (?, ?, ?, ?)")
                    .bind(&date)
                    .bind(model)
                    .bind(cost)
                    .bind(now)
                    .execute(self.db.as_ref())
                    .await;

            if let Err(e) = result {
                warn!(error = %e, "cost-tracker: failed to record spend");
            }
        }
        Ok(HookAction::Continue)
    }
}

// ── CostGuardHook ───────────────────────────────────────────────────────────

/// Blocks agent starts when the daily spend exceeds a configured USD limit.
pub struct CostGuardHook {
    db: Arc<sqlx::SqlitePool>,
    daily_limit_usd: f64,
}

impl CostGuardHook {
    pub fn new(db: Arc<sqlx::SqlitePool>, daily_limit_usd: f64) -> Self {
        Self {
            db,
            daily_limit_usd,
        }
    }
}

#[async_trait]
impl HookHandler for CostGuardHook {
    fn name(&self) -> &str {
        "cost-guard"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::BeforeAgentStart]
    }

    fn priority(&self) -> i32 {
        100
    }

    async fn handle(&self, _event: HookEvent, payload: &HookPayload) -> Result<HookAction> {
        if let HookPayload::BeforeAgentStart { .. } = payload {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            // Get all spend from the last 24 hours
            let cutoff = now - time::Duration::DAY.whole_seconds();

            let total: Option<f64> =
                match sqlx::query_scalar("SELECT SUM(cost) FROM agent_spend WHERE ts >= ?")
                    .bind(cutoff)
                    .fetch_optional(self.db.as_ref())
                    .await
                {
                    Ok(total) => total,
                    Err(e) => {
                        warn!(error = %e, "cost-guard: failed to query spend; failing closed");
                        return Ok(HookAction::Block(
                            "cost guard unavailable: unable to verify spend".to_string(),
                        ));
                    },
                };

            let total = total.unwrap_or(0.0);
            if total >= self.daily_limit_usd {
                warn!(
                    total_usd = total,
                    limit_usd = self.daily_limit_usd,
                    "cost-guard: daily spend limit reached"
                );
                return Ok(HookAction::Block(format!(
                    "Daily spend limit reached (${total:.2} / ${:.2})",
                    self.daily_limit_usd
                )));
            }
        }
        Ok(HookAction::Continue)
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_pool() -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS agent_spend (
                date TEXT NOT NULL,
                model TEXT NOT NULL,
                cost REAL NOT NULL,
                ts INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn blocks_when_daily_limit_reached() {
        let pool = setup_pool().await;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();

        sqlx::query("INSERT INTO agent_spend (date, model, cost, ts) VALUES (?, ?, ?, ?)")
            .bind("2026-01-01")
            .bind("m")
            .bind(10.0_f64)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();

        let hook = CostGuardHook::new(Arc::new(pool), 5.0);
        let payload = HookPayload::BeforeAgentStart {
            session_key: "s1".into(),
            model: "m".into(),
            trace_id: None,
        };
        let result = hook
            .handle(HookEvent::BeforeAgentStart, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(_)));
    }

    #[tokio::test]
    async fn fails_closed_when_spend_query_errors() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let hook = CostGuardHook::new(Arc::new(pool), 100.0);
        let payload = HookPayload::BeforeAgentStart {
            session_key: "s1".into(),
            model: "m".into(),
            trace_id: None,
        };
        let result = hook
            .handle(HookEvent::BeforeAgentStart, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Block(msg) if msg.contains("cost guard unavailable")));
    }

    #[test]
    fn test_compute_cost_claude() {
        let cost = CostTrackerHook::compute_cost("claude-3-opus", 1000, 500);
        // 1000 * 0.000015 + 500 * 0.000075 = 0.015 + 0.0375 = 0.0525
        assert!((cost - 0.0525).abs() < 0.0001);
    }

    #[test]
    fn test_compute_cost_gpt4() {
        let cost = CostTrackerHook::compute_cost("gpt-4-turbo", 1000, 500);
        // 1000 * 0.00003 + 500 * 0.00006 = 0.03 + 0.03 = 0.06
        assert!((cost - 0.06).abs() < 0.0001);
    }

    #[test]
    fn test_compute_cost_fallback() {
        let cost = CostTrackerHook::compute_cost("unknown-model", 1000, 500);
        // 1000 * 0.000001 + 500 * 0.000001 = 0.001 + 0.0005 = 0.0015
        assert!((cost - 0.0015).abs() < 0.0001);
    }

    #[test]
    fn test_compute_cost_zero_tokens() {
        let cost = CostTrackerHook::compute_cost("claude-3-opus", 0, 0);
        assert!((cost - 0.0).abs() < 0.0001);
    }
}
