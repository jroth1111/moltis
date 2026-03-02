CREATE TABLE IF NOT EXISTS pending_approvals (
    id          TEXT    PRIMARY KEY,
    session_key TEXT    NOT NULL,
    tool_name   TEXT    NOT NULL,
    arguments   TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_approvals_session ON pending_approvals(session_key);
CREATE INDEX IF NOT EXISTS idx_approvals_expiry ON pending_approvals(expires_at);
