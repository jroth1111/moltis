use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use {
    anyhow::{Context, Result, anyhow},
    async_trait::async_trait,
    futures::future::BoxFuture,
    moltis_agents::tool_registry::{AgentTool, ToolEffectClass},
    moltis_mcp::ToolDetailLevel,
    moltis_mcp::types::{McpToolDef, ToolContent, ToolsCallResult},
    serde::{Deserialize, Serialize},
    serde_json::Value,
    sha2::{Digest, Sha256},
    sqlx::{Row, SqlitePool, sqlite::SqliteConnectOptions},
    time::OffsetDateTime,
    tokio::fs,
    tokio::sync::OnceCell,
    tokio::time::{sleep, timeout},
    tracing::info,
    tracing::warn,
    uuid::Uuid,
};

#[derive(Deserialize)]
struct SelectedTool {
    server: String,
    tool: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Program {
    #[serde(default = "default_program_version")]
    version: u8,
    steps: Vec<ProgramStep>,
    #[serde(default)]
    return_step: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProgramStep {
    id: String,
    #[serde(flatten)]
    op: ProgramOp,
    #[serde(default)]
    output: OutputShape,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum ProgramOp {
    Tool {
        server: String,
        tool: String,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        retry: Option<RetryPolicy>,
        #[serde(default)]
        idempotency_key: Option<String>,
    },
    Transform {
        #[serde(default)]
        input: Value,
        #[serde(default)]
        shape: OutputShape,
    },
    If {
        condition: ConditionExpr,
        then_steps: Vec<ProgramStep>,
        #[serde(default)]
        else_steps: Vec<ProgramStep>,
    },
    ForEach {
        items: Value,
        #[serde(default = "default_item_var")]
        item_var: String,
        #[serde(default = "default_index_var")]
        index_var: String,
        steps: Vec<ProgramStep>,
        #[serde(default)]
        collect_step: Option<String>,
        #[serde(default)]
        max_items: Option<usize>,
    },
    Retry {
        retry: RetryPolicy,
        step: Box<ProgramStep>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct OutputShape {
    #[serde(default)]
    select: Vec<String>,
    #[serde(default)]
    map: Option<OutputMap>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum OutputMap {
    Keys,
    Values,
    Entries,
    Count,
    First,
    Last,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConditionExpr {
    Truthy { value: Value },
    Equals { left: Value, right: Value },
    NotEquals { left: Value, right: Value },
    Exists { value: Value },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RetryPolicy {
    #[serde(default = "default_retry_attempts")]
    attempts: u32,
    #[serde(default = "default_retry_backoff_ms")]
    backoff_ms: u64,
    #[serde(default = "default_retry_max_backoff_ms")]
    max_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: default_retry_attempts(),
            backoff_ms: default_retry_backoff_ms(),
            max_backoff_ms: default_retry_max_backoff_ms(),
        }
    }
}

fn default_program_version() -> u8 {
    2
}

fn default_item_var() -> String {
    "item".to_string()
}

fn default_index_var() -> String {
    "index".to_string()
}

fn default_retry_attempts() -> u32 {
    2
}

fn default_retry_backoff_ms() -> u64 {
    250
}

fn default_retry_max_backoff_ms() -> u64 {
    4_000
}

fn parse_program(params: &Value) -> Result<Program> {
    let parsed = if let Some(program) = params.get("program") {
        if program.is_string() {
            let src = program
                .as_str()
                .ok_or_else(|| anyhow!("invalid 'program' parameter"))?;
            serde_json::from_str::<Program>(src)?
        } else {
            serde_json::from_value::<Program>(program.clone())?
        }
    } else if let Some(code) = params.get("code") {
        if let Some(src) = code.as_str() {
            serde_json::from_str::<Program>(src)?
        } else {
            serde_json::from_value::<Program>(code.clone())?
        }
    } else {
        return Err(anyhow!("missing 'program' or 'code' parameter"));
    };

    if parsed.version != 2 {
        return Err(anyhow!(
            "unsupported program version '{}'; expected version=2",
            parsed.version
        ));
    }

    Ok(parsed)
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
    match raw
        .unwrap_or("summary")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "name" => Ok(ToolDetailLevel::Name),
        "summary" => Ok(ToolDetailLevel::Summary),
        "full" => Ok(ToolDetailLevel::Full),
        other => Err(anyhow!(
            "invalid detail_level '{other}', expected one of: name, summary, full"
        )),
    }
}

fn normalize_selector(selector: &str) -> Option<String> {
    let trimmed = selector.trim();
    let mut parts = trimmed.split("::");
    let server = parts.next()?.trim();
    let tool = parts.next()?.trim();
    if parts.next().is_some() || server.is_empty() || tool.is_empty() {
        return None;
    }
    Some(format!("{server}::{tool}"))
}

fn redact_tokenized_text<F>(input: &str, replacement: &str, mut should_redact: F) -> String
where
    F: FnMut(&str) -> bool,
{
    let mut output = String::with_capacity(input.len());
    let mut token = String::new();

    let mut flush = |token: &mut String, output: &mut String| {
        if token.is_empty() {
            return;
        }
        if should_redact(token.as_str()) {
            output.push_str(replacement);
        } else {
            output.push_str(token);
        }
        token.clear();
    };

    for ch in input.chars() {
        if ch.is_whitespace() {
            flush(&mut token, &mut output);
            output.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush(&mut token, &mut output);

    output
}

fn is_email_like(token: &str) -> bool {
    let candidate = token.trim_matches(|c: char| c.is_ascii_punctuation());
    let mut parts = candidate.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return false;
    }
    !local.is_empty()
        && domain.contains('.')
        && domain.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

fn is_ssn_like(token: &str) -> bool {
    let candidate = token.trim_matches(|c: char| c.is_ascii_punctuation());
    let digits: String = candidate.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 9 {
        return false;
    }
    candidate.len() == 11
        && candidate.chars().nth(3) == Some('-')
        && candidate.chars().nth(6) == Some('-')
}

fn is_phone_like(token: &str) -> bool {
    let candidate = token.trim_matches(|c: char| c.is_ascii_punctuation());
    let digits = candidate.chars().filter(|c| c.is_ascii_digit()).count();
    digits >= 10
        && candidate
            .chars()
            .all(|c| c.is_ascii_digit() || c == '+' || c == '-' || c == '(' || c == ')' || c == '.')
}

fn redact_pii_text(input: &str) -> String {
    let with_ssn = redact_tokenized_text(input, "[REDACTED_SSN]", is_ssn_like);
    let with_email = redact_tokenized_text(&with_ssn, "[REDACTED_EMAIL]", is_email_like);
    redact_tokenized_text(&with_email, "[REDACTED_PHONE]", is_phone_like)
}

fn redact_pii_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_pii_text(s)),
        Value::Array(items) => Value::Array(items.iter().map(redact_pii_value).collect()),
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), redact_pii_value(v)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn code_runs_dir() -> PathBuf {
    moltis_config::data_dir().join("mcp").join("runs")
}

fn skill_programs_dir() -> PathBuf {
    moltis_config::data_dir().join("mcp").join("skills")
}

fn normalize_skill_name(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("skill name must not be empty"));
    }
    if trimmed.contains("..") || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(anyhow!("invalid skill name '{trimmed}'"));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "invalid skill name '{trimmed}': allowed chars are [A-Za-z0-9_-]"
        ));
    }
    Ok(trimmed.to_string())
}

