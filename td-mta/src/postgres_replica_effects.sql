CREATE TABLE td_mta.replica_email_receipts (
    account bytea NOT NULL REFERENCES td_mta.replica_accounts(account),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    generation bytea NOT NULL CHECK(octet_length(generation)=8 AND generation>decode(repeat('00',8),'hex')),
    observed bytea NOT NULL CHECK(octet_length(observed)=8),
    digest bytea NOT NULL CHECK(octet_length(digest)=32),
    present boolean NOT NULL,
    PRIMARY KEY(account,id)
);
CREATE TABLE td_mta.replica_blob_identities (
    account bytea NOT NULL REFERENCES td_mta.replica_accounts(account),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    length bigint NOT NULL CHECK(length BETWEEN 0 AND 67108864),
    digest bytea NOT NULL CHECK(octet_length(digest)=32),
    PRIMARY KEY(account,id),
    FOREIGN KEY(account,id) REFERENCES td_mta.blob_ids(account,id)
);
REVOKE ALL ON TABLE td_mta.replica_email_receipts,td_mta.replica_blob_identities FROM PUBLIC;
