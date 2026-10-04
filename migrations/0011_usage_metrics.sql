ALTER TABLE request_history ADD COLUMN cached_tokens INTEGER
    CHECK (cached_tokens IS NULL OR (cached_tokens BETWEEN 0 AND 1000000000));
ALTER TABLE request_history ADD COLUMN cost_usd REAL
    CHECK (cost_usd IS NULL OR (cost_usd >= 0 AND cost_usd <= 1000000));
ALTER TABLE request_attempts ADD COLUMN provider_slug TEXT
    CHECK (provider_slug IS NULL OR (length(provider_slug) <= 64 AND provider_slug <> '' AND provider_slug NOT GLOB '*[^a-z0-9-]*'));
ALTER TABLE request_attempts ADD COLUMN provider_group_name TEXT
    CHECK (provider_group_name IS NULL OR length(provider_group_name) <= 128);
-- Additive, metadata-only metrics. Old rows stay unknown; historical pricing
-- and provider identities must not be fabricated from current configuration.