fn now_unix_ts() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn extract_ref_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for segment in path.split('.').filter(|segment| !segment.is_empty()) {
        match current {
            Value::Object(map) => {
                current = map.get(segment)?;
            },
            Value::Array(items) => {
                let idx = segment.parse::<usize>().ok()?;
                current = items.get(idx)?;
            },
            _ => return None,
        }
    }
    Some(current)
}

fn apply_select(value: Value, select: &[String]) -> Value {
    if select.is_empty() {
        return value;
    }

    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for key in select {
                if let Some(extracted) = extract_ref_path(&Value::Object(map.clone()), key) {
                    out.insert(key.clone(), extracted.clone());
                }
            }
            Value::Object(out)
        },
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| match item {
                    Value::Object(obj) => {
                        let src = Value::Object(obj);
                        let mut out = serde_json::Map::new();
                        for key in select {
                            if let Some(extracted) = extract_ref_path(&src, key) {
                                out.insert(key.clone(), extracted.clone());
                            }
                        }
                        Value::Object(out)
                    },
                    _ => item,
                })
                .collect(),
        ),
        _ => value,
    }
}

fn apply_map(value: Value, map: OutputMap) -> Value {
    match map {
        OutputMap::Keys => match value {
            Value::Object(obj) => {
                Value::Array(obj.keys().map(|key| Value::String(key.clone())).collect())
            },
            _ => value,
        },
        OutputMap::Values => match value {
            Value::Object(obj) => Value::Array(obj.into_values().collect()),
            _ => value,
        },
        OutputMap::Entries => match value {
            Value::Object(obj) => Value::Array(
                obj.into_iter()
                    .map(|(key, value)| serde_json::json!({ "key": key, "value": value }))
                    .collect(),
            ),
            _ => value,
        },
        OutputMap::Count => match value {
            Value::Array(items) => Value::from(items.len()),
            Value::Object(obj) => Value::from(obj.len()),
            Value::String(text) => Value::from(text.chars().count()),
            _ => Value::from(0),
        },
        OutputMap::First => match value {
            Value::Array(items) => items.into_iter().next().unwrap_or(Value::Null),
            _ => value,
        },
        OutputMap::Last => match value {
            Value::Array(items) => items.into_iter().last().unwrap_or(Value::Null),
            _ => value,
        },
    }
}

fn apply_limit(value: Value, limit: usize) -> Value {
    match value {
        Value::Array(mut items) => {
            items.truncate(limit);
            Value::Array(items)
        },
        Value::Object(obj) => Value::Object(obj.into_iter().take(limit).collect()),
        Value::String(text) => Value::String(text.chars().take(limit).collect()),
        _ => value,
    }
}

fn enforce_max_bytes(mut value: Value, max_bytes: usize) -> Value {
    if serde_json::to_vec(&value)
        .map(|bytes| bytes.len() <= max_bytes)
        .unwrap_or(false)
    {
        return value;
    }

    loop {
        let size = serde_json::to_vec(&value)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        if size <= max_bytes {
            return value;
        }
        value = match value {
            Value::String(text) => {
                if text.is_empty() {
                    Value::String(text)
                } else {
                    let mut chars = text.chars();
                    chars.next_back();
                    Value::String(chars.collect())
                }
            },
            Value::Array(mut items) => {
                if items.is_empty() {
                    Value::Array(items)
                } else {
                    items.pop();
                    Value::Array(items)
                }
            },
            Value::Object(mut obj) => {
                if let Some(last) = obj.keys().last().cloned() {
                    obj.remove(&last);
                }
                Value::Object(obj)
            },
            _ => Value::Null,
        };
    }
}

fn apply_output_shape(value: Value, output: &OutputShape, configured_max_bytes: usize) -> Value {
    let mut out = apply_select(value, &output.select);
    if let Some(map) = output.map {
        out = apply_map(out, map);
    }
    if let Some(limit) = output.limit {
        out = apply_limit(out, limit);
    }
    if let Some(max_bytes) = output.max_bytes {
        out = enforce_max_bytes(out, max_bytes.min(configured_max_bytes).max(256));
    }
    out
}

fn evaluate_condition(condition: &ConditionExpr, outputs: &HashMap<String, Value>) -> Result<bool> {
    match condition {
        ConditionExpr::Truthy { value } => {
            let resolved = resolve_refs(value, outputs)?;
            Ok(match resolved {
                Value::Null => false,
                Value::Bool(flag) => flag,
                Value::Number(number) => number.as_f64().is_some_and(|v| v != 0.0),
                Value::String(text) => !text.trim().is_empty(),
                Value::Array(items) => !items.is_empty(),
                Value::Object(obj) => !obj.is_empty(),
            })
        },
        ConditionExpr::Equals { left, right } => {
            let resolved_left = resolve_refs(left, outputs)?;
            let resolved_right = resolve_refs(right, outputs)?;
            Ok(resolved_left == resolved_right)
        },
        ConditionExpr::NotEquals { left, right } => {
            let resolved_left = resolve_refs(left, outputs)?;
            let resolved_right = resolve_refs(right, outputs)?;
            Ok(resolved_left != resolved_right)
        },
        ConditionExpr::Exists { value } => match resolve_refs(value, outputs) {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        },
    }
}

