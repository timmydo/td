-- Retained ordered root-folder creation outcomes.
CREATE TABLE td_mta.mailbox_create_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>=old_sequence),
    results bytea NOT NULL CHECK(octet_length(results)<=4352 AND octet_length(results)%17=0),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.mailbox_create_receipts FROM PUBLIC;
