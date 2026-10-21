-- Values sealed with ROUTER_MASTER_KEY: 'check' detects starting with the wrong
-- key; 'fingerprint' is the HMAC key for gateway-key fingerprints, written by
-- the first master-key rotation so those fingerprints stay valid. Until then
-- the master key itself is the fingerprint key.
CREATE TABLE master_key_material (
    purpose TEXT PRIMARY KEY CHECK (purpose IN ('check', 'fingerprint')),
    envelope BLOB NOT NULL
);
