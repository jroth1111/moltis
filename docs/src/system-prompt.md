# System Prompt Architecture

The system prompt sent to the LLM is assembled dynamically from multiple
components. Each piece is optional and loaded only when relevant, keeping the
prompt compact while adapting to the current session context.

## Assembly Order

The prompt is built in `crates/agents/src/prompt.rs` by
`build_system_prompt_full()`. Components are appended in this order:

1. **Base introduction** — one-liner announcing tool access (or not)
2. **Agent identity** — name, emoji, creature, vibe from `IDENTITY.md`
3. **Soul** — personality directives from `SOUL.md` (or built-in default)
4. **User profile** — user's name from `USER.md`
5. **Project context** — `CLAUDE.md` / `CLAUDE.local.md` / `.claude/rules/*.md`
   walked up the directory tree
6. **Runtime context** — host info, sandbox config, execution routing hints
7. **Skills listing** — available skills as XML block
8. **Workspace files** — `AGENTS.md`, `TOOLS.md`, and `HEARTBEAT.md` from the
   data directory (plus any SOUL sections explicitly routed there)
9. **Long-term memory hint** — added when memory tools are registered
10. **Tool schemas** — compact list (native) or full JSON (fallback)
11. **Tool-calling format** — JSON block instructions (fallback providers only)
12. **Guidelines** — tool usage guidance, silent reply protocol

## Persona Precedence Matrix

Prompt persona files are resolved with a consistent cascade:

| File | Resolution order |
|------|------------------|
| `IDENTITY.md` | `agents/<id>/IDENTITY.md` -> root `IDENTITY.md` |
| `SOUL.md` | `agents/<id>/SOUL.md` -> root `SOUL.md` -> built-in `DEFAULT_SOUL` |
| `USER.md` | `agents/<id>/USER.md` -> root `USER.md` |
| `AGENTS.md` | `agents/<id>/AGENTS.md` -> root `AGENTS.md` |
| `TOOLS.md` | `agents/<id>/TOOLS.md` -> root `TOOLS.md` |
| `HEARTBEAT.md` | `agents/<id>/HEARTBEAT.md` -> root `HEARTBEAT.md` |
| `MEMORY.md` | `agents/<id>/MEMORY.md` -> root `MEMORY.md` |

`agent_id` must map to a known workspace (`main` or existing `agents/<id>/`).
Unknown IDs fail fast instead of silently falling back to `main`.

## Components in Detail

### Base Introduction

A single sentence that sets the assistant role:

- With tools: *"You are a helpful assistant. You can use tools when needed."*
- Without tools: *"You are a helpful assistant. Answer questions clearly and
  concisely."*

### Agent Identity (`IDENTITY.md`)

Loaded from `~/.moltis/IDENTITY.md` using YAML frontmatter. Fields:

| Field | Prompt output |
|-------|---------------|
| `name` + `emoji` | "Your name is {name} {emoji}." |
| `theme` | "Your theme: {theme}." |

All fields are optional. When identity is present, the soul section is always
included.

### Soul (`SOUL.md`)

Loaded from `~/.moltis/SOUL.md`. When the file is absent or empty, the
built-in `DEFAULT_SOUL` is used. The default is sourced from
[OpenClaw's SOUL.md template](https://github.com/openclaw/openclaw/blob/main/docs/reference/templates/SOUL.md):

> **SOUL.md - Who You Are**
>
> _You're not a chatbot. You're becoming someone._
>
> **Core Truths**
>
> **Be genuinely helpful, not performatively helpful.** Skip the "Great
> question!" and "I'd be happy to help!" — just help. Actions speak louder
> than filler words.
>
> **Have opinions.** You're allowed to disagree, prefer things, find stuff
> amusing or boring. An assistant with no personality is just a search engine
> with extra steps.
>
> **Be resourceful before asking.** Try to figure it out. Read the file. Check
> the context. Search for it. _Then_ ask if you're stuck. The goal is to come
> back with answers, not questions.
>
> **Earn trust through competence.** Your human gave you access to their stuff.
> Don't make them regret it. Be careful with external actions (emails, tweets,
> anything public). Be bold with internal ones (reading, organizing, learning).
>
> **Remember you're a guest.** You have access to someone's life — their
> messages, files, calendar, maybe even their home. That's intimacy. Treat it
> with respect.
>
> **Boundaries** — Private things stay private. When in doubt, ask before
> acting externally. Never send half-baked replies to messaging surfaces.
> You're not the user's voice — be careful in group chats.
>
> **Vibe** — Be the assistant you'd actually want to talk to. Concise when
> needed, thorough when it matters. Not a corporate drone. Not a sycophant.
> Just... good.
>
> **Continuity** — Each session, you wake up fresh. These files _are_ your
> memory. Read them. Update them. They're how you persist. If you change this
> file, tell the user — it's your soul, and they should know.

