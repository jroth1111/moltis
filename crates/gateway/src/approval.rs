//! Live approval service and broadcaster for the gateway.

use std::sync::Arc;

use {async_trait::async_trait, serde_json::Value, tracing::info};

use moltis_tools::{
    approval::{ApprovalDecision, ApprovalManager, cleanup_approval},
    exec::ApprovalBroadcaster,
};

use crate::{
    broadcast::{BroadcastEvent, BroadcastOpts, broadcast},
    services::{ExecApprovalService, ServiceResult},
    state::GatewayState,
};

/// Live approval service backed by an `ApprovalManager`.
pub struct LiveExecApprovalService {
    manager: Arc<ApprovalManager>,
    pool: Option<Arc<sqlx::SqlitePool>>,
}

impl LiveExecApprovalService {
    pub fn new(manager: Arc<ApprovalManager>, pool: Option<Arc<sqlx::SqlitePool>>) -> Self {
        Self { manager, pool }
    }
}

#[async_trait]
impl ExecApprovalService for LiveExecApprovalService {
    async fn get(&self) -> ServiceResult {
        Ok(serde_json::json!({
            "mode": self.manager.mode,
            "securityLevel": self.manager.security_level,
        }))
    }

    async fn set(&self, _params: Value) -> ServiceResult {
        // Config mutation not yet implemented.
        Ok(serde_json::json!({}))
    }

    async fn node_get(&self, _params: Value) -> ServiceResult {
        Ok(serde_json::json!({ "mode": self.manager.mode }))
    }

    async fn node_set(&self, _params: Value) -> ServiceResult {
        Ok(serde_json::json!({}))
    }

    async fn request(&self, _params: Value) -> ServiceResult {
        let ids = self.manager.pending_ids().await;
        Ok(serde_json::json!({ "pending": ids }))
    }

    async fn resolve(&self, params: Value) -> ServiceResult {
        let id = params
            .get("requestId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'requestId'".to_string())?;

        let decision_str = params
            .get("decision")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'decision'".to_string())?;

        let decision = match decision_str {
            "approved" => ApprovalDecision::Approved,
            "denied" => ApprovalDecision::Denied,
            _ => return Err(format!("invalid decision: {decision_str}").into()),
        };

        let command = params.get("command").and_then(|v| v.as_str());

        info!(id, ?decision, "resolving approval request");
        self.manager.resolve(id, decision, command).await;
        if let Some(pool) = self.pool.as_ref()
            && let Err(e) = cleanup_approval(pool.as_ref(), id).await
        {
            tracing::warn!(id, error = %e, "failed to cleanup persisted approval row");
        }

        Ok(serde_json::json!({ "ok": true }))
    }
}

/// Broadcasts approval requests to connected WebSocket clients.
pub struct GatewayApprovalBroadcaster {
    state: Arc<GatewayState>,
}

impl GatewayApprovalBroadcaster {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ApprovalBroadcaster for GatewayApprovalBroadcaster {
    async fn broadcast_request(&self, request_id: &str, command: &str) -> moltis_tools::Result<()> {
        broadcast(
            &self.state,
            BroadcastEvent::exec_approval_requested(request_id, command),
            BroadcastOpts::default(),
        )
        .await;
        Ok(())
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_live_service_resolve() {
        let mgr = Arc::new(ApprovalManager::default());
        let svc = LiveExecApprovalService::new(Arc::clone(&mgr), None);

        // Create a pending request.
        let (id, mut rx) = mgr.create_request("rm -rf /").await;

        // Resolve via the service.
        let result = svc
            .resolve(serde_json::json!({
                "requestId": id,
                "decision": "denied",
            }))
            .await;
        assert!(result.is_ok());

        // The receiver should get Denied.
        let decision = rx.try_recv().unwrap();
        assert_eq!(decision, ApprovalDecision::Denied);
    }

    #[tokio::test]
    async fn test_live_service_get() {
        let mgr = Arc::new(ApprovalManager::default());
        let svc = LiveExecApprovalService::new(mgr, None);
        let result = svc.get().await.unwrap();
        // Default mode is on-miss.
        assert_eq!(result["mode"], "on-miss");
    }
}
