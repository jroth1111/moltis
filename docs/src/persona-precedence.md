# Persona Precedence

This document is the source-of-truth for persona file resolution and prompt
assembly precedence.

## Fallback Cascade

All persona files use the same runtime cascade:

1. `~/.moltis/agents/<id>/<FILE>`
2. `~/.moltis/<FILE>`

For `SOUL.md` only, if both are missing the built-in `DEFAULT_SOUL` is used.

## File Matrix

| File | Resolution |
|------|------------|
| `IDENTITY.md` | agent -> root |
| `SOUL.md` | agent -> root -> default |
| `USER.md` | agent -> root |
| `AGENTS.md` | agent -> root |
| `TOOLS.md` | agent -> root |
| `HEARTBEAT.md` | agent -> root |
| `MEMORY.md` | agent -> root |

## Agent Resolution

- Missing/empty `agent_id` resolves to `main`.
- Unknown non-empty `agent_id` is an error (no silent fallback).

## Privacy Policy

Private persona data (`USER.md` personal fields and `MEMORY.md` bootstrap) is
injected only on private surfaces.

- Telegram: private chats only.
- WhatsApp: non-`@g.us` chats only.
- Discord/Teams: only when chat type is explicitly `private`.
- Unclassified channel surfaces: treated as non-private (fail-safe).

## SOUL Routing Precedence

SOUL section routing precedence:

1. Explicit lane marker (`<!-- lane:agents|tools|heartbeat|soul -->`)
2. Default `soul` lane

With `chat.deterministic_policy.strict_soul_routing = true` (default), invalid
or orphan lane markers are treated as hard errors.

## Prompt Budgets

`[chat.prompt_budgets]` controls max character budgets for major prompt
sections. Over-budget sections are truncated, marked in the prompt, and emitted
as truncation metadata in debug endpoints.
