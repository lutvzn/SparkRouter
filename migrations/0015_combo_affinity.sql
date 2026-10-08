-- Optional per-combo routing affinity. Existing combos keep their old behavior.
ALTER TABLE combos ADD COLUMN affinity_mode TEXT NOT NULL DEFAULT 'off'
    CHECK (affinity_mode IN ('off', 'basic', 'smart', 'sticky'));
ALTER TABLE combos ADD COLUMN sticky_minutes INTEGER NOT NULL DEFAULT 5
    CHECK (sticky_minutes BETWEEN 1 AND 1440);
-- A configuration edit cannot reuse affinity from an earlier routing snapshot.
ALTER TABLE combos ADD COLUMN routing_revision INTEGER NOT NULL DEFAULT 0;
