//! Live MCP service implementation backed by `McpManager`.

use std::{collections::HashSet, sync::Arc};

use {
    anyhow::Result,
    async_trait::async_trait,
    serde_json::Value,
    std::time::{Duration, SystemTime, UNIX_EPOCH},
    tokio::sync::RwLock,
    tracing::info,
};

use {
    moltis_agents::tool_registry::{AgentTool, ToolRegistry},
    moltis_mcp::tool_bridge::{McpAgentTool, McpToolBridge},
};

use crate::services::{McpService, ServiceError, ServiceResult};

#[derive(Debug, Clone)]
pub(crate) struct LegacyDirectState {
    enabled_until: Option<SystemTime>,
    allow_servers: HashSet<String>,
}

impl LegacyDirectState {
    fn disabled() -> Self {
        Self {
            enabled_until: None,
            allow_servers: HashSet::new(),
        }
    }

    fn is_enabled(&self, now: SystemTime) -> bool {
        self.enabled_until.map(|until| now < until).unwrap_or(false)
    }

    fn status_json(&self, now: SystemTime) -> Value {
        serde_json::json!({
            "enabled": self.is_enabled(now),
            "enabledUntil": self.enabled_until.and_then(|dt| dt.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)),
            "allowServers": self.allow_servers.iter().cloned().collect::<Vec<_>>(),
        })
    }
}

// ── McpToolAdapter: bridge McpAgentTool → AgentTool ─────────────────────────

/// Thin adapter that implements `AgentTool` (agents crate) by delegating to
/// `McpToolBridge` which implements `McpAgentTool` (mcp crate).
struct McpToolAdapter(McpToolBridge);

#[async_trait]
impl AgentTool for McpToolAdapter {
    fn name(&self) -> &str {
        McpAgentTool::name(&self.0)
    }

    fn description(&self) -> &str {
        McpAgentTool::description(&self.0)
    }

    fn parameters_schema(&self) -> Value {
        McpAgentTool::parameters_schema(&self.0)
    }

    async fn execute(&self, params: Value) -> Result<Value> {
        McpAgentTool::execute(&self.0, params)
            .await
            .map_err(anyhow::Error::from)
    }
}

// ── Sync helper ─────────────────────────────────────────────────────────────

/// Synchronize MCP tool bridges into the shared `ToolRegistry`.
///
/// Removes all existing `mcp__*` tools and re-registers current bridges.
pub(crate) async fn sync_mcp_tools(
    manager: &moltis_mcp::McpManager,
    registry: &Arc<RwLock<ToolRegistry>>,
    legacy_direct: &LegacyDirectState,
) {
    let mut reg = registry.write().await;

    // Remove all MCP-sourced tools before re-registering current ones.
    reg.unregister_mcp();

    if !legacy_direct.is_enabled(SystemTime::now()) {
        return;
    }

    let bridges = manager.tool_bridges().await;

    // Register current bridges with their server name metadata.
    let mut count = 0usize;
    for bridge in bridges {
        let server = bridge.server_name().to_string();
        if !legacy_direct.allow_servers.is_empty() && !legacy_direct.allow_servers.contains(&server)
        {
            continue;
        }
        reg.register_mcp(Box::new(McpToolAdapter(bridge)), server);
        count += 1;
    }

    if count > 0 {
        info!(tools = count, "legacy direct MCP tools synced into tool registry");
    }
}

// ── Config parsing helper ───────────────────────────────────────────────────

