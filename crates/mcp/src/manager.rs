//! McpManager: lifecycle management for multiple MCP server connections.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use {
    tokio::sync::RwLock,
    tracing::{info, warn},
};

use crate::{
    auth::{McpAuthState, McpOAuthOverride, McpOAuthProvider, SharedAuthProvider},
    client::{McpClient, McpClientState},
    error::{Context, Error, Result},
    registry::{McpOAuthConfig, McpRegistry, McpServerConfig, TransportType},
    tool_bridge::McpToolBridge,
    traits::McpClientTrait,
    types::{McpManagerError, McpToolDef, McpTransportError, ToolsCallResult},
};

/// Status of a managed MCP server.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerStatus {
    pub name: String,
    pub state: String,
    pub enabled: bool,
    pub tool_count: usize,
    pub server_info: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub transport: crate::registry::TransportType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// OAuth authentication state (only for SSE servers with auth).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_state: Option<McpAuthState>,
    /// Pending OAuth URL to open in browser (when auth_state is awaiting_browser).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_url: Option<String>,
}

/// Mutable state behind the single `RwLock` on [`McpManager`].
pub struct McpManagerInner {
    pub clients: HashMap<String, Arc<RwLock<dyn McpClientTrait>>>,
    pub tools: HashMap<String, Vec<McpToolDef>>,
    tool_outcomes: HashMap<String, ToolOutcomeStats>,
    tool_search_cache: HashMap<SearchCacheKey, SearchCacheEntry>,
    tool_describe_cache: HashMap<DescribeCacheKey, DescribeCacheEntry>,
    pub registry: McpRegistry,
    /// OAuth auth providers for SSE servers, keyed by server name.
    pub auth_providers: HashMap<String, SharedAuthProvider>,
}

/// Manages the lifecycle of multiple MCP server connections.
pub struct McpManager {
    pub inner: RwLock<McpManagerInner>,
    tool_cache_ttl: Duration,
    search_server_priors: HashMap<String, i32>,
    search_success_weight: i32,
    search_semantic_weight: i32,
}

/// Manager options controlling runtime behavior.
#[derive(Debug, Clone)]
pub struct McpManagerOptions {
    pub tool_summary_cache_ttl_secs: u64,
    pub search_server_priors: HashMap<String, i32>,
    pub search_success_weight: i32,
    pub search_semantic_weight: i32,
}

