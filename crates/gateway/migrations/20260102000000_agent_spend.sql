CREATE TABLE IF NOT EXISTS agent_spend (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    date   TEXT    NOT NULL,
    model  TEXT    NOT NULL,
    cost   REAL    NOT NULL,
    ts     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_spend_date ON agent_spend(date);