fn resolve_idempotency_key(
    raw: &Option<String>,
    outputs: &HashMap<String, Value>,
) -> Result<Option<String>> {
    let Some(value) = raw else {
        return Ok(None);
    };
    if value.starts_with('$') {
        let resolved = resolve_ref_path(value, outputs)?;
        return Ok(Some(match resolved {
            Value::String(text) => text,
            other => other.to_string(),
        }));
    }
    Ok(Some(value.clone()))
}

fn program_fingerprint(program: &Program) -> Result<String> {
    let encoded = serde_json::to_vec(program)?;
    let mut hasher = Sha256::new();
    hasher.update(encoded);
    let digest = hasher.finalize();
    Ok(format!("{digest:x}"))
}

#[derive(Clone)]
struct CodeExecStore {
    db_path: PathBuf,
    pool: Arc<OnceCell<SqlitePool>>,
}

impl CodeExecStore {
    fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            pool: Arc::new(OnceCell::new()),
        }
    }

    async fn pool(&self) -> Result<&SqlitePool> {
        self.pool
            .get_or_try_init(|| async {
                if let Some(parent) = self.db_path.parent() {
                    fs::create_dir_all(parent).await?;
                }
                let options = SqliteConnectOptions::from_str(
                    format!("sqlite://{}", self.db_path.display()).as_str(),
                )?
                .create_if_missing(true);
                let pool = SqlitePool::connect_with(options).await?;
                sqlx::query(
                    r#"
                    CREATE TABLE IF NOT EXISTS mcp_code_runs (
                        run_id TEXT PRIMARY KEY,
                        created_at INTEGER NOT NULL,
                        updated_at INTEGER NOT NULL,
                        status TEXT NOT NULL,
                        program_json TEXT NOT NULL,
                        tool_calls INTEGER NOT NULL DEFAULT 0,
                        result_json TEXT,
                        error_text TEXT
                    )
                    "#,
                )
                .execute(&pool)
                .await?;
                sqlx::query(
                    r#"
                    CREATE TABLE IF NOT EXISTS mcp_code_steps (
                        run_id TEXT NOT NULL,
                        step_id TEXT NOT NULL,
                        ordinal INTEGER NOT NULL,
                        op TEXT NOT NULL,
                        server TEXT,
                        tool TEXT,
                        attempts INTEGER NOT NULL DEFAULT 0,
                        idempotency_key TEXT,
                        output_json TEXT,
                        error_text TEXT,
                        status TEXT NOT NULL,
                        updated_at INTEGER NOT NULL,
                        PRIMARY KEY(run_id, step_id)
                    )
                    "#,
                )
                .execute(&pool)
                .await?;
                sqlx::query(
                    r#"
                    CREATE TABLE IF NOT EXISTS mcp_code_patterns (
                        fingerprint TEXT PRIMARY KEY,
                        success_count INTEGER NOT NULL DEFAULT 0,
                        last_seen_at INTEGER NOT NULL,
                        sample_program_json TEXT NOT NULL,
                        promoted_skill_name TEXT
                    )
                    "#,
                )
                .execute(&pool)
                .await?;
                Ok(pool)
            })
            .await
    }

    async fn start_run(&self, run_id: &str, program: &Program) -> Result<()> {
        let pool = self.pool().await?;
        let now = now_unix_ts();
        let program_json = serde_json::to_string(program)?;
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO mcp_code_runs (run_id, created_at, updated_at, status, program_json)
            VALUES (?, ?, ?, 'running', ?)
            "#,
        )
        .bind(run_id)
        .bind(now)
        .bind(now)
        .bind(program_json)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn mark_run_done(
        &self,
        run_id: &str,
        status: &str,
        tool_calls: usize,
        payload: Option<&Value>,
        error: Option<&str>,
    ) -> Result<()> {
        let pool = self.pool().await?;
        let now = now_unix_ts();
        let result_json = payload.map(serde_json::to_string).transpose()?;
        sqlx::query(
            r#"
            UPDATE mcp_code_runs
            SET updated_at = ?, status = ?, tool_calls = ?, result_json = ?, error_text = ?
            WHERE run_id = ?
            "#,
        )
        .bind(now)
        .bind(status)
        .bind(tool_calls as i64)
        .bind(result_json)
        .bind(error)
        .bind(run_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn upsert_step(
        &self,
        run_id: &str,
        step_id: &str,
        ordinal: usize,
        op: &str,
        server: Option<&str>,
        tool: Option<&str>,
        attempts: u32,
        idempotency_key: Option<&str>,
        output: Option<&Value>,
        error: Option<&str>,
        status: &str,
    ) -> Result<()> {
        let pool = self.pool().await?;
        let updated_at = now_unix_ts();
        let output_json = output.map(serde_json::to_string).transpose()?;
        sqlx::query(
            r#"
            INSERT INTO mcp_code_steps (
                run_id, step_id, ordinal, op, server, tool, attempts, idempotency_key, output_json, error_text, status, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(run_id, step_id) DO UPDATE SET
                ordinal = excluded.ordinal,
                op = excluded.op,
                server = excluded.server,
                tool = excluded.tool,
                attempts = excluded.attempts,
                idempotency_key = excluded.idempotency_key,
                output_json = excluded.output_json,
                error_text = excluded.error_text,
                status = excluded.status,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(run_id)
        .bind(step_id)
        .bind(ordinal as i64)
        .bind(op)
        .bind(server)
        .bind(tool)
        .bind(attempts as i64)
        .bind(idempotency_key)
        .bind(output_json)
        .bind(error)
        .bind(status)
        .bind(updated_at)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn load_successful_outputs(&self, run_id: &str) -> Result<Vec<(String, usize, Value)>> {
        let pool = self.pool().await?;
        let rows = sqlx::query(
            r#"
            SELECT step_id, ordinal, output_json
            FROM mcp_code_steps
            WHERE run_id = ? AND status = 'success' AND output_json IS NOT NULL
            ORDER BY ordinal ASC
            "#,
        )
        .bind(run_id)
        .fetch_all(pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let step_id: String = row.try_get("step_id")?;
            let ordinal: i64 = row.try_get("ordinal")?;
            let output_json: String = row.try_get("output_json")?;
            let parsed: Value = serde_json::from_str(&output_json)?;
            out.push((step_id, ordinal.max(0) as usize, parsed));
        }
        Ok(out)
    }

    async fn lookup_idempotent_result(
        &self,
        run_id: &str,
        step_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<Value>> {
        let pool = self.pool().await?;
        let row = sqlx::query(
            r#"
            SELECT output_json
            FROM mcp_code_steps
            WHERE run_id = ? AND step_id = ? AND idempotency_key = ? AND status = 'success'
            LIMIT 1
            "#,
        )
        .bind(run_id)
        .bind(step_id)
        .bind(idempotency_key)
        .fetch_optional(pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let raw: Option<String> = row.try_get("output_json")?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_str(&raw)?))
    }

    async fn update_pattern(
        &self,
        fingerprint: &str,
        program: &Program,
    ) -> Result<(u32, Option<String>)> {
        let pool = self.pool().await?;
        let now = now_unix_ts();
        let program_json = serde_json::to_string(program)?;
        sqlx::query(
            r#"
            INSERT INTO mcp_code_patterns (fingerprint, success_count, last_seen_at, sample_program_json)
            VALUES (?, 1, ?, ?)
            ON CONFLICT(fingerprint) DO UPDATE SET
                success_count = mcp_code_patterns.success_count + 1,
                last_seen_at = excluded.last_seen_at
            "#,
        )
        .bind(fingerprint)
        .bind(now)
        .bind(program_json)
        .execute(pool)
        .await?;

        let row = sqlx::query(
            r#"
            SELECT success_count, promoted_skill_name
            FROM mcp_code_patterns
            WHERE fingerprint = ?
            "#,
        )
        .bind(fingerprint)
        .fetch_one(pool)
        .await?;
        let success_count: i64 = row.try_get("success_count")?;
        let promoted_skill_name: Option<String> = row.try_get("promoted_skill_name")?;
        Ok((success_count.max(0) as u32, promoted_skill_name))
    }

    async fn set_promoted_skill(&self, fingerprint: &str, skill_name: &str) -> Result<()> {
        let pool = self.pool().await?;
        sqlx::query("UPDATE mcp_code_patterns SET promoted_skill_name = ? WHERE fingerprint = ?")
            .bind(skill_name)
            .bind(fingerprint)
            .execute(pool)
            .await?;
        Ok(())
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
        let detail_level = parse_detail_level(params.get("detail_level").and_then(Value::as_str))?;
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

#[derive(Clone)]
pub struct McpCodeExecTool {
    manager: Arc<moltis_mcp::McpManager>,
    enabled: bool,
    timeout_ms: u64,
    allowed_servers: HashSet<String>,
    denied_servers: HashSet<String>,
    allowed_tools: HashSet<String>,
    denied_tools: HashSet<String>,
    redact_pii: bool,
    max_steps: usize,
    max_tool_calls: usize,
    max_result_bytes: usize,
    default_retry_attempts: u32,
    default_retry_backoff_ms: u64,
    default_retry_max_backoff_ms: u64,
    auto_promote_enabled: bool,
    auto_promote_min_successes: u32,
    auto_skill_prefix: String,
    runs_dir: PathBuf,
    store: CodeExecStore,
}

#[derive(Default)]
struct ExecutionState {
    outputs: HashMap<String, Value>,
    summaries: Vec<Value>,
    tool_calls: usize,
    step_ordinal: usize,
}

impl McpCodeExecTool {
    pub fn new(
        manager: Arc<moltis_mcp::McpManager>,
        cfg: &moltis_config::schema::McpCodeConfig,
    ) -> Self {
        let normalize_set = |items: &[String]| -> HashSet<String> {
            items
                .iter()
                .filter_map(|entry| normalize_selector(entry))
                .collect()
        };

        Self {
            manager,
            enabled: cfg.enabled,
            timeout_ms: cfg.timeout_ms.max(1_000),
            allowed_servers: cfg
                .allow_servers
                .iter()
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect(),
            denied_servers: cfg
                .deny_servers
                .iter()
                .map(|entry| entry.trim().to_string())
                .filter(|entry| !entry.is_empty())
                .collect(),
            allowed_tools: normalize_set(&cfg.allow_tools),
            denied_tools: normalize_set(&cfg.deny_tools),
            redact_pii: cfg.redact_pii,
            max_steps: cfg.max_steps.max(1),
            max_tool_calls: cfg.max_tool_calls.max(1),
            max_result_bytes: cfg.max_result_bytes.max(1_024),
            default_retry_attempts: cfg.default_retry_attempts.max(1),
            default_retry_backoff_ms: cfg.default_retry_backoff_ms.max(50),
            default_retry_max_backoff_ms: cfg.default_retry_max_backoff_ms.max(100),
            auto_promote_enabled: cfg.auto_promote_enabled,
            auto_promote_min_successes: cfg.auto_promote_min_successes.max(1),
            auto_skill_prefix: cfg.auto_skill_prefix.trim().to_string(),
            runs_dir: code_runs_dir(),
            store: CodeExecStore::new(
                moltis_config::data_dir()
                    .join("mcp")
                    .join("code_exec.sqlite"),
            ),
        }
    }

    fn is_tool_allowed_by_policy(&self, server: &str, tool: &str) -> Result<()> {
        let selector = format!("{server}::{tool}");

        if self.denied_servers.contains(server) {
            return Err(anyhow!(
                "MCP server '{server}' is blocked by mcp.code.deny_servers"
            ));
        }
        if self.denied_tools.contains(&selector) {
            return Err(anyhow!(
                "MCP tool '{selector}' is blocked by mcp.code.deny_tools"
            ));
        }
        if !self.allowed_servers.is_empty() && !self.allowed_servers.contains(server) {
            return Err(anyhow!(
                "MCP server '{server}' is not in mcp.code.allow_servers"
            ));
        }
        if !self.allowed_tools.is_empty() && !self.allowed_tools.contains(&selector) {
            return Err(anyhow!(
                "MCP tool '{selector}' is not in mcp.code.allow_tools"
            ));
        }
        Ok(())
    }

    fn op_name(op: &ProgramOp) -> &'static str {
        match op {
            ProgramOp::Tool { .. } => "tool",
            ProgramOp::Transform { .. } => "transform",
            ProgramOp::If { .. } => "if",
            ProgramOp::ForEach { .. } => "for_each",
            ProgramOp::Retry { .. } => "retry",
        }
    }

    fn retry_policy(&self, retry: Option<RetryPolicy>) -> RetryPolicy {
        let mut policy = retry.unwrap_or_default();
        if policy.attempts == default_retry_attempts()
            && policy.backoff_ms == default_retry_backoff_ms()
            && policy.max_backoff_ms == default_retry_max_backoff_ms()
        {
            policy.attempts = self.default_retry_attempts;
            policy.backoff_ms = self.default_retry_backoff_ms;
            policy.max_backoff_ms = self.default_retry_max_backoff_ms;
        }
        policy.attempts = policy.attempts.max(1);
        policy.backoff_ms = policy.backoff_ms.max(50);
        policy.max_backoff_ms = policy.max_backoff_ms.max(policy.backoff_ms);
        policy
    }

    fn prefixed_steps(prefix: &str, steps: &[ProgramStep]) -> Vec<ProgramStep> {
        steps
            .iter()
            .map(|step| {
                let mut cloned = step.clone();
                cloned.id = format!("{prefix}__{}", cloned.id);
                cloned
            })
            .collect()
    }

    fn first_tool_selector(program: &Program) -> Option<String> {
        fn walk(steps: &[ProgramStep]) -> Option<String> {
            for step in steps {
                match &step.op {
                    ProgramOp::Tool { server, tool, .. } => {
                        return Some(format!("{server}_{tool}"));
                    },
                    ProgramOp::If {
                        then_steps,
                        else_steps,
                        ..
                    } => {
                        if let Some(found) = walk(then_steps).or_else(|| walk(else_steps)) {
                            return Some(found);
                        }
                    },
                    ProgramOp::ForEach { steps, .. } => {
                        if let Some(found) = walk(steps) {
                            return Some(found);
                        }
                    },
                    ProgramOp::Retry { step, .. } => {
                        if let Some(found) = walk(std::slice::from_ref(step)) {
                            return Some(found);
                        }
                    },
                    ProgramOp::Transform { .. } => {},
                }
            }
            None
        }
        walk(&program.steps)
    }

    async fn maybe_promote_skill(&self, program: &Program) -> Result<Option<String>> {
        if !self.auto_promote_enabled {
            return Ok(None);
        }

        let fingerprint = program_fingerprint(program)?;
        let (success_count, existing) = self.store.update_pattern(&fingerprint, program).await?;
        if existing.is_some() || success_count < self.auto_promote_min_successes {
            return Ok(existing);
        }

        let selector = Self::first_tool_selector(program).unwrap_or_else(|| "workflow".to_string());
        let prefix = if self.auto_skill_prefix.is_empty() {
            "auto".to_string()
        } else {
            self.auto_skill_prefix.clone()
        };
        let short_hash = &fingerprint[..12];
        let candidate = normalize_skill_name(&format!("{prefix}_{selector}_{short_hash}"))?;
        let path = skill_programs_dir().join(format!("{candidate}.json"));
        fs::create_dir_all(skill_programs_dir()).await?;
        fs::write(&path, serde_json::to_vec_pretty(program)?).await?;
        self.store
            .set_promoted_skill(&fingerprint, &candidate)
            .await?;
        info!(skill = %candidate, "auto-promoted MCP skill");
        Ok(Some(candidate))
    }

    async fn execute_tool_with_retry(
        &self,
        run_id: &str,
        step_id: &str,
        server: &str,
        tool: &str,
        arguments: Value,
        retry: RetryPolicy,
        idempotency_key: Option<String>,
    ) -> Result<(Value, u32, bool)> {
        if let Some(ref key) = idempotency_key
            && let Some(cached) = self
                .store
                .lookup_idempotent_result(run_id, step_id, key)
                .await?
        {
            return Ok((cached, 0, true));
        }

        let mut attempt = 0u32;
        let mut backoff = retry.backoff_ms;
        let mut last_error: Option<anyhow::Error> = None;
        while attempt < retry.attempts {
            attempt += 1;
            let called = self
                .manager
                .call_server_tool(server, tool, arguments.clone())
                .await;
            match called {
                Ok(called) => match flatten_tool_result(called) {
                    Ok(flattened) => {
                        self.manager.record_tool_outcome(server, tool, true).await;
                        return Ok((flattened, attempt, false));
                    },
                    Err(error) => {
                        self.manager.record_tool_outcome(server, tool, false).await;
                        last_error = Some(error);
                    },
                },
                Err(error) => {
                    self.manager.record_tool_outcome(server, tool, false).await;
                    last_error = Some(error.into());
                },
            }
            if attempt < retry.attempts {
                sleep(Duration::from_millis(backoff)).await;
                backoff = backoff
                    .saturating_mul(2)
                    .min(retry.max_backoff_ms.max(backoff));
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow!("tool retry exhausted")))
    }

    fn step_summary(step: &ProgramStep, status: &str, attempts: u32, reused: bool) -> Value {
        serde_json::json!({
            "id": step.id,
            "op": Self::op_name(&step.op),
            "status": status,
            "attempts": attempts,
            "reusedIdempotentResult": reused,
        })
    }

    fn execute_step<'a>(
        &'a self,
        run_id: &'a str,
        step: &'a ProgramStep,
        selected_tools: &'a HashSet<String>,
        effective_max_steps: usize,
        effective_max_tool_calls: usize,
        state: &'a mut ExecutionState,
    ) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            if state.outputs.contains_key(&step.id) {
                let existing = state
                    .outputs
                    .get(&step.id)
                    .cloned()
                    .ok_or_else(|| anyhow!("missing cached step output '{}'", step.id))?;
                state
                    .summaries
                    .push(Self::step_summary(step, "resumed", 0, false));
                return Ok(existing);
            }

            state.step_ordinal = state.step_ordinal.saturating_add(1);
            if state.step_ordinal > effective_max_steps {
                return Err(anyhow!(
                    "step budget exceeded: {} > {}",
                    state.step_ordinal,
                    effective_max_steps
                ));
            }

            let raw_result = match &step.op {
                ProgramOp::Tool {
                    server,
                    tool,
                    arguments,
                    retry,
                    idempotency_key,
                } => {
                    state.tool_calls = state.tool_calls.saturating_add(1);
                    if state.tool_calls > effective_max_tool_calls {
                        return Err(anyhow!(
                            "tool call budget exceeded: {} > {}",
                            state.tool_calls,
                            effective_max_tool_calls
                        ));
                    }
                    self.is_tool_allowed_by_policy(server, tool)?;
                    let selector = format!("{server}::{tool}");
                    if !selected_tools.is_empty() && !selected_tools.contains(&selector) {
                        return Err(anyhow!(
                            "step '{}' is not in selected_tools allowlist: {}",
                            step.id,
                            selector
                        ));
                    }
                    let resolved_args = resolve_refs(arguments, &state.outputs)?;
                    let resolved_idempotency =
                        resolve_idempotency_key(idempotency_key, &state.outputs)?;
                    let (value, attempts, reused) = self
                        .execute_tool_with_retry(
                            run_id,
                            &step.id,
                            server,
                            tool,
                            resolved_args,
                            self.retry_policy(retry.clone()),
                            resolved_idempotency.clone(),
                        )
                        .await
                        .with_context(|| format!("tool step '{}' failed", step.id))?;
                    self.store
                        .upsert_step(
                            run_id,
                            &step.id,
                            state.step_ordinal,
                            "tool",
                            Some(server),
                            Some(tool),
                            attempts,
                            resolved_idempotency.as_deref(),
                            Some(&value),
                            None,
                            "success",
                        )
                        .await?;
                    state
                        .summaries
                        .push(Self::step_summary(step, "success", attempts, reused));
                    value
                },
                ProgramOp::Transform { input, shape } => {
                    let resolved_input = resolve_refs(input, &state.outputs)?;
                    let transformed =
                        apply_output_shape(resolved_input, shape, self.max_result_bytes);
                    self.store
                        .upsert_step(
                            run_id,
                            &step.id,
                            state.step_ordinal,
                            "transform",
                            None,
                            None,
                            1,
                            None,
                            Some(&transformed),
                            None,
                            "success",
                        )
                        .await?;
                    state
                        .summaries
                        .push(Self::step_summary(step, "success", 1, false));
                    transformed
                },
                ProgramOp::If {
                    condition,
                    then_steps,
                    else_steps,
                } => {
                    let predicate = evaluate_condition(condition, &state.outputs)?;
                    let branch = if predicate {
                        then_steps
                    } else {
                        else_steps
                    };
                    let branch_steps = Self::prefixed_steps(&step.id, branch);
                    for nested in &branch_steps {
                        self.execute_step(
                            run_id,
                            nested,
                            selected_tools,
                            effective_max_steps,
                            effective_max_tool_calls,
                            state,
                        )
                        .await?;
                    }
                    let branch_result = branch_steps
                        .last()
                        .and_then(|nested| state.outputs.get(&nested.id))
                        .cloned()
                        .unwrap_or(Value::Null);
                    self.store
                        .upsert_step(
                            run_id,
                            &step.id,
                            state.step_ordinal,
                            "if",
                            None,
                            None,
                            1,
                            None,
                            Some(&branch_result),
                            None,
                            "success",
                        )
                        .await?;
                    state.summaries.push(serde_json::json!({
                        "id": step.id,
                        "op": "if",
                        "status": "success",
                        "branch": if predicate { "then" } else { "else" },
                    }));
                    branch_result
                },
                ProgramOp::ForEach {
                    items,
                    item_var,
                    index_var,
                    steps,
                    collect_step,
                    max_items,
                } => {
                    let resolved_items = resolve_refs(items, &state.outputs)?;
                    let Value::Array(mut items) = resolved_items else {
                        return Err(anyhow!("for_each step '{}' expected array items", step.id));
                    };
                    if let Some(limit) = max_items {
                        items.truncate(*limit);
                    }
                    let mut collected = Vec::with_capacity(items.len());
                    for (index, item) in items.into_iter().enumerate() {
                        state.outputs.insert(item_var.clone(), item);
                        state
                            .outputs
                            .insert(index_var.clone(), Value::from(index as u64));
                        let branch_prefix = format!("{}__{}", step.id, index);
                        let loop_steps = Self::prefixed_steps(&branch_prefix, steps);
                        for nested in &loop_steps {
                            self.execute_step(
                                run_id,
                                nested,
                                selected_tools,
                                effective_max_steps,
                                effective_max_tool_calls,
                                state,
                            )
                            .await?;
                        }
                        let chosen = if let Some(collect_name) = collect_step {
                            state
                                .outputs
                                .get(&format!("{branch_prefix}__{collect_name}"))
                                .cloned()
                        } else {
                            loop_steps
                                .last()
                                .and_then(|nested| state.outputs.get(&nested.id))
                                .cloned()
                        };
                        if let Some(value) = chosen {
                            collected.push(value);
                        }
                        state.outputs.remove(item_var);
                        state.outputs.remove(index_var);
                    }
                    let result = Value::Array(collected);
                    self.store
                        .upsert_step(
                            run_id,
                            &step.id,
                            state.step_ordinal,
                            "for_each",
                            None,
                            None,
                            1,
                            None,
                            Some(&result),
                            None,
                            "success",
                        )
                        .await?;
                    state
                        .summaries
                        .push(Self::step_summary(step, "success", 1, false));
                    result
                },
                ProgramOp::Retry {
                    retry,
                    step: wrapped,
                } => {
                    let mut cloned = (**wrapped).clone();
                    cloned.id = step.id.clone();
                    match &mut cloned.op {
                        ProgramOp::Tool {
                            retry: existing_retry,
                            ..
                        } => {
                            *existing_retry = Some(retry.clone());
                        },
                        _ => {},
                    }
                    self.execute_step(
                        run_id,
                        &cloned,
                        selected_tools,
                        effective_max_steps,
                        effective_max_tool_calls,
                        state,
                    )
                    .await?
                },
            };

            let shaped_result = apply_output_shape(raw_result, &step.output, self.max_result_bytes);
            state.outputs.insert(step.id.clone(), shaped_result.clone());
            Ok(shaped_result)
        })
    }

    async fn execute_program(
        &self,
        run_id: &str,
        program: &Program,
        selected_tools: &HashSet<String>,
        effective_max_steps: usize,
        effective_max_tool_calls: usize,
        resume_from_step: Option<&str>,
    ) -> Result<(Value, Vec<Value>, usize)> {
        if program.steps.is_empty() {
            return Err(anyhow!("program must include at least one step"));
        }

        let mut state = ExecutionState::default();
        let persisted = self.store.load_successful_outputs(run_id).await?;
        if !persisted.is_empty() {
            let resume_threshold = if let Some(from_step) = resume_from_step {
                let maybe_ordinal = persisted
                    .iter()
                    .find_map(|(step_id, ordinal, _)| (step_id == from_step).then_some(*ordinal));
                Some(maybe_ordinal.ok_or_else(|| {
                    anyhow!("from_step '{}' not found in run '{}'", from_step, run_id)
                })?)
            } else {
                None
            };
            for (step_id, ordinal, value) in persisted {
                if resume_threshold.is_some_and(|threshold| ordinal >= threshold) {
                    continue;
                }
                state.step_ordinal = state.step_ordinal.max(ordinal);
                state.outputs.insert(step_id, value);
            }
        }

        for step in &program.steps {
            if let Err(error) = self
                .execute_step(
                    run_id,
                    step,
                    selected_tools,
                    effective_max_steps,
                    effective_max_tool_calls,
                    &mut state,
                )
                .await
            {
                let error_string = error.to_string();
                let _ = self
                    .store
                    .upsert_step(
                        run_id,
                        &step.id,
                        state.step_ordinal.saturating_add(1),
                        Self::op_name(&step.op),
                        None,
                        None,
                        1,
                        None,
                        None,
                        Some(error_string.as_str()),
                        "failed",
                    )
                    .await;
                return Err(error);
            }
        }

        let mut final_result = if let Some(return_step) = &program.return_step {
            state
                .outputs
                .get(return_step)
                .cloned()
                .ok_or_else(|| anyhow!("return_step '{}' not found", return_step))?
        } else {
            let last_id = &program
                .steps
                .last()
                .ok_or_else(|| anyhow!("program must include at least one step"))?
                .id;
            state
                .outputs
                .get(last_id)
                .cloned()
                .ok_or_else(|| anyhow!("last step '{}' result missing", last_id))?
        };

        if self.redact_pii {
            final_result = redact_pii_value(&final_result);
        }

        let serialized = serde_json::to_vec(&final_result)?;
        if serialized.len() > self.max_result_bytes {
            return Err(anyhow!(
                "result exceeds max_result_bytes: {} > {}",
                serialized.len(),
                self.max_result_bytes
            ));
        }

        Ok((final_result, state.summaries, state.tool_calls))
    }

    async fn persist_run_artifact(
        &self,
        run_id: &str,
        program: &Program,
        summaries: &[Value],
        tool_calls: usize,
        final_result: &Value,
        promoted_skill: Option<&str>,
    ) -> Result<PathBuf> {
        fs::create_dir_all(&self.runs_dir).await?;
        let path = self.runs_dir.join(format!("run-{run_id}.json"));
        let payload = serde_json::json!({
            "runId": run_id,
            "program": program,
            "steps": summaries,
            "toolCalls": tool_calls,
            "result": final_result,
            "promotedSkill": promoted_skill,
        });
        let encoded = serde_json::to_vec_pretty(&payload)?;
        fs::write(&path, encoded).await?;
        Ok(path)
    }

    pub(crate) async fn run(&self, params: Value) -> Result<Value> {
        let language = params
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("plan-json");
        if language != "plan-json" {
            return Err(anyhow!(
                "unsupported language '{language}', expected 'plan-json'"
            ));
        }

        if !self.enabled {
            return Err(anyhow!(
                "mcp_code_exec is disabled by config (mcp.code.enabled = false)"
            ));
        }

        let program = parse_program(&params)?;
        let selected_tools = parse_selected_tools(&params);

        let run_id = params
            .get("run_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let from_step = params.get("from_step").and_then(Value::as_str);

        self.store.start_run(&run_id, &program).await?;

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

        let timeout_duration = Duration::from_millis(self.timeout_ms);
        let execution = timeout(
            timeout_duration,
            self.execute_program(
                &run_id,
                &program,
                &selected_tools,
                effective_max_steps,
                effective_max_tool_calls,
                from_step,
            ),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "mcp_code_exec timed out after {} ms",
                timeout_duration.as_millis()
            )
        });

        let (final_result, summaries, tool_calls) = match execution {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                let error_string = error.to_string();
                let _ = self
                    .store
                    .mark_run_done(&run_id, "failed", 0, None, Some(error_string.as_str()))
                    .await;
                return Err(error);
            },
            Err(error) => {
                let error_string = error.to_string();
                let _ = self
                    .store
                    .mark_run_done(
                        &run_id,
                        "timed_out",
                        0,
                        None,
                        Some(error_string.as_str()),
                    )
                    .await;
                return Err(error);
            },
        };

        let promoted_skill = self.maybe_promote_skill(&program).await?;
        self.store
            .mark_run_done(&run_id, "success", tool_calls, Some(&final_result), None)
            .await?;

        if let Err(error) = self
            .persist_run_artifact(
                &run_id,
                &program,
                &summaries,
                tool_calls,
                &final_result,
                promoted_skill.as_deref(),
            )
            .await
        {
            warn!(%error, "failed to persist MCP code run artifact");
        }

        Ok(serde_json::json!({
            "runId": run_id,
            "result": final_result,
            "steps": summaries,
            "toolCalls": tool_calls,
            "promotedSkill": promoted_skill,
        }))
    }
}