/// Extract an `McpServerConfig` from JSON params.
///
/// For updates, omitted fields inherit from `existing`.
fn parse_server_config(
    params: &Value,
    existing: Option<&moltis_mcp::McpServerConfig>,
) -> Result<moltis_mcp::McpServerConfig, ServiceError> {
    let transport = match params.get("transport").and_then(|v| v.as_str()) {
        Some("sse") => moltis_mcp::TransportType::Sse,
        Some(_) => moltis_mcp::TransportType::Stdio,
        None => existing
            .map(|cfg| cfg.transport)
            .unwrap_or(moltis_mcp::TransportType::Stdio),
    };

    let command = params
        .get("command")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| existing.map(|cfg| cfg.command.clone()))
        .unwrap_or_default();

    if matches!(transport, moltis_mcp::TransportType::Stdio) && command.trim().is_empty() {
        return Err(ServiceError::message("missing 'command' parameter"));
    }

    let args: Vec<String> = if params.get("args").is_some() {
        params
            .get("args")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    } else {
        existing.map(|cfg| cfg.args.clone()).unwrap_or_default()
    };

    let env: std::collections::HashMap<String, String> = if params.get("env").is_some() {
        params
            .get("env")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    } else {
        existing.map(|cfg| cfg.env.clone()).unwrap_or_default()
    };

    let enabled = params
        .get("enabled")
        .and_then(|v| v.as_bool())
        .or_else(|| existing.map(|cfg| cfg.enabled))
        .unwrap_or(true);

    let url = if params.get("url").is_some() {
        if params.get("url").is_some_and(Value::is_null) {
            None
        } else {
            params.get("url").and_then(|v| v.as_str()).map(String::from)
        }
    } else {
        existing.and_then(|cfg| cfg.url.clone())
    };

    if matches!(transport, moltis_mcp::TransportType::Sse)
        && url
            .as_deref()
            .is_none_or(|candidate| candidate.trim().is_empty())
    {
        return Err(ServiceError::message(
            "missing 'url' parameter for 'sse' transport",
        ));
    }

    let oauth = if let Some(v) = params.get("oauth") {
        if v.is_null() {
            None
        } else {
            let client_id = v
                .get("client_id")
                .and_then(|val| val.as_str())
                .ok_or_else(|| ServiceError::message("missing 'oauth.client_id' parameter"))?
                .to_string();
            let auth_url = v
                .get("auth_url")
                .and_then(|val| val.as_str())
                .ok_or_else(|| ServiceError::message("missing 'oauth.auth_url' parameter"))?
                .to_string();
            let token_url = v
                .get("token_url")
                .and_then(|val| val.as_str())
                .ok_or_else(|| ServiceError::message("missing 'oauth.token_url' parameter"))?
                .to_string();
            let scopes: Vec<String> = v
                .get("scopes")
                .and_then(|s| serde_json::from_value(s.clone()).ok())
                .unwrap_or_default();
            Some(moltis_mcp::registry::McpOAuthConfig {
                client_id,
                auth_url,
                token_url,
                scopes,
            })
        }
    } else {
        existing.and_then(|cfg| cfg.oauth.clone())
    };

    Ok(moltis_mcp::McpServerConfig {
        command,
        args,
        env,
        enabled,
        transport,
        url,
        oauth,
    })
}

// ── LiveMcpService ──────────────────────────────────────────────────────────

/// Live MCP service delegating to `McpManager`.
pub struct LiveMcpService {
    manager: Arc<moltis_mcp::McpManager>,
    /// Shared tool registry for syncing MCP tools into the agent loop.
    /// Set after construction via `set_tool_registry`.
    tool_registry: RwLock<Option<Arc<RwLock<ToolRegistry>>>>,
    /// Emergency state for legacy direct MCP bridges.
    legacy_direct: RwLock<LegacyDirectState>,
}

impl LiveMcpService {
    pub fn new(manager: Arc<moltis_mcp::McpManager>) -> Self {
        Self {
            manager,
            tool_registry: RwLock::new(None),
            legacy_direct: RwLock::new(LegacyDirectState::disabled()),
        }
    }

    /// Configure startup defaults for emergency legacy direct mode.
    pub async fn configure_legacy_direct(
        &self,
        cfg: &moltis_config::schema::McpLegacyDirectConfig,
    ) {
        let now = SystemTime::now();
        let ttl_minutes = cfg.ttl_minutes.clamp(1, i64::MAX as u64);
        let enabled_until = if cfg.enabled {
            Some(now + Duration::from_secs(ttl_minutes.saturating_mul(60)))
        } else {
            None
        };
        let allow_servers = cfg.allow_servers.iter().cloned().collect::<HashSet<_>>();
        *self.legacy_direct.write().await = LegacyDirectState {
            enabled_until,
            allow_servers,
        };
    }

    /// Store a reference to the shared tool registry so MCP mutations
    /// can automatically sync tools.
    pub async fn set_tool_registry(&self, registry: Arc<RwLock<ToolRegistry>>) {
        *self.tool_registry.write().await = Some(registry);
    }

    /// Sync MCP tools into the shared tool registry (if set).
    pub async fn sync_tools_if_ready(&self) {
        let maybe_reg = self.tool_registry.read().await.clone();
        if let Some(reg) = maybe_reg {
            let legacy_state = self.legacy_direct.read().await.clone();
            sync_mcp_tools(&self.manager, &reg, &legacy_state).await;
        }
    }

    /// Access the underlying manager.
    pub fn manager(&self) -> &Arc<moltis_mcp::McpManager> {
        &self.manager
    }

