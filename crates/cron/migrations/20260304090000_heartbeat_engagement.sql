-- Heartbeat engagement tracking metadata for cron deliveries.
-- Owned by: moltis-cron crate

ALTER TABLE cron_runs
    ADD COLUMN delivery_channel TEXT;

ALTER TABLE cron_runs
    ADD COLUMN delivery_to TEXT;

ALTER TABLE cron_runs
    ADD COLUMN delivered_at_ms INTEGER;

ALTER TABLE cron_runs
    ADD COLUMN user_responded INTEGER NOT NULL DEFAULT 0;

ALTER TABLE cron_runs
    ADD COLUMN user_response_at_ms INTEGER;
