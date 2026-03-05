use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use {
    anyhow::{Result, anyhow},
    async_trait::async_trait,
    moltis_agents::tool_registry::{AgentTool, ToolEffectClass},
    moltis_mcp::ToolDetailLevel,
    moltis_mcp::types::{McpToolDef, ToolContent, ToolsCallResult},
    serde::Deserialize,
    serde_json::Value,
};

#[derive(Deserialize)]
struct SelectedTool {
    server: String,
    tool: String,
}

#[derive(Deserialize)]
struct Program {
    steps: Vec<ProgramStep>,
    #[serde(default)]
    return_step: Option<String>,
}

#[derive(Deserialize)]
struct ProgramStep {
    id: String,
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Value,
}

fn parse_program(params: &Value) -> Result<Program> {
    if let Some(program) = params.get("program") {
        if program.is_string() {
            let src = program
                .as_str()
                .ok_or_else(|| anyhow!("invalid 'program' parameter"))?;
            return Ok(serde_json::from_str::<Program>(src)?);
        }
        return Ok(serde_json::from_value::<Program>(program.clone())?);
    }

    if let Some(code) = params.get("code") {
        if let Some(src) = code.as_str() {
            return Ok(serde_json::from_str::<Program>(src)?);
        }
        return Ok(serde_json::from_value::<Program>(code.clone())?);
    }

    Err(anyhow!("missing 'program' or 'code' parameter"))
}

fn resolve_ref_path(path: &str, values: &HashMap<String, Value>) -> Result<Value> {
    let stripped = path
        .strip_prefix('$')
        .ok_or_else(|| anyhow!("reference must start with '$': {path}"))?;
    let mut segments = stripped.split('.');
    let root = segments
        .next()
        .ok_or_else(|| anyhow!("invalid reference: {path}"))?;
    let mut current = values
        .get(root)
        .cloned()
        .ok_or_else(|| anyhow!("unknown step reference '{root}'"))?;

    for seg in segments {
        match current {
            Value::Object(obj) => {
                current = obj
                    .get(seg)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing key '{seg}' in reference '{path}'"))?;
            },
            Value::Array(items) => {
                let idx = seg
                    .parse::<usize>()
                    .map_err(|_| anyhow!("invalid index '{seg}' in reference '{path}'"))?;
                current = items
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| anyhow!("index '{idx}' out of bounds in '{path}'"))?;
            },
            _ => {
                return Err(anyhow!(
                    "cannot dereference '{seg}' in '{path}' from scalar value"
                ));
            },
        }
    }

    Ok(current)
}

fn resolve_refs(value: &Value, values: &HashMap<String, Value>) -> Result<Value> {
    match value {
        Value::String(s) if s.starts_with('$') => resolve_ref_path(s, values),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(resolve_refs(item, values)?);
            }
            Ok(Value::Array(out))
        },
        Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(k.clone(), resolve_refs(v, values)?);
            }
            Ok(Value::Object(out))
        },
        _ => Ok(value.clone()),
    }
}