    async fn legacy_direct_status_value(&self) -> Value {
        let now = SystemTime::now();
        self.legacy_direct.read().await.status_json(now)
    }
}

#[async_trait]
impl McpService for LiveMcpService {
    async fn list(&self) -> ServiceResult {
        let statuses = self.manager.status_all().await;
        Ok(serde_json::to_value(&statuses)?)
    }

    async fn add(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;
        let redirect_uri = params
            .get("redirectUri")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned);
        let config = parse_server_config(&params, None)?;

        // If a server with this name already exists, append a numeric suffix.
        let final_name = {
            let reg = self.manager.registry_snapshot().await;
            let mut candidate = name.to_string();
            let mut n = 2u32;
            while reg.servers.contains_key(&candidate) {
                candidate = format!("{name}-{n}");
                n += 1;
            }
            candidate
        };

        info!(server = %final_name, "adding MCP server via API");
        match self
            .manager
            .add_server(final_name.clone(), config, true)
            .await
        {
            Ok(_) => {
                self.sync_tools_if_ready().await;
                Ok(serde_json::json!({ "ok": true, "name": final_name }))
            },
            Err(e) => {
                if matches!(
                    e,
                    moltis_mcp::Error::Manager(moltis_mcp::McpManagerError::OAuthRequired { .. })
                ) {
                    if let Some(uri) = redirect_uri {
                        let auth_url = self
                            .manager
                            .oauth_start_server(&final_name, &uri)
                            .await
                            .map_err(ServiceError::message)?;
                        Ok(serde_json::json!({
                            "ok": true,
                            "name": final_name,
                            "oauthPending": true,
                            "authUrl": auth_url
                        }))
                    } else {
                        Ok(serde_json::json!({
                            "ok": true,
                            "name": final_name,
                            "oauthPending": true
                        }))
                    }
                } else {
                    Err(ServiceError::message(e))
                }
            },
        }
    }

    async fn remove(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;

        let removed = self
            .manager
            .remove_server(name)
            .await
            .map_err(ServiceError::message)?;

        self.sync_tools_if_ready().await;

        Ok(serde_json::json!({ "removed": removed }))
    }

    async fn enable(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;
        let redirect_uri = params
            .get("redirectUri")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned);

        match self.manager.enable_server(name).await {
            Ok(_) => {
                self.sync_tools_if_ready().await;
                Ok(serde_json::json!({ "enabled": true }))
            },
            Err(e) => {
                if matches!(
                    e,
                    moltis_mcp::Error::Manager(moltis_mcp::McpManagerError::OAuthRequired { .. })
                ) {
                    if let Some(uri) = redirect_uri {
                        let auth_url = self
                            .manager
                            .oauth_start_server(name, &uri)
                            .await
                            .map_err(ServiceError::message)?;
                        Ok(serde_json::json!({
                            "enabled": false,
                            "oauthPending": true,
                            "authUrl": auth_url
                        }))
                    } else {
                        Ok(serde_json::json!({
                            "enabled": false,
                            "oauthPending": true
                        }))
                    }
                } else {
                    Err(ServiceError::message(e))
                }
            },
        }
    }

    async fn disable(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;

        let ok = self
            .manager
            .disable_server(name)
            .await
            .map_err(ServiceError::message)?;

        self.sync_tools_if_ready().await;

        Ok(serde_json::json!({ "disabled": ok }))
    }

    async fn status(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;

        match self.manager.status(name).await {
            Some(s) => Ok(serde_json::to_value(&s)?),
            None => Err(format!("MCP server '{name}' not found").into()),
        }
    }

    async fn tools(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;

        match self.manager.server_tools(name).await {
            Some(tools) => Ok(serde_json::to_value(&tools)?),
            None => Err(format!("MCP server '{name}' not found or not running").into()),
        }
    }

    async fn restart(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;

        self.manager
            .restart_server(name)
            .await
            .map_err(ServiceError::message)?;

        self.sync_tools_if_ready().await;

        Ok(serde_json::json!({ "ok": true }))
    }

    async fn update(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;
        let existing = self
            .manager
            .registry_snapshot()
            .await
            .servers
            .get(name)
            .cloned()
            .ok_or_else(|| format!("MCP server '{name}' not found"))?;
        let config = parse_server_config(&params, Some(&existing))?;

        self.manager
            .update_server(name, config)
            .await
            .map_err(ServiceError::message)?;

        self.sync_tools_if_ready().await;

        Ok(serde_json::json!({ "ok": true }))
    }

    async fn reauth(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;
        let redirect_uri = params
            .get("redirectUri")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "missing 'redirectUri' parameter".to_string())?;

        let auth_url = self
            .manager
            .reauth_server(name, redirect_uri)
            .await
            .map_err(ServiceError::message)?;

        Ok(serde_json::json!({
            "ok": true,
            "oauthPending": true,
            "authUrl": auth_url
        }))
    }

    async fn oauth_start(&self, params: Value) -> ServiceResult {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'name' parameter".to_string())?;
        let redirect_uri = params
            .get("redirectUri")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| "missing 'redirectUri' parameter".to_string())?;

        let auth_url = self
            .manager
            .oauth_start_server(name, redirect_uri)
            .await
            .map_err(ServiceError::message)?;

        Ok(serde_json::json!({
            "ok": true,
            "oauthPending": true,
            "authUrl": auth_url
        }))
    }

    async fn oauth_complete(&self, params: Value) -> ServiceResult {
        let state = params
            .get("state")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'state' parameter".to_string())?;
        let code = params
            .get("code")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'code' parameter".to_string())?;

        let server_name = self
            .manager
            .oauth_complete_callback(state, code)
            .await
            .map_err(ServiceError::message)?;

        self.sync_tools_if_ready().await;

        Ok(serde_json::json!({
            "ok": true,
            "name": server_name
        }))
    }

    async fn search_tools(&self, params: Value) -> ServiceResult {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let server = params.get("server").and_then(|v| v.as_str());
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(25);
        let tools = self.manager.search_tools(query, server, limit).await;
        Ok(serde_json::json!({ "tools": tools }))
    }

    async fn describe_tool(&self, params: Value) -> ServiceResult {
        let server = params
            .get("server")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'server' parameter".to_string())?;
        let tool = params
            .get("tool")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'tool' parameter".to_string())?;
        let described = self
            .manager
            .describe_tool(server, tool)
            .await
            .ok_or_else(|| format!("MCP tool '{tool}' not found for server '{server}'"))?;
        Ok(serde_json::json!({ "tool": described }))
    }

    async fn legacy_direct_set(&self, params: Value) -> ServiceResult {
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| "missing 'enabled' parameter".to_string())?;

        let mut state = self.legacy_direct.write().await;
        if enabled {
            let ttl_minutes = params
                .get("ttlMinutes")
                .and_then(|v| v.as_u64())
                .unwrap_or(15)
                .clamp(1, i64::MAX as u64);
            state.enabled_until = Some(SystemTime::now() + Duration::from_secs(ttl_minutes.saturating_mul(60)));
            state.allow_servers = params
                .get("allowServers")
                .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
                .unwrap_or_default()
                .into_iter()
                .collect();
        } else {
            *state = LegacyDirectState::disabled();
        }
        drop(state);

        self.sync_tools_if_ready().await;
        Ok(self.legacy_direct_status_value().await)
    }

    async fn legacy_direct_status(&self) -> ServiceResult {
        Ok(self.legacy_direct_status_value().await)
    }
}

