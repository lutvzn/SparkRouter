-- Dashboard classification overrides; NULL group_id denotes a catalog preset.
CREATE TABLE provider_tags (
    identity TEXT PRIMARY KEY,
    group_id TEXT REFERENCES custom_provider_groups(id) ON DELETE CASCADE,
    tags TEXT NOT NULL
);