impl Default for McpManagerOptions {
    fn default() -> Self {
        Self {
            tool_summary_cache_ttl_secs: 300,
            search_server_priors: HashMap::new(),
            search_success_weight: 120,
            search_semantic_weight: 60,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ToolOutcomeStats {
    success_count: u64,
    failure_count: u64,
}

/// Detail level for MCP tool search responses.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolDetailLevel {
    Name,
    #[default]
    Summary,
    Full,
}

/// A compact MCP tool descriptor for search/browse workflows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpToolSummary {
    pub server: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SearchCacheKey {
    query: String,
    server: Option<String>,
    limit: usize,
    detail_level: ToolDetailLevel,
}

#[derive(Debug, Clone)]
struct SearchCacheEntry {
    created_at: Instant,
    tools: Vec<McpToolSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DescribeCacheKey {
    server: String,
    tool: String,
}

#[derive(Debug, Clone)]
struct DescribeCacheEntry {
    created_at: Instant,
    described: Option<McpToolDef>,
}

impl McpManager {
    pub fn new(registry: McpRegistry) -> Self {
        Self::new_with_options(registry, McpManagerOptions::default())
    }

    pub fn new_with_options(registry: McpRegistry, options: McpManagerOptions) -> Self {
        let ttl_secs = options.tool_summary_cache_ttl_secs.max(1);
        Self {
            inner: RwLock::new(McpManagerInner {
                clients: HashMap::new(),
                tools: HashMap::new(),
                tool_outcomes: HashMap::new(),
                tool_search_cache: HashMap::new(),
                tool_describe_cache: HashMap::new(),
                registry,
                auth_providers: HashMap::new(),
            }),
            tool_cache_ttl: Duration::from_secs(ttl_secs),
            search_server_priors: options.search_server_priors,
            search_success_weight: options.search_success_weight.max(0),
            search_semantic_weight: options.search_semantic_weight.max(0),
        }
    }

    fn cache_is_fresh(created_at: Instant, ttl: Duration) -> bool {
        created_at.elapsed() <= ttl
    }

    fn clear_tool_caches(inner: &mut McpManagerInner) {
        inner.tool_search_cache.clear();
        inner.tool_describe_cache.clear();
    }

    fn semantic_tokenize(input: &str) -> HashSet<String> {
        input
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|token| !token.trim().is_empty())
            .map(|token| token.to_ascii_lowercase())
            .collect()
    }

    fn semantic_match_score(query: &str, text: &str) -> i32 {
        let query_tokens = Self::semantic_tokenize(query);
        if query_tokens.is_empty() {
            return 0;
        }
        let text_tokens = Self::semantic_tokenize(text);
        if text_tokens.is_empty() {
            return 0;
        }
        let overlap = query_tokens.intersection(&text_tokens).count() as f64;
        let union = query_tokens.union(&text_tokens).count() as f64;
        if union <= 0.0 {
            return 0;
        }
        ((overlap / union) * 100.0).round() as i32
    }

    fn tool_outcome_key(server: &str, tool: &str) -> String {
        format!("{server}::{tool}")
    }

    pub async fn record_tool_outcome(&self, server: &str, tool: &str, success: bool) {
        let mut inner = self.inner.write().await;
        let key = Self::tool_outcome_key(server, tool);
        let stats = inner.tool_outcomes.entry(key).or_default();
        if success {
            stats.success_count = stats.success_count.saturating_add(1);
        } else {
            stats.failure_count = stats.failure_count.saturating_add(1);
        }
        inner
            .tool_search_cache
            .retain(|_, entry| Self::cache_is_fresh(entry.created_at, self.tool_cache_ttl));
    }

    fn build_auth_provider(
        name: &str,
        url: &str,
        oauth: Option<&McpOAuthConfig>,
    ) -> SharedAuthProvider {
        let provider = if let Some(ov) = oauth {
            McpOAuthProvider::new(name, url).with_oauth_override(McpOAuthOverride {
                client_id: ov.client_id.clone(),
                auth_url: ov.auth_url.clone(),
                token_url: ov.token_url.clone(),
                scopes: ov.scopes.clone(),
            })
        } else {
            McpOAuthProvider::new(name, url)
        };
        Arc::new(provider)
    }

    fn should_attempt_auth_connection(
        has_existing_auth_provider: bool,
        has_oauth_override: bool,
        has_stored_token: bool,
    ) -> bool {
        has_existing_auth_provider || has_oauth_override || has_stored_token
    }

    /// Start all enabled servers from the registry.
    pub async fn start_enabled(&self) -> Vec<String> {
        let enabled: Vec<(String, McpServerConfig)> = {
            let inner = self.inner.read().await;
            inner
                .registry
                .enabled_servers()
                .into_iter()
                .map(|(name, cfg)| (name.to_string(), cfg.clone()))
                .collect()
        };

        let mut started = Vec::new();
        for (name, config) in enabled {
            match self.start_server(&name, &config).await {
                Ok(()) => started.push(name),
                Err(e) => warn!(server = %name, error = %e, "failed to start MCP server"),
            }
        }
        started
    }

    /// Start a single server connection.
    ///
    /// For SSE servers: attempts unauthenticated first. On 401 Unauthorized,
    /// stores auth context and returns `McpManagerError::OAuthRequired`.
    pub async fn start_server(&self, name: &str, config: &McpServerConfig) -> Result<()> {
        // Shut down existing connection if any.
        self.stop_server(name).await;

        // Network work happens outside the lock.
        let (client, auth_provider) = match config.transport {
            TransportType::Sse => {
                let url = config
                    .url
                    .as_deref()
                    .with_context(|| format!("SSE transport for '{name}' requires a url"))?;

                // Check if we already have an auth provider (from a previous connection).
                let existing_auth = {
                    let inner = self.inner.read().await;
                    inner.auth_providers.get(name).cloned()
                };

                let has_existing_auth_provider = existing_auth.is_some();
                let auth_provider = existing_auth
                    .unwrap_or_else(|| Self::build_auth_provider(name, url, config.oauth.as_ref()));

                // If we have a stored token, prefer auth transport immediately.
                // This avoids forced re-auth at process start for OAuth-backed servers.
                let has_stored_token = if has_existing_auth_provider {
                    false
                } else {
                    auth_provider.access_token().await?.is_some()
                };

                if Self::should_attempt_auth_connection(
                    has_existing_auth_provider,
                    config.oauth.is_some(),
                    has_stored_token,
                ) {
                    let client =
                        McpClient::connect_sse_with_auth(name, url, auth_provider.clone()).await?;
                    (client, Some(auth_provider))
                } else {
                    // No hint that auth is needed yet, probe unauthenticated first.
                    match McpClient::connect_sse(name, url).await {
                        Ok(client) => (client, None),
                        Err(e) => {
                            // Check if it's a 401 Unauthorized.
                            if let Error::Transport(McpTransportError::Unauthorized {
                                www_authenticate,
                            }) = &e
                            {
                                info!(
                                    server = %name,
                                    "SSE server requires auth"
                                );

                                // Mark auth as required and persist challenge metadata.
                                let auth_ok = auth_provider
                                    .handle_unauthorized(www_authenticate.as_deref())
                                    .await?;

                                if !auth_ok {
                                    let mut inner = self.inner.write().await;
                                    inner.auth_providers.insert(name.to_string(), auth_provider);
                                    return Err(McpManagerError::OAuthRequired {
                                        server: name.to_string(),
                                    }
                                    .into());
                                }

                                // Retry with auth.
                                let client = McpClient::connect_sse_with_auth(
                                    name,
                                    url,
                                    auth_provider.clone(),
                                )
                                .await?;
                                (client, Some(auth_provider))
                            } else {
                                return Err(e);
                            }
                        },
                    }
                }
            },
            TransportType::Stdio => {
                let client =
                    McpClient::connect(name, &config.command, &config.args, &config.env).await?;
                (client, None)
            },
        };

        // Fetch tools.
        let mut client = client;
        let tool_defs = client.list_tools().await?.to_vec();
        info!(
            server = %name,
            tools = tool_defs.len(),
            "MCP server started with tools"
        );

        // Atomic insert of client, tools, and auth provider.
        let client: Arc<RwLock<dyn McpClientTrait>> = Arc::new(RwLock::new(client));
        let mut inner = self.inner.write().await;
        inner.clients.insert(name.to_string(), client);
        inner.tools.insert(name.to_string(), tool_defs);
        Self::clear_tool_caches(&mut inner);

        if let Some(auth) = auth_provider {
            inner.auth_providers.insert(name.to_string(), auth);
        }

        Ok(())
    }

    /// Stop a server connection.
    pub async fn stop_server(&self, name: &str) {
        // Atomically remove client and tools, then drop the lock before async shutdown.
        // Keep auth_providers for potential reconnection.
        let client = {
            let mut inner = self.inner.write().await;
            inner.tools.remove(name);
            Self::clear_tool_caches(&mut inner);
            inner.clients.remove(name)
        };
        if let Some(client) = client {
            let mut c = client.write().await;
            c.shutdown().await;
        }
    }

    /// Restart a server.
    pub async fn restart_server(&self, name: &str) -> Result<()> {
        let config = {
            let inner = self.inner.read().await;
            inner
                .registry
                .get(name)
                .cloned()
                .with_context(|| format!("MCP server '{name}' not found in registry"))?
        };
        self.start_server(name, &config).await
    }

    /// Start OAuth for an SSE server and return the browser authorization URL.
    pub async fn oauth_start_server(&self, name: &str, redirect_uri: &str) -> Result<String> {
        let config =
            {
                let inner = self.inner.read().await;
                inner.registry.get(name).cloned().ok_or_else(|| {
                    McpManagerError::ServerNotFound {
                        server: name.to_string(),
                    }
                })?
            };

        if !matches!(config.transport, TransportType::Sse) {
            return Err(McpManagerError::NotSseTransport {
                server: name.to_string(),
            }
            .into());
        }

        let url = config
            .url
            .as_deref()
            .ok_or_else(|| McpManagerError::MissingSseUrl {
                server: name.to_string(),
            })?;

        let existing_auth = {
            let inner = self.inner.read().await;
            inner.auth_providers.get(name).cloned()
        };
        let has_existing_auth_provider = existing_auth.is_some();
        let auth_provider = existing_auth
            .unwrap_or_else(|| Self::build_auth_provider(name, url, config.oauth.as_ref()));

        if !has_existing_auth_provider {
            let mut inner = self.inner.write().await;
            inner
                .auth_providers
                .insert(name.to_string(), auth_provider.clone());
        }

        auth_provider
            .start_oauth(redirect_uri, None)
            .await?
            .with_context(|| format!("MCP server '{name}' does not support OAuth"))
    }

    /// Complete an OAuth callback by matching state across MCP auth providers.
    ///
    /// Returns the server name whose OAuth flow was completed.
    pub async fn oauth_complete_callback(&self, state: &str, code: &str) -> Result<String> {
        let providers: Vec<(String, SharedAuthProvider)> = {
            let inner = self.inner.read().await;
            inner
                .auth_providers
                .iter()
                .map(|(name, provider)| (name.clone(), provider.clone()))
                .collect()
        };

        for (name, provider) in providers {
            if provider.complete_oauth(state, code).await? {
                self.restart_server(&name).await?;
                return Ok(name);
            }
        }

        Err(McpManagerError::OAuthStateNotFound.into())
    }

    /// Trigger re-authentication for an SSE server.
    pub async fn reauth_server(&self, name: &str, redirect_uri: &str) -> Result<String> {
        self.oauth_start_server(name, redirect_uri).await
    }

    /// Get the status of all configured servers.
    pub async fn status_all(&self) -> Vec<ServerStatus> {
        let inner = self.inner.read().await;

        let mut statuses = Vec::new();
        for (name, config) in &inner.registry.servers {
            let state = if let Some(client) = inner.clients.get(name) {
                let c = client.read().await;
                match c.state() {
                    McpClientState::Ready => {
                        if c.is_alive().await {
                            "running"
                        } else {
                            "dead"
                        }
                    },
                    McpClientState::Connected => "connecting",
                    McpClientState::Authenticating => "authenticating",
                    McpClientState::Closed => "stopped",
                }
            } else {
                "stopped"
            };

            let auth_state = inner.auth_providers.get(name).map(|a| a.auth_state());
            let auth_url = inner
                .auth_providers
                .get(name)
                .and_then(|a| a.pending_auth_url());

            statuses.push(ServerStatus {
                name: name.clone(),
                state: state.into(),
                enabled: config.enabled,
                tool_count: inner.tools.get(name).map_or(0, |t| t.len()),
                server_info: None,
                command: config.command.clone(),
                args: config.args.clone(),
                env: config.env.clone(),
                transport: config.transport,
                url: config.url.clone(),
                auth_state,
                auth_url,
            });
        }
        statuses
    }

    /// Get the status of a single server.
    pub async fn status(&self, name: &str) -> Option<ServerStatus> {
        self.status_all().await.into_iter().find(|s| s.name == name)
    }

    /// Get tool bridges for all running servers (for registration into ToolRegistry).
    pub async fn tool_bridges(&self) -> Vec<McpToolBridge> {
        let inner = self.inner.read().await;
        let mut bridges = Vec::new();

        for (name, client) in inner.clients.iter() {
            if let Some(tool_defs) = inner.tools.get(name) {
                bridges.extend(McpToolBridge::from_client(
                    name,
                    tool_defs,
                    Arc::clone(client),
                ));
            }
        }

        bridges
    }

    /// Get tools for a specific server.
    pub async fn server_tools(&self, name: &str) -> Option<Vec<McpToolDef>> {
        self.inner.read().await.tools.get(name).cloned()
    }

    /// Search tools across running servers by name/description.
    pub async fn search_tools(
        &self,
        query: &str,
        server: Option<&str>,
        limit: usize,
        detail_level: ToolDetailLevel,
    ) -> Vec<McpToolSummary> {
        let q = query.trim().to_lowercase();
        let server_filter = server
            .map(str::trim)
            .filter(|candidate| !candidate.is_empty())
            .map(str::to_string);
        let max = limit.clamp(1, 200);
        let cache_key = SearchCacheKey {
            query: q.clone(),
            server: server_filter.clone(),
            limit: max,
            detail_level,
        };

        {
            let inner = self.inner.read().await;
            if let Some(entry) = inner.tool_search_cache.get(&cache_key)
                && Self::cache_is_fresh(entry.created_at, self.tool_cache_ttl)
            {
                return entry.tools.clone();
            }
        }

        let inner = self.inner.read().await;

        let mut scored: Vec<(i32, McpToolSummary)> = Vec::new();
        for (server_name, tools) in &inner.tools {
            if let Some(filter_server) = server_filter.as_deref()
                && filter_server != server_name
            {
                continue;
            }
            for tool in tools {
                let desc = tool.description.as_deref().unwrap_or("");
                let mut score = 0i32;
                if q.is_empty() {
                    score = 1;
                } else {
                    let name_l = tool.name.to_lowercase();
                    let desc_l = desc.to_lowercase();
                    if name_l == q {
                        score += 1_000;
                    }
                    if name_l.starts_with(&q) {
                        score += 250;
                    }
                    if name_l.contains(&q) {
                        score += 100;
                    }
                    if desc_l.contains(&q) {
                        score += 25;
                    }
                }
                let semantic_score =
                    Self::semantic_match_score(&q, format!("{} {}", tool.name, desc).as_str());
                score += semantic_score.saturating_mul(self.search_semantic_weight) / 100;

                let key = Self::tool_outcome_key(server_name, &tool.name);
                if let Some(stats) = inner.tool_outcomes.get(&key) {
                    let success = stats.success_count as i64;
                    let failure = stats.failure_count as i64;
                    let weighted = ((success * 3) - (failure * 2)).clamp(
                        -(self.search_success_weight as i64),
                        self.search_success_weight as i64,
                    );
                    score += weighted as i32;
                }

                score += self
                    .search_server_priors
                    .get(server_name)
                    .copied()
                    .unwrap_or_default();

                if score > 0 {
                    let description = match detail_level {
                        ToolDetailLevel::Name => None,
                        ToolDetailLevel::Summary | ToolDetailLevel::Full => {
                            (!desc.is_empty()).then(|| desc.to_string())
                        },
                    };
                    let input_schema = match detail_level {
                        ToolDetailLevel::Full => Some(tool.input_schema.clone()),
                        ToolDetailLevel::Name | ToolDetailLevel::Summary => None,
                    };
                    scored.push((
                        score,
                        McpToolSummary {
                            server: server_name.clone(),
                            name: tool.name.clone(),
                            description,
                            input_schema,
                        },
                    ));
                }
            }
        }

        scored.sort_by(|(score_a, tool_a), (score_b, tool_b)| {
            score_b
                .cmp(score_a)
                .then_with(|| tool_a.server.cmp(&tool_b.server))
                .then_with(|| tool_a.name.cmp(&tool_b.name))
        });
        let tools: Vec<McpToolSummary> = scored
            .into_iter()
            .take(max)
            .map(|(_, summary)| summary)
            .collect();

        drop(inner);
        let mut inner = self.inner.write().await;
        inner
            .tool_search_cache
            .retain(|_, entry| Self::cache_is_fresh(entry.created_at, self.tool_cache_ttl));
        inner.tool_search_cache.insert(
            cache_key,
            SearchCacheEntry {
                created_at: Instant::now(),
                tools: tools.clone(),
            },
        );

        tools
    }

    /// Return the full schema for a specific server tool.
    pub async fn describe_tool(&self, server: &str, tool: &str) -> Option<McpToolDef> {
        let cache_key = DescribeCacheKey {
            server: server.to_string(),
            tool: tool.to_string(),
        };

        {
            let inner = self.inner.read().await;
            if let Some(entry) = inner.tool_describe_cache.get(&cache_key)
                && Self::cache_is_fresh(entry.created_at, self.tool_cache_ttl)
            {
                return entry.described.clone();
            }
        }

        let described = {
            let inner = self.inner.read().await;
            let tools = inner.tools.get(server)?;
            tools.iter().find(|t| t.name == tool).cloned()
        };

        let mut inner = self.inner.write().await;
        inner
            .tool_describe_cache
            .retain(|_, entry| Self::cache_is_fresh(entry.created_at, self.tool_cache_ttl));
        inner.tool_describe_cache.insert(
            cache_key,
            DescribeCacheEntry {
                created_at: Instant::now(),
                described: described.clone(),
            },
        );
        described
    }

    /// Call a tool by `(server, tool)` reference.
    pub async fn call_server_tool(
        &self,
        server: &str,
        tool: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolsCallResult> {
        let client =
            {
                let inner = self.inner.read().await;
                inner.clients.get(server).cloned().ok_or_else(|| {
                    McpManagerError::ServerNotFound {
                        server: server.to_string(),
                    }
                })?
            };
        let c = client.read().await;
        c.call_tool(tool, arguments).await
    }

    // ── Registry operations ─────────────────────────────────────────

    /// Add a server to the registry and optionally start it.
    pub async fn add_server(
        &self,
        name: String,
        config: McpServerConfig,
        start: bool,
    ) -> Result<()> {
        let enabled = config.enabled;
        {
            let mut inner = self.inner.write().await;
            inner.registry.add(name.clone(), config.clone())?;
        }
        if start && enabled {
            self.start_server(&name, &config).await?;
        }
        Ok(())
    }

    /// Remove a server from the registry and stop it.
    pub async fn remove_server(&self, name: &str) -> Result<bool> {
        self.stop_server(name).await;
        let mut inner = self.inner.write().await;
        inner.auth_providers.remove(name);
        inner.registry.remove(name)
    }

    /// Enable a server and start it.
    pub async fn enable_server(&self, name: &str) -> Result<bool> {
        let config = {
            let mut inner = self.inner.write().await;
            if !inner.registry.enable(name)? {
                return Ok(false);
            }
            inner.registry.get(name).cloned()
        };
        if let Some(config) = config {
            self.start_server(name, &config).await?;
        }
        Ok(true)
    }

    /// Disable a server and stop it.
    pub async fn disable_server(&self, name: &str) -> Result<bool> {
        self.stop_server(name).await;
        let mut inner = self.inner.write().await;
        inner.registry.disable(name)
    }

    /// Get a snapshot of the registry for serialization.
    pub async fn registry_snapshot(&self) -> McpRegistry {
        self.inner.read().await.registry.clone()
    }

    /// Update a server's configuration and restart it if running.
    pub async fn update_server(&self, name: &str, config: McpServerConfig) -> Result<()> {
        let was_running = {
            let inner = self.inner.read().await;
            inner.clients.contains_key(name)
        };
        {
            let mut inner = self.inner.write().await;
            let enabled = inner.registry.get(name).is_none_or(|c| c.enabled);
            let mut new_config = config;
            new_config.enabled = enabled;
            inner.registry.add(name.to_string(), new_config)?;
        }
        if was_running {
            self.restart_server(name).await?;
        }
        Ok(())
    }

    /// Shut down all servers.
    pub async fn shutdown_all(&self) {
        let names: Vec<String> = self.inner.read().await.clients.keys().cloned().collect();
        for name in names {
            self.stop_server(&name).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct FailingMcpClient;

    #[async_trait]
    impl McpClientTrait for FailingMcpClient {
        fn server_name(&self) -> &str {
            "failing"
        }

        fn state(&self) -> McpClientState {
            McpClientState::Ready
        }

        fn tools(&self) -> &[McpToolDef] {
            &[]
        }

        async fn list_tools(&mut self) -> Result<&[McpToolDef]> {
            Ok(&[])
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: serde_json::Value,
        ) -> Result<ToolsCallResult> {
            Err(Error::message("transport failed"))
        }

        async fn is_alive(&self) -> bool {
            true
        }

        async fn shutdown(&mut self) {}
    }

    #[test]
    fn test_manager_creation() {
        let reg = McpRegistry::new();
        let _mgr = McpManager::new(reg);
    }

    #[test]
    fn test_should_attempt_auth_connection_with_existing_provider() {
        assert!(McpManager::should_attempt_auth_connection(
            true, false, false
        ));
    }

    #[test]
    fn test_should_attempt_auth_connection_with_oauth_override() {
        assert!(McpManager::should_attempt_auth_connection(
            false, true, false
        ));
    }

    #[test]
    fn test_should_attempt_auth_connection_with_stored_token() {
        assert!(McpManager::should_attempt_auth_connection(
            false, false, true
        ));
    }

    #[test]
    fn test_should_attempt_auth_connection_without_auth_signals() {
        assert!(!McpManager::should_attempt_auth_connection(
            false, false, false
        ));
    }

    #[tokio::test]
    async fn test_status_all_empty() {
        let mgr = McpManager::new(McpRegistry::new());
        let statuses = mgr.status_all().await;
        assert!(statuses.is_empty());
    }

    #[tokio::test]
    async fn test_tool_bridges_empty() {
        let mgr = McpManager::new(McpRegistry::new());
        let bridges = mgr.tool_bridges().await;
        assert!(bridges.is_empty());
    }

    #[tokio::test]
    async fn test_status_shows_stopped_for_configured_but_not_started() {
        let mut reg = McpRegistry::new();
        reg.servers.insert(
            "test".into(),
            McpServerConfig {
                command: "echo".into(),
                ..Default::default()
            },
        );
        let mgr = McpManager::new(reg);

        let statuses = mgr.status_all().await;
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].state, "stopped");
        assert!(statuses[0].enabled);
        assert!(statuses[0].auth_state.is_none());
    }

    #[tokio::test]
    async fn test_reauth_server_no_auth_provider() {
        let mgr = McpManager::new(McpRegistry::new());
        let result = mgr
            .reauth_server("nonexistent", "https://example.com/auth/callback")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_oauth_start_server_requires_sse_transport() {
        let mut reg = McpRegistry::new();
        reg.servers.insert(
            "stdio".into(),
            McpServerConfig {
                command: "echo".into(),
                transport: TransportType::Stdio,
                ..Default::default()
            },
        );
        let mgr = McpManager::new(reg);
        let err = mgr
            .oauth_start_server("stdio", "https://example.com/auth/callback")
            .await
            .expect_err("expected oauth start to fail for stdio transport");
        assert!(matches!(
            err,
            Error::Manager(McpManagerError::NotSseTransport { .. })
        ));
    }

    #[tokio::test]
    async fn test_oauth_complete_callback_unknown_state() {
        let mgr = McpManager::new(McpRegistry::new());
        let err = mgr
            .oauth_complete_callback("unknown-state", "code")
            .await
            .expect_err("expected unknown state to fail");
        assert!(matches!(
            err,
            Error::Manager(McpManagerError::OAuthStateNotFound)
        ));
    }

    #[tokio::test]
    async fn test_search_tools_ranks_name_matches() {
        let mgr = McpManager::new(McpRegistry::new());
        {
            let mut inner = mgr.inner.write().await;
            inner.tools.insert(
                "filesystem".to_string(),
                vec![
                    McpToolDef {
                        name: "read_file".to_string(),
                        description: Some("Read file contents".to_string()),
                        input_schema: serde_json::json!({ "type": "object" }),
                    },
                    McpToolDef {
                        name: "write_file".to_string(),
                        description: Some("Write file contents".to_string()),
                        input_schema: serde_json::json!({ "type": "object" }),
                    },
                ],
            );
            inner.tools.insert(
                "github".to_string(),
                vec![McpToolDef {
                    name: "search_code".to_string(),
                    description: Some("Search source code".to_string()),
                    input_schema: serde_json::json!({ "type": "object" }),
                }],
            );
        }

        let results = mgr
            .search_tools("read", None, 10, ToolDetailLevel::Summary)
            .await;
        assert!(!results.is_empty());
        assert_eq!(results[0].server, "filesystem");
        assert_eq!(results[0].name, "read_file");
        assert_eq!(
            results[0].description.as_deref(),
            Some("Read file contents")
        );
        assert!(results[0].input_schema.is_none());
    }

    #[tokio::test]
    async fn test_search_tools_name_level_omits_description_and_schema() {
        let mgr = McpManager::new(McpRegistry::new());
        {
            let mut inner = mgr.inner.write().await;
            inner.tools.insert(
                "filesystem".to_string(),
                vec![McpToolDef {
                    name: "read_file".to_string(),
                    description: Some("Read file contents".to_string()),
                    input_schema: serde_json::json!({ "type": "object" }),
                }],
            );
        }

        let results = mgr
            .search_tools("read", None, 10, ToolDetailLevel::Name)
            .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].description.is_none());
        assert!(results[0].input_schema.is_none());
    }

    #[tokio::test]
    async fn test_search_tools_full_level_includes_schema() {
        let mgr = McpManager::new(McpRegistry::new());
        {
            let mut inner = mgr.inner.write().await;
            inner.tools.insert(
                "filesystem".to_string(),
                vec![McpToolDef {
                    name: "read_file".to_string(),
                    description: Some("Read file contents".to_string()),
                    input_schema: serde_json::json!({ "type": "object" }),
                }],
            );
        }

        let results = mgr
            .search_tools("read", None, 10, ToolDetailLevel::Full)
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].description.as_deref(),
            Some("Read file contents")
        );
        assert_eq!(
            results[0].input_schema.as_ref().and_then(|v| v.get("type")),
            Some(&serde_json::json!("object"))
        );
    }

