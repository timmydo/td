CREATE TABLE td_mta.submission_expiry_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    submission_id bytea NOT NULL CHECK(octet_length(submission_id)=16),
    recipient_count bigint NOT NULL CHECK(recipient_count BETWEEN 1 AND 100),
    cutoff_at bigint NOT NULL CHECK(cutoff_at>=0),
    published_at bigint NOT NULL CHECK(published_at>=cutoff_at),
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>old_sequence),
    PRIMARY KEY(account,operation)
);
CREATE TABLE td_mta.queue_expiry_progress(
    singleton boolean PRIMARY KEY CHECK(singleton),
    published_at bigint CHECK(published_at>=0)
);
INSERT INTO td_mta.queue_expiry_progress VALUES(true,NULL);
REVOKE ALL ON TABLE td_mta.submission_expiry_receipts,td_mta.queue_expiry_progress FROM PUBLIC;
