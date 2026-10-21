-- Basic/Smart now use the combo's affinity timeout instead of a fixed 30
-- minutes. Existing Basic/Smart combos keep the 30 minutes they had.
UPDATE combos SET sticky_minutes = 30 WHERE affinity_mode IN ('basic', 'smart');
