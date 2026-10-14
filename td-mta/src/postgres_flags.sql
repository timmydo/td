CREATE TABLE td_mta.flag_receipts (
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>=old_sequence),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.flag_receipts FROM PUBLIC;
