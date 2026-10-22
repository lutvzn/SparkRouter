-- The connection that created each stored Responses API response, so the
-- gateway can retrieve, cancel, or delete it there, and continue
-- `previous_response_id` on it. `scope` is the caller (generated key,
-- environment key, or open access); `model` is the public ID it requested.
-- Rows are kept 30 days, OpenAI's default retention.
CREATE TABLE stored_responses (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) BETWEEN 1 AND 128),
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    model TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
) WITHOUT ROWID;
CREATE INDEX stored_responses_created ON stored_responses(created_at);
