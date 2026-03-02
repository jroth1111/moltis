use std::sync::Arc;

use async_trait::async_trait;
use moltis_common::hooks::{HookAction, HookEvent, HookHandler, HookPayload};
use once_cell::sync::Lazy;
use regex::Regex;
use tracing::{info, warn};

use crate::funnel::{self, FunnelState};

/// Regex that matches phone numbers, social handles, and contact-sharing patterns.
#[allow(clippy::expect_used)]
static CONTACT_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}|@\w+|whatsapp|snapchat|instagram",
    )
    .expect("CONTACT_PATTERN regex must compile")
});

pub struct FunnelGuardHook {
    pool: Arc<sqlx::SqlitePool>,
}

impl FunnelGuardHook {
    pub fn new(pool: Arc<sqlx::SqlitePool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl HookHandler for FunnelGuardHook {
    fn name(&self) -> &str {
        "funnel_guard"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::BeforeToolCall]
    }

    fn priority(&self) -> i32 {
        90
    }

    async fn handle(
        &self,
        _event: HookEvent,
        payload: &HookPayload,
    ) -> moltis_common::error::Result<HookAction> {
        let (tool_name, arguments) = match payload {
            HookPayload::BeforeToolCall {
                tool_name,
                arguments,
                ..
            } => (tool_name.as_str(), arguments),
            _ => return Ok(HookAction::Continue),
        };

        // Guard tinder_funnel advance calls.
        if tool_name == "tinder_funnel" {
            if let Some("advance") = arguments.get("action").and_then(|v| v.as_str()) {
                return self.guard_advance(arguments).await;
            }
        }

        // Guard tinder_browser type calls for contact pattern leakage.
        if tool_name == "tinder_browser" {
            if let Some("type") = arguments.get("command").and_then(|v| v.as_str()) {
                return self.guard_browser_type(arguments).await;
            }
        }

        Ok(HookAction::Continue)
    }
}

impl FunnelGuardHook {
    async fn guard_advance(
        &self,
        arguments: &serde_json::Value,
    ) -> moltis_common::error::Result<HookAction> {
        let match_id = match arguments.get("match_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Ok(HookAction::Continue),
        };
        let target_str = match arguments.get("to").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return Ok(HookAction::Continue),
        };

        let target: FunnelState = match target_str.parse() {
            Ok(s) => s,
            Err(_) => return Ok(HookAction::Continue),
        };

        // FAIL-CLOSED: on DB error, block the action.
        let current = match funnel::get_match(&self.pool, match_id).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                return Ok(HookAction::Block(format!(
                    "match not found: {match_id}"
                )));
            }
            Err(e) => {
                warn!(error = %e, "funnel guard DB error — blocking action");
                return Ok(HookAction::Block(
                    "guard unavailable, action blocked".to_string(),
                ));
            }
        };

        // Block transition to number_secured with < 3 exchanges.
        if target == FunnelState::NumberSecured && current.exchange_count < 3 {
            info!(
                match_id = %match_id,
                exchange_count = current.exchange_count,
                "blocked advance to number_secured: insufficient exchanges"
            );
            return Ok(HookAction::Block(format!(
                "need at least 3 exchanges before number_secured (current: {})",
                current.exchange_count
            )));
        }

        // Block transition to date_proposed if not in engaged state.
        if target == FunnelState::DateProposed && current.funnel_state != FunnelState::Engaged {
            info!(
                match_id = %match_id,
                current_state = %current.funnel_state,
                "blocked advance to date_proposed: must be engaged"
            );
            return Ok(HookAction::Block(format!(
                "must be in engaged state to propose date (current: {})",
                current.funnel_state
            )));
        }

        Ok(HookAction::Continue)
    }

    async fn guard_browser_type(
        &self,
        arguments: &serde_json::Value,
    ) -> moltis_common::error::Result<HookAction> {
        let text = match arguments.get("text").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => return Ok(HookAction::Continue),
        };

        // Check if the typed text contains contact patterns.
        if !CONTACT_PATTERN.is_match(text) {
            return Ok(HookAction::Continue);
        }

        // If a match_id is provided, check if the match is at least engaged.
        if let Some(match_id) = arguments.get("match_id").and_then(|v| v.as_str()) {
            let current = match funnel::get_match(&self.pool, match_id).await {
                Ok(Some(m)) => m,
                Ok(None) => {
                    return Ok(HookAction::Block(
                        "contact pattern detected but match not found".to_string(),
                    ));
                }
                Err(e) => {
                    warn!(error = %e, "funnel guard DB error — blocking action");
                    return Ok(HookAction::Block(
                        "guard unavailable, action blocked".to_string(),
                    ));
                }
            };

            // Allow contact sharing only if engaged or beyond.
            match current.funnel_state {
                FunnelState::Engaged
                | FunnelState::DateProposed
                | FunnelState::NumberSecured => {
                    return Ok(HookAction::Continue);
                }
                _ => {
                    info!(
                        match_id = %match_id,
                        state = %current.funnel_state,
                        "blocked contact sharing: match not yet engaged"
                    );
                    return Ok(HookAction::Block(format!(
                        "contact sharing blocked: match {} is in {} state (need engaged+)",
                        match_id, current.funnel_state
                    )));
                }
            }
        }

        // No match_id provided but contact pattern found — block as precaution.
        Ok(HookAction::Block(
            "contact pattern detected in browser type without match_id".to_string(),
        ))
    }
}