#[async_trait]
impl AgentTool for McpCodeExecTool {
    fn name(&self) -> &str {
        "mcp_code_exec"
    }

    fn description(&self) -> &str {
        "Execute MCP workflow programs (v2) with control flow, retries, output shaping, run resume, and persisted artifacts."
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
                    "description": "Program object or JSON string (v2 schema with op-typed steps). Alias: code.",
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
                "run_id": {
                    "type": "string",
                    "description": "Optional explicit run id for resume-aware execution."
                },
                "from_step": {
                    "type": "string",
                    "description": "Optional step id to restart from within an existing run id."
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
        self.run(params).await
    }
}

pub struct McpSkillRunTool {
    code_exec: McpCodeExecTool,
}

impl McpSkillRunTool {
    pub fn new(code_exec: McpCodeExecTool) -> Self {
        Self { code_exec }
    }
}

#[async_trait]
impl AgentTool for McpSkillRunTool {
    fn name(&self) -> &str {
        "mcp_skill_run"
    }

    fn description(&self) -> &str {
        "Run a saved MCP skill program from ~/.moltis/mcp/skills/<name>.json through mcp_code_exec. Use this first for repeat tasks, then fall back to mcp_code_exec planning."
    }

    fn side_effect_class(&self) -> ToolEffectClass {
        ToolEffectClass::ExternalEffect
    }

