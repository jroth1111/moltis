use {
    crate::tool_registry::ToolRegistry,
    moltis_config::{AgentIdentity, DEFAULT_SOUL, PromptBudgetsConfig, UserProfile},
    moltis_skills::types::SkillMetadata,
    std::collections::{HashMap, HashSet},
    tracing::warn,
};

// ── Model family detection ──────────────────────────────────────────────────

/// Broad model family classification, used to tune text-based tool prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Llama,
    Qwen,
    Mistral,
    DeepSeek,
    Gemma,
    Phi,
    Unknown,
}

impl ModelFamily {
    /// Detect the model family from a model identifier string.
    #[must_use]
    pub fn from_model_id(id: &str) -> Self {
        let lower = id.to_ascii_lowercase();
        if lower.contains("llama") {
            Self::Llama
        } else if lower.contains("qwen") {
            Self::Qwen
        } else if lower.contains("mistral") || lower.contains("mixtral") {
            Self::Mistral
        } else if lower.contains("deepseek") {
            Self::DeepSeek
        } else if lower.contains("gemma") {
            Self::Gemma
        } else if lower.contains("phi") {
            Self::Phi
        } else {
            Self::Unknown
        }
    }
}

/// Runtime context for the host process running the current agent turn.
#[derive(Debug, Clone, Default)]
pub struct PromptHostRuntimeContext {
    pub host: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub shell: Option<String>,
    /// Current datetime string for prompt context, localized when timezone is known.
    pub time: Option<String>,
    /// Current date string (`YYYY-MM-DD`) for prompt context.
    pub today: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub session_key: Option<String>,
    /// Runtime surface the assistant is currently operating in
    /// (for example: "web", "telegram", "discord", "cron", "heartbeat").
    pub surface: Option<String>,
    /// High-level session kind (`web`, `channel`, `cron`).
    pub session_kind: Option<String>,
    /// Active channel type when running in a channel-bound session.
    pub channel_type: Option<String>,
    /// Active channel account identifier when running in a channel-bound session.
    pub channel_account_id: Option<String>,
    /// Active channel chat/recipient ID when running in a channel-bound session.
    pub channel_chat_id: Option<String>,
    /// Best-effort channel chat type (for example `private`, `group`, `channel`).
    pub channel_chat_type: Option<String>,
    /// Persistent Moltis workspace root (`data_dir`), e.g. `~/.moltis`
    /// or `/home/moltis/.moltis` in containerized deploys.
    pub data_dir: Option<String>,
    pub sudo_non_interactive: Option<bool>,
    pub sudo_status: Option<String>,
    pub timezone: Option<String>,
    pub accept_language: Option<String>,
    pub remote_ip: Option<String>,
    /// `"lat,lon"` (e.g. `"48.8566,2.3522"`) from browser geolocation or `USER.md`.
    pub location: Option<String>,
}

/// Runtime context for sandbox execution routing used by the `exec` tool.
#[derive(Debug, Clone, Default)]
pub struct PromptSandboxRuntimeContext {
    pub exec_sandboxed: bool,
    pub mode: Option<String>,
    pub backend: Option<String>,
    pub scope: Option<String>,
    pub image: Option<String>,
    /// Sandbox HOME directory used for `~` and relative paths in `exec`.
    pub home: Option<String>,
    pub workspace_mount: Option<String>,
    /// Mounted workspace/data path inside sandbox when available.
    pub workspace_path: Option<String>,
    pub no_network: Option<bool>,
    /// Per-session override for sandbox enablement.
    pub session_override: Option<bool>,
}

/// Combined runtime context injected into the system prompt.
#[derive(Debug, Clone, Default)]
pub struct PromptRuntimeContext {
    pub host: PromptHostRuntimeContext,
    pub sandbox: Option<PromptSandboxRuntimeContext>,
}

/// Section-level truncation metadata for system-prompt assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSectionTruncation {
    pub section: String,
    pub max_chars: usize,
    pub original_chars: usize,
}

/// Metadata for markdown sections dropped during budget enforcement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedPromptSection {
    pub section: String,
    pub section_id: String,
    pub bucket: String,
    pub reason: String,
    pub original_chars: usize,
    pub max_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoulRoutingIssueCode {
    InvalidSoulMarker,
    OrphanSoulMarker,
    DuplicateSoulSection,
    SectionPlacementMismatch,
}

