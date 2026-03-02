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
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            let date = {
                let secs = now;
                let days = secs / 86400;
                // Simple date: YYYY-MM-DD from epoch days
                // Use the time crate which is already a dep
                format!(
                    "{:04}-{:02}-{:02}",
                    1970 + (days / 365),
                    ((days % 365) / 30) + 1,
                    ((days % 365) % 30) + 1
                )
            };

            let result = sqlx::query(
                "INSERT INTO agent_spend (date, model, cost, ts) VALUES (?, ?, ?, ?)",
            )
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
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            // Get all spend from the last 24 hours
            let cutoff = now - 86400;

            let total: Option<f64> = sqlx::query_scalar(
                "SELECT SUM(cost) FROM agent_spend WHERE ts >= ?",
            )
            .bind(cutoff)
            .fetch_optional(self.db.as_ref())
            .await
            .unwrap_or(None);

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
