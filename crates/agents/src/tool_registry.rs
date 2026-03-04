use {
    anyhow::Result,
    async_trait::async_trait,
    std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    },
};

struct RateLimitState {
    window_started_at: Instant,
    used: u32,
}

/// Per-tool rate limiter with a sliding-window reset every 60 seconds.
#[derive(Clone)]
pub struct RateLimit {
    pub max_per_minute: u32,
    state: Arc<Mutex<RateLimitState>>,
}

impl RateLimit {
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            state: Arc::new(Mutex::new(RateLimitState {
                window_started_at: Instant::now(),
                used: 0,
            })),
        }
    }

    pub fn try_acquire(&self) -> std::result::Result<(), &'static str> {
        let mut state = self.state.lock().map_err(|_| "rate limit unavailable")?;

        if state.window_started_at.elapsed() >= Duration::from_secs(60) {
            state.window_started_at = Instant::now();
            state.used = 0;
        }

        if state.used >= self.max_per_minute {
            return Err("rate limit exceeded");
        }

        state.used += 1;
        Ok(())
    }
}

/// Agent-callable tool.
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(&self, params: serde_json::Value) -> Result<serde_json::Value>;
}

/// Where a tool originates from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// Built-in tool shipped with the binary.
    Builtin,
    /// Tool provided by an MCP server.
    Mcp { server: String },
    /// Tool provided by a precompiled WASM component.
    Wasm { component_hash: [u8; 32] },
}

/// Internal entry pairing a tool with its source metadata.
struct ToolEntry {
    tool: Arc<dyn AgentTool>,
    source: ToolSource,
    rate_limit: Option<RateLimit>,
}

/// Registry of available tools for an agent run.
///
/// Tools are stored as `Arc<dyn AgentTool>` so the registry can be cheaply
/// cloned (e.g. for sub-agents that need a filtered copy of the parent's tools).
pub struct ToolRegistry {
    tools: HashMap<String, ToolEntry>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a built-in tool.
    pub fn register(&mut self, tool: Box<dyn AgentTool>) {
        self.register_with_limit(tool, None);
    }

    /// Register a built-in tool with an optional per-tool rate limit.
    pub fn register_with_limit(&mut self, tool: Box<dyn AgentTool>, limit: Option<RateLimit>) {
        let name = tool.name().to_string();
        self.tools.insert(
            name,
            ToolEntry {
                tool: Arc::from(tool),
                source: ToolSource::Builtin,
                rate_limit: limit,
            },
        );
    }

    /// Register a tool from an MCP server.
    pub fn register_mcp(&mut self, tool: Box<dyn AgentTool>, server: String) {
        let name = tool.name().to_string();
        self.tools.insert(
            name,
            ToolEntry {
                tool: Arc::from(tool),
                source: ToolSource::Mcp { server },
                rate_limit: None,
            },
        );
    }

    /// Register a tool from a WASM component.
    pub fn register_wasm(&mut self, tool: Box<dyn AgentTool>, component_hash: [u8; 32]) {
        let name = tool.name().to_string();
        self.tools.insert(
            name,
            ToolEntry {
                tool: Arc::from(tool),
                source: ToolSource::Wasm { component_hash },
                rate_limit: None,
            },
        );
    }

