CREATE TABLE td_mta.submission_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    submission_id bytea NOT NULL CHECK(octet_length(submission_id)=16),
    blob_id bytea NOT NULL CHECK(octet_length(blob_id)=16),
    length bigint NOT NULL CHECK(length BETWEEN 0 AND 33554432),
    send_at bigint NOT NULL,
    expires_at bigint NOT NULL CHECK(expires_at>=send_at),
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>old_sequence),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.submission_receipts FROM PUBLIC;
