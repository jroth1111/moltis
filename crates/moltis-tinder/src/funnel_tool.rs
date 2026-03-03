use std::sync::Arc;

use {
    anyhow::Result,
    async_trait::async_trait,
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    tracing::info,
};

use crate::funnel::{self, FunnelState, TinderMatch};

pub struct TinderFunnelTool {
    pool: Arc<sqlx::SqlitePool>,
}

impl TinderFunnelTool {
    pub fn new(pool: Arc<sqlx::SqlitePool>) -> Self {
        Self { pool }
    }
}

use crate::util::now_ms;

#[async_trait]
impl AgentTool for TinderFunnelTool {
    fn name(&self) -> &str {
        "tinder_funnel"
    }

    fn description(&self) -> &str {
        "Manage Tinder match funnel: add matches, advance states, list, stats, and notes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["add_match", "advance", "list", "stats", "note"],
                    "description": "Action to perform"
                },
                "name": {
                    "type": "string",
                    "description": "Match name (for add_match)"
                },
                "match_id": {
                    "type": "string",
                    "description": "Match ID (for advance, note)"
                },
                "id": {
                    "type": "string",
                    "description": "Optional custom ID (for add_match)"
                },
                "to": {
                    "type": "string",
                    "description": "Target funnel state (for advance)"
                },
                "state": {
                    "type": "string",
                    "description": "Filter by funnel state (for list)"
                },
                "text": {
                    "type": "string",
                    "description": "Note text (for note)"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, params: Value) -> Result<Value> {
        let action = params["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing action"))?;

        match action {
            "add_match" => {
                let name = params["name"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("add_match requires 'name'"))?;
                let id = params["id"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let now = now_ms();
                let m = TinderMatch {
                    id: id.clone(),
                    name: name.to_string(),
                    funnel_state: FunnelState::Matched,
                    exchange_count: 0,
                    last_message_ts: None,
                    notes: String::new(),
                    created_at: now,
                    updated_at: now,
                };
                funnel::upsert_match(&self.pool, &m).await?;
                info!(match_id = %id, name = %name, "tinder match added");
                Ok(json!({ "status": "ok", "match_id": id }))
            },
            "advance" => {
                let match_id = params["match_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("advance requires 'match_id'"))?;
                let to_str = params["to"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("advance requires 'to'"))?;
                let target: FunnelState = to_str.parse()?;

                let current = funnel::get_match(&self.pool, match_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("match not found: {match_id}"))?;

                if !current.funnel_state.can_advance_to(&target) {
                    return Ok(json!({
                        "status": "error",
                        "error": format!(
                            "invalid transition: {} -> {}",
                            current.funnel_state, target
                        )
                    }));
                }

                // Gate: require at least 3 exchanges before securing number.
                if target == FunnelState::NumberSecured && current.exchange_count < 3 {
                    return Ok(json!({
                        "status": "error",
                        "error": format!(
                            "need at least 3 exchanges before number_secured (current: {})",
                            current.exchange_count
                        )
                    }));
                }

                funnel::update_funnel(&self.pool, match_id, target.clone()).await?;
                info!(match_id = %match_id, to = %target, "funnel advanced");
                Ok(json!({ "status": "ok", "match_id": match_id, "new_state": target.to_string() }))
            },
            "list" => {
                let state_filter = params["state"]
                    .as_str()
                    .map(|s| s.parse::<FunnelState>())
                    .transpose()?;
                let matches = funnel::list_matches(&self.pool, state_filter.as_ref()).await?;
                let items: Vec<Value> = matches.iter().map(|m| json!(m)).collect();
                Ok(json!({ "status": "ok", "matches": items, "count": items.len() }))
            },
            "stats" => {
                let all = funnel::list_matches(&self.pool, None).await?;
                let mut stats = serde_json::Map::new();
                for state in &[
                    FunnelState::Matched,
                    FunnelState::OpenerSent,
                    FunnelState::Engaged,
                    FunnelState::DateProposed,
                    FunnelState::NumberSecured,
                    FunnelState::Closed,
                ] {
                    let count = all.iter().filter(|m| &m.funnel_state == state).count();
                    stats.insert(state.to_string(), json!(count));
                }
                stats.insert("total".to_string(), json!(all.len()));
                Ok(json!({ "status": "ok", "stats": stats }))
            },
            "note" => {
                let match_id = params["match_id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("note requires 'match_id'"))?;
                let text = params["text"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("note requires 'text'"))?;

                let mut m = funnel::get_match(&self.pool, match_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("match not found: {match_id}"))?;

                if !m.notes.is_empty() {
                    m.notes.push('\n');
                }
                m.notes.push_str(text);
                m.updated_at = now_ms();
                funnel::upsert_match(&self.pool, &m).await?;
                Ok(json!({ "status": "ok", "match_id": match_id }))
            },
            other => Ok(json!({ "status": "error", "error": format!("unknown action: {other}") })),
        }
    }
}