#[cfg(test)]
mod tests {
    use {super::*, moltis_mcp::McpRegistry};

    #[test]
    fn parse_server_config_allows_sse_without_command() {
        let cfg = parse_server_config(
            &serde_json::json!({
                "transport": "sse",
                "url": "https://mcp.linear.app/mcp",
                "enabled": true
            }),
            None,
        );
        assert!(
            cfg.is_ok(),
            "expected SSE config to parse without command, got: {cfg:?}"
        );
        let Ok(cfg) = cfg else {
            panic!("SSE config unexpectedly failed to parse");
        };

        assert!(matches!(cfg.transport, moltis_mcp::TransportType::Sse));
        assert_eq!(cfg.command, "");
        assert_eq!(cfg.url.as_deref(), Some("https://mcp.linear.app/mcp"));
    }

    #[test]
    fn parse_server_config_requires_command_for_stdio() {
        let err = parse_server_config(
            &serde_json::json!({
                "transport": "stdio",
                "args": ["-y", "@modelcontextprotocol/server-filesystem"]
            }),
            None,
        )
        .err();

        assert_eq!(
            err.as_ref().map(ToString::to_string).as_deref(),
            Some("missing 'command' parameter")
        );
    }

    #[test]
    fn parse_server_config_requires_url_for_sse() {
        let err = parse_server_config(
            &serde_json::json!({
                "transport": "sse",
            }),
            None,
        )
        .err();

        assert_eq!(
            err.as_ref().map(ToString::to_string).as_deref(),
            Some("missing 'url' parameter for 'sse' transport")
        );
    }