    #[tokio::test]
    async fn test_describe_tool_returns_schema() {
        let mgr = McpManager::new(McpRegistry::new());
        {
            let mut inner = mgr.inner.write().await;
            inner.tools.insert(
                "filesystem".to_string(),
                vec![McpToolDef {
                    name: "read_file".to_string(),
                    description: Some("Read file contents".to_string()),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } }
                    }),
                }],
            );
        }

        let described = mgr.describe_tool("filesystem", "read_file").await;
        assert!(described.is_some());
        let Some(tool) = described else {
            panic!("expected tool to be present");
        };
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.input_schema["type"], "object");
    }

    #[tokio::test]
    async fn test_call_server_tool_missing_server() {
        let mgr = McpManager::new(McpRegistry::new());
        let err = mgr
            .call_server_tool("missing", "tool", serde_json::json!({}))
            .await
            .expect_err("missing server should fail");
        assert!(matches!(
            err,
            Error::Manager(McpManagerError::ServerNotFound { .. })
        ));
    }

    #[tokio::test]
    async fn test_call_server_tool_does_not_double_record_failures() {
        let mgr = McpManager::new(McpRegistry::new());
        {
            let mut inner = mgr.inner.write().await;
            inner.clients.insert(
                "failing".to_string(),
                Arc::new(RwLock::new(FailingMcpClient)),
            );
        }

        let err = mgr
            .call_server_tool("failing", "tool", serde_json::json!({}))
            .await
            .expect_err("call should fail");
        assert!(matches!(err, Error::Message { .. }));

        let inner = mgr.inner.read().await;
        assert!(inner.tool_outcomes.get("failing::tool").is_none());
    }

    #[tokio::test]
    async fn test_search_tools_historical_success_affects_rank() {
        let mgr = McpManager::new(McpRegistry::new());
        {
            let mut inner = mgr.inner.write().await;
            inner.tools.insert(
                "filesystem".to_string(),
                vec![
                    McpToolDef {
                        name: "read_file".to_string(),
                        description: Some("Read files".to_string()),
                        input_schema: serde_json::json!({}),
                    },
                    McpToolDef {
                        name: "search_file".to_string(),
                        description: Some("Search file text".to_string()),
                        input_schema: serde_json::json!({}),
                    },
                ],
            );
        }
        mgr.record_tool_outcome("filesystem", "search_file", true)
            .await;
        mgr.record_tool_outcome("filesystem", "search_file", true)
            .await;
        mgr.record_tool_outcome("filesystem", "read_file", false)
            .await;

        let results = mgr
            .search_tools("file", Some("filesystem"), 5, ToolDetailLevel::Summary)
            .await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "search_file");
    }

    #[tokio::test]
    async fn test_search_tools_server_prior_affects_rank() {
        let mut priors = HashMap::new();
        priors.insert("preferred".to_string(), 50);
        let mgr = McpManager::new_with_options(
            McpRegistry::new(),
            McpManagerOptions {
                search_server_priors: priors,
                ..McpManagerOptions::default()
            },
        );
        {
            let mut inner = mgr.inner.write().await;
            inner.tools.insert(
                "preferred".to_string(),
                vec![McpToolDef {
                    name: "lookup".to_string(),
                    description: Some("lookup".to_string()),
                    input_schema: serde_json::json!({}),
                }],
            );
            inner.tools.insert(
                "other".to_string(),
                vec![McpToolDef {
                    name: "lookup".to_string(),
                    description: Some("lookup".to_string()),
                    input_schema: serde_json::json!({}),
                }],
            );
        }
        let results = mgr
            .search_tools("lookup", None, 5, ToolDetailLevel::Name)
            .await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].server, "preferred");
    }
}
