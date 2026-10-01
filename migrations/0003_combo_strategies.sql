-- Combo routing strategies: how a combo orders its fallback attempts.
-- Existing combos keep the historical behavior, which is ordered priority.
ALTER TABLE combos
    ADD COLUMN strategy TEXT NOT NULL DEFAULT 'priority'
    CHECK (strategy IN ('priority', 'round_robin', 'random'));
