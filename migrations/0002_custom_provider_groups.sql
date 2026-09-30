-- Groups are metadata; connection UUIDs, ciphertext, and combo references stay
-- in providers unchanged. Membership explicitly distinguishes named custom
-- providers even when they happen to use a catalog preset's endpoint.
CREATE TABLE custom_provider_groups (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 100),
    base_url TEXT NOT NULL,
    protocol TEXT NOT NULL CHECK (protocol IN ('openai', 'anthropic')),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE provider_group_connections (
    provider_id TEXT PRIMARY KEY NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    group_id TEXT NOT NULL REFERENCES custom_provider_groups(id) ON DELETE RESTRICT
);
CREATE INDEX provider_group_connections_group ON provider_group_connections(group_id);

-- Preserve the old automatic grouping of non-preset endpoints. The imported
-- group's initial name comes from an existing connection; credentials are not
-- copied, decrypted, re-encrypted, or modified by this migration.
INSERT INTO custom_provider_groups (id, name, base_url, protocol)
SELECT 'custom-' || MIN(id),
       CASE WHEN length(trim(MIN(name))) > 0 THEN substr(MIN(name), 1, 100)
            ELSE 'Custom provider' END,
       rtrim(base_url, '/'), protocol
FROM providers
WHERE NOT (
    (protocol = 'openai' AND rtrim(base_url, '/') IN (
        'https://api.openai.com/v1',
        'https://openrouter.ai/api/v1',
        'https://api.groq.com/openai/v1',
        'https://api.deepseek.com/v1',
        'https://api.together.xyz/v1',
        'https://api.fireworks.ai/inference/v1'
    )) OR (protocol = 'anthropic' AND rtrim(base_url, '/') = 'https://api.anthropic.com/v1')
)
GROUP BY rtrim(base_url, '/'), protocol;

INSERT INTO provider_group_connections (provider_id, group_id)
SELECT p.id, g.id FROM providers p JOIN custom_provider_groups g
ON rtrim(p.base_url, '/') = g.base_url AND p.protocol = g.protocol;
