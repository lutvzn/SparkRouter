-- Widen custom-provider protocols, retaining the provider/group identities and tags.
CREATE TEMP TABLE saved_media_groups AS SELECT * FROM custom_provider_groups;
CREATE TEMP TABLE saved_media_members AS SELECT * FROM provider_group_connections;
CREATE TEMP TABLE saved_media_slugs AS SELECT * FROM provider_slugs;
CREATE TEMP TABLE saved_media_tags AS SELECT * FROM provider_tags;
DROP TABLE provider_tags;
DROP TABLE provider_group_connections;
DROP TABLE provider_slugs;
DROP TABLE custom_provider_groups;
CREATE TABLE custom_provider_groups (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 100),
    base_url TEXT NOT NULL,
    protocol TEXT NOT NULL CHECK (protocol IN ('openai', 'anthropic', 'systemone', 'fal', 'minimax', 'deepgram', 'elevenlabs', 'xai')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
INSERT INTO custom_provider_groups SELECT * FROM saved_media_groups;
CREATE TABLE provider_group_connections (
    provider_id TEXT PRIMARY KEY NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    group_id TEXT NOT NULL REFERENCES custom_provider_groups(id) ON DELETE RESTRICT
);
CREATE INDEX provider_group_connections_group ON provider_group_connections(group_id);
INSERT INTO provider_group_connections SELECT * FROM saved_media_members;
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
INSERT INTO provider_slugs SELECT * FROM saved_media_slugs;
CREATE TABLE provider_tags (
    identity TEXT PRIMARY KEY,
    group_id TEXT REFERENCES custom_provider_groups(id) ON DELETE CASCADE,
    tags TEXT NOT NULL
);
INSERT INTO provider_tags SELECT * FROM saved_media_tags;
DROP TABLE saved_media_groups;
DROP TABLE saved_media_members;
DROP TABLE saved_media_slugs;
DROP TABLE saved_media_tags;
