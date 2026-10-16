CREATE TABLE td_mta.submission_cancel_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    submission_id bytea NOT NULL CHECK(octet_length(submission_id)=16),
    recipient_count bigint NOT NULL CHECK(recipient_count BETWEEN 1 AND 1000),
    changed_count bigint NOT NULL CHECK(changed_count BETWEEN 0 AND recipient_count),
    completed_at bigint NOT NULL,
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8 AND old_sequence>decode(repeat('00',8),'hex')),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>=old_sequence),
    CHECK((changed_count=0)=(new_sequence=old_sequence)),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.submission_cancel_receipts FROM PUBLIC;
