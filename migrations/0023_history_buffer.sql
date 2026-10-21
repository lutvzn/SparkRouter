-- Multiple of the history writer's default queue (8192 writes) and Extended
-- body budget (32 MiB), raised from Settings to ride out longer disk stalls.
ALTER TABLE dashboard_settings ADD COLUMN history_buffer INTEGER NOT NULL DEFAULT 1
    CHECK (history_buffer IN (1, 2, 4, 8));