    #[test]
    fn parse_server_config_update_preserves_existing_sse_fields() {
        let existing = moltis_mcp::McpServerConfig {
            transport: moltis_mcp::TransportType::Sse,
            url: Some("https://mcp.linear.app/mcp".to_string()),
            ..Default::default()
        };

        let cfg = parse_server_config(
            &serde_json::json!({
                "enabled": false
            }),
            Some(&existing),
        );
        assert!(
            cfg.is_ok(),
            "expected parser to preserve SSE defaults from existing config, got: {cfg:?}"
        );
        let Ok(cfg) = cfg else {
            panic!("failed to parse update with inherited SSE config");
        };

        assert!(matches!(cfg.transport, moltis_mcp::TransportType::Sse));
        assert_eq!(cfg.url.as_deref(), Some("https://mcp.linear.app/mcp"));
        assert!(!cfg.enabled);
    }

    #[test]
    fn parse_server_config_update_preserves_oauth_when_omitted() {
        let existing = moltis_mcp::McpServerConfig {
            transport: moltis_mcp::TransportType::Sse,
            url: Some("https://mcp.linear.app/mcp".to_string()),
            oauth: Some(moltis_mcp::McpOAuthConfig {
                client_id: "linear-client".to_string(),
                auth_url: "https://linear.app/oauth/authorize".to_string(),
                token_url: "https://api.linear.app/oauth/token".to_string(),
                scopes: vec!["read".to_string(), "write".to_string()],
            }),
            ..Default::default()
        };

        let cfg = parse_server_config(
            &serde_json::json!({
                "transport": "sse"
            }),
            Some(&existing),
        );
        assert!(
            cfg.is_ok(),
            "expected parser to preserve existing oauth fields, got: {cfg:?}"
        );
        let Ok(cfg) = cfg else {
            panic!("failed to parse update while preserving oauth");
        };

        assert!(cfg.oauth.is_some(), "expected oauth to be preserved");
        let Some(oauth) = cfg.oauth else {
            panic!("oauth missing after parse");
        };
        assert_eq!(oauth.client_id, "linear-client");
        assert_eq!(oauth.auth_url, "https://linear.app/oauth/authorize");
        assert_eq!(oauth.token_url, "https://api.linear.app/oauth/token");
        assert_eq!(oauth.scopes, vec!["read".to_string(), "write".to_string()]);
    }

    #[tokio::test]
    async fn test_sync_mcp_tools_empty_manager() {
        let manager = moltis_mcp::McpManager::new(McpRegistry::new());
        let registry = Arc::new(RwLock::new(ToolRegistry::new()));

        sync_mcp_tools(&manager, &registry, &LegacyDirectState::disabled()).await;

        let reg = registry.read().await;
        assert!(reg.list_schemas().is_empty());
    }

    #[tokio::test]
    async fn test_sync_mcp_tools_removes_stale_tools() {
        let manager = moltis_mcp::McpManager::new(McpRegistry::new());
        let registry = Arc::new(RwLock::new(ToolRegistry::new()));

        // Manually register a fake MCP tool to simulate a stale entry.
        {
            let mut reg = registry.write().await;
            reg.register_mcp(
                Box::new(FakeTool("mcp__old__tool".into())),
                "old".to_string(),
            );
        }

        // Sync should remove it since there are no running MCP servers.
        sync_mcp_tools(&manager, &registry, &LegacyDirectState::disabled()).await;

        let reg = registry.read().await;
        assert!(reg.get("mcp__old__tool").is_none());
    }

    #[tokio::test]
    async fn test_sync_preserves_non_mcp_tools() {
        let manager = moltis_mcp::McpManager::new(McpRegistry::new());
        let registry = Arc::new(RwLock::new(ToolRegistry::new()));

        {
            let mut reg = registry.write().await;
            reg.register(Box::new(FakeTool("exec".into())));
        }

        sync_mcp_tools(&manager, &registry, &LegacyDirectState::disabled()).await;

        let reg = registry.read().await;
        assert!(reg.get("exec").is_some());
    }

    /// Minimal AgentTool implementation for testing.
    struct FakeTool(String);

    #[async_trait]
    impl AgentTool for FakeTool {
        fn name(&self) -> &str {
            &self.0
        }

        fn description(&self) -> &str {
            "fake"
        }

        fn parameters_schema(&self) -> Value {
            serde_json::json!({})
        }

        async fn execute(&self, _params: Value) -> Result<Value> {
            Ok(serde_json::json!({}))
        }
    }
}
