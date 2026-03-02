-- moltis-tasks: Task orchestration tables
--
-- tasks: One row per task. spec_json and runtime_json are opaque blobs
-- (serde_json serializations of TaskSpec and TaskRuntime). version is the
-- optimistic-concurrency counter; UPDATE ... WHERE version = :expected enforces CAS.
CREATE TABLE IF NOT EXISTS tasks (
    id           TEXT    NOT NULL,
    list_id      TEXT    NOT NULL,
    spec_json    TEXT    NOT NULL,
    runtime_json TEXT    NOT NULL,
    blocked_by   TEXT    NOT NULL DEFAULT '[]',
    version      INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (list_id, id)
);

CREATE INDEX IF NOT EXISTS tasks_list_id       ON tasks (list_id);
CREATE INDEX IF NOT EXISTS tasks_updated_at    ON tasks (updated_at DESC);

-- task_events: Append-only event ledger per task.
CREATE TABLE IF NOT EXISTS task_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id    TEXT    NOT NULL,
    list_id    TEXT    NOT NULL,
    event_type TEXT    NOT NULL,
    from_state TEXT    NOT NULL,
    to_state   TEXT    NOT NULL,
    agent_id   TEXT,
    detail     TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS task_events_task    ON task_events (list_id, task_id, id);
CREATE INDEX IF NOT EXISTS task_events_recent  ON task_events (list_id, created_at DESC);
