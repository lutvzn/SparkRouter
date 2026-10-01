-- Dashboard-generated gateway keys. Only a master-key HMAC and a short prefix
-- are kept after the one-time reveal. Keys are unrestricted: any valid key can
-- call every combo.
CREATE TABLE api_keys (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 64),
    key_prefix TEXT NOT NULL,
    key_hash TEXT NOT NULL UNIQUE,
    reveal_envelope BLOB,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
