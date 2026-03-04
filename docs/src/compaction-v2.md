# Compaction V2

Compaction V2 replaces blind truncation with a layered, importance-aware policy.

## Layer Model

- **Layer 0 (Critical anchors)**: tool call/result chains, explicit decisions, code-heavy messages.
- **Layer 1 (Working memory)**: recent verbatim turns used for active task continuity.
- **Layer 2 (Background history)**: older resolved turns that are summarized or reduced first.

Compaction always prioritizes preserving Layer 0 before reducing Layer 1/2.

## Triggers

`[chat.compaction]` controls thresholds and budgets:

```toml
[chat.compaction]
enabled = true
soft_trigger_percent = 80
hard_trigger_percent = 90
emergency_trigger_percent = 95
verbatim_turns = 10
min_verbatim_turns = 6
anchor_budget_tokens = 5000
summary_budget_tokens = 2500
```

Behavior by threshold:

- **Soft**: summarize Layer 2, preserve anchors + working turns.
- **Hard**: reduce Layer 2 and shrink Layer 1 toward `min_verbatim_turns`.
- **Emergency**: preserve critical anchors and minimal Layer 1, drop low-importance non-anchors first.

Soft/overflow summaries are structured into fixed sections for deterministic shape:

- `Decisions`
- `State`
- `Open Items`
- `Constraints`
- `Artifacts`

If there is no compressible background gap, V2 does not summarize anchors as fallback; it keeps anchors/working context verbatim.

## Overflow Retry

Context-overflow retry compaction uses the same anchor-preserving summary engine as normal compaction, so tool/result continuity and anchor rules are consistent across both paths.

## Telemetry

`auto_compact` events include:

- `reason`
- `layerStats`
- `anchorCount`
- `summaryChars`
- `messagesRemoved`
- `messagesKept`

Compaction stages (`plan`, `summarize`, `apply`) are traced and emitted as duration metrics.

## Security

Compaction artifacts written to `memory/` are redacted before persistence:

- Assignment-style secrets (`*_API_KEY=...`, `token: ...`, etc.)
- Bearer tokens (`Authorization: Bearer ...`)

## Out of Scope

Archive retrieval via external RAG is intentionally deferred to a later phase. V2 focuses on in-context retention quality and deterministic layered reduction.
