-- task_outputs: Per-shift execution output for context injection into subsequent shifts
-- and TTL-based cleanup.
--
-- Rows are written once (at shift finalization) and never mutated.  The
-- `output` column is capped at 65 536 bytes on insert.  The `created_at`
-- index supports efficient TTL sweeps.
CREATE TABLE IF NOT EXISTS task_outputs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    intent_id     TEXT    NOT NULL,
    shift_id      TEXT    NOT NULL,
    list_id       TEXT    NOT NULL,
    shift_num     INTEGER NOT NULL,
    output        TEXT    NOT NULL,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

-- Most-recent N outputs for an intent (context injection).
CREATE INDEX IF NOT EXISTS task_outputs_intent
    ON task_outputs (intent_id, shift_num DESC);

-- TTL sweep: delete rows older than retention threshold.
CREATE INDEX IF NOT EXISTS task_outputs_ttl
    ON task_outputs (created_at ASC);
