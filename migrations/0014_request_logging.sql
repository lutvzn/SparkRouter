ALTER TABLE dashboard_settings ADD COLUMN logging_mode TEXT NOT NULL DEFAULT 'basic'
    CHECK (logging_mode IN ('off', 'basic', 'extended'));
CREATE TABLE request_payloads (
    request_id TEXT PRIMARY KEY REFERENCES request_history(id) ON DELETE CASCADE,
    request_body TEXT NOT NULL,
    response_body TEXT NOT NULL DEFAULT '',
    request_truncated INTEGER NOT NULL CHECK (request_truncated IN (0, 1)),
    response_truncated INTEGER NOT NULL DEFAULT 0 CHECK (response_truncated IN (0, 1)),
    response_complete INTEGER NOT NULL DEFAULT 0 CHECK (response_complete IN (0, 1))
);
