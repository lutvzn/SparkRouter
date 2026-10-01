-- Last successful model list for a connection. Routing suggestions read this
-- snapshot; it is refreshed only when the dashboard asks, never in the background.
CREATE TABLE connection_model_lists (
    provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    refreshed_at TEXT NOT NULL,
    truncated INTEGER NOT NULL CHECK (truncated IN (0, 1))
);

CREATE TABLE connection_models (
    provider_id TEXT NOT NULL REFERENCES connection_model_lists(provider_id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    model_id TEXT NOT NULL,
    PRIMARY KEY (provider_id, position),
    UNIQUE (provider_id, model_id)
);
