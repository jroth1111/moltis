CREATE TABLE IF NOT EXISTS tinder_matches (
    id              TEXT    PRIMARY KEY,
    name            TEXT    NOT NULL,
    funnel_state    TEXT    NOT NULL DEFAULT 'matched',
    exchange_count  INTEGER NOT NULL DEFAULT 0,
    last_message_ts INTEGER,
    notes           TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tinder_funnel ON tinder_matches(funnel_state);
