-- One bounded, persisted retention preference. Secrets stay in deployment config.
CREATE TABLE dashboard_settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    history_retention INTEGER NOT NULL CHECK (history_retention BETWEEN 1 AND 10000)
);
INSERT INTO dashboard_settings (id, history_retention) VALUES (1, 10000);
