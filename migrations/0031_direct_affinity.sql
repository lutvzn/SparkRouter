-- How long a direct model ID keeps routing to the connection that last
-- succeeded for it. 0 turns direct-route affinity off.
ALTER TABLE dashboard_settings ADD COLUMN direct_affinity_minutes INTEGER NOT NULL DEFAULT 5
    CHECK (direct_affinity_minutes BETWEEN 0 AND 1440);
