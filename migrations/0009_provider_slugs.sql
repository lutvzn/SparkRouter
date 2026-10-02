-- Public routing namespaces belong to provider groups, not API-key connections.
-- Existing groups are assigned readable slugs transactionally after migrations.
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