    pub fn unregister(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    /// Remove all MCP-sourced tools. Returns the number of tools removed.
    pub fn unregister_mcp(&mut self) -> usize {
        let before = self.tools.len();
        self.tools
            .retain(|_, entry| !matches!(entry.source, ToolSource::Mcp { .. }));
        before - self.tools.len()
    }

    pub fn get(&self, name: &str) -> Option<&dyn AgentTool> {
        self.tools.get(name).map(|e| e.tool.as_ref())
    }

    /// Return a cloned tool handle by name.
    pub fn get_arc(&self, name: &str) -> Option<Arc<dyn AgentTool>> {
        self.tools.get(name).map(|e| Arc::clone(&e.tool))
    }

    /// Dispatch a tool call by name: check rate limit, then execute.
    pub async fn call(&self, name: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let entry = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))?;
        let schema = entry.tool.parameters_schema();
        validate_tool_call_arguments(name, &schema, &params)?;
        if let Some(ref rl) = entry.rate_limit
            && let Err(e) = rl.try_acquire()
        {
            return Err(anyhow::anyhow!("{}", e));
        }
        entry.tool.execute(params).await
    }

    pub fn list_schemas(&self) -> Vec<serde_json::Value> {
        self.tools
            .values()
            .map(|e| {
                let mut schema = serde_json::json!({
                    "name": e.tool.name(),
                    "description": e.tool.description(),
                    "parameters": e.tool.parameters_schema(),
                });
                match &e.source {
                    ToolSource::Builtin => {
                        schema["source"] = serde_json::json!("builtin");
                    },
                    ToolSource::Mcp { server } => {
                        schema["source"] = serde_json::json!("mcp");
                        schema["mcpServer"] = serde_json::json!(server);
                    },
                    ToolSource::Wasm { component_hash } => {
                        schema["source"] = serde_json::json!("wasm");
                        schema["componentHash"] =
                            serde_json::json!(hex_component_hash(*component_hash));
                    },
                }
                schema
            })
            .collect()
    }

    /// List registered tool names.
    pub fn list_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Clone the registry, excluding tools whose names start with `prefix`.
    pub fn clone_without_prefix(&self, prefix: &str) -> ToolRegistry {
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| !name.starts_with(prefix))
            .map(|(name, entry)| {
                (
                    name.clone(),
                    ToolEntry {
                        tool: Arc::clone(&entry.tool),
                        source: entry.source.clone(),
                        rate_limit: entry.rate_limit.clone(),
                    },
                )
            })
            .collect();
        ToolRegistry { tools }
    }

    /// Clone the registry, excluding all MCP-sourced tools.
    pub fn clone_without_mcp(&self) -> ToolRegistry {
        let tools = self
            .tools
            .iter()
            .filter(|(_, entry)| !matches!(entry.source, ToolSource::Mcp { .. }))
            .map(|(name, entry)| {
                (
                    name.clone(),
                    ToolEntry {
                        tool: Arc::clone(&entry.tool),
                        source: entry.source.clone(),
                        rate_limit: entry.rate_limit.clone(),
                    },
                )
            })
            .collect();
        ToolRegistry { tools }
    }

    /// Clone the registry, excluding tools whose names are in `exclude`.
    pub fn clone_without(&self, exclude: &[&str]) -> ToolRegistry {
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| !exclude.contains(&name.as_str()))
            .map(|(name, entry)| {
                (
                    name.clone(),
                    ToolEntry {
                        tool: Arc::clone(&entry.tool),
                        source: entry.source.clone(),
                        rate_limit: entry.rate_limit.clone(),
                    },
                )
            })
            .collect();
        ToolRegistry { tools }
    }

    /// Clone the registry keeping only tools that match `predicate`.
    pub fn clone_allowed_by<F>(&self, mut predicate: F) -> ToolRegistry
    where
        F: FnMut(&str) -> bool,
    {
        let tools = self
            .tools
            .iter()
            .filter(|(name, _)| predicate(name))
            .map(|(name, entry)| {
                (
                    name.clone(),
                    ToolEntry {
                        tool: Arc::clone(&entry.tool),
                        source: entry.source.clone(),
                        rate_limit: entry.rate_limit.clone(),
                    },
                )
            })
            .collect();
        ToolRegistry { tools }
    }
}

