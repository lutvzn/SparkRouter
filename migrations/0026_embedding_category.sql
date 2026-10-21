-- Add the 'embedding' combo/model category. SQLite cannot widen a CHECK
-- constraint, so the tables are rebuilt with every row, ID and step position.
CREATE TEMP TABLE saved_embedding_combos AS SELECT * FROM combos;
CREATE TEMP TABLE saved_embedding_steps AS SELECT * FROM combo_steps;
CREATE TEMP TABLE saved_embedding_categories AS SELECT * FROM model_categories;
DROP TABLE combo_steps;
DROP TABLE combos;
DROP TABLE model_categories;
CREATE TABLE combos (
    id         TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    strategy TEXT NOT NULL DEFAULT 'priority'
        CHECK (strategy IN ('priority', 'round_robin', 'random')),
    affinity_mode TEXT NOT NULL DEFAULT 'off'
        CHECK (affinity_mode IN ('off', 'basic', 'smart', 'sticky')),
    sticky_minutes INTEGER NOT NULL DEFAULT 5 CHECK (sticky_minutes BETWEEN 1 AND 1440),
    routing_revision INTEGER NOT NULL DEFAULT 0,
    category TEXT NOT NULL DEFAULT 'text'
        CHECK (category IN ('text','decision','image','video','music','tts','stt','embedding'))
);
INSERT INTO combos SELECT * FROM saved_embedding_combos;
CREATE TABLE combo_steps (
    id          TEXT PRIMARY KEY NOT NULL,
    combo_id    TEXT NOT NULL REFERENCES combos (id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL REFERENCES providers (id) ON DELETE RESTRICT,
    position    INTEGER NOT NULL,
    model       TEXT NOT NULL,
    UNIQUE (combo_id, position)
);
CREATE INDEX combo_steps_provider_id ON combo_steps (provider_id);
INSERT INTO combo_steps SELECT * FROM saved_embedding_steps;
CREATE TABLE model_categories (
    provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model TEXT NOT NULL,
    category TEXT NOT NULL
        CHECK (category IN ('text','decision','image','video','music','tts','stt','embedding')),
    PRIMARY KEY (provider_id, model)
);
INSERT INTO model_categories SELECT * FROM saved_embedding_categories;
DROP TABLE saved_embedding_combos;
DROP TABLE saved_embedding_steps;
DROP TABLE saved_embedding_categories;