    fn categories(&self) -> &'static [&'static str] {
        &["mcp", "code", "skill"]
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill file name from ~/.moltis/mcp/skills/<name>.json"
                },
                "selected_tools": {
                    "type": "array",
                    "description": "Optional allowlist override for this run",
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
                },
                "run_id": {
                    "type": "string",
                    "description": "Optional explicit run id used by the underlying mcp_code_exec call."
                },
                "from_step": {
                    "type": "string",
                    "description": "Optional resume step for the underlying mcp_code_exec call."
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, params: Value) -> Result<Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing 'name' parameter"))?;
        let normalized = normalize_skill_name(name)?;
        let path = skill_programs_dir().join(format!("{normalized}.json"));
        let content = fs::read_to_string(&path)
            .await
            .map_err(|error| anyhow!("failed to load skill '{}': {error}", path.display()))?;
        let program: Value = serde_json::from_str(&content)
            .map_err(|error| anyhow!("skill '{}' is not valid JSON: {error}", path.display()))?;

        let mut exec_params = serde_json::Map::new();
        exec_params.insert("program".to_string(), program);
        if let Some(value) = params.get("selected_tools").cloned() {
            exec_params.insert("selected_tools".to_string(), value);
        }
        if let Some(value) = params.get("max_steps").cloned() {
            exec_params.insert("max_steps".to_string(), value);
        }
        if let Some(value) = params.get("max_tool_calls").cloned() {
            exec_params.insert("max_tool_calls".to_string(), value);
        }
        if let Some(value) = params.get("run_id").cloned() {
            exec_params.insert("run_id".to_string(), value);
        }
        if let Some(value) = params.get("from_step").cloned() {
            exec_params.insert("from_step".to_string(), value);
        }
        self.code_exec.run(Value::Object(exec_params)).await
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
            "code": "{\"version\":2,\"steps\":[{\"id\":\"s1\",\"op\":\"tool\",\"server\":\"x\",\"tool\":\"y\"}]}"
        });
        let parsed = parse_program(&params).expect("parse program");
        assert_eq!(parsed.steps.len(), 1);
        assert_eq!(parsed.steps[0].id, "s1");
    }

    #[test]
    fn parse_program_rejects_legacy_version() {
        let params = serde_json::json!({
            "program": {
                "version": 1,
                "steps": []
            }
        });
        let err = parse_program(&params).expect_err("legacy version should fail");
        assert!(err.to_string().contains("expected version=2"));
    }

    #[test]
    fn normalize_selector_requires_server_and_tool() {
        assert_eq!(
            normalize_selector("filesystem::read_file"),
            Some("filesystem::read_file".to_string())
        );
        assert_eq!(normalize_selector("filesystem"), None);
        assert_eq!(normalize_selector("filesystem::"), None);
        assert_eq!(normalize_selector("::read_file"), None);
    }

    #[test]
    fn redact_pii_text_masks_common_patterns() {
        let input = "email alice@example.com phone +1-415-555-1212 ssn 123-45-6789";
        let redacted = redact_pii_text(input);
        assert!(!redacted.contains("alice@example.com"));
        assert!(!redacted.contains("+1-415-555-1212"));
        assert!(!redacted.contains("123-45-6789"));
        assert!(redacted.contains("[REDACTED_EMAIL]"));
        assert!(redacted.contains("[REDACTED_PHONE]"));
        assert!(redacted.contains("[REDACTED_SSN]"));
    }

    #[test]
    fn redact_pii_value_masks_nested_strings() {
        let input = serde_json::json!({
            "primary": "alice@example.com",
            "nested": ["123-45-6789", {"phone": "(415)5551212"}],
        });
        let redacted = redact_pii_value(&input);
        let rendered = serde_json::to_string(&redacted).expect("serialize");
        assert!(!rendered.contains("alice@example.com"));
        assert!(!rendered.contains("123-45-6789"));
        assert!(!rendered.contains("(415)5551212"));
    }

    #[test]
    fn normalize_skill_name_rejects_path_traversal() {
        assert!(normalize_skill_name("daily_sync").is_ok());
        assert!(normalize_skill_name("../daily_sync").is_err());
        assert!(normalize_skill_name("nested/path").is_err());
    }

    #[test]
    fn output_shape_select_map_limit_applies() {
        let shaped = apply_output_shape(
            serde_json::json!({
                "a": 1,
                "b": 2,
                "c": 3
            }),
            &OutputShape {
                select: vec!["a".to_string(), "c".to_string()],
                map: Some(OutputMap::Values),
                limit: Some(1),
                max_bytes: None,
            },
            1_024,
        );
        assert_eq!(shaped, serde_json::json!([1]));
    }

    #[test]
    fn evaluate_condition_supports_equals_and_exists() {
        let mut outputs = HashMap::new();
        outputs.insert("step1".to_string(), serde_json::json!({"ok": true}));

        let equals = ConditionExpr::Equals {
            left: serde_json::json!("$step1.ok"),
            right: Value::Bool(true),
        };
        assert!(evaluate_condition(&equals, &outputs).expect("condition should evaluate"));

        let exists = ConditionExpr::Exists {
            value: serde_json::json!("$step1.ok"),
        };
        assert!(evaluate_condition(&exists, &outputs).expect("exists should evaluate"));
    }
}
