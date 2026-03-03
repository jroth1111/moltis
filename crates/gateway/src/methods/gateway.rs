use crate::broadcast::{BroadcastOpts, broadcast_raw};

use super::MethodRegistry;

async fn collect_stuck_agents(
    store: Option<&std::sync::Arc<moltis_sessions::state_store::SessionStateStore>>,
) -> Vec<moltis_agents::self_repair::StuckSessionInfo> {
    let Some(store) = store else {
        return Vec::new();
    };

    let keys = store.list_running_sessions().await.unwrap_or_default();
    if keys.is_empty() {
        return Vec::new();
    }

    moltis_agents::self_repair::get_stuck_sessions(
        store,
        &keys,
        moltis_agents::self_repair::DEFAULT_STUCK_THRESHOLD,
    )
    .await
}

pub(super) fn register(reg: &mut MethodRegistry) {
    // health
    reg.register(
        "health",
        Box::new(|ctx| {
            Box::pin(async move {
                let count = ctx.state.client_count().await;

                // Surface stuck agents if the session state store is available.
                let stuck_agents =
                    collect_stuck_agents(ctx.state.services.session_state_store.as_ref()).await;

                Ok(serde_json::json!({
                    "status": "ok",
                    "version": ctx.state.version,
                    "connections": count,
                    "stuckAgents": stuck_agents,
                }))
            })
        }),
    );

    // status
    reg.register(
        "status",
        Box::new(|ctx| {
            Box::pin(async move {
                let inner = ctx.state.inner.read().await;
                let nodes = &inner.nodes;
                Ok(serde_json::json!({
                    "version": ctx.state.version,
                    "hostname": ctx.state.hostname,
                    "connections": inner.clients.len(),
                    "uptimeMs": ctx.state.uptime_ms(),
                    "nodes": nodes.count(),
                    "hasMobileNode": nodes.has_mobile_node(),
                }))
            })
        }),
    );

    // system-presence
    reg.register(
        "system-presence",
        Box::new(|ctx| {
            Box::pin(async move {
                let inner = ctx.state.inner.read().await;

                let client_list: Vec<_> = inner
                    .clients
                    .values()
                    .map(|c| {
                        serde_json::json!({
                            "connId": c.conn_id,
                            "clientId": c.connect_params.client.id,
                            "role": c.role(),
                            "platform": c.connect_params.client.platform,
                            "connectedAt": c.connected_at.elapsed().as_secs(),
                            "lastActivity": c.last_activity.elapsed().as_secs(),
                        })
                    })
                    .collect();

                let node_list: Vec<_> = inner
                    .nodes
                    .list()
                    .iter()
                    .map(|n| {
                        serde_json::json!({
                            "nodeId": n.node_id,
                            "displayName": n.display_name,
                            "platform": n.platform,
                            "version": n.version,
                            "capabilities": n.capabilities,
                            "commands": n.commands,
                            "connectedAt": n.connected_at.elapsed().as_secs(),
                        })
                    })
                    .collect();

                Ok(serde_json::json!({
                    "clients": client_list,
                    "nodes": node_list,
                }))
            })
        }),
    );

    // system-event: broadcast an event to all operator clients
    reg.register(
        "system-event",
        Box::new(|ctx| {
            Box::pin(async move {
                let event = ctx
                    .params
                    .get("event")
                    .and_then(|v| v.as_str())
                    .unwrap_or("system");
                let payload = ctx
                    .params
                    .get("payload")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                broadcast_raw(&ctx.state, event, payload, BroadcastOpts::default()).await;
                Ok(serde_json::json!({}))
            })
        }),
    );

    // last-heartbeat
    reg.register(
        "last-heartbeat",
        Box::new(|ctx| {
            Box::pin(async move {
                let inner = ctx.state.inner.read().await;
                if let Some(client) = inner.clients.get(&ctx.client_conn_id) {
                    Ok(serde_json::json!({
                        "lastActivitySecs": client.last_activity.elapsed().as_secs(),
                    }))
                } else {
                    Ok(serde_json::json!({ "lastActivitySecs": 0 }))
                }
            })
        }),
    );

    // set-heartbeats (touch activity for the caller)
    reg.register(
        "set-heartbeats",
        Box::new(|ctx| {
            Box::pin(async move {
                if let Some(client) = ctx
                    .state
                    .inner
                    .write()
                    .await
                    .clients
                    .get_mut(&ctx.client_conn_id)
                {
                    client.touch();
                }
                Ok(serde_json::json!({}))
            })
        }),
    );

    // system.describe: protocol schema discovery (v4)
    reg.register(
        "system.describe",
        Box::new(|_ctx| {
            Box::pin(async move {
                let methods: Vec<serde_json::Value> = reg_method_names()
                    .iter()
                    .map(|name| {
                        serde_json::json!({
                            "name": name,
                        })
                    })
                    .collect();

                let event_descriptors: Vec<serde_json::Value> = moltis_protocol::KNOWN_EVENTS
                    .iter()
                    .map(|name| serde_json::json!({ "name": name }))
                    .collect();

                Ok(serde_json::json!({
                    "protocol": moltis_protocol::PROTOCOL_VERSION,
                    "methods": methods,
                    "events": event_descriptors,
                }))
            })
        }),
    );
}

/// Core protocol method names for `system.describe`.
///
/// This is a static subset of methods registered in `gateway.rs`, `node.rs`,
/// `subscribe.rs`, and `channel_mux.rs`. The full method list (including all
/// service methods) is already available in `HelloOk.features.methods`.
///
/// TODO: store Arc<MethodRegistry> on GatewayState so this handler can query
/// the live registry instead of maintaining a static list.
fn reg_method_names() -> Vec<&'static str> {
    vec![
        "health",
        "status",
        "system-presence",
        "system-event",
        "last-heartbeat",
        "set-heartbeats",
        "system.describe",
        "node.list",
        "node.describe",
        "node.rename",
        "node.invoke",
        "node.invoke.result",
        "node.event",
        "location.result",
        "subscribe",
        "unsubscribe",
        "channel.join",
        "channel.leave",
    ]
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::collect_stuck_agents;

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    async fn make_state_store()
    -> anyhow::Result<Arc<moltis_sessions::state_store::SessionStateStore>> {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS session_state (
                session_key TEXT NOT NULL,
                namespace   TEXT NOT NULL,
                key         TEXT NOT NULL,
                value       TEXT NOT NULL,
                updated_at  INTEGER NOT NULL,
                PRIMARY KEY (session_key, namespace, key)
            )"#,
        )
        .execute(&pool)
        .await?;
        Ok(Arc::new(
            moltis_sessions::state_store::SessionStateStore::new(pool),
        ))
    }

    #[tokio::test]
    async fn collect_stuck_agents_returns_empty_without_store() {
        assert!(collect_stuck_agents(None).await.is_empty());
    }

    #[tokio::test]
    async fn collect_stuck_agents_reports_only_stale_running_sessions() -> anyhow::Result<()> {
        let store = make_state_store().await?;
        store
            .set("session:old", "self_repair", "running_since", "0")
            .await?;
        store
            .set("session:old", "self_repair", "repair_attempts", "2")
            .await?;

        let fresh_since = now_ms().to_string();
        store
            .set(
                "session:fresh",
                "self_repair",
                "running_since",
                &fresh_since,
            )
            .await?;

        let stuck = collect_stuck_agents(Some(&store)).await;
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].session_key, "session:old");
        assert_eq!(stuck[0].repair_attempts, 2);
        Ok(())
    }
}