impl SoulRoutingIssueCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidSoulMarker => "invalid_soul_marker",
            Self::OrphanSoulMarker => "orphan_soul_marker",
            Self::DuplicateSoulSection => "duplicate_soul_section",
            Self::SectionPlacementMismatch => "section_placement_mismatch",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoulRoutingIssue {
    pub code: SoulRoutingIssueCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SoulRoutingDiagnostics {
    pub issues: Vec<SoulRoutingIssue>,
}

impl SoulRoutingDiagnostics {
    fn push(&mut self, code: SoulRoutingIssueCode, message: impl Into<String>) {
        self.issues.push(SoulRoutingIssue {
            code,
            message: message.into(),
        });
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }

    #[must_use]
    pub fn joined_messages(&self) -> String {
        self.issues
            .iter()
            .map(|issue| issue.message.clone())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

impl std::fmt::Display for SoulRoutingDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.joined_messages())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PriorityBucket {
    SafetyBoundaries,
    IdentityConstraints,
    ExecutionPolicy,
    ToolingRules,
    StyleVibe,
}

impl PriorityBucket {
    fn as_str(self) -> &'static str {
        match self {
            Self::SafetyBoundaries => "safety_boundaries",
            Self::IdentityConstraints => "identity_constraints",
            Self::ExecutionPolicy => "execution_policy",
            Self::ToolingRules => "tooling_rules",
            Self::StyleVibe => "style_vibe",
        }
    }
}

/// Suffix appended to the system prompt when the user's reply medium is voice.
///
/// Instructs the LLM to produce speech-friendly output: no raw URLs, no markdown
/// formatting, concise conversational prose. This is Layer 1 of the voice-friendly
/// response pipeline; Layer 2 (`sanitize_text_for_tts`) catches anything the model
/// misses.
pub const VOICE_REPLY_SUFFIX: &str = "\n\n\
## Voice Reply Mode\n\n\
The user is speaking to you via voice messages. Their messages are transcribed from \
speech-to-text, so treat this as a spoken conversation. You will hear their words as \
text, and your response will be converted to spoken audio for them.\n\n\
Write for speech, not for reading:\n\
- Use natural, conversational sentences. No bullet lists, numbered lists, or headings.\n\
- NEVER include raw URLs. Instead describe the resource by name \
(e.g. \"the Rust documentation website\" instead of \"https://doc.rust-lang.org\").\n\
- No markdown formatting: no bold, italic, headers, code fences, or inline backticks.\n\
- Spell out abbreviations that a text-to-speech engine might mispronounce \
(e.g. \"API\" → \"A-P-I\", \"CLI\" → \"C-L-I\").\n\
- Keep responses concise — two to three short paragraphs at most.\n\
- Use complete sentences and natural transitions between ideas.\n";

/// One-shot prompt appended on the first real post-onboarding chat turn.
pub const FIRST_CHAT_IDENTITY_PROMPT: &str = "\n\n\
## First Chat Identity Moment\n\n\
This is your first real conversation after onboarding. Offer a brief identity refinement moment:\n\
- Ask whether the user wants to refine your personality, tone, or vibe now.\n\
- If they provide refinements, persist them using `memory_save` to `SOUL.md` and/or `IDENTITY.md`.\n\
- Keep this short and conversational.\n\
- If the user declines, continue normally without repeating this prompt in future turns.\n";

/// Build the system prompt for an agent run, including available tools.
///
/// When `native_tools` is true, tool schemas are sent via the API's native
/// tool-calling mechanism (e.g. OpenAI function calling, Anthropic tool_use).
/// When false, tools are described in the prompt itself and the LLM is
/// instructed to emit tool calls as JSON blocks that the runner can parse.
pub fn build_system_prompt(
    tools: &ToolRegistry,
    native_tools: bool,
    project_context: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    build_system_prompt_with_session_runtime(
        tools,
        native_tools,
        project_context,
        &[],
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

/// Build the system prompt with explicit runtime context.
pub fn build_system_prompt_with_session_runtime(
    tools: &ToolRegistry,
    native_tools: bool,
    project_context: Option<&str>,
    skills: &[SkillMetadata],
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    runtime_context: Option<&PromptRuntimeContext>,
    memory_text: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    build_system_prompt_with_session_runtime_workspace(
        tools,
        native_tools,
        project_context,
        skills,
        identity,
        user,
        soul_text,
        agents_text,
        tools_text,
        None,
        runtime_context,
        memory_text,
    )
}

/// Build the system prompt with explicit runtime context and optional
/// workspace heartbeat guidance text.
pub fn build_system_prompt_with_session_runtime_workspace(
    tools: &ToolRegistry,
    native_tools: bool,
    project_context: Option<&str>,
    skills: &[SkillMetadata],
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    runtime_context: Option<&PromptRuntimeContext>,
    memory_text: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    build_system_prompt_with_session_runtime_workspace_budgets(
        tools,
        native_tools,
        project_context,
        skills,
        identity,
        user,
        soul_text,
        agents_text,
        tools_text,
        heartbeat_text,
        None,
        runtime_context,
        memory_text,
    )
}

/// Build the system prompt with explicit runtime context, optional workspace
/// heartbeat guidance text, and configurable section budgets.
pub fn build_system_prompt_with_session_runtime_workspace_budgets(
    tools: &ToolRegistry,
    native_tools: bool,
    project_context: Option<&str>,
    skills: &[SkillMetadata],
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    prompt_budgets: Option<&PromptBudgetsConfig>,
    runtime_context: Option<&PromptRuntimeContext>,
    memory_text: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    let budgets = prompt_budgets.cloned().unwrap_or_default();
    build_system_prompt_full(
        tools,
        native_tools,
        project_context,
        skills,
        identity,
        user,
        soul_text,
        agents_text,
        tools_text,
        heartbeat_text,
        &budgets,
        runtime_context,
        true, // include_tools
        memory_text,
    )
}

/// Build a minimal system prompt with explicit runtime context.
pub fn build_system_prompt_minimal_runtime(
    project_context: Option<&str>,
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    runtime_context: Option<&PromptRuntimeContext>,
    memory_text: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    build_system_prompt_minimal_runtime_workspace(
        project_context,
        identity,
        user,
        soul_text,
        agents_text,
        tools_text,
        None,
        runtime_context,
        memory_text,
    )
}

/// Build a minimal system prompt with explicit runtime context and optional
/// workspace heartbeat guidance text.
pub fn build_system_prompt_minimal_runtime_workspace(
    project_context: Option<&str>,
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    runtime_context: Option<&PromptRuntimeContext>,
    memory_text: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    build_system_prompt_minimal_runtime_workspace_budgets(
        project_context,
        identity,
        user,
        soul_text,
        agents_text,
        tools_text,
        heartbeat_text,
        None,
        runtime_context,
        memory_text,
    )
}

/// Build a minimal system prompt with explicit runtime context, optional
/// workspace heartbeat guidance text, and configurable section budgets.
pub fn build_system_prompt_minimal_runtime_workspace_budgets(
    project_context: Option<&str>,
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    prompt_budgets: Option<&PromptBudgetsConfig>,
    runtime_context: Option<&PromptRuntimeContext>,
    memory_text: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    let budgets = prompt_budgets.cloned().unwrap_or_default();
    build_system_prompt_full(
        &ToolRegistry::new(),
        true,
        project_context,
        &[],
        identity,
        user,
        soul_text,
        agents_text,
        tools_text,
        heartbeat_text,
        &budgets,
        runtime_context,
        false, // include_tools
        memory_text,
    )
}

const EXEC_ROUTING_GUIDANCE: &str = "Execution routing:\n\
- `exec` runs inside sandbox when `Sandbox(exec): enabled=true`.\n\
- When sandbox is disabled, `exec` runs on the host and may require approval.\n\
- In sandbox mode, `~` and relative paths resolve under `Sandbox(exec): home=...` (usually `/home/sandbox`).\n\
- Persistent workspace files live under `Host: data_dir=...`; when mounted, the same path appears as `Sandbox(exec): workspace_path=...`.\n\
- `Host: sudo_non_interactive=true` means non-interactive sudo is available.\n\
- Sandbox/host routing changes are expected runtime behavior. Do not frame them as surprising or anomalous.\n\n";
/// Build model-family-aware tool call guidance for text-based tool mode.
fn tool_call_guidance(model_id: Option<&str>) -> String {
    let _family = model_id
        .map(ModelFamily::from_model_id)
        .unwrap_or(ModelFamily::Unknown);

    let mut g = String::with_capacity(800);
    g.push_str("## How to call tools\n\n");
    g.push_str("When you need to use a tool, output EXACTLY this fenced block:\n\n");
    g.push_str("```tool_call\n");
    g.push_str("{\"tool\": \"<tool_name>\", \"arguments\": {<arguments>}}\n");
    g.push_str("```\n\n");
    g.push_str("**Rules:**\n");
    g.push_str("- The JSON must be valid. No comments, no trailing commas.\n");
    g.push_str("- One tool call per fenced block. You may include multiple blocks.\n");
    g.push_str("- Wait for the tool result before continuing.\n");
    g.push_str("- You may include brief reasoning text before the block.\n\n");

    // Few-shot example
    g.push_str("**Example:**\n");
    g.push_str("User: What files are in the current directory?\n");
    g.push_str("Assistant: I'll list the files for you.\n");
    g.push_str("```tool_call\n");
    g.push_str("{\"tool\": \"exec\", \"arguments\": {\"command\": \"ls -la\"}}\n");
    g.push_str("```\n\n");

    g
}

/// Format a tool schema in compact human-readable form for text-mode prompts.
///
/// Output: `### tool_name\ndescription\nParams: param1 (type, required), param2 (type)\n`
///
/// This is much shorter than dumping full JSON schema, saving ~60% context tokens.
fn format_compact_tool_schema(schema: &serde_json::Value) -> String {
    let name = schema["name"].as_str().unwrap_or("unknown");
    let desc = schema["description"].as_str().unwrap_or("");
    let params = &schema["parameters"];

    let mut out = format!("### {name}\n{desc}\n");

    if let Some(properties) = params.get("properties").and_then(|v| v.as_object()) {
        let required: Vec<&str> = params
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();

        let mut param_parts: Vec<String> = Vec::with_capacity(properties.len());
        for (param_name, param_schema) in properties {
            let type_str = param_schema
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("any");
            if required.contains(&param_name.as_str()) {
                param_parts.push(format!("{param_name} ({type_str}, required)"));
            } else {
                param_parts.push(format!("{param_name} ({type_str})"));
            }
        }

        if !param_parts.is_empty() {
            out.push_str("Params: ");
            out.push_str(&param_parts.join(", "));
            out.push('\n');
        }
    }
    out.push('\n');
    out
}
const TOOL_GUIDELINES: &str = concat!(
    "## Guidelines\n\n",
    "- Start with a normal conversational response. Do not call tools for greetings, small talk, ",
    "or questions you can answer directly.\n",
    "- Use the calc tool for arithmetic and expressions.\n",
    "- Use the exec tool for shell/system tasks.\n",
    "- If the user starts a message with `/sh `, run it with `exec` exactly as written.\n",
    "- Use the browser tool when the user asks to visit/read/interact with web pages.\n",
    "- Before tool calls, briefly state what you are about to do.\n",
    "- For multi-step tasks, execute one step at a time and check results before proceeding.\n",
    "- Be careful with destructive operations, confirm with the user first.\n",
    "- Do not express surprise about sandbox vs host execution. Route changes are normal.\n",
    "- Do not suggest disabling sandbox unless the user explicitly asks for host execution or ",
    "the task cannot be completed in sandbox.\n",
    "- The UI already shows raw tool output (stdout/stderr/exit). Summarize outcomes instead.\n\n",
    "## Silent Replies\n\n",
    "When you have nothing meaningful to add after a tool call, return an empty response.\n",
);
const MINIMAL_GUIDELINES: &str = concat!(
    "## Guidelines\n\n",
    "- Be helpful, accurate, and concise.\n",
    "- If you don't know something, say so rather than making things up.\n",
    "- For coding questions, provide clear explanations with examples.\n",
);
const HARD_PROMPT_MAX_CHARS: usize = 120_000;

/// Internal: build system prompt with full control over what's included.
fn build_system_prompt_full(
    tools: &ToolRegistry,
    native_tools: bool,
    project_context: Option<&str>,
    skills: &[SkillMetadata],
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    prompt_budgets: &PromptBudgetsConfig,
    runtime_context: Option<&PromptRuntimeContext>,
    include_tools: bool,
    memory_text: Option<&str>,
) -> Result<String, SoulRoutingDiagnostics> {
    let tool_schemas = if include_tools {
        tools.list_schemas()
    } else {
        Vec::new()
    };
    let mut prompt = String::from(if include_tools {
        "You are a helpful assistant. You can use tools when needed.\n\n"
    } else {
        "You are a helpful assistant. Answer questions clearly and concisely.\n\n"
    });

    let prepared_soul = prepare_soul_sections(soul_text.unwrap_or(DEFAULT_SOUL))?;
    let effective_agents_text = merge_agents_text(
        agents_text,
        prepared_soul.redistributed_agents_text.as_deref(),
    );
    let effective_tools_text = merge_agents_text(
        tools_text,
        prepared_soul.redistributed_tools_text.as_deref(),
    );
    let effective_heartbeat_text = merge_agents_text(
        heartbeat_text,
        prepared_soul.redistributed_heartbeat_text.as_deref(),
    );

    append_identity_and_user_sections(
        &mut prompt,
        identity,
        user,
        Some(&prepared_soul.identity_soul_text),
        prompt_budgets.soul_max_chars,
    );
    append_project_context(
        &mut prompt,
        project_context,
        prompt_budgets.project_context_max_chars,
    );
    append_runtime_section(&mut prompt, runtime_context, include_tools);
    append_skills_section(&mut prompt, include_tools, skills);
    append_workspace_files_section(
        &mut prompt,
        effective_agents_text.as_deref(),
        effective_tools_text.as_deref(),
        effective_heartbeat_text.as_deref(),
        prompt_budgets.workspace_file_max_chars,
    );
    append_memory_section(
        &mut prompt,
        memory_text,
        &tool_schemas,
        prompt_budgets.memory_bootstrap_max_chars,
    );
    let model_id = runtime_context.and_then(|ctx| ctx.host.model.as_deref());
    append_available_tools_section(&mut prompt, native_tools, &tool_schemas);
    append_tool_call_guidance(&mut prompt, native_tools, &tool_schemas, model_id);
    append_guidelines_section(&mut prompt, include_tools);
    append_runtime_datetime_tail(&mut prompt, runtime_context);

    if prompt.chars().count() > HARD_PROMPT_MAX_CHARS {
        prompt = truncate_prompt_text(&prompt, HARD_PROMPT_MAX_CHARS);
    }
    Ok(prompt)
}

#[derive(Debug)]
struct PreparedSoulSections {
    identity_soul_text: String,
    redistributed_agents_text: Option<String>,
    redistributed_tools_text: Option<String>,
    redistributed_heartbeat_text: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedSoulSection {
    heading: String,
    body: String,
    lane: SoulLane,
    section_id: String,
}

fn merge_agents_text(primary_agents: Option<&str>, redistributed: Option<&str>) -> Option<String> {
    match (primary_agents, redistributed) {
        (None, None) => None,
        (Some(primary), None) => Some(primary.to_string()),
        (None, Some(derived)) => Some(derived.to_string()),
        (Some(primary), Some(derived)) => Some(format!("{primary}\n\n{derived}")),
    }
}

fn section_heading_key(line: &str) -> Option<String> {
    let heading = line.trim().strip_prefix("## ")?;
    let key = heading.trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_ascii_lowercase())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoulLane {
    Soul,
    Agents,
    Tools,
    Heartbeat,
}

impl SoulLane {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "soul" => Some(Self::Soul),
            "agents" => Some(Self::Agents),
            "tools" => Some(Self::Tools),
            "heartbeat" => Some(Self::Heartbeat),
            _ => None,
        }
    }
}

fn parse_lane_marker(line: &str) -> Option<Result<SoulLane, String>> {
    let start = line.find("<!--")?;
    let rest = &line[start + 4..];
    let end = rest.find("-->")?;
    let comment = rest[..end].trim();
    let mut parts = comment.splitn(2, ':');
    let Some(key) = parts.next().map(str::trim) else {
        return None;
    };
    if !key.eq_ignore_ascii_case("lane") {
        return None;
    }
    let value = parts.next().map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Some(Err("".to_string()));
    }
    if let Some(lane) = SoulLane::parse(value) {
        return Some(Ok(lane));
    }
    Some(Err(value.to_string()))
}

fn strip_lane_marker(line: &str) -> String {
    let Some(start) = line.find("<!--") else {
        return line.to_string();
    };
    let rest = &line[start + 4..];
    let Some(rel_end) = rest.find("-->") else {
        return line.to_string();
    };
    let end = start + 4 + rel_end + 3;
    let mut merged = String::new();
    let prefix = line[..start].trim_end();
    if !prefix.is_empty() {
        merged.push_str(prefix);
    }
    let suffix = line[end..].trim_start();
    if !suffix.is_empty() {
        if !merged.is_empty() {
            merged.push(' ');
        }
        merged.push_str(suffix);
    }
    merged
}

fn join_non_empty_blocks(blocks: Vec<String>) -> String {
    blocks
        .into_iter()
        .filter_map(|block| {
            let trimmed = block.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn build_derived_soul_section(title: &str, intro: &str, blocks: Vec<String>) -> Option<String> {
    if blocks.is_empty() {
        return None;
    }
    let mut redistributed = format!("## {title}\n\n{intro}\n\n");
    redistributed.push_str(&join_non_empty_blocks(blocks));
    Some(redistributed)
}

/// Split SOUL.md into identity/value content and execution-heavy sections,
/// then route derived sections into AGENTS/TOOLS/HEARTBEAT prompt contexts.
///
/// Routing precedence:
/// - `<!-- lane:agents -->`
/// - `<!-- lane:tools -->`
/// - `<!-- lane:heartbeat -->`
/// - `<!-- lane:soul -->`
/// - default `soul` lane
fn prepare_soul_sections(soul_text: &str) -> Result<PreparedSoulSections, SoulRoutingDiagnostics> {
    let mut diagnostics = SoulRoutingDiagnostics::default();
    let mut preamble_lines: Vec<String> = Vec::new();
    let mut sections: Vec<ParsedSoulSection> = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut current_lane = SoulLane::Soul;
    let mut pending_lane: Option<SoulLane> = None;

    for raw_line in soul_text.lines() {
        let marker = parse_lane_marker(raw_line);
        let line = strip_lane_marker(raw_line);
        let trimmed = line.trim();
        let line_heading = section_heading_key(trimmed);

        if let Some(marker) = marker.as_ref().map(|result| result.as_ref()) {
            match marker {
                Ok(lane) => {
                    if trimmed.is_empty() || line_heading.is_none() {
                        pending_lane = Some(*lane);
                    }
                },
                Err(value) => {
                    let rendered = if value.is_empty() {
                        "empty".to_string()
                    } else {
                        value.to_string()
                    };
                    diagnostics.push(
                        SoulRoutingIssueCode::InvalidSoulMarker,
                        format!(
                            "invalid SOUL lane marker '{rendered}'; expected one of: soul, agents, tools, heartbeat"
                        ),
                    );
                },
            }
        }

        if let Some(new_heading) = line_heading {
            if let Some(previous_heading) = current_heading.take() {
                let section_index = sections.len() + 1;
                sections.push(ParsedSoulSection {
                    section_id: format!("{}-{section_index}", slugify_heading(&previous_heading)),
                    heading: previous_heading,
                    body: current_lines.join("\n"),
                    lane: current_lane,
                });
                current_lines.clear();
            } else if !current_lines.is_empty() {
                preamble_lines.extend(current_lines.drain(..));
            }
            let lane_for_heading = match marker {
                Some(Ok(lane)) => lane,
                _ => pending_lane.take().unwrap_or(SoulLane::Soul),
            };
            current_heading = Some(new_heading);
            current_lane = lane_for_heading;
            if !trimmed.is_empty() {
                current_lines.push(line);
            }
            continue;
        }

        if trimmed.is_empty() {
            continue;
        }
        current_lines.push(line);
    }

    if let Some(last_heading) = current_heading.take() {
        let section_index = sections.len() + 1;
        sections.push(ParsedSoulSection {
            section_id: format!("{}-{section_index}", slugify_heading(&last_heading)),
            heading: last_heading,
            body: current_lines.join("\n"),
            lane: current_lane,
        });
    } else if !current_lines.is_empty() {
        preamble_lines.extend(current_lines);
    }
    if pending_lane.is_some() {
        diagnostics.push(
            SoulRoutingIssueCode::OrphanSoulMarker,
            "orphan SOUL lane marker found with no following `##` section; marker was ignored",
        );
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    if sections.is_empty() {
        return Ok(PreparedSoulSections {
            identity_soul_text: soul_text.trim().to_string(),
            redistributed_agents_text: None,
            redistributed_tools_text: None,
            redistributed_heartbeat_text: None,
        });
    }

    let mut identity_blocks = Vec::new();
    let preamble = preamble_lines.join("\n");
    if !preamble.trim().is_empty() {
        identity_blocks.push(preamble);
    }

    let mut operational_blocks = Vec::new();
    let mut tools_blocks = Vec::new();
    let mut heartbeat_blocks = Vec::new();
    let mut input_ids = HashSet::new();
    let mut output_ids = HashSet::new();
    let mut seen_heading_lanes: HashMap<String, SoulLane> = HashMap::new();
    for section in sections {
        let heading_key = section.heading.trim().to_ascii_lowercase();
        if let Some(existing_lane) = seen_heading_lanes.get(&heading_key)
            && *existing_lane != section.lane
        {
            diagnostics.push(
                SoulRoutingIssueCode::DuplicateSoulSection,
                format!("duplicate SOUL section placement for heading '{heading_key}' across lanes"),
            );
        } else {
            let _ = seen_heading_lanes.insert(heading_key.clone(), section.lane);
        }
        let _ = input_ids.insert(section.section_id.clone());
        if !output_ids.insert(section.section_id) {
            diagnostics.push(
                SoulRoutingIssueCode::DuplicateSoulSection,
                format!("duplicate SOUL section placement detected for heading '{heading_key}'"),
            );
        }
        match section.lane {
            SoulLane::Soul => identity_blocks.push(section.body),
            SoulLane::Agents => operational_blocks.push(section.body),
            SoulLane::Tools => tools_blocks.push(section.body),
            SoulLane::Heartbeat => heartbeat_blocks.push(section.body),
        }
    }

    if input_ids != output_ids {
        diagnostics.push(
            SoulRoutingIssueCode::SectionPlacementMismatch,
            "SOUL redistribution failed one-to-one section placement validation",
        );
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let identity_soul_text = {
        let joined = join_non_empty_blocks(identity_blocks);
        if joined.is_empty() {
            soul_text.trim().to_string()
        } else {
            joined
        }
    };

    let redistributed_agents_text = build_derived_soul_section(
        "Derived From SOUL.md (Operational Rules)",
        "These sections are redistributed from SOUL.md for execution salience. \
Keep SOUL focused on identity/values/style and keep process execution in \
AGENTS/rules.",
        operational_blocks,
    );
    let redistributed_tools_text = build_derived_soul_section(
        "Derived From SOUL.md (Tooling Rules)",
        "These sections are redistributed from SOUL.md to keep tool/routing policy in TOOLS context.",
        tools_blocks,
    );
    let redistributed_heartbeat_text = build_derived_soul_section(
        "Derived From SOUL.md (Heartbeat/Proactivity Rules)",
        "These sections are redistributed from SOUL.md for heartbeat/proactive cadence handling.",
        heartbeat_blocks,
    );

    Ok(PreparedSoulSections {
        identity_soul_text,
        redistributed_agents_text,
        redistributed_tools_text,
        redistributed_heartbeat_text,
    })
}

/// Collect SOUL routing diagnostics (invalid/orphan/duplicate lane placement).
#[must_use]
pub fn collect_soul_routing_issues(soul_text: &str) -> Vec<SoulRoutingIssue> {
    match prepare_soul_sections(soul_text) {
        Ok(_) => Vec::new(),
        Err(diag) => diag.issues,
    }
}

/// Validate SOUL routing and return diagnostics when invalid.
pub fn validate_soul_routing(soul_text: &str) -> Result<(), SoulRoutingDiagnostics> {
    prepare_soul_sections(soul_text).map(|_| ())
}

fn prepare_soul_sections_best_effort(soul_text: &str) -> PreparedSoulSections {
    match prepare_soul_sections(soul_text) {
        Ok(prepared) => prepared,
        Err(diag) => {
            for issue in diag.issues {
                warn!(
                    code = issue.code.as_str(),
                    message = %issue.message,
                    "SOUL redistribution validation issue"
                );
            }
            PreparedSoulSections {
                identity_soul_text: soul_text.trim().to_string(),
                redistributed_agents_text: None,
                redistributed_tools_text: None,
                redistributed_heartbeat_text: None,
            }
        },
    }
}

fn append_identity_and_user_sections(
    prompt: &mut String,
    identity: Option<&AgentIdentity>,
    user: Option<&UserProfile>,
    soul_text: Option<&str>,
    soul_max_chars: usize,
) {
    if let Some(id) = identity {
        let mut parts = Vec::new();
        match (id.name.as_deref(), id.emoji.as_deref()) {
            (Some(name), Some(emoji)) => parts.push(format!("Your name is {name} {emoji}.")),
            (Some(name), None) => parts.push(format!("Your name is {name}.")),
            _ => {},
        }
        if let Some(theme) = id.theme.as_deref() {
            parts.push(format!("Your theme: {theme}."));
        }
        if !parts.is_empty() {
            prompt.push_str(&parts.join(" "));
            prompt.push('\n');
        }
        prompt.push_str("\n## Soul\n\n");
        let soul = soul_text.unwrap_or(DEFAULT_SOUL);
        let was_truncated = append_truncated_text_block(
            prompt,
            soul,
            soul_max_chars,
            "\n*(SOUL.md truncated for prompt size.)*\n",
        );
        if was_truncated {
            warn_prompt_truncation("soul", soul_max_chars, soul);
        }
        prompt.push('\n');
    }

    if let Some(name) = user.and_then(|profile| profile.name.as_deref()) {
        prompt.push_str(&format!("The user's name is {name}.\n"));
    }
    if identity.is_some() || user.is_some() {
        prompt.push('\n');
    }
}

fn append_project_context(
    prompt: &mut String,
    project_context: Option<&str>,
    project_context_max_chars: usize,
) {
    if let Some(context) = project_context {
        let was_truncated = append_truncated_text_block(
            prompt,
            context,
            project_context_max_chars,
            "\n*(Project context truncated for prompt size; use tools/files for full details.)*\n",
        );
        if was_truncated {
            warn_prompt_truncation("project_context", project_context_max_chars, context);
        }
        prompt.push('\n');
    }
}

fn append_runtime_section(
    prompt: &mut String,
    runtime_context: Option<&PromptRuntimeContext>,
    include_tools: bool,
) {
    let Some(runtime) = runtime_context else {
        return;
    };

    let host_line = format_host_runtime_line(&runtime.host);
    let sandbox_line = runtime.sandbox.as_ref().map(format_sandbox_runtime_line);
    if host_line.is_none() && sandbox_line.is_none() {
        return;
    }

    prompt.push_str("## Runtime\n\n");
    if let Some(line) = host_line {
        prompt.push_str(&line);
        prompt.push('\n');
    }
    if let Some(line) = sandbox_line {
        prompt.push_str(&line);
        prompt.push('\n');
    }
    if include_tools {
        prompt.push_str(EXEC_ROUTING_GUIDANCE);
    } else {
        prompt.push('\n');
    }
}

fn append_skills_section(prompt: &mut String, include_tools: bool, skills: &[SkillMetadata]) {
    if include_tools && !skills.is_empty() {
        prompt.push_str(&moltis_skills::prompt_gen::generate_skills_prompt(skills));
    }
}

fn append_workspace_files_section(
    prompt: &mut String,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    workspace_file_max_chars: usize,
) {
    if agents_text.is_none() && tools_text.is_none() && heartbeat_text.is_none() {
        return;
    }

    prompt.push_str("## Workspace Files\n\n");
    if let Some(agents_md) = agents_text {
        prompt.push_str("### AGENTS.md (workspace)\n\n");
        let was_truncated = append_truncated_text_block(
            prompt,
            agents_md,
            workspace_file_max_chars,
            "\n*(AGENTS.md truncated for prompt size.)*\n",
        );
        if was_truncated {
            warn_prompt_truncation("agents_md", workspace_file_max_chars, agents_md);
        }
        prompt.push_str("\n\n");
    }
    if let Some(tools_md) = tools_text {
        prompt.push_str("### TOOLS.md (workspace)\n\n");
        let was_truncated = append_truncated_text_block(
            prompt,
            tools_md,
            workspace_file_max_chars,
            "\n*(TOOLS.md truncated for prompt size.)*\n",
        );
        if was_truncated {
            warn_prompt_truncation("tools_md", workspace_file_max_chars, tools_md);
        }
        prompt.push_str("\n\n");
    }
    if let Some(heartbeat_md) = heartbeat_text {
        prompt.push_str("### HEARTBEAT.md (workspace)\n\n");
        let was_truncated = append_truncated_text_block(
            prompt,
            heartbeat_md,
            workspace_file_max_chars,
            "\n*(HEARTBEAT.md truncated for prompt size.)*\n",
        );
        if was_truncated {
            warn_prompt_truncation("heartbeat_md", workspace_file_max_chars, heartbeat_md);
        }
        prompt.push_str("\n\n");
    }
}

fn append_memory_section(
    prompt: &mut String,
    memory_text: Option<&str>,
    tool_schemas: &[serde_json::Value],
    memory_bootstrap_max_chars: usize,
) {
    let has_memory_search = has_tool_schema(tool_schemas, "memory_search");
    let has_memory_save = has_tool_schema(tool_schemas, "memory_save");
    let memory_content = memory_text.filter(|text| !text.is_empty());
    if memory_content.is_none() && !has_memory_search && !has_memory_save {
        return;
    }

    prompt.push_str("## Long-Term Memory\n\n");
    if let Some(text) = memory_content {
        let was_truncated = append_truncated_text_block(
            prompt,
            text,
            memory_bootstrap_max_chars,
            "\n\n*(MEMORY.md truncated — use `memory_search` for full content)*\n",
        );
        if was_truncated {
            warn_prompt_truncation("memory_bootstrap", memory_bootstrap_max_chars, text);
        }
        prompt.push_str(concat!(
            "\n\n**The information above is memory bootstrap context. ",
            "Use it when relevant and safe for the current request.** ",
            "Do not force unrelated personalization.\n",
        ));
    }
    if has_memory_search {
        prompt.push_str(concat!(
            "\nYou also have `memory_search` to find additional details from ",
            "`memory/*.md` files and past session history beyond what is shown above. ",
            "**Search memory when relevance is non-trivial before claiming you don't know something.** ",
            "The long-term memory system holds user facts, past decisions, project context, ",
            "and anything previously stored.\n",
        ));
    }
    if has_memory_save {
        prompt.push_str(concat!(
            "\n**When the user asks you to remember, save, or note something, ",
            "you MUST call `memory_save` to persist it.** ",
            "Do not just acknowledge verbally — without calling the tool, ",
            "the information will be lost after the session.\n",
            "\nChoose the right target to keep context lean:\n",
            "- **MEMORY.md** — only core identity facts (name, age, location, ",
            "language, key preferences). This is loaded into every conversation, ",
            "so keep it short.\n",
            "- **memory/&lt;topic&gt;.md** — everything else (detailed notes, project ",
            "context, decisions, session summaries). These are only retrieved via ",
            "`memory_search` and do not consume prompt space.\n",
        ));
    }
    prompt.push('\n');
}

fn has_tool_schema(tool_schemas: &[serde_json::Value], tool_name: &str) -> bool {
    tool_schemas
        .iter()
        .any(|schema| schema["name"].as_str() == Some(tool_name))
}

fn append_available_tools_section(
    prompt: &mut String,
    native_tools: bool,
    tool_schemas: &[serde_json::Value],
) {
    if tool_schemas.is_empty() {
        return;
    }

    prompt.push_str("## Available Tools\n\n");
    if native_tools {
        // Native tool-calling providers already receive full schemas via API.
        // Keep this section compact so we don't duplicate large JSON payloads.
        for schema in tool_schemas {
            let name = schema["name"].as_str().unwrap_or("unknown");
            let desc = schema["description"].as_str().unwrap_or("");
            let compact_desc = truncate_prompt_text(desc, 160);
            if compact_desc.is_empty() {
                prompt.push_str(&format!("- `{name}`\n"));
            } else {
                prompt.push_str(&format!("- `{name}`: {compact_desc}\n"));
            }
        }
        prompt.push('\n');
        return;
    }

    // Text-mode: use compact schema format to save context tokens.
    for schema in tool_schemas {
        prompt.push_str(&format_compact_tool_schema(schema));
    }
}

fn append_tool_call_guidance(
    prompt: &mut String,
    native_tools: bool,
    tool_schemas: &[serde_json::Value],
    model_id: Option<&str>,
) {
    if !native_tools && !tool_schemas.is_empty() {
        prompt.push_str(&tool_call_guidance(model_id));
    }
}

fn append_guidelines_section(prompt: &mut String, include_tools: bool) {
    prompt.push_str(if include_tools {
        TOOL_GUIDELINES
    } else {
        MINIMAL_GUIDELINES
    });
}

fn append_runtime_datetime_tail(
    prompt: &mut String,
    runtime_context: Option<&PromptRuntimeContext>,
) {
    let Some(runtime) = runtime_context else {
        return;
    };

    if let Some(time) = runtime
        .host
        .time
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        prompt.push_str("\nThe current user datetime is ");
        prompt.push_str(time);
        prompt.push_str(".\n");
        return;
    }

    if let Some(today) = runtime
        .host
        .today
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        prompt.push_str("\nThe current user date is ");
        prompt.push_str(today);
        prompt.push_str(".\n");
    }
}

fn push_non_empty_runtime_field(parts: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        parts.push(format!("{key}={value}"));
    }
}

fn format_host_runtime_line(host: &PromptHostRuntimeContext) -> Option<String> {
    let mut parts = Vec::new();
    for (key, value) in [
        ("host", host.host.as_deref()),
        ("os", host.os.as_deref()),
        ("arch", host.arch.as_deref()),
        ("shell", host.shell.as_deref()),
        ("today", host.today.as_deref()),
        ("provider", host.provider.as_deref()),
        ("model", host.model.as_deref()),
        ("session", host.session_key.as_deref()),
        ("surface", host.surface.as_deref()),
        ("session_kind", host.session_kind.as_deref()),
        ("channel_type", host.channel_type.as_deref()),
        ("channel_account", host.channel_account_id.as_deref()),
        ("channel_chat_id", host.channel_chat_id.as_deref()),
        ("channel_chat_type", host.channel_chat_type.as_deref()),
        ("data_dir", host.data_dir.as_deref()),
    ] {
        push_non_empty_runtime_field(&mut parts, key, value);
    }
    if let Some(sudo_non_interactive) = host.sudo_non_interactive {
        parts.push(format!("sudo_non_interactive={sudo_non_interactive}"));
    }
    for (key, value) in [
        ("sudo_status", host.sudo_status.as_deref()),
        ("timezone", host.timezone.as_deref()),
        ("accept_language", host.accept_language.as_deref()),
        ("remote_ip", host.remote_ip.as_deref()),
        ("location", host.location.as_deref()),
    ] {
        push_non_empty_runtime_field(&mut parts, key, value);
    }

    (!parts.is_empty()).then(|| format!("Host: {}", parts.join(" | ")))
}

fn truncate_prompt_text(text: &str, max_chars: usize) -> String {
    if text.is_empty() || max_chars == 0 {
        return String::new();
    }
    let original_chars = text.chars().count();
    if original_chars <= max_chars {
        return text.to_string();
    }

    // Prefer full markdown sections when possible so policy blocks are never
    // cut mid-section.
    if let Some(section_truncated) = truncate_markdown_sections(text, max_chars) {
        return section_truncated;
    }

    truncate_paragraphs(text, max_chars)
}

fn truncate_chars_with_ellipsis(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        if max_chars > 3 {
            out.truncate(out.chars().take(max_chars - 3).map(char::len_utf8).sum());
            out.push_str("...");
        } else {
            out.truncate(out.chars().take(max_chars).map(char::len_utf8).sum());
        }
    }
    out
}

#[derive(Debug, Clone)]
struct MarkdownSection {
    index: usize,
    body: String,
    section_id: String,
    bucket: PriorityBucket,
}

fn slugify_heading(heading: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in heading.to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

fn classify_priority_bucket(heading: &str) -> PriorityBucket {
    let key = heading.to_ascii_lowercase();
    if ["security", "boundary", "risk", "privacy", "safety", "non-negotiable"]
        .iter()
        .any(|needle| key.contains(needle))
    {
        return PriorityBucket::SafetyBoundaries;
    }
    if ["identity", "constitutional", "role", "conflict", "precedence"]
        .iter()
        .any(|needle| key.contains(needle))
    {
        return PriorityBucket::IdentityConstraints;
    }
    if ["tool", "routing", "tooling"]
        .iter()
        .any(|needle| key.contains(needle))
    {
        return PriorityBucket::ToolingRules;
    }
    if ["style", "vibe", "tone"]
        .iter()
        .any(|needle| key.contains(needle))
    {
        return PriorityBucket::StyleVibe;
    }
    PriorityBucket::ExecutionPolicy
}

fn split_markdown_sections(text: &str) -> Option<Vec<MarkdownSection>> {
    let mut sections: Vec<MarkdownSection> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_heading = String::from("preamble");
    let mut saw_heading = false;

    for line in text.lines() {
        if line.trim_start().starts_with("## ") {
            saw_heading = true;
            if !current.is_empty() {
                let body = current.join("\n");
                let section_idx = sections.len() + 1;
                sections.push(MarkdownSection {
                    index: sections.len(),
                    section_id: format!(
                        "{}-{}",
                        if current_heading == "preamble" {
                            "preamble".to_string()
                        } else {
                            slugify_heading(&current_heading)
                        },
                        section_idx
                    ),
                    bucket: classify_priority_bucket(&current_heading),
                    body,
                });
                current.clear();
            }
            current_heading = line
                .trim()
                .trim_start_matches("##")
                .trim()
                .to_string();
        }
        current.push(line.to_string());
    }
    if !current.is_empty() {
        let body = current.join("\n");
        let section_idx = sections.len() + 1;
        sections.push(MarkdownSection {
            index: sections.len(),
            section_id: format!(
                "{}-{}",
                if current_heading == "preamble" {
                    "preamble".to_string()
                } else {
                    slugify_heading(&current_heading)
                },
                section_idx
            ),
            bucket: classify_priority_bucket(&current_heading),
            body,
        });
    }

    saw_heading.then_some(sections)
}

fn truncate_markdown_sections(text: &str, max_chars: usize) -> Option<String> {
    let sections = split_markdown_sections(text)?;
    let mut ordered = sections.clone();
    ordered.sort_by_key(|section| (section.bucket, section.index));

    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;

    for section in ordered {
        let section = section.body.trim();
        if section.is_empty() {
            continue;
        }
        let section_chars = section.chars().count();
        let sep = usize::from(!kept.is_empty()) * 2;
        let next_total = used + sep + section_chars;
        if next_total > max_chars {
            if kept.is_empty() {
                return Some(truncate_paragraphs(section, max_chars));
            }
            break;
        }
        kept.push(section.to_string());
        used = next_total;
    }

    if kept.is_empty() {
        return Some(truncate_paragraphs(text, max_chars));
    }
    Some(kept.join("\n\n"))
}

fn truncate_paragraphs(text: &str, max_chars: usize) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0usize;

    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let para_chars = para.chars().count();
        let sep = usize::from(!kept.is_empty()) * 2;
        let next_total = used + sep + para_chars;
        if next_total > max_chars {
            if kept.is_empty() {
                let mut line_kept = Vec::new();
                let mut line_used = 0usize;
                for line in para.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let line_chars = line.chars().count();
                    let line_sep = usize::from(!line_kept.is_empty());
                    if line_used + line_sep + line_chars > max_chars {
                        break;
                    }
                    line_kept.push(line.to_string());
                    line_used += line_sep + line_chars;
                }
                if line_kept.is_empty() {
                    return truncate_chars_with_ellipsis(para, max_chars);
                }
                return line_kept.join("\n");
            }
            break;
        }
        kept.push(para.to_string());
        used = next_total;
    }

    if kept.is_empty() {
        truncate_chars_with_ellipsis(text, max_chars)
    } else {
        kept.join("\n\n")
    }
}

