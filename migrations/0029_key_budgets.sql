-- Optional per-key token and USD budgets over a UTC day or month, and the
-- usage counted against the current period. Only usage that upstreams report
-- is counted; rows for earlier periods are replaced as new periods begin.
ALTER TABLE api_keys ADD COLUMN budget_period TEXT
    CHECK (budget_period IS NULL OR budget_period IN ('day', 'month'));
ALTER TABLE api_keys ADD COLUMN token_budget INTEGER
    CHECK (token_budget IS NULL OR token_budget BETWEEN 1 AND 1000000000000);
ALTER TABLE api_keys ADD COLUMN cost_budget_usd REAL
    CHECK (cost_budget_usd IS NULL OR (cost_budget_usd > 0 AND cost_budget_usd <= 1000000));
CREATE TABLE api_key_spend (
    key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
    period TEXT NOT NULL,
    tokens INTEGER NOT NULL DEFAULT 0 CHECK (tokens >= 0),
    cost_usd REAL NOT NULL DEFAULT 0 CHECK (cost_usd >= 0),
    PRIMARY KEY (key_id, period)
) WITHOUT ROWID;
