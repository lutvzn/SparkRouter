-- Widen the custom-provider protocol constraint without changing provider IDs,
-- credentials, memberships, slugs, or combo references. Rebuild referencing
-- tables explicitly with foreign keys enabled (SQLx migrations are transactional).
CREATE TEMP TABLE saved_group_connections AS SELECT * FROM provider_group_connections;
CREATE TEMP TABLE saved_provider_slugs AS SELECT * FROM provider_slugs;
CREATE TEMP TABLE saved_custom_groups AS SELECT * FROM custom_provider_groups;
DROP TABLE provider_group_connections;
DROP TABLE provider_slugs;
DROP TABLE custom_provider_groups;

CREATE TABLE custom_provider_groups (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 100),
    base_url TEXT NOT NULL,
    protocol TEXT NOT NULL CHECK (protocol IN ('openai', 'anthropic', 'systemone')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
INSERT INTO custom_provider_groups SELECT * FROM saved_custom_groups;
CREATE TABLE provider_group_connections (
    provider_id TEXT PRIMARY KEY NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    group_id TEXT NOT NULL REFERENCES custom_provider_groups(id) ON DELETE RESTRICT
);
CREATE INDEX provider_group_connections_group ON provider_group_connections(group_id);
INSERT INTO provider_group_connections SELECT * FROM saved_group_connections;
CREATE TABLE provider_slugs (
    group_id TEXT PRIMARY KEY REFERENCES custom_provider_groups(id) ON DELETE CASCADE,
    slug TEXT NOT NULL UNIQUE CHECK (
        length(slug) BETWEEN 1 AND 64
        AND slug NOT GLOB '*[^a-z0-9-]*'
        AND substr(slug, 1, 1) != '-'
        AND substr(slug, -1, 1) != '-'
        AND slug != 'combo'
        AND slug NOT LIKE 'connection-%'
    )
);
INSERT INTO provider_slugs SELECT * FROM saved_provider_slugs;
DROP TABLE saved_group_connections;
DROP TABLE saved_provider_slugs;
DROP TABLE saved_custom_groups;