fn warn_prompt_truncation(section: &str, max_chars: usize, text: &str) {
    warn!(
        section,
        max_chars,
        original_chars = text.chars().count(),
        "prompt section truncated due to budget"
    );
}

fn push_truncation_if_needed(
    out: &mut Vec<PromptSectionTruncation>,
    section: &str,
    text: Option<&str>,
    max_chars: usize,
) {
    let Some(text) = text.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    let original_chars = text.chars().count();
    if original_chars > max_chars {
        out.push(PromptSectionTruncation {
            section: section.to_string(),
            max_chars,
            original_chars,
        });
    }
}

/// Compute section-level truncation metadata for the current persona/context.
pub fn collect_prompt_section_truncations(
    project_context: Option<&str>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    memory_text: Option<&str>,
    prompt_budgets: Option<&PromptBudgetsConfig>,
) -> Vec<PromptSectionTruncation> {
    let budgets = prompt_budgets.cloned().unwrap_or_default();
    let prepared_soul = prepare_soul_sections_best_effort(soul_text.unwrap_or(DEFAULT_SOUL));
    let effective_agents_text = merge_agents_text(
        agents_text,
        prepared_soul.redistributed_agents_text.as_deref(),
    );
    let effective_tools_text = merge_agents_text(
        tools_text,
        prepared_soul.redistributed_tools_text.as_deref(),
    );
    let effective_heartbeat_text = merge_agents_text(
        heartbeat_text,
        prepared_soul.redistributed_heartbeat_text.as_deref(),
    );

    let mut out = Vec::new();
    push_truncation_if_needed(
        &mut out,
        "soul",
        Some(prepared_soul.identity_soul_text.as_str()),
        budgets.soul_max_chars,
    );
    push_truncation_if_needed(
        &mut out,
        "project_context",
        project_context,
        budgets.project_context_max_chars,
    );
    push_truncation_if_needed(
        &mut out,
        "agents_md",
        effective_agents_text.as_deref(),
        budgets.workspace_file_max_chars,
    );
    push_truncation_if_needed(
        &mut out,
        "tools_md",
        effective_tools_text.as_deref(),
        budgets.workspace_file_max_chars,
    );
    push_truncation_if_needed(
        &mut out,
        "heartbeat_md",
        effective_heartbeat_text.as_deref(),
        budgets.workspace_file_max_chars,
    );
    push_truncation_if_needed(
        &mut out,
        "memory_bootstrap",
        memory_text,
        budgets.memory_bootstrap_max_chars,
    );
    out
}

