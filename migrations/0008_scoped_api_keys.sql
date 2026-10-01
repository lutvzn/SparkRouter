-- Existing keys retain unrestricted, non-expiring access. Empty selected
-- allowlists deny all models; they must never imply unrestricted access.
ALTER TABLE api_keys ADD COLUMN restricted INTEGER NOT NULL DEFAULT 0 CHECK (restricted IN (0, 1));
ALTER TABLE api_keys ADD COLUMN expires_at INTEGER CHECK (expires_at IS NULL OR expires_at > 0);

CREATE TABLE api_key_models (
    key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 256),
    PRIMARY KEY (key_id, model)
);
