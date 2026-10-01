-- Metadata-only request history: what each routed request did, never what it
-- contained. Rows hold outcome labels, HTTP status, latency, token counts,
-- and snapshots of the provider/model identifiers attempted; prompts,
-- completions, credentials, endpoint URLs, and upstream error bodies are
-- never stored. Provider identifiers are plain snapshots (no foreign keys
-- into live configuration), so deleting a provider keeps its history.
-- Retention is bounded by the application on insert: the newest 10,000
-- requests are kept and older rows cascade their attempts away.
CREATE TABLE request_history (
    id            TEXT PRIMARY KEY,
    source        TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 32),
    model         TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 256),
    streaming     INTEGER NOT NULL DEFAULT 0 CHECK (streaming IN (0, 1)),
    outcome       TEXT NOT NULL CHECK (length(outcome) BETWEEN 1 AND 64),
    status        INTEGER NOT NULL CHECK (status >= 0),
    latency_ms    INTEGER NOT NULL DEFAULT 0 CHECK (latency_ms >= 0),
    input_tokens  INTEGER,
    output_tokens INTEGER,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX request_history_created_at ON request_history (created_at);

-- Per-request attempt snapshots. `ordinal` is the recorded attempt sequence
-- (insertion order, unique within a request) and drives display order,
-- because a round-robin or random start can record steps out of order;
-- `position` stays the snapshot of the configured step position. Rows are
-- deleted with their request.
CREATE TABLE request_attempts (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id    TEXT NOT NULL REFERENCES request_history (id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL CHECK (ordinal >= 0),
    position      INTEGER NOT NULL CHECK (position >= 0),
    provider_id   TEXT NOT NULL CHECK (length(provider_id) BETWEEN 1 AND 64),
    provider_name TEXT NOT NULL CHECK (length(provider_name) BETWEEN 1 AND 128),
    model         TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 256),
    outcome       TEXT NOT NULL CHECK (length(outcome) BETWEEN 1 AND 64),
    latency_ms    INTEGER NOT NULL DEFAULT 0 CHECK (latency_ms >= 0),
    UNIQUE (request_id, ordinal)
);
