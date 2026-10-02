-- Public model IDs may combine a provider slug (64) and upstream model ID
-- (256), separated by a slash.
CREATE TABLE api_key_models_new (
    key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    model TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 321),
    PRIMARY KEY (key_id, model)
);
INSERT INTO api_key_models_new (key_id, model)
    SELECT key_id, model FROM api_key_models;
DROP TABLE api_key_models;
ALTER TABLE api_key_models_new RENAME TO api_key_models;

CREATE TABLE request_history_new (
    id            TEXT PRIMARY KEY,
    source        TEXT NOT NULL CHECK (length(source) BETWEEN 1 AND 32),
    model         TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 321),
    streaming     INTEGER NOT NULL DEFAULT 0 CHECK (streaming IN (0, 1)),
    outcome       TEXT NOT NULL CHECK (length(outcome) BETWEEN 1 AND 64),
    status        INTEGER NOT NULL CHECK (status >= 0),
    latency_ms    INTEGER NOT NULL DEFAULT 0 CHECK (latency_ms >= 0),
    input_tokens  INTEGER,
    output_tokens INTEGER,
    created_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
INSERT INTO request_history_new
    (id, source, model, streaming, outcome, status, latency_ms, input_tokens, output_tokens, created_at)
    SELECT id, source, model, streaming, outcome, status, latency_ms, input_tokens, output_tokens, created_at
    FROM request_history;

CREATE TABLE request_attempts_new (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id    TEXT NOT NULL REFERENCES request_history_new (id) ON DELETE CASCADE,
    ordinal       INTEGER NOT NULL CHECK (ordinal >= 0),
    position      INTEGER NOT NULL CHECK (position >= 0),
    provider_id   TEXT NOT NULL CHECK (length(provider_id) BETWEEN 1 AND 64),
    provider_name TEXT NOT NULL CHECK (length(provider_name) BETWEEN 1 AND 128),
    model         TEXT NOT NULL CHECK (length(model) BETWEEN 1 AND 256),
    outcome       TEXT NOT NULL CHECK (length(outcome) BETWEEN 1 AND 64),
    latency_ms    INTEGER NOT NULL DEFAULT 0 CHECK (latency_ms >= 0),
    UNIQUE (request_id, ordinal)
);
INSERT INTO request_attempts_new
    (id, request_id, ordinal, position, provider_id, provider_name, model, outcome, latency_ms)
    SELECT id, request_id, ordinal, position, provider_id, provider_name, model, outcome, latency_ms
    FROM request_attempts;

-- Explicitly copied IDs advance the new sequence only as far as the largest
-- surviving row. Preserve the previous high-water mark as well, even if its
-- attempt row was deleted before this migration.
UPDATE sqlite_sequence
SET seq = MAX(seq, COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'request_attempts'), 0))
WHERE name = 'request_attempts_new';

DROP TABLE request_attempts;
DROP TABLE request_history;
ALTER TABLE request_history_new RENAME TO request_history;
ALTER TABLE request_attempts_new RENAME TO request_attempts;
CREATE INDEX request_history_created_at ON request_history (created_at);