fn default_bucket_for_section(section: &str) -> PriorityBucket {
    match section {
        "soul" => PriorityBucket::IdentityConstraints,
        "tools_md" => PriorityBucket::ToolingRules,
        "project_context" | "agents_md" | "heartbeat_md" => PriorityBucket::ExecutionPolicy,
        "memory_bootstrap" => PriorityBucket::SafetyBoundaries,
        _ => PriorityBucket::ExecutionPolicy,
    }
}

fn collect_dropped_markdown_sections(
    section_name: &str,
    text: &str,
    max_chars: usize,
) -> Vec<DroppedPromptSection> {
    let Some(sections) = split_markdown_sections(text) else {
        return Vec::new();
    };
    let mut ordered = sections.clone();
    ordered.sort_by_key(|section| (section.bucket, section.index));

    let mut kept_ids: HashSet<String> = HashSet::new();
    let mut used = 0usize;
    for section in ordered {
        let body = section.body.trim();
        if body.is_empty() {
            continue;
        }
        let section_chars = body.chars().count();
        let sep = usize::from(!kept_ids.is_empty()) * 2;
        if used + sep + section_chars > max_chars {
            continue;
        }
        let _ = kept_ids.insert(section.section_id.clone());
        used += sep + section_chars;
    }

    sections
        .into_iter()
        .filter_map(|section| {
            let body = section.body.trim();
            if body.is_empty() || kept_ids.contains(&section.section_id) {
                return None;
            }
            Some(DroppedPromptSection {
                section: section_name.to_string(),
                section_id: section.section_id,
                bucket: section.bucket.as_str().to_string(),
                reason: "budget_exceeded".to_string(),
                original_chars: body.chars().count(),
                max_chars,
            })
        })
        .collect()
}

