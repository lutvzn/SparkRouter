ALTER TABLE api_keys ADD COLUMN requests_per_minute INTEGER CHECK (requests_per_minute BETWEEN 1 AND 1000000);
ALTER TABLE api_keys ADD COLUMN max_concurrent INTEGER CHECK (max_concurrent BETWEEN 1 AND 10000);