The default soul is currently ~18,357 characters and should be treated as a
large baseline prompt component.

`SOUL.md` redistribution uses explicit lane markers. Use HTML comments to route
a section to a workspace lane:

- `<!-- lane:agents -->`
- `<!-- lane:tools -->`
- `<!-- lane:heartbeat -->`
- `<!-- lane:soul -->`

Place markers immediately before a `##` section (or inline on the same line).
The marker is stripped from prompt output. Routed content is removed from
`## Soul` and injected once into the target workspace block (no duplication).
Invalid/orphan markers are ignored and produce warnings.

Routing precedence is:

1. explicit marker
2. default `soul` lane

### User Profile (`USER.md`)

Loaded from `~/.moltis/USER.md` using YAML frontmatter.

- `name` is injected as: *"The user's name is {name}."*
- `timezone` is used by runtime context to localize `Host: time=...` and
  `Host: today=...` fields.

For channel-bound sessions that are detected as non-private surfaces (Telegram
groups/channels and WhatsApp groups), private persona data (`USER.md` name and
`MEMORY.md` bootstrap text) is not injected by default.

### Project Context

Resolved by `moltis_projects::context::load_context_files()`. The loader walks
from the project directory upward to the filesystem root, collecting:

- `CLAUDE.md`
- `CLAUDE.local.md`
- `.claude/rules/*.md`
- `AGENTS.md`

Files are merged outermost-first (root before project directory), so
project-specific instructions override workspace-level ones.

### Runtime Context

Injected as compact key=value lines under a `## Runtime` heading:

```
Host: host=moltis-devbox | os=macos | arch=aarch64 | shell=zsh | time=2026-02-17 16:18:00 CET | today=2026-02-17 | provider=openai | model=gpt-5 | session=main | sudo_non_interactive=true | timezone=Europe/Paris
Sandbox(exec): enabled=true | mode=all | backend=docker | scope=session | image=moltis-sandbox:abc123 | workspace_mount=ro | network=disabled
```

When tools are included, an **Execution routing** block explains how `exec`
routes commands between sandbox and host.

The runtime context is populated at request time in `chat.rs` by detecting:

- Host name, OS, architecture, shell
- Active LLM provider and model
- Session key
- Sudo availability
- Timezone and accept-language from the browser
- Geolocation (from browser or `USER.md`)
- Sandbox configuration from the sandbox router

### Skills

When skills are registered, they are listed as an XML block generated by
`moltis_skills::prompt_gen::generate_skills_prompt()`:

```xml
## Available Skills
<available_skills>
<skill name="commit" source="skill" path="/skills/commit">
Create git commits
</skill>
</available_skills>
```

### Workspace Files

Optional markdown files from the data directory (`~/.moltis/`):

- **AGENTS.md** — workspace-level agent instructions
- **TOOLS.md** — tool preferences and guidance
- **HEARTBEAT.md** — proactive cadence and wake-up policy

Each is rendered under `## Workspace Files` with its own `###` subheading.
Leading HTML comments (`<!-- ... -->`) are stripped before injection.

If `SOUL.md` sections are redistributed, they are moved without duplication:

- `## Derived From SOUL.md (Operational Rules)` inside the AGENTS workspace block
- `## Derived From SOUL.md (Tooling Rules)` inside the TOOLS workspace block
- `## Derived From SOUL.md (Heartbeat/Proactivity Rules)` inside the HEARTBEAT workspace block