/// Compute dropped markdown sections for budget diagnostics.
pub fn collect_dropped_prompt_sections(
    project_context: Option<&str>,
    soul_text: Option<&str>,
    agents_text: Option<&str>,
    tools_text: Option<&str>,
    heartbeat_text: Option<&str>,
    memory_text: Option<&str>,
    prompt_budgets: Option<&PromptBudgetsConfig>,
) -> Vec<DroppedPromptSection> {
    let budgets = prompt_budgets.cloned().unwrap_or_default();
    let prepared_soul = prepare_soul_sections_best_effort(soul_text.unwrap_or(DEFAULT_SOUL));
    let effective_agents_text = merge_agents_text(
        agents_text,
        prepared_soul.redistributed_agents_text.as_deref(),
    );
    let effective_tools_text = merge_agents_text(
        tools_text,
        prepared_soul.redistributed_tools_text.as_deref(),
    );
    let effective_heartbeat_text = merge_agents_text(
        heartbeat_text,
        prepared_soul.redistributed_heartbeat_text.as_deref(),
    );
    let sections: [(&str, Option<&str>, usize); 6] = [
        (
            "soul",
            Some(prepared_soul.identity_soul_text.as_str()),
            budgets.soul_max_chars,
        ),
        (
            "project_context",
            project_context,
            budgets.project_context_max_chars,
        ),
        (
            "agents_md",
            effective_agents_text.as_deref(),
            budgets.workspace_file_max_chars,
        ),
        (
            "tools_md",
            effective_tools_text.as_deref(),
            budgets.workspace_file_max_chars,
        ),
        (
            "heartbeat_md",
            effective_heartbeat_text.as_deref(),
            budgets.workspace_file_max_chars,
        ),
        (
            "memory_bootstrap",
            memory_text,
            budgets.memory_bootstrap_max_chars,
        ),
    ];

    let mut dropped = Vec::new();
    for (section_name, text_opt, max_chars) in sections {
        let Some(text) = text_opt.filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        let original_chars = text.chars().count();
        if original_chars <= max_chars {
            continue;
        }
        let mut markdown_dropped = collect_dropped_markdown_sections(section_name, text, max_chars);
        if markdown_dropped.is_empty() {
            markdown_dropped.push(DroppedPromptSection {
                section: section_name.to_string(),
                section_id: format!("{section_name}-overflow"),
                bucket: default_bucket_for_section(section_name).as_str().to_string(),
                reason: "budget_exceeded".to_string(),
                original_chars,
                max_chars,
            });
        }
        dropped.extend(markdown_dropped);
    }
    dropped
}

fn append_truncated_text_block(
    prompt: &mut String,
    text: &str,
    max_chars: usize,
    truncated_notice: &str,
) -> bool {
    let truncated = truncate_prompt_text(text, max_chars);
    prompt.push_str(&truncated);
    let was_truncated = text.chars().count() > max_chars;
    if was_truncated {
        prompt.push_str(truncated_notice);
    }
    was_truncated
}

