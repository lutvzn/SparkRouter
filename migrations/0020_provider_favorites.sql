-- Dashboard preferences only; identities match provider tiles, not connections.
CREATE TABLE provider_favorites (
    identity TEXT PRIMARY KEY,
    group_id TEXT REFERENCES custom_provider_groups(id) ON DELETE CASCADE,
    favorite INTEGER NOT NULL CHECK (favorite IN (0, 1))
);

-- Seed once. Explicit unfavorites remain off across restarts.
INSERT INTO provider_favorites(identity, favorite) VALUES
    ('/providers/catalog/anthropic', 1),
    ('/providers/catalog/openai', 1),
    ('/providers/catalog/openrouter', 1);
