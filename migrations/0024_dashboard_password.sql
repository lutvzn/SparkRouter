-- The stored dashboard admin password as an Argon2id PHC string, never the
-- password itself. Absent until first-run setup; ROUTER_DASHBOARD_PASSWORD
-- takes precedence. Deleting the row (sparkrouter reset-dashboard-password)
-- reopens first-run setup on the next start.
CREATE TABLE dashboard_admin (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    password_hash TEXT NOT NULL CHECK (password_hash LIKE '$argon2id$%'),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
