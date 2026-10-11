-- Refresh failures must not remove the last successful routing snapshot.
CREATE TABLE connection_refresh_status (
    provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    attempted_at TEXT NOT NULL,
    error TEXT NOT NULL DEFAULT ''
);

-- Metadata-only routing snapshots, retained/cleared with request history.
CREATE TABLE request_routing (
    request_id TEXT PRIMARY KEY REFERENCES request_history(id) ON DELETE CASCADE,
    details TEXT NOT NULL
);