## Prompt Budgets

Per-section character budgets are configurable in `moltis.toml`:

```toml
[chat.prompt_budgets]
soul_max_chars = 20000
project_context_max_chars = 8000
workspace_file_max_chars = 6000
memory_bootstrap_max_chars = 8000
```

When a section exceeds its budget, prompt assembly truncates it and appends a
truncation notice in the prompt. The runtime also logs a warning with section
name and original size.

Debug endpoints include this as structured metadata:

- `personaDiagnostics`: health-check warnings on the assembled persona
- `truncations`: section-level budget overflow details
- `policyDecisions`: deterministic policy decisions for surface/privacy gating

### Tool Schemas

How tools are described depends on whether the provider supports native
tool calling:

- **Native tools** (`native_tools=true`): compact one-liner per tool with
  description truncated to 160 characters. Full JSON schemas are sent via the
  provider's tool-calling API.
- **Fallback** (`native_tools=false`): full JSON parameter schemas are inlined
  in the prompt, followed by instructions for emitting `tool_call` JSON blocks.

### Guidelines and Silent Replies

The final section contains:

- Tool usage guidelines (conversation first, when to use exec/browser, `/sh`
  explicit shell prefix)
- A reminder not to parrot raw tool output
- **Silent reply protocol**: when tool output speaks for itself, the LLM should
  return an empty response rather than acknowledging it

## Entry Points

| Function | Use case |
|----------|----------|
| `build_system_prompt()` | Simple: tools + optional project context |
| `build_system_prompt_with_session_runtime()` | Full: identity, soul, user, skills, runtime, tools |
| `build_system_prompt_with_session_runtime_workspace()` | Full + explicit workspace `HEARTBEAT.md` text |
| `build_system_prompt_with_session_runtime_workspace_budgets()` | Full + explicit workspace text + custom budgets |
| `build_system_prompt_minimal_runtime()` | No tools (e.g. title generation, summaries) |
| `build_system_prompt_minimal_runtime_workspace()` | No tools + explicit workspace `HEARTBEAT.md` text |
| `build_system_prompt_minimal_runtime_workspace_budgets()` | No tools + explicit workspace text + custom budgets |

## Size Estimates

| Configuration | ~Characters | ~Tokens |
|---------------|-------------|---------|
| Minimal (no tools, no context) | 200 | 50 |
| Soul + identity + guidelines | 2,000 | 500 |
| Typical with tools | 5,000 | 1,250 |
| Full (tools + project context + skills) | 7,000-10,000 | 1,750-2,500 |
| Large (many MCP tools + full context) | 12,000-15,000 | 3,000-3,750 |

A typical session with a few tools and project context lands around **6k
characters (~1,500 tokens)**, which is well within normal range for production
agents (most use 2k-8k tokens for their system prompt).

The biggest variable-size contributors are **tool schemas** (especially with
many MCP servers) and **project context** (deep directory hierarchies with
multiple `CLAUDE.md` files). These are worth auditing if prompt costs are a
concern.

## File Locations

```
~/.moltis/
├── IDENTITY.md          # Agent identity (name, emoji, theme)
├── SOUL.md              # Personality directives
├── USER.md              # User profile (name, timezone, location)
├── AGENTS.md            # Workspace agent instructions
├── TOOLS.md             # Tool preferences
└── HEARTBEAT.md         # Proactive cadence policy

<project>/
├── CLAUDE.md            # Project instructions
├── CLAUDE.local.md      # Local overrides (gitignored)
└── .claude/rules/*.md   # Additional rule files
```

## Key Source Files

- `crates/agents/src/prompt.rs` — prompt assembly logic and `DEFAULT_SOUL`
- `crates/chat/src/lib.rs` — `load_prompt_persona_for_*()`, runtime context
  detection, project context resolution, persona privacy gating
- `crates/config/src/loader.rs` — file loading (`load_soul()`,
  `load_agents_md()`, `load_identity()`, etc.)
- `crates/projects/src/context.rs` — `CLAUDE.md` hierarchy walker
- `crates/skills/src/prompt_gen.rs` — skills XML generation
