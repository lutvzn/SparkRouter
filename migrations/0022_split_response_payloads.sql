-- Response captures arrive after their request capture. Updating the shared
-- row in place rewrote the stored request body, so responses get their own.
CREATE TABLE request_payload_responses (
    request_id TEXT PRIMARY KEY REFERENCES request_payloads(request_id) ON DELETE CASCADE,
    response_body TEXT NOT NULL,
    response_truncated INTEGER NOT NULL CHECK (response_truncated IN (0, 1)),
    response_complete INTEGER NOT NULL CHECK (response_complete IN (0, 1))
);

INSERT INTO request_payload_responses
    (request_id, response_body, response_truncated, response_complete)
SELECT request_id, response_body, response_truncated, response_complete
FROM request_payloads
WHERE response_body != '' OR response_truncated = 1 OR response_complete = 1;

ALTER TABLE request_payloads DROP COLUMN response_body;
ALTER TABLE request_payloads DROP COLUMN response_truncated;
ALTER TABLE request_payloads DROP COLUMN response_complete;