fn hex_component_hash(component_hash: [u8; 32]) -> String {
    let mut output = String::with_capacity(component_hash.len() * 2);
    for byte in component_hash {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn value_type_name(value: &serde_json::Value) -> &'static str {
    if value.is_string() {
        "string"
    } else if value.is_boolean() {
        "boolean"
    } else if value.is_number() {
        "number"
    } else if value.is_array() {
        "array"
    } else if value.is_object() {
        "object"
    } else if value.is_null() {
        "null"
    } else {
        "unknown"
    }
}

fn type_matches(expected: &str, value: &serde_json::Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn validate_value_constraints(
    tool_name: &str,
    arg_name: &str,
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<()> {
    if let Some(expected_type) = schema.get("type").and_then(serde_json::Value::as_str)
        && !type_matches(expected_type, value)
    {
        return Err(anyhow::anyhow!(
            "invalid parameter '{arg_name}' for tool '{tool_name}': expected {expected_type}, got {}",
            value_type_name(value)
        ));
    }

    if let Some(options) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !options.iter().any(|candidate| candidate == value)
    {
        return Err(anyhow::anyhow!(
            "invalid parameter '{arg_name}' for tool '{tool_name}': value {value} not in enum"
        ));
    }

    if let Some(as_f64) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(serde_json::Value::as_f64)
            && as_f64 < min
        {
            return Err(anyhow::anyhow!(
                "invalid parameter '{arg_name}' for tool '{tool_name}': {as_f64} < minimum {min}"
            ));
        }
        if let Some(max) = schema.get("maximum").and_then(serde_json::Value::as_f64)
            && as_f64 > max
        {
            return Err(anyhow::anyhow!(
                "invalid parameter '{arg_name}' for tool '{tool_name}': {as_f64} > maximum {max}"
            ));
        }
    }

    if let Some(as_str) = value.as_str() {
        let len = as_str.chars().count();
        if let Some(min_len) = schema.get("minLength").and_then(serde_json::Value::as_u64)
            && len < min_len as usize
        {
            return Err(anyhow::anyhow!(
                "invalid parameter '{arg_name}' for tool '{tool_name}': length {len} < minLength {min_len}"
            ));
        }
        if let Some(max_len) = schema.get("maxLength").and_then(serde_json::Value::as_u64)
            && len > max_len as usize
        {
            return Err(anyhow::anyhow!(
                "invalid parameter '{arg_name}' for tool '{tool_name}': length {len} > maxLength {max_len}"
            ));
        }
    }

    if let Some(items) = value.as_array() {
        let len = items.len();
        if let Some(min_items) = schema.get("minItems").and_then(serde_json::Value::as_u64)
            && len < min_items as usize
        {
            return Err(anyhow::anyhow!(
                "invalid parameter '{arg_name}' for tool '{tool_name}': item count {len} < minItems {min_items}"
            ));
        }
        if let Some(max_items) = schema.get("maxItems").and_then(serde_json::Value::as_u64)
            && len > max_items as usize
        {
            return Err(anyhow::anyhow!(
                "invalid parameter '{arg_name}' for tool '{tool_name}': item count {len} > maxItems {max_items}"
            ));
        }
    }

    Ok(())
}

fn validate_tool_call_arguments(
    tool_name: &str,
    schema: &serde_json::Value,
    params: &serde_json::Value,
) -> Result<()> {
    if schema.is_null() {
        return Ok(());
    }
    let Some(object_schema) = schema.as_object() else {
        return Ok(());
    };
    if object_schema
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind != "object")
    {
        return Ok(());
    }

    let Some(args_obj) = params.as_object() else {
        return Err(anyhow::anyhow!(
            "invalid arguments for tool '{tool_name}': expected object, got {}",
            value_type_name(params)
        ));
    };

    let properties = object_schema
        .get("properties")
        .and_then(serde_json::Value::as_object);
    if let Some(required) = object_schema
        .get("required")
        .and_then(serde_json::Value::as_array)
    {
        for req in required.iter().filter_map(serde_json::Value::as_str) {
            if !args_obj.contains_key(req) {
                return Err(anyhow::anyhow!(
                    "missing required parameter '{req}' for tool '{tool_name}'"
                ));
            }
        }
    }

    let additional_allowed = object_schema
        .get("additionalProperties")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);

    for (arg_name, arg_value) in args_obj {
        if arg_name.starts_with('_') {
            // Runtime/session metadata injected by the chat runtime.
            continue;
        }
        let Some(prop_schema) = properties.and_then(|props| props.get(arg_name)) else {
            if !additional_allowed {
                return Err(anyhow::anyhow!(
                    "unknown parameter '{arg_name}' for tool '{tool_name}'"
                ));
            }
            continue;
        };
        validate_value_constraints(tool_name, arg_name, prop_schema, arg_value)?;
    }

    Ok(())
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool {
        name: String,
    }

    #[async_trait]
    impl AgentTool for DummyTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "test"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        async fn execute(&self, _params: serde_json::Value) -> Result<serde_json::Value> {
            Ok(serde_json::json!({}))
        }
    }

    struct StrictArgsTool;

    #[async_trait]
    impl AgentTool for StrictArgsTool {
        fn name(&self) -> &str {
            "strict"
        }

        fn description(&self) -> &str {
            "strict schema"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "minLength": 1 },
                    "timeout": { "type": "integer", "minimum": 1, "maximum": 300 }
                },
                "required": ["command"],
                "additionalProperties": false
            })
        }

        async fn execute(&self, _params: serde_json::Value) -> Result<serde_json::Value> {
            Ok(serde_json::json!({ "ok": true }))
        }
    }

    #[test]
    fn test_clone_without_prefix() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        registry.register(Box::new(DummyTool {
            name: "web_fetch".to_string(),
        }));
        registry.register(Box::new(DummyTool {
            name: "mcp__github_search".to_string(),
        }));
        registry.register(Box::new(DummyTool {
            name: "mcp__memory_store".to_string(),
        }));

        let filtered = registry.clone_without_prefix("mcp__");
        assert_eq!(filtered.list_schemas().len(), 2);
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get("web_fetch").is_some());
        assert!(filtered.get("mcp__github_search").is_none());
        assert!(filtered.get("mcp__memory_store").is_none());
    }

    #[test]
    fn test_clone_without_prefix_no_match() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        registry.register(Box::new(DummyTool {
            name: "web_fetch".to_string(),
        }));

        let filtered = registry.clone_without_prefix("mcp__");
        assert_eq!(filtered.list_schemas().len(), 2);
    }

    #[test]
    fn test_clone_without_mcp() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        registry.register_mcp(
            Box::new(DummyTool {
                name: "mcp__github__search".to_string(),
            }),
            "github".to_string(),
        );
        registry.register_mcp(
            Box::new(DummyTool {
                name: "mcp__memory__store".to_string(),
            }),
            "memory".to_string(),
        );

        let filtered = registry.clone_without_mcp();
        assert_eq!(filtered.list_schemas().len(), 1);
        assert!(filtered.get("exec").is_some());
        assert!(filtered.get("mcp__github__search").is_none());
        assert!(filtered.get("mcp__memory__store").is_none());
    }

    #[test]
    fn test_unregister_mcp() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        registry.register_mcp(
            Box::new(DummyTool {
                name: "mcp__github__search".to_string(),
            }),
            "github".to_string(),
        );
        registry.register_mcp(
            Box::new(DummyTool {
                name: "mcp__memory__store".to_string(),
            }),
            "memory".to_string(),
        );

        let removed = registry.unregister_mcp();
        assert_eq!(removed, 2);
        assert_eq!(registry.list_schemas().len(), 1);
        assert!(registry.get("exec").is_some());
    }

    #[test]
    fn test_list_schemas_includes_source() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        registry.register_mcp(
            Box::new(DummyTool {
                name: "mcp__github__search".to_string(),
            }),
            "github".to_string(),
        );
        registry.register_wasm(
            Box::new(DummyTool {
                name: "calc_wasm".to_string(),
            }),
            [0xAB; 32],
        );

        let schemas = registry.list_schemas();
        let builtin = schemas
            .iter()
            .find(|s| s["name"] == "exec")
            .expect("exec should exist");
        assert_eq!(builtin["source"], "builtin");
        assert!(builtin.get("mcpServer").is_none() || builtin["mcpServer"].is_null());

        let mcp = schemas
            .iter()
            .find(|s| s["name"] == "mcp__github__search")
            .expect("mcp tool should exist");
        assert_eq!(mcp["source"], "mcp");
        assert_eq!(mcp["mcpServer"], "github");

        let wasm = schemas
            .iter()
            .find(|s| s["name"] == "calc_wasm")
            .expect("wasm tool should exist");
        assert_eq!(wasm["source"], "wasm");
        assert_eq!(
            wasm["componentHash"],
            "abababababababababababababababababababababababababababababababab"
        );
    }

    #[test]
    fn test_list_names() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        registry.register(Box::new(DummyTool {
            name: "web_fetch".to_string(),
        }));

        let mut names = registry.list_names();
        names.sort();
        assert_eq!(names, vec!["exec".to_string(), "web_fetch".to_string()]);
    }

    #[test]
    fn test_get_arc_returns_cloned_tool_handle() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        assert!(registry.get_arc("exec").is_some());
        assert!(registry.get_arc("missing").is_none());
    }

    #[test]
    fn test_clone_allowed_by() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DummyTool {
            name: "exec".to_string(),
        }));
        registry.register(Box::new(DummyTool {
            name: "web_fetch".to_string(),
        }));
        registry.register(Box::new(DummyTool {
            name: "session_state".to_string(),
        }));

        let filtered = registry.clone_allowed_by(|name| name.starts_with("web") || name == "exec");
        let mut names = filtered.list_names();
        names.sort();
        assert_eq!(names, vec!["exec".to_string(), "web_fetch".to_string()]);
    }

    #[tokio::test]
    async fn test_clone_without_preserves_rate_limit() {
        let mut registry = ToolRegistry::new();
        registry.register_with_limit(
            Box::new(DummyTool {
                name: "limited".to_string(),
            }),
            Some(RateLimit::new(1)),
        );

        let cloned = registry.clone_without(&[]);
        assert!(cloned.call("limited", serde_json::json!({})).await.is_ok());
        let err = cloned.call("limited", serde_json::json!({})).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("rate limit exceeded"));
    }

    #[tokio::test]
    async fn test_clone_allowed_by_preserves_rate_limit() {
        let mut registry = ToolRegistry::new();
        registry.register_with_limit(
            Box::new(DummyTool {
                name: "limited".to_string(),
            }),
            Some(RateLimit::new(1)),
        );

        let cloned = registry.clone_allowed_by(|name| name == "limited");
        assert!(cloned.call("limited", serde_json::json!({})).await.is_ok());
        let err = cloned.call("limited", serde_json::json!({})).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("rate limit exceeded"));
    }

    #[tokio::test]
    async fn test_call_rejects_missing_required_argument() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StrictArgsTool));
        let err = registry
            .call("strict", serde_json::json!({ "timeout": 30 }))
            .await
            .expect_err("missing required parameter should fail");
        assert!(
            err.to_string()
                .contains("missing required parameter 'command'")
        );
    }

    #[tokio::test]
    async fn test_call_rejects_wrong_argument_type() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StrictArgsTool));
        let err = registry
            .call("strict", serde_json::json!({ "command": 42 }))
            .await
            .expect_err("wrong type should fail");
        assert!(err.to_string().contains("expected string"));
    }

    #[tokio::test]
    async fn test_call_allows_runtime_metadata_fields() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(StrictArgsTool));
        let result = registry
            .call(
                "strict",
                serde_json::json!({
                    "command": "ls -la",
                    "_session_key": "main",
                    "_accept_language": "en-US",
                }),
            )
            .await;
        assert!(result.is_ok(), "runtime metadata fields should be ignored");
    }
}
