-- Opt-in passive cooldown for targets that just failed, and the SHA-256 of
-- the Prometheus scrape token. The token itself is shown once, never stored.
ALTER TABLE dashboard_settings ADD COLUMN target_cooldown INTEGER NOT NULL DEFAULT 0
    CHECK (target_cooldown IN (0, 1));
ALTER TABLE dashboard_settings ADD COLUMN metrics_token_hash TEXT
    CHECK (metrics_token_hash IS NULL OR length(metrics_token_hash) = 64);
