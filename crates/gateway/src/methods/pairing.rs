use moltis_protocol::{ErrorShape, error_codes};
use serde::{Deserialize, de::DeserializeOwned};

use crate::broadcast::{BroadcastOpts, broadcast};

use super::MethodRegistry;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairRequestParams {
    device_id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default = "default_platform")]
    platform: String,
    #[serde(default)]
    public_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PairIdParams {
    id: String,
}

#[derive(Debug, Deserialize)]
struct PairVerifyParams {
    id: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceIdParams {
    device_id: String,
}

fn default_platform() -> String {
    "unknown".to_string()
}

fn parse_params<T: DeserializeOwned>(params: serde_json::Value) -> Result<T, ErrorShape> {
    serde_json::from_value(params)
        .map_err(|e| ErrorShape::new(error_codes::INVALID_REQUEST, format!("invalid params: {e}")))
}

fn pairing_error(err: crate::pairing::Error) -> ErrorShape {
    ErrorShape::new(error_codes::INVALID_REQUEST, err.to_string())
}

pub(super) fn register(reg: &mut MethodRegistry) {
    // node.pair.request
    reg.register(
        "node.pair.request",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: PairRequestParams = parse_params(ctx.params.clone())?;
                let req = ctx.state.inner.write().await.pairing.request_pair(
                    &params.device_id,
                    params.display_name.as_deref(),
                    &params.platform,
                    params.public_key.as_deref(),
                );

                // Broadcast pair request to operators with pairing scope.
                broadcast(
                    &ctx.state,
                    "node.pair.requested",
                    serde_json::json!({
                        "id": req.id,
                        "deviceId": req.device_id,
                        "displayName": req.display_name,
                        "platform": req.platform,
                    }),
                    BroadcastOpts::default(),
                )
                .await;

                Ok(serde_json::json!({
                    "id": req.id,
                    "nonce": req.nonce,
                }))
            })
        }),
    );

    // node.pair.list
    reg.register(
        "node.pair.list",
        Box::new(|ctx| {
            Box::pin(async move {
                let inner = ctx.state.inner.read().await;
                let list: Vec<_> = inner
                    .pairing
                    .list_pending()
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "deviceId": r.device_id,
                            "displayName": r.display_name,
                            "platform": r.platform,
                        })
                    })
                    .collect();
                Ok(serde_json::json!(list))
            })
        }),
    );

    // node.pair.approve
    reg.register(
        "node.pair.approve",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: PairIdParams = parse_params(ctx.params.clone())?;
                let pair_id = params.id;
                let token = ctx
                    .state
                    .inner
                    .write()
                    .await
                    .pairing
                    .approve(&pair_id)
                    .map_err(pairing_error)?;

                broadcast(
                    &ctx.state,
                    "node.pair.resolved",
                    serde_json::json!({
                        "id": pair_id, "status": "approved",
                    }),
                    BroadcastOpts::default(),
                )
                .await;

                Ok(serde_json::json!({
                    "deviceToken": token.token,
                    "scopes": token.scopes,
                }))
            })
        }),
    );

    // node.pair.reject
    reg.register(
        "node.pair.reject",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: PairIdParams = parse_params(ctx.params.clone())?;
                let pair_id = params.id;
                ctx.state
                    .inner
                    .write()
                    .await
                    .pairing
                    .reject(&pair_id)
                    .map_err(pairing_error)?;

                broadcast(
                    &ctx.state,
                    "node.pair.resolved",
                    serde_json::json!({
                        "id": pair_id, "status": "rejected",
                    }),
                    BroadcastOpts::default(),
                )
                .await;

                Ok(serde_json::json!({}))
            })
        }),
    );

    // node.pair.verify
    reg.register(
        "node.pair.verify",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: PairVerifyParams = parse_params(ctx.params.clone())?;
                ctx.state
                    .inner
                    .write()
                    .await
                    .pairing
                    .verify(&params.id, &params.signature)
                    .map_err(pairing_error)?;
                Ok(serde_json::json!({ "verified": true }))
            })
        }),
    );

    // device.pair.list
    reg.register(
        "device.pair.list",
        Box::new(|ctx| {
            Box::pin(async move {
                let inner = ctx.state.inner.read().await;
                let list: Vec<_> = inner
                    .pairing
                    .list_devices()
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "deviceId": d.device_id,
                            "scopes": d.scopes,
                            "issuedAtMs": d.issued_at_ms,
                        })
                    })
                    .collect();
                Ok(serde_json::json!(list))
            })
        }),
    );

    // device.pair.approve (alias for node.pair.approve)
    reg.register(
        "device.pair.approve",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: PairIdParams = parse_params(ctx.params.clone())?;
                let pair_id = params.id;
                let token = ctx
                    .state
                    .inner
                    .write()
                    .await
                    .pairing
                    .approve(&pair_id)
                    .map_err(pairing_error)?;

                broadcast(
                    &ctx.state,
                    "device.pair.resolved",
                    serde_json::json!({
                        "id": pair_id, "status": "approved",
                    }),
                    BroadcastOpts::default(),
                )
                .await;

                Ok(serde_json::json!({ "deviceToken": token.token, "scopes": token.scopes }))
            })
        }),
    );

    // device.pair.reject
    reg.register(
        "device.pair.reject",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: PairIdParams = parse_params(ctx.params.clone())?;
                let pair_id = params.id;
                ctx.state
                    .inner
                    .write()
                    .await
                    .pairing
                    .reject(&pair_id)
                    .map_err(pairing_error)?;

                broadcast(
                    &ctx.state,
                    "device.pair.resolved",
                    serde_json::json!({
                        "id": pair_id, "status": "rejected",
                    }),
                    BroadcastOpts::default(),
                )
                .await;

                Ok(serde_json::json!({}))
            })
        }),
    );

    // device.token.rotate
    reg.register(
        "device.token.rotate",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: DeviceIdParams = parse_params(ctx.params.clone())?;
                let token = ctx
                    .state
                    .inner
                    .write()
                    .await
                    .pairing
                    .rotate_token(&params.device_id)
                    .map_err(pairing_error)?;
                Ok(serde_json::json!({ "deviceToken": token.token, "scopes": token.scopes }))
            })
        }),
    );

    // device.token.revoke
    reg.register(
        "device.token.revoke",
        Box::new(|ctx| {
            Box::pin(async move {
                let params: DeviceIdParams = parse_params(ctx.params.clone())?;
                ctx.state
                    .inner
                    .write()
                    .await
                    .pairing
                    .revoke_token(&params.device_id)
                    .map_err(pairing_error)?;
                Ok(serde_json::json!({}))
            })
        }),
    );
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        auth::{AuthMode, ResolvedAuth},
        methods::MethodContext,
        services::GatewayServices,
        state::GatewayState,
    };

    fn test_state() -> Arc<GatewayState> {
        GatewayState::new(
            ResolvedAuth {
                mode: AuthMode::Token,
                token: None,
                password: None,
            },
            GatewayServices::noop(),
        )
    }

    fn test_context(
        state: Arc<GatewayState>,
        method: &str,
        params: serde_json::Value,
        request_id: &str,
    ) -> MethodContext {
        MethodContext {
            request_id: request_id.to_string(),
            method: method.to_string(),
            params,
            client_conn_id: "test-conn".to_string(),
            client_role: "operator".to_string(),
            client_scopes: vec!["operator.pairing".to_string()],
            state,
            channel: None,
            trace_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    #[test]
    fn approve_aliases_require_verified_pair_request() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        runtime.block_on(async {
            let registry = super::super::MethodRegistry::new();
            let state = test_state();

            let pair_request = registry
                .dispatch(test_context(
                    state.clone(),
                    "node.pair.request",
                    serde_json::json!({
                        "deviceId": "ios-device-1",
                        "displayName": "iPhone",
                        "platform": "ios",
                    }),
                    "request-1",
                ))
                .await;
            assert!(pair_request.ok, "pair request should succeed");

            let pair_id = pair_request
                .payload
                .as_ref()
                .and_then(|payload| payload.get("id"))
                .and_then(|value| value.as_str())
                .expect("pair id")
                .to_string();

            for method in ["node.pair.approve", "device.pair.approve"] {
                let response = registry
                    .dispatch(test_context(
                        state.clone(),
                        method,
                        serde_json::json!({ "id": pair_id }),
                        method,
                    ))
                    .await;
                assert!(!response.ok, "{method} must fail for unverified request");
                let err = response.error.expect("error response");
                assert_eq!(err.code, error_codes::INVALID_REQUEST);
                assert!(
                    err.message.contains("not verified"),
                    "unexpected error message: {}",
                    err.message
                );
            }
        });
    }
}
