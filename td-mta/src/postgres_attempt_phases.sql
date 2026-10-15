CREATE TABLE td_mta.attempt_phase_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    attempt_id bytea NOT NULL CHECK(octet_length(attempt_id)=16),
    phase bigint NOT NULL CHECK(phase IN (2,3)),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    recipient_count bigint NOT NULL CHECK(recipient_count BETWEEN 1 AND 100),
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>old_sequence),
    PRIMARY KEY(account,attempt_id,phase)
);
REVOKE ALL ON TABLE td_mta.attempt_phase_receipts FROM PUBLIC;
