-- Retained SMTP publication outcomes survive deletion of the delivered Email.
CREATE TABLE td_mta.receiving_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    email_id bytea NOT NULL CHECK(octet_length(email_id)=16),
    blob_id bytea NOT NULL CHECK(octet_length(blob_id)=16),
    thread_id bytea NOT NULL CHECK(octet_length(thread_id)=16),
    mailbox_id bytea NOT NULL CHECK(octet_length(mailbox_id)=16),
    length bigint NOT NULL CHECK(length BETWEEN 0 AND 33554432),
    created_at bigint NOT NULL,
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>old_sequence),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.receiving_receipts FROM PUBLIC;