fn format_sandbox_runtime_line(sandbox: &PromptSandboxRuntimeContext) -> String {
    let mut parts = vec![format!("enabled={}", sandbox.exec_sandboxed)];

    for (key, value) in [
        ("mode", sandbox.mode.as_deref()),
        ("backend", sandbox.backend.as_deref()),
        ("scope", sandbox.scope.as_deref()),
        ("image", sandbox.image.as_deref()),
        ("home", sandbox.home.as_deref()),
        ("workspace_mount", sandbox.workspace_mount.as_deref()),
        ("workspace_path", sandbox.workspace_path.as_deref()),
    ] {
        push_non_empty_runtime_field(&mut parts, key, value);
    }
    if let Some(no_network) = sandbox.no_network {
        let network_state = if no_network {
            "disabled"
        } else {
            "enabled"
        };
        parts.push(format!("network={network_state}"));
    }
    if let Some(session_override) = sandbox.session_override {
        parts.push(format!("session_override={session_override}"));
    }

    format!("Sandbox(exec): {}", parts.join(" | "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn must_render(prompt: Result<String, SoulRoutingDiagnostics>) -> String {
        prompt.expect("prompt assembly should succeed for this test case")
    }

    fn build_system_prompt(
        tools: &ToolRegistry,
        native_tools: bool,
        project_context: Option<&str>,
    ) -> String {
        must_render(super::build_system_prompt(
            tools,
            native_tools,
            project_context,
        ))
    }

    fn build_system_prompt_with_session_runtime(
        tools: &ToolRegistry,
        native_tools: bool,
        project_context: Option<&str>,
        skills: &[SkillMetadata],
        identity: Option<&AgentIdentity>,
        user: Option<&UserProfile>,
        soul_text: Option<&str>,
        agents_text: Option<&str>,
        tools_text: Option<&str>,
        runtime_context: Option<&PromptRuntimeContext>,
        memory_text: Option<&str>,
    ) -> String {
        must_render(super::build_system_prompt_with_session_runtime(
            tools,
            native_tools,
            project_context,
            skills,
            identity,
            user,
            soul_text,
            agents_text,
            tools_text,
            runtime_context,
            memory_text,
        ))
    }

    fn build_system_prompt_with_session_runtime_workspace(
        tools: &ToolRegistry,
        native_tools: bool,
        project_context: Option<&str>,
        skills: &[SkillMetadata],
        identity: Option<&AgentIdentity>,
        user: Option<&UserProfile>,
        soul_text: Option<&str>,
        agents_text: Option<&str>,
        tools_text: Option<&str>,
        heartbeat_text: Option<&str>,
        runtime_context: Option<&PromptRuntimeContext>,
        memory_text: Option<&str>,
    ) -> String {
        must_render(super::build_system_prompt_with_session_runtime_workspace(
            tools,
            native_tools,
            project_context,
            skills,
            identity,
            user,
            soul_text,
            agents_text,
            tools_text,
            heartbeat_text,
            runtime_context,
            memory_text,
        ))
    }

    fn build_system_prompt_with_session_runtime_workspace_budgets(
        tools: &ToolRegistry,
        native_tools: bool,
        project_context: Option<&str>,
        skills: &[SkillMetadata],
        identity: Option<&AgentIdentity>,
        user: Option<&UserProfile>,
        soul_text: Option<&str>,
        agents_text: Option<&str>,
        tools_text: Option<&str>,
        heartbeat_text: Option<&str>,
        prompt_budgets: Option<&PromptBudgetsConfig>,
        runtime_context: Option<&PromptRuntimeContext>,
        memory_text: Option<&str>,
    ) -> String {
        must_render(super::build_system_prompt_with_session_runtime_workspace_budgets(
            tools,
            native_tools,
            project_context,
            skills,
            identity,
            user,
            soul_text,
            agents_text,
            tools_text,
            heartbeat_text,
            prompt_budgets,
            runtime_context,
            memory_text,
        ))
    }

    fn build_system_prompt_minimal_runtime(
        project_context: Option<&str>,
        identity: Option<&AgentIdentity>,
        user: Option<&UserProfile>,
        soul_text: Option<&str>,
        agents_text: Option<&str>,
        tools_text: Option<&str>,
        runtime_context: Option<&PromptRuntimeContext>,
        memory_text: Option<&str>,
    ) -> String {
        must_render(super::build_system_prompt_minimal_runtime(
            project_context,
            identity,
            user,
            soul_text,
            agents_text,
            tools_text,
            runtime_context,
            memory_text,
        ))
    }

    fn prepare_soul_sections(soul_text: &str) -> PreparedSoulSections {
        super::prepare_soul_sections(soul_text)
            .expect("SOUL routing should succeed for this test case")
    }

    #[test]
    fn test_native_prompt_does_not_include_tool_call_format() {
        let tools = ToolRegistry::new();
        let prompt = build_system_prompt(&tools, true, None);
        assert!(!prompt.contains("```tool_call"));
    }

    #[test]
    fn test_fallback_prompt_includes_tool_call_format() {
        let mut tools = ToolRegistry::new();
        struct Dummy;
        #[async_trait::async_trait]
        impl crate::tool_registry::AgentTool for Dummy {
            fn name(&self) -> &str {
                "test"
            }

            fn description(&self) -> &str {
                "A test tool"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }

            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
        }
        tools.register(Box::new(Dummy));

        let prompt = build_system_prompt(&tools, false, None);
        assert!(prompt.contains("```tool_call"));
        assert!(prompt.contains("### test"));
    }

    #[test]
    fn test_native_prompt_uses_compact_tool_list() {
        let mut tools = ToolRegistry::new();
        struct Dummy;
        #[async_trait::async_trait]
        impl crate::tool_registry::AgentTool for Dummy {
            fn name(&self) -> &str {
                "test"
            }

            fn description(&self) -> &str {
                "A test tool"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string"}}})
            }

            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
        }
        tools.register(Box::new(Dummy));

        let prompt = build_system_prompt(&tools, true, None);
        assert!(prompt.contains("## Available Tools"));
        assert!(prompt.contains("- `test`: A test tool"));
        assert!(!prompt.contains("Parameters:"));
    }

    #[test]
    fn test_skills_injected_into_prompt() {
        let tools = ToolRegistry::new();
        let skills = vec![SkillMetadata {
            name: "commit".into(),
            description: "Create git commits".into(),
            license: None,
            compatibility: None,
            allowed_tools: vec![],
            homepage: None,
            dockerfile: None,
            requires: Default::default(),
            path: std::path::PathBuf::from("/skills/commit"),
            source: None,
        }];
        let prompt = build_system_prompt_with_session_runtime(
            &tools, true, None, &skills, None, None, None, None, None, None, None,
        );
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("commit"));
    }

    #[test]
    fn test_no_skills_block_when_empty() {
        let tools = ToolRegistry::new();
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!prompt.contains("<available_skills>"));
    }

    #[test]
    fn test_identity_injected_into_prompt() {
        let tools = ToolRegistry::new();
        let identity = AgentIdentity {
            name: Some("Momo".into()),
            emoji: Some("🦜".into()),
            theme: Some("cheerful parrot".into()),
        };
        let user = UserProfile {
            name: Some("Alice".into()),
            timezone: None,
            location: None,
        };
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            Some(&identity),
            Some(&user),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(prompt.contains("Your name is Momo 🦜."));
        assert!(prompt.contains("Your theme: cheerful parrot."));
        assert!(prompt.contains("The user's name is Alice."));
        // Default soul should be injected when soul is None.
        assert!(prompt.contains("## Soul"));
        assert!(prompt.contains("Be genuinely helpful"));
    }

    #[test]
    fn test_custom_soul_injected() {
        let tools = ToolRegistry::new();
        let identity = AgentIdentity {
            name: Some("Rex".into()),
            ..Default::default()
        };
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            Some(&identity),
            None,
            Some("You are a loyal companion who loves fetch."),
            None,
            None,
            None,
            None,
        );
        assert!(prompt.contains("## Soul"));
        assert!(prompt.contains("loyal companion who loves fetch"));
        assert!(!prompt.contains("Be genuinely helpful"));
    }

    #[test]
    fn test_no_identity_no_extra_lines() {
        let tools = ToolRegistry::new();
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!prompt.contains("Your name is"));
        assert!(!prompt.contains("The user's name is"));
        assert!(!prompt.contains("## Soul"));
    }

    #[test]
    fn test_workspace_files_injected_when_provided() {
        let tools = ToolRegistry::new();
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            Some("Follow workspace agent instructions."),
            Some("Prefer read-only tools first."),
            None,
            None,
        );
        assert!(prompt.contains("## Workspace Files"));
        assert!(prompt.contains("### AGENTS.md (workspace)"));
        assert!(prompt.contains("Follow workspace agent instructions."));
        assert!(prompt.contains("### TOOLS.md (workspace)"));
        assert!(prompt.contains("Prefer read-only tools first."));
    }

    #[test]
    fn test_runtime_context_injected_when_provided() {
        let tools = ToolRegistry::new();
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext {
                host: Some("moltis-devbox".into()),
                os: Some("macos".into()),
                arch: Some("aarch64".into()),
                shell: Some("zsh".into()),
                time: Some("2026-02-17 16:18:00 CET".into()),
                today: Some("2026-02-17".into()),
                provider: Some("openai".into()),
                model: Some("gpt-5".into()),
                session_key: Some("main".into()),
                surface: None,
                session_kind: None,
                channel_type: None,
                channel_account_id: None,
                channel_chat_id: None,
                channel_chat_type: None,
                data_dir: Some("/home/moltis/.moltis".into()),
                sudo_non_interactive: Some(true),
                sudo_status: Some("passwordless".into()),
                timezone: Some("Europe/Paris".into()),
                accept_language: Some("en-US,fr;q=0.9".into()),
                remote_ip: Some("203.0.113.42".into()),
                location: None,
            },
            sandbox: Some(PromptSandboxRuntimeContext {
                exec_sandboxed: true,
                mode: Some("all".into()),
                backend: Some("docker".into()),
                scope: Some("session".into()),
                image: Some("moltis-sandbox:abc123".into()),
                home: Some("/home/sandbox".into()),
                workspace_mount: Some("ro".into()),
                workspace_path: Some("/home/moltis/.moltis".into()),
                no_network: Some(true),
                session_override: Some(true),
            }),
        };

        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        assert!(prompt.contains("## Runtime"));
        assert!(prompt.contains("Host: host=moltis-devbox"));
        assert!(!prompt.contains("time=2026-02-17 16:18:00 CET"));
        assert!(prompt.contains("today=2026-02-17"));
        assert!(prompt.contains("The current user datetime is 2026-02-17 16:18:00 CET."));
        assert!(prompt.contains("provider=openai"));
        assert!(prompt.contains("model=gpt-5"));
        assert!(prompt.contains("data_dir=/home/moltis/.moltis"));
        assert!(prompt.contains("sudo_non_interactive=true"));
        assert!(prompt.contains("sudo_status=passwordless"));
        assert!(prompt.contains("timezone=Europe/Paris"));
        assert!(prompt.contains("accept_language=en-US,fr;q=0.9"));
        assert!(prompt.contains("remote_ip=203.0.113.42"));
        assert!(prompt.contains("Sandbox(exec): enabled=true"));
        assert!(prompt.contains("backend=docker"));
        assert!(prompt.contains("home=/home/sandbox"));
        assert!(prompt.contains("workspace_path=/home/moltis/.moltis"));
        assert!(prompt.contains("network=disabled"));
        assert!(prompt.contains("Execution routing:"));
        assert!(prompt.contains("`~` and relative paths resolve under"));
        assert!(prompt.contains("Sandbox/host routing changes are expected runtime behavior"));
    }

    #[test]
    fn test_runtime_context_includes_location_when_set() {
        let tools = ToolRegistry::new();
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext {
                host: Some("devbox".into()),
                location: Some("48.8566,2.3522".into()),
                ..Default::default()
            },
            sandbox: None,
        };

        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        assert!(prompt.contains("location=48.8566,2.3522"));
    }

    #[test]
    fn test_runtime_context_includes_channel_surface_fields_when_set() {
        let tools = ToolRegistry::new();
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext {
                session_key: Some("telegram:bot-main:123456".into()),
                surface: Some("telegram".into()),
                session_kind: Some("channel".into()),
                channel_type: Some("telegram".into()),
                channel_account_id: Some("bot-main".into()),
                channel_chat_id: Some("123456".into()),
                channel_chat_type: Some("private".into()),
                ..Default::default()
            },
            sandbox: None,
        };

        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        assert!(prompt.contains("surface=telegram"));
        assert!(prompt.contains("session_kind=channel"));
        assert!(prompt.contains("channel_type=telegram"));
        assert!(prompt.contains("channel_account=bot-main"));
        assert!(prompt.contains("channel_chat_id=123456"));
        assert!(prompt.contains("channel_chat_type=private"));
    }

    #[test]
    fn test_runtime_context_omits_location_when_none() {
        let tools = ToolRegistry::new();
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext {
                host: Some("devbox".into()),
                location: None,
                ..Default::default()
            },
            sandbox: None,
        };

        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        assert!(!prompt.contains("location="));
    }

    #[test]
    fn test_minimal_prompt_runtime_does_not_add_exec_routing_block() {
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext {
                host: Some("moltis-devbox".into()),
                ..Default::default()
            },
            sandbox: Some(PromptSandboxRuntimeContext {
                exec_sandboxed: false,
                ..Default::default()
            }),
        };

        let prompt = build_system_prompt_minimal_runtime(
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        assert!(prompt.contains("## Runtime"));
        assert!(prompt.contains("Host: host=moltis-devbox"));
        assert!(prompt.contains("Sandbox(exec): enabled=false"));
        assert!(!prompt.contains("Execution routing:"));
    }

    #[test]
    fn test_silent_replies_section_in_tool_prompt() {
        let tools = ToolRegistry::new();
        let prompt = build_system_prompt(&tools, true, None);
        assert!(prompt.contains("## Silent Replies"));
        assert!(prompt.contains("empty response"));
        assert!(prompt.contains("Do not call tools for greetings"));
        assert!(prompt.contains("`/sh `"));
        assert!(prompt.contains("run it with `exec` exactly as written"));
        assert!(prompt.contains("Do not express surprise about sandbox vs host execution"));
        assert!(!prompt.contains("__SILENT__"));
    }

    #[test]
    fn test_silent_replies_not_in_minimal_prompt() {
        let prompt =
            build_system_prompt_minimal_runtime(None, None, None, None, None, None, None, None);
        assert!(!prompt.contains("## Silent Replies"));
    }

    #[test]
    fn test_memory_text_injected_into_prompt() {
        let tools = ToolRegistry::new();
        let memory = "## User Facts\n- Lives in Paris\n- Speaks French";
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            Some(memory),
        );
        assert!(prompt.contains("## Long-Term Memory"));
        assert!(prompt.contains("Lives in Paris"));
        assert!(prompt.contains("Speaks French"));
        // Memory content should include the "already know" hint so models
        // don't ignore it when tool searches return empty.
        assert!(prompt.contains("memory bootstrap context"));
    }

    #[test]
    fn test_memory_text_truncated_at_limit() {
        let tools = ToolRegistry::new();
        let memory_budget = PromptBudgetsConfig::default().memory_bootstrap_max_chars;
        // Create content larger than configured default memory budget.
        let large_memory = "x".repeat(memory_budget + 500);
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&large_memory),
        );
        assert!(prompt.contains("## Long-Term Memory"));
        assert!(prompt.contains("MEMORY.md truncated"));
        // The full content should NOT be present
        assert!(!prompt.contains(&large_memory));
    }

    #[test]
    fn test_no_memory_section_without_memory_or_tools() {
        let tools = ToolRegistry::new();
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!prompt.contains("## Long-Term Memory"));
    }

    #[test]
    fn test_memory_text_in_minimal_prompt() {
        let memory = "## Notes\n- Important fact";
        let prompt = build_system_prompt_minimal_runtime(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(memory),
        );
        assert!(prompt.contains("## Long-Term Memory"));
        assert!(prompt.contains("Important fact"));
        // Minimal prompts have no tools, so no memory_search hint
        assert!(!prompt.contains("memory_search"));
    }

    /// Helper to create a [`ToolRegistry`] with one or more named stub tools.
    fn registry_with_tools(names: &[&'static str]) -> ToolRegistry {
        struct NamedStub(&'static str);
        #[async_trait::async_trait]
        impl crate::tool_registry::AgentTool for NamedStub {
            fn name(&self) -> &str {
                self.0
            }

            fn description(&self) -> &str {
                "stub"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }

            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
        }
        let mut reg = ToolRegistry::new();
        for name in names {
            reg.register(Box::new(NamedStub(name)));
        }
        reg
    }

    #[test]
    fn test_memory_save_hint_injected_when_tool_registered() {
        let tools = registry_with_tools(&["memory_save"]);
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(prompt.contains("## Long-Term Memory"));
        assert!(prompt.contains("MUST call `memory_save`"));
    }

    #[test]
    fn test_memory_save_hint_absent_without_tool() {
        let tools = ToolRegistry::new();
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(!prompt.contains("memory_save"));
    }

    #[test]
    fn test_memory_search_and_save_hints_both_present() {
        let tools = registry_with_tools(&["memory_search", "memory_save"]);
        let memory = "## User Facts\n- Likes coffee";
        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            None,
            Some(memory),
        );
        assert!(prompt.contains("## Long-Term Memory"));
        assert!(prompt.contains("Likes coffee"));
        assert!(prompt.contains("memory_search"));
        assert!(prompt.contains("MUST call `memory_save`"));
    }

    #[test]
    fn test_datetime_tail_appended_at_end_when_runtime_time_present() {
        let tools = ToolRegistry::new();
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext {
                time: Some("2026-02-17 16:18:00 CET".into()),
                ..Default::default()
            },
            sandbox: None,
        };

        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        let expected = "The current user datetime is 2026-02-17 16:18:00 CET.";
        assert!(prompt.contains(expected));
        assert!(prompt.trim_end().ends_with(expected));
    }

    #[test]
    fn test_datetime_tail_falls_back_to_today_when_time_missing() {
        let tools = ToolRegistry::new();
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext {
                today: Some("2026-02-17".into()),
                ..Default::default()
            },
            sandbox: None,
        };

        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        assert!(prompt.contains("The current user date is 2026-02-17."));
        assert!(
            prompt
                .trim_end()
                .ends_with("The current user date is 2026-02-17.")
        );
    }

    #[test]
    fn test_datetime_tail_not_injected_without_time_or_date() {
        let tools = ToolRegistry::new();
        let runtime = PromptRuntimeContext {
            host: PromptHostRuntimeContext::default(),
            sandbox: None,
        };

        let prompt = build_system_prompt_with_session_runtime(
            &tools,
            true,
            None,
            &[],
            None,
            None,
            None,
            None,
            None,
            Some(&runtime),
            None,
        );

        assert!(!prompt.contains("The current user datetime is "));
        assert!(!prompt.contains("The current user date is "));
    }

    // ── Phase 4: ModelFamily, compact schema, tool call guidance ────────

    #[test]
    fn model_family_detects_llama() {
        assert_eq!(
            ModelFamily::from_model_id("llama3.1:8b"),
            ModelFamily::Llama
        );
        assert_eq!(
            ModelFamily::from_model_id("meta-llama/Llama-3.3-70B"),
            ModelFamily::Llama,
        );
    }

    #[test]
    fn model_family_detects_qwen() {
        assert_eq!(ModelFamily::from_model_id("qwen2.5:7b"), ModelFamily::Qwen);
        assert_eq!(
            ModelFamily::from_model_id("Qwen/Qwen2.5-Coder-32B"),
            ModelFamily::Qwen,
        );
    }

    #[test]
    fn model_family_detects_mistral() {
        assert_eq!(
            ModelFamily::from_model_id("mistral:latest"),
            ModelFamily::Mistral,
        );
        assert_eq!(
            ModelFamily::from_model_id("mixtral-8x7b"),
            ModelFamily::Mistral,
        );
    }

    #[test]
    fn model_family_detects_others() {
        assert_eq!(
            ModelFamily::from_model_id("deepseek-coder-v2:16b"),
            ModelFamily::DeepSeek,
        );
        assert_eq!(ModelFamily::from_model_id("gemma:7b"), ModelFamily::Gemma);
        assert_eq!(ModelFamily::from_model_id("phi-3:mini"), ModelFamily::Phi);
    }

    #[test]
    fn model_family_unknown_for_unrecognized() {
        assert_eq!(ModelFamily::from_model_id("gpt-4o"), ModelFamily::Unknown,);
        assert_eq!(
            ModelFamily::from_model_id("claude-3-opus"),
            ModelFamily::Unknown,
        );
    }

    #[test]
    fn compact_schema_formats_required_and_optional_params() {
        let schema = serde_json::json!({
            "name": "exec",
            "description": "Run a shell command",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer"}
                },
                "required": ["command"]
            }
        });
        let out = format_compact_tool_schema(&schema);
        assert!(out.contains("### exec"));
        assert!(out.contains("Run a shell command"));
        assert!(out.contains("command (string, required)"));
        assert!(out.contains("timeout (integer)"));
    }

    #[test]
    fn compact_schema_no_params_section_when_empty() {
        let schema = serde_json::json!({
            "name": "noop",
            "description": "Does nothing",
            "parameters": {"type": "object", "properties": {}}
        });
        let out = format_compact_tool_schema(&schema);
        assert!(out.contains("### noop"));
        assert!(!out.contains("Params:"));
    }

    #[test]
    fn tool_call_guidance_includes_fenced_example() {
        let g = tool_call_guidance(Some("llama3.1:8b"));
        assert!(g.contains("```tool_call"));
        assert!(g.contains("\"tool\":"));
        assert!(g.contains("Example:"));
    }

    #[test]
    fn tool_call_guidance_works_with_no_model() {
        let g = tool_call_guidance(None);
        assert!(g.contains("## How to call tools"));
        assert!(g.contains("```tool_call"));
    }

    #[test]
    fn text_mode_prompt_uses_compact_schema() {
        let mut tools = ToolRegistry::new();
        struct ParamTool;
        #[async_trait::async_trait]
        impl crate::tool_registry::AgentTool for ParamTool {
            fn name(&self) -> &str {
                "exec"
            }

            fn description(&self) -> &str {
                "Run a shell command"
            }

            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "timeout": {"type": "integer"}
                    },
                    "required": ["command"]
                })
            }

            async fn execute(&self, _: serde_json::Value) -> anyhow::Result<serde_json::Value> {
                Ok(serde_json::json!({}))
            }
        }
        tools.register(Box::new(ParamTool));

        let prompt = build_system_prompt(&tools, false, None);
        // Text-mode should use compact format
        assert!(prompt.contains("### exec"));
        assert!(prompt.contains("Params: command (string, required)"));
        // Should include tool call guidance
        assert!(prompt.contains("## How to call tools"));
        assert!(prompt.contains("```tool_call"));
    }

    #[test]
    fn test_prepare_soul_sections_redistributes_marked_sections() {
        let soul = r#"
# SOUL.md

## Identity
I am calm and direct.

<!-- lane:agents -->
## Advanced Operating Principles
- Verify before claiming done.

## Vibe
No fluff.
"#;

        let prepared = prepare_soul_sections(soul);
        assert!(prepared.identity_soul_text.contains("## Identity"));
        assert!(prepared.identity_soul_text.contains("## Vibe"));
        assert!(
            !prepared
                .identity_soul_text
                .contains("## Advanced Operating Principles")
        );

        let redistributed = prepared
            .redistributed_agents_text
            .expect("operational sections should be redistributed");
        assert!(redistributed.contains("## Derived From SOUL.md"));
        assert!(redistributed.contains("## Advanced Operating Principles"));
        assert!(redistributed.contains("Verify before claiming done"));
    }

    #[test]
    fn test_prepare_soul_sections_distributes_without_loss() {
        let soul = r#"
# SOUL.md

## Identity
I am calm and direct.

<!-- lane:agents -->
## Work Style
- Understand -> Execute -> Verify -> Report.

<!-- lane:tools -->
## Routing Defaults
- Use browser for web tasks.

<!-- lane:heartbeat -->
## Proactive Disposition
- Run heartbeat checks hourly.

## Vibe
No fluff.
"#;

        let prepared = prepare_soul_sections(soul);

        // Non-operational sections remain in Soul.
        assert!(prepared.identity_soul_text.contains("## Identity"));
        assert!(
            prepared
                .identity_soul_text
                .contains("I am calm and direct.")
        );
        assert!(prepared.identity_soul_text.contains("## Vibe"));
        assert!(prepared.identity_soul_text.contains("No fluff."));

        // Execution/process sections are redistributed to workspace contexts.
        assert!(!prepared.identity_soul_text.contains("## Work Style"));
        assert!(!prepared.identity_soul_text.contains("## Routing Defaults"));
        assert!(
            !prepared
                .identity_soul_text
                .contains("## Proactive Disposition")
        );

        let redistributed_agents = prepared
            .redistributed_agents_text
            .as_deref()
            .expect("work style should be redistributed");
        let redistributed_tools = prepared
            .redistributed_tools_text
            .as_deref()
            .expect("routing defaults should be redistributed");
        let redistributed_heartbeat = prepared
            .redistributed_heartbeat_text
            .as_deref()
            .expect("proactive disposition should be redistributed");

        assert!(redistributed_agents.contains("## Work Style"));
        assert!(redistributed_agents.contains("Understand -> Execute -> Verify -> Report"));
        assert!(!redistributed_agents.contains("## Routing Defaults"));
        assert!(!redistributed_agents.contains("## Proactive Disposition"));

        assert!(redistributed_tools.contains("## Routing Defaults"));
        assert!(redistributed_tools.contains("Use browser for web tasks"));
        assert!(!redistributed_tools.contains("## Work Style"));
        assert!(!redistributed_tools.contains("## Proactive Disposition"));

        assert!(redistributed_heartbeat.contains("## Proactive Disposition"));
        assert!(redistributed_heartbeat.contains("Run heartbeat checks hourly"));
        assert!(!redistributed_heartbeat.contains("## Work Style"));
        assert!(!redistributed_heartbeat.contains("## Routing Defaults"));

        // Each redistributed heading appears exactly once across identity + derived blocks.
        let work_style_count = prepared.identity_soul_text.matches("## Work Style").count()
            + redistributed_agents.matches("## Work Style").count()
            + redistributed_tools.matches("## Work Style").count()
            + redistributed_heartbeat.matches("## Work Style").count();
        assert_eq!(work_style_count, 1);

        let routing_defaults_count = prepared
            .identity_soul_text
            .matches("## Routing Defaults")
            .count()
            + redistributed_agents.matches("## Routing Defaults").count()
            + redistributed_tools.matches("## Routing Defaults").count()
            + redistributed_heartbeat
                .matches("## Routing Defaults")
                .count();
        assert_eq!(routing_defaults_count, 1);

        let proactive_disposition_count = prepared
            .identity_soul_text
            .matches("## Proactive Disposition")
            .count()
            + redistributed_agents
                .matches("## Proactive Disposition")
                .count()
            + redistributed_tools
                .matches("## Proactive Disposition")
                .count()
            + redistributed_heartbeat
                .matches("## Proactive Disposition")
                .count();
        assert_eq!(proactive_disposition_count, 1);
    }

    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.is_empty() {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for idx in 0..items.len() {
            let mut rest = items.to_vec();
            let item = rest.remove(idx);
            for mut tail in permutations(&rest) {
                let mut combined = vec![item.clone()];
                combined.append(&mut tail);
                out.push(combined);
            }
        }
        out
    }

    #[test]
    fn test_prepare_soul_sections_permutations_preserve_one_to_one_section_placement() {
        let blocks: [(&str, &str); 4] = [
            (
                "<!-- lane:agents -->\n## Work Style\n- Understand -> Execute -> Verify -> Report.",
                "## Work Style",
            ),
            (
                "<!-- lane:tools -->\n## Routing Defaults\n- Use browser for web tasks.",
                "## Routing Defaults",
            ),
            (
                "<!-- lane:heartbeat -->\n## Proactive Disposition\n- Run heartbeat checks hourly.",
                "## Proactive Disposition",
            ),
            ("## Identity\nI am calm and direct.", "## Identity"),
        ];

        let all_orders = permutations(&blocks);
        assert_eq!(all_orders.len(), 24, "expected 4! permutations");

        for (idx, order) in all_orders.iter().enumerate() {
            let mut soul = String::from("# SOUL.md\n\n");
            for (block, _) in order {
                soul.push_str(block);
                soul.push_str("\n\n");
            }

            let prepared = prepare_soul_sections(&soul);
            let combined = [
                prepared.identity_soul_text.as_str(),
                prepared.redistributed_agents_text.as_deref().unwrap_or_default(),
                prepared.redistributed_tools_text.as_deref().unwrap_or_default(),
                prepared
                    .redistributed_heartbeat_text
                    .as_deref()
                    .unwrap_or_default(),
            ]
            .join("\n\n");

            for (_, heading) in blocks {
                let count = combined.matches(heading).count();
                assert_eq!(
                    count, 1,
                    "heading `{heading}` must appear exactly once in permutation {idx}"
                );
            }
        }
    }

    #[test]
    fn test_prepare_soul_sections_without_lane_markers_stays_in_soul_lane() {
        let soul = r#"
# SOUL.md

## Identity
I am calm and direct.

## Work Style
- Understand -> Execute -> Verify -> Report.

## Routing Defaults
- Use browser for web tasks.

## Proactive Disposition
- Run heartbeat checks hourly.
"#;

        let prepared = prepare_soul_sections(soul);
        assert!(prepared.identity_soul_text.contains("## Work Style"));
        assert!(prepared.identity_soul_text.contains("## Routing Defaults"));
        assert!(
            prepared
                .identity_soul_text
                .contains("## Proactive Disposition")
        );
        assert!(prepared.redistributed_agents_text.is_none());
        assert!(prepared.redistributed_tools_text.is_none());
        assert!(prepared.redistributed_heartbeat_text.is_none());
    }

    #[test]
    fn test_prepare_soul_sections_fails_on_invalid_marker() {
        let soul = r#"
# SOUL.md

<!-- lane:invalid_lane -->
## Personal Signature
- Verify before claiming done.
"#;
        let diagnostics =
            super::prepare_soul_sections(soul).expect_err("invalid lane marker should fail");
        assert!(
            diagnostics
                .issues
                .iter()
                .any(|issue| issue.code == SoulRoutingIssueCode::InvalidSoulMarker)
        );
    }

    #[test]
    fn test_prepare_soul_sections_fails_on_orphan_marker() {
        let soul = r#"
# SOUL.md

## Identity
I am calm and direct.

<!-- lane:agents -->
"#;
        let diagnostics =
            super::prepare_soul_sections(soul).expect_err("orphan marker should fail");
        assert!(
            diagnostics
                .issues
                .iter()
                .any(|issue| issue.code == SoulRoutingIssueCode::OrphanSoulMarker)
        );
    }

    #[test]
    fn test_prepare_soul_sections_fails_on_duplicate_heading_in_multiple_lanes() {
        let soul = r#"
# SOUL.md

<!-- lane:agents -->
## Work Style
- lane one

<!-- lane:tools -->
## Work Style
- lane two
"#;
        let diagnostics = super::prepare_soul_sections(soul)
            .expect_err("duplicate cross-lane section headings should fail");
        assert!(
            diagnostics
                .issues
                .iter()
                .any(|issue| issue.code == SoulRoutingIssueCode::DuplicateSoulSection)
        );
    }

    #[test]
    fn test_prompt_merges_existing_workspace_files_with_redistributed_soul_sections() {
        let tools = ToolRegistry::new();
        let identity = AgentIdentity {
            name: Some("Momo".into()),
            ..Default::default()
        };
        let soul = r#"
# SOUL.md

## Identity
I value truth.

<!-- lane:agents -->
## Work Style
- Understand -> Execute -> Verify -> Report.

<!-- lane:tools -->
## Routing Defaults
- Use browser for web tasks.

<!-- lane:heartbeat -->
## Proactive Disposition
- Run heartbeat checks hourly.
"#;
        let prompt = build_system_prompt_with_session_runtime_workspace(
            &tools,
            true,
            None,
            &[],
            Some(&identity),
            None,
            Some(soul),
            Some("### Existing AGENTS\n- Ask before destructive actions."),
            Some("### Existing TOOLS\n- Prefer read-only tools first."),
            Some("### Existing HEARTBEAT\n- Keep cycles tight."),
            None,
            None,
        );

        assert!(prompt.contains("### Existing AGENTS"));
        assert!(prompt.contains("### Existing TOOLS"));
        assert!(prompt.contains("### Existing HEARTBEAT"));
        assert!(prompt.contains("## Derived From SOUL.md (Operational Rules)"));
        assert!(prompt.contains("## Derived From SOUL.md (Tooling Rules)"));
        assert!(prompt.contains("## Derived From SOUL.md (Heartbeat/Proactivity Rules)"));
        assert!(prompt.contains("## Work Style"));
        assert!(prompt.contains("## Routing Defaults"));
        assert!(prompt.contains("## Proactive Disposition"));
        assert!(prompt.contains("## Soul\n\n# SOUL.md"));
        assert!(prompt.contains("Understand -> Execute -> Verify -> Report"));
        assert!(!prompt.contains("<!-- lane:"));

        // No duplicated headings after redistribution.
        assert_eq!(prompt.matches("## Work Style").count(), 1);
        assert_eq!(prompt.matches("## Routing Defaults").count(), 1);
        assert_eq!(prompt.matches("## Proactive Disposition").count(), 1);
    }

    #[test]
    fn test_custom_prompt_budgets_apply_to_soul_workspace_and_memory() {
        let tools = ToolRegistry::new();
        let identity = AgentIdentity {
            name: Some("Momo".into()),
            ..Default::default()
        };
        let budgets = PromptBudgetsConfig {
            soul_max_chars: 60,
            project_context_max_chars: 80,
            workspace_file_max_chars: 50,
            memory_bootstrap_max_chars: 70,
        };
        let prompt = build_system_prompt_with_session_runtime_workspace_budgets(
            &tools,
            true,
            Some("P".repeat(300).as_str()),
            &[],
            Some(&identity),
            None,
            Some("S".repeat(300).as_str()),
            Some("A".repeat(300).as_str()),
            Some("T".repeat(300).as_str()),
            Some("H".repeat(300).as_str()),
            Some(&budgets),
            None,
            Some("M".repeat(300).as_str()),
        );

        assert!(prompt.contains("SOUL.md truncated for prompt size"));
        assert!(prompt.contains("Project context truncated for prompt size"));
        assert!(prompt.contains("AGENTS.md truncated for prompt size"));
        assert!(prompt.contains("TOOLS.md truncated for prompt size"));
        assert!(prompt.contains("HEARTBEAT.md truncated for prompt size"));
        assert!(prompt.contains("MEMORY.md truncated"));
    }

    #[test]
    fn test_truncate_prompt_text_keeps_whole_markdown_sections() {
        let text = r#"
# SOUL.md

## Core Truths
- Rule A
- Rule B

## Style
- Keep it concise.
"#;
        // Fits preamble + first section, but not the second section.
        let truncated = truncate_prompt_text(text, 45);
        assert!(truncated.contains("## Core Truths"));
        assert!(!truncated.contains("## Style"));
        assert!(!truncated.ends_with("..."));
    }

    #[test]
    fn test_collect_prompt_section_truncations_reports_all_over_budget_sections() {
        let budgets = PromptBudgetsConfig {
            soul_max_chars: 40,
            project_context_max_chars: 30,
            workspace_file_max_chars: 20,
            memory_bootstrap_max_chars: 25,
        };
        let truncations = collect_prompt_section_truncations(
            Some("P".repeat(100).as_str()),
            Some(
                r#"
# SOUL.md

## Identity
I am calm and direct.

<!-- lane:agents -->
## Work Style
- Understand -> Execute -> Verify -> Report.
"#,
            ),
            Some("A".repeat(100).as_str()),
            Some("T".repeat(100).as_str()),
            Some("H".repeat(100).as_str()),
            Some("M".repeat(100).as_str()),
            Some(&budgets),
        );
        let sections: Vec<&str> = truncations
            .iter()
            .map(|item| item.section.as_str())
            .collect();
        assert!(sections.contains(&"soul"));
        assert!(sections.contains(&"project_context"));
        assert!(sections.contains(&"agents_md"));
        assert!(sections.contains(&"tools_md"));
        assert!(sections.contains(&"heartbeat_md"));
        assert!(sections.contains(&"memory_bootstrap"));
    }
}
