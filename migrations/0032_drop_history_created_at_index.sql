-- History is written for every request and pruned oldest first, so each
-- index on it costs pages per insert and per prune. Insertion (rowid) order
-- is already the time order, so the created_at index only duplicated it.
DROP INDEX request_history_created_at;
