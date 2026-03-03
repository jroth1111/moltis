# Memory Lifecycle and Controls

This page documents how Moltis memory is produced, indexed, and reused across sessions.
All automated writes are local markdown files that are re-indexed into SQLite.

## Lifecycle Overview

Moltis has four local memory flows:

1. **Session export**: optional transcript export on session reset/new boundaries.
2. **Compaction facts**: facts extracted during context compaction.
3. **Turn-level auto-extract**: optional fact extraction after assistant turns.
4. **Auto-reconcile**: optional periodic dedupe/update pipeline for machine-managed facts.

## File Artifacts

| Flow | Artifact | Notes |
|---|---|---|
| Session export | `memory/sessions/*.md` | Hook-driven transcript snapshots |
| Compaction | `memory/compaction-facts-<session>-<timestamp>.md` | One facts artifact per compaction run |
| Auto-extract staging | `memory/auto-YYYY-MM-DD-facts.md` | Daily append-only staging file |
| Auto-reconcile canonical | `memory/auto-facts.md` | Machine-managed canonical fact set |
| Auto-reconcile audit | `memory/reconciliation-log.md` | Append-only reconcile decision log |

`MEMORY.md` remains user-managed in this phase. Auto-reconcile does not edit it.

## Session Export

Enable/disable in config:

```toml
[memory]
session_export = true
```

When `session_export = false`, the built-in `session-memory` hook is not registered and is marked disabled in hook discovery metadata.

## Compaction Facts

During context compaction, Moltis runs a silent memory turn and writes one consolidated facts artifact per run:

- `memory/compaction-facts-<session>-<timestamp>.md`

This replaces per-fact file fan-out and keeps indexing/search behavior unchanged.

## Turn-Level Auto-Extract

Auto-extract is opt-in and non-blocking for user responses.

```toml
[memory]
auto_extract = false
auto_extract_min_chars = 120
auto_extract_debounce_ms = 30000
auto_extract_max_facts = 8
auto_extract_model_id = "openai::gpt-4.1-mini" # optional
```

Behavior:

1. Runs only after assistant response persistence succeeds.
2. Skips when disabled, under min chars, inside debounce window, extraction already in-flight, or memory manager unavailable.
3. Uses `auto_extract_model_id` if available; otherwise falls back to the current session model.
4. Extracts strict JSON facts, dedupes in-memory, caps to `auto_extract_max_facts`.
5. Appends extracted entries to `memory/auto-YYYY-MM-DD-facts.md`.

## Auto-Reconcile

Auto-reconcile is opt-in and time-gated with persisted scheduler state.

```toml
[memory]
auto_reconcile = false
auto_reconcile_min_interval_secs = 900
auto_reconcile_similarity_threshold = 0.95
```

Behavior:

1. Scheduler state is persisted in SQLite `memory_state` using key `auto_reconcile_last_ts::<agent_id>`.
2. Reconcile runs only when min interval has elapsed since last successful run.
3. Scope is machine-managed facts only (`memory/auto-*.md`, canonical `memory/auto-facts.md`).
4. Fast path skips near-duplicates via similarity threshold.
5. LLM decision actions: `ADD`, `UPDATE`, `DELETE`, `SKIP`.
6. Canonical facts are rewritten atomically; audit record appended to `memory/reconciliation-log.md`.
7. Any reconcile parse/apply/provider failure is fail-closed: no canonical mutation.

## Reranking in Memory Search

When enabled, memory search applies LLM reranking in the tool search pipeline and falls back to hybrid ranking on rerank errors.

```toml
[memory]
llm_reranking = false
```

## Rollout and Rollback

Suggested rollout:

1. Start with defaults (`auto_extract = false`, `auto_reconcile = false`).
2. Enable `auto_extract` first in low-traffic environments.
3. Enable `auto_reconcile` only after extraction quality/cost is stable.

Immediate rollback:

```toml
[memory]
auto_extract = false
auto_reconcile = false
```

With those flags off, no new turn-level extraction or reconcile runs occur.

## Observability Metrics

Auto-extract:

- `moltis_memory_auto_extract_attempts_total`
- `moltis_memory_auto_extract_skips_total{reason=...}`
- `moltis_memory_auto_extract_success_total`
- `moltis_memory_auto_extract_failures_total`
- `moltis_memory_auto_extract_duration_seconds`

Auto-reconcile:

- `moltis_memory_auto_reconcile_attempts_total`
- `moltis_memory_auto_reconcile_success_total`
- `moltis_memory_auto_reconcile_failures_total`
- `moltis_memory_auto_reconcile_duration_seconds`

Reranking:

- `moltis_memory_rerank_attempts_total`
- `moltis_memory_rerank_failures_total`
- `moltis_memory_rerank_latency_seconds`
