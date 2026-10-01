-- Display fields from the last model-list refresh. Existing rows keep their IDs
-- and show no extra fields until the connection is refreshed again.
ALTER TABLE connection_models ADD COLUMN details TEXT NOT NULL DEFAULT '{}';
