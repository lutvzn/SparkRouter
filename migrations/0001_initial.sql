-- SparkRouter initial schema: providers, combos, and ordered combo steps.

CREATE TABLE providers (
    id                TEXT PRIMARY KEY NOT NULL,
    name              TEXT NOT NULL,
    base_url          TEXT NOT NULL,
    protocol          TEXT NOT NULL,
    encrypted_api_key BLOB NOT NULL,
    enabled           INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at        TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE combos (
    id         TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE combo_steps (
    id          TEXT PRIMARY KEY NOT NULL,
    combo_id    TEXT NOT NULL REFERENCES combos (id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers (id) ON DELETE RESTRICT,
    position    INTEGER NOT NULL,
    model       TEXT NOT NULL,
    UNIQUE (combo_id, position)
);

CREATE INDEX combo_steps_provider_id ON combo_steps (provider_id);
