//! Gateway: central WebSocket/HTTP server, protocol dispatch, session/node registry.
//!
//! Lifecycle:
//! 1. Load + validate config
//! 2. Resolve auth, bind address
//! 3. Start HTTP server (health, control UI, hooks)
//! 4. Attach WebSocket upgrade handler
//! 5. Start channel accounts, cron, maintenance timers
//!
//! All domain logic (agents, channels, etc.) lives in other crates and is
//! invoked through method handlers registered in `methods.rs`.

#![cfg_attr(
    test,
    allow(
        clippy::await_holding_lock,
        clippy::expect_used,
        clippy::field_reassign_with_default,
        clippy::unwrap_used
    )
)]

pub mod agent_persona;
pub mod approval;
pub mod auth;
pub mod auth_middleware;
pub mod auth_routes;
pub mod auth_webauthn;
pub mod broadcast;
pub mod broadcast_types;
pub mod channel;
pub mod channel_agent_tools;
pub mod channel_events;
pub mod channel_outbound;
pub mod channel_store;
pub mod chat;
pub mod chat_error;
pub mod cron;
pub mod dispatch;
pub mod env_routes;
#[cfg(feature = "graphql")]
pub mod graphql_routes;
#[cfg(feature = "local-llm")]
pub mod local_llm_setup;
pub mod logs;
pub mod mcp_agent_tools;
pub mod mcp_health;
pub mod mcp_service;
#[cfg(feature = "mdns")]
pub mod mdns;
pub mod message_log_store;
pub mod methods;
#[cfg(feature = "metrics")]
pub mod metrics_middleware;
#[cfg(feature = "metrics")]
pub mod metrics_routes;
pub mod network_audit;
pub mod nodes;
pub mod onboarding;
pub mod pairing;
pub mod project;
pub mod provider_setup;
#[cfg(feature = "push-notifications")]
pub mod push;
#[cfg(feature = "push-notifications")]
pub mod push_routes;
pub mod request_throttle;
pub mod server;
pub mod services;
pub mod session;
pub mod session_types;
pub mod share_store;
pub mod state;
#[cfg(feature = "tailscale")]
pub mod tailscale;
#[cfg(feature = "tailscale")]
pub mod tailscale_routes;
#[cfg(feature = "tls")]
pub mod tls;
pub mod tools_routes;
pub mod tts_phrases;
pub mod update_check;
pub mod upload_routes;
pub mod voice;
pub mod voice_agent_tools;
pub mod ws;

/// Run database migrations for the gateway crate.
///
/// This creates the auth tables (auth_password, passkeys, api_keys, auth_sessions),
/// env_variables, message_log, and channels tables. Should be called at application
/// startup after the other crate migrations (projects, sessions, cron).
pub async fn run_migrations(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations")
        .set_ignore_missing(true)
        .run(pool)
        .await?;
    Ok(())
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn gateway_migrations_do_not_create_tinder_tables() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();

        super::run_migrations(&pool).await.unwrap();

        let tinder_table_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'tinder_matches'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(tinder_table_count, 0);
    }
}