fn flatten_tool_result(result: ToolsCallResult) -> Result<Value> {
    if result.is_error {
        let error_text = result
            .content
            .iter()
            .filter_map(|item| match item {
                ToolContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let msg = if error_text.trim().is_empty() {
            "MCP tool returned an error".to_string()
        } else {
            error_text
        };
        return Err(anyhow!(msg));
    }

    let text_items: Vec<&str> = result
        .content
        .iter()
        .filter_map(|item| match item {
            ToolContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    if text_items.len() == result.content.len() {
        if text_items.len() == 1 {
            if let Ok(parsed) = serde_json::from_str::<Value>(text_items[0]) {
                return Ok(parsed);
            }
            return Ok(Value::String(text_items[0].to_string()));
        }
        return Ok(serde_json::json!({ "content": text_items }));
    }

    let content_json = serde_json::to_value(&result.content)?;
    Ok(serde_json::json!({ "content": content_json }))
}

fn parse_selected_tools(params: &Value) -> HashSet<String> {
    params
        .get("selected_tools")
        .and_then(|v| serde_json::from_value::<Vec<SelectedTool>>(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|entry| format!("{}::{}", entry.server, entry.tool))
        .collect()
}

fn parse_detail_level(raw: Option<&str>) -> Result<ToolDetailLevel> {
    match raw.unwrap_or("summary").trim().to_ascii_lowercase().as_str() {
        "name" => Ok(ToolDetailLevel::Name),
        "summary" => Ok(ToolDetailLevel::Summary),
        "full" => Ok(ToolDetailLevel::Full),
        other => Err(anyhow!(
            "invalid detail_level '{other}', expected one of: name, summary, full"
        )),
    }
}

pub struct McpSearchToolsTool {
    manager: Arc<moltis_mcp::McpManager>,
}

impl McpSearchToolsTool {
    pub fn new(manager: Arc<moltis_mcp::McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl AgentTool for McpSearchToolsTool {
    fn name(&self) -> &str {
        "mcp_search_tools"
    }

    fn description(&self) -> &str {
        "Search available MCP tools by name/description and return compact tool summaries."
    }

    fn categories(&self) -> &'static [&'static str] {
        &["mcp", "code"]
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query for tool name/description"
                },
                "server": {
                    "type": "string",
                    "description": "Optional MCP server filter"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max tool summaries to return (default 25, max 200)",
                    "default": 25
                },
                "detail_level": {
                    "type": "string",
                    "description": "Summary detail level: name, summary, or full",
                    "default": "summary"
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> Result<Value> {
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let server = params.get("server").and_then(Value::as_str);
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(25)
            .clamp(1, 200);
        let detail_level =
            parse_detail_level(params.get("detail_level").and_then(Value::as_str))?;
        let tools = self
            .manager
            .search_tools(query, server, limit, detail_level)
            .await;
        Ok(serde_json::json!({ "tools": tools }))
    }
}

pub struct McpDescribeToolTool {
    manager: Arc<moltis_mcp::McpManager>,
}

impl McpDescribeToolTool {
    pub fn new(manager: Arc<moltis_mcp::McpManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl AgentTool for McpDescribeToolTool {
    fn name(&self) -> &str {
        "mcp_describe_tool"
    }

    fn description(&self) -> &str {
        "Load full schema and metadata for a specific MCP tool."
    }

    fn categories(&self) -> &'static [&'static str] {
        &["mcp", "code"]
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "server": {
                    "type": "string",
                    "description": "MCP server name"
                },
                "tool": {
                    "type": "string",
                    "description": "Tool name on the MCP server"
                }
            },
            "required": ["server", "tool"]
        })
    }

    async fn execute(&self, params: Value) -> Result<Value> {
        let server = params
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing 'server' parameter"))?;
        let tool = params
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing 'tool' parameter"))?;
        let described: McpToolDef = self
            .manager
            .describe_tool(server, tool)
            .await
            .ok_or_else(|| anyhow!("MCP tool '{tool}' not found for server '{server}'"))?;
        Ok(serde_json::json!({ "tool": described }))
    }
}

pub struct McpCodeExecTool {
    manager: Arc<moltis_mcp::McpManager>,
    max_steps: usize,
    max_tool_calls: usize,
    max_result_bytes: usize,
}

impl McpCodeExecTool {
    pub fn new(
        manager: Arc<moltis_mcp::McpManager>,
        cfg: &moltis_config::schema::McpCodeConfig,
    ) -> Self {
        Self {
            manager,
            max_steps: cfg.max_steps.max(1),
            max_tool_calls: cfg.max_tool_calls.max(1),
            max_result_bytes: cfg.max_result_bytes.max(1_024),
        }
    }
}

#[async_trait]
impl AgentTool for McpCodeExecTool {
    fn name(&self) -> &str {
        "mcp_code_exec"
    }

    fn description(&self) -> &str {
        "Execute a code-like MCP program as a single tool: resolve references between steps, call selected MCP tools, and return only the final output."
    }

    fn side_effect_class(&self) -> ToolEffectClass {
        ToolEffectClass::ExternalEffect
    }

    fn categories(&self) -> &'static [&'static str] {
        &["mcp", "code"]
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "language": {
                    "type": "string",
                    "description": "Program format. Supported: plan-json",
                    "default": "plan-json"
                },
                "program": {
                    "description": "Program object or JSON string. Alias: code.",
                    "oneOf": [
                        { "type": "object" },
                        { "type": "string" }
                    ]
                },
                "code": {
                    "description": "Alias for program object/string",
                    "oneOf": [
                        { "type": "object" },
                        { "type": "string" }
                    ]
                },
                "selected_tools": {
                    "type": "array",
                    "description": "Optional allowlist of server/tool pairs",
                    "items": {
                        "type": "object",
                        "properties": {
                            "server": { "type": "string" },
                            "tool": { "type": "string" }
                        },
                        "required": ["server", "tool"]
                    }
                },
                "max_steps": {
                    "type": "integer",
                    "description": "Optional per-run max steps (capped by config)"
                },
                "max_tool_calls": {
                    "type": "integer",
                    "description": "Optional per-run max tool calls (capped by config)"
                }
            },
            "required": []
        })
    }

    async fn execute(&self, params: Value) -> Result<Value> {
        let language = params
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("plan-json");
        if language != "plan-json" {
            return Err(anyhow!(
                "unsupported language '{language}', expected 'plan-json'"
            ));
        }

        let program = parse_program(&params)?;
        let selected_tools = parse_selected_tools(&params);

        let effective_max_steps = params
            .get("max_steps")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(self.max_steps)
            .clamp(1, self.max_steps);

        let effective_max_tool_calls = params
            .get("max_tool_calls")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(self.max_tool_calls)
            .clamp(1, self.max_tool_calls);

        if program.steps.is_empty() {
            return Err(anyhow!("program must include at least one step"));
        }

        if program.steps.len() > effective_max_steps {
            return Err(anyhow!(
                "program has {} steps, max allowed is {}",
                program.steps.len(),
                effective_max_steps
            ));
        }

        let mut outputs: HashMap<String, Value> = HashMap::new();
        let mut summaries = Vec::with_capacity(program.steps.len());
        let mut tool_calls = 0usize;

        for step in &program.steps {
            tool_calls = tool_calls.saturating_add(1);
            if tool_calls > effective_max_tool_calls {
                return Err(anyhow!(
                    "tool call budget exceeded: {} > {}",
                    tool_calls,
                    effective_max_tool_calls
                ));
            }

            if outputs.contains_key(&step.id) {
                return Err(anyhow!("duplicate step id '{}'", step.id));
            }

            let selector = format!("{}::{}", step.server, step.tool);
            if !selected_tools.is_empty() && !selected_tools.contains(&selector) {
                return Err(anyhow!(
                    "step '{}' is not in selected_tools allowlist: {}",
                    step.id,
                    selector
                ));
            }

            let resolved_args = resolve_refs(&step.arguments, &outputs)?;
            let called = self
                .manager
                .call_server_tool(&step.server, &step.tool, resolved_args)
                .await?;
            let flattened = flatten_tool_result(called)?;
            outputs.insert(step.id.clone(), flattened);
            summaries.push(serde_json::json!({
                "id": step.id,
                "server": step.server,
                "tool": step.tool,
            }));
        }

        let final_result = if let Some(return_step) = &program.return_step {
            outputs
                .get(return_step)
                .cloned()
                .ok_or_else(|| anyhow!("return_step '{}' not found", return_step))?
        } else {
            let last_id = &program
                .steps
                .last()
                .ok_or_else(|| anyhow!("program must include at least one step"))?
                .id;
            outputs
                .get(last_id)
                .cloned()
                .ok_or_else(|| anyhow!("last step '{}' result missing", last_id))?
        };

        let serialized = serde_json::to_vec(&final_result)?;
        if serialized.len() > self.max_result_bytes {
            return Err(anyhow!(
                "result exceeds max_result_bytes: {} > {}",
                serialized.len(),
                self.max_result_bytes
            ));
        }

        Ok(serde_json::json!({
            "result": final_result,
            "steps": summaries,
            "toolCalls": tool_calls,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_refs_handles_nested_paths() {
        let mut values = HashMap::new();
        values.insert(
            "a".to_string(),
            serde_json::json!({ "x": [1, {"y": "ok"}] }),
        );
        let resolved =
            resolve_refs(&serde_json::json!({ "v": "$a.x.1.y" }), &values).expect("resolve refs");
        assert_eq!(resolved["v"], "ok");
    }

    #[test]
    fn parse_program_supports_json_string() {
        let params = serde_json::json!({
            "code": "{\"steps\":[{\"id\":\"s1\",\"server\":\"x\",\"tool\":\"y\"}]}"
        });
        let parsed = parse_program(&params).expect("parse program");
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].id, "s1");
    }
}
