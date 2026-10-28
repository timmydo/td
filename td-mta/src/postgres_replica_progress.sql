-- Copy bookkeeping is separate from local JMAP sequences and immutable bodies.
CREATE TABLE td_mta.replica_accounts (
    account bytea PRIMARY KEY REFERENCES td_mta.accounts(id),
    source boolean NOT NULL DEFAULT true CHECK(source) REFERENCES td_mta.replica_source(singleton),
    seed_digest bytea NOT NULL CHECK(octet_length(seed_digest)=32),
    body_limit bigint NOT NULL CHECK(body_limit>0),
    blob_limit bigint NOT NULL CHECK(blob_limit>0),
    email_limit bigint NOT NULL CHECK(email_limit>0),
    receipt_limit bigint NOT NULL CHECK(receipt_limit>0)
);
CREATE TABLE td_mta.replica_progress (
    account bytea PRIMARY KEY REFERENCES td_mta.replica_accounts(account),
    generation bytea NOT NULL CHECK(octet_length(generation)=8 AND generation>decode(repeat('00',8),'hex')),
    email_cursor bytea NOT NULL CHECK(octet_length(email_cursor)=8),
    mailbox_checkpoint bytea NOT NULL CHECK(octet_length(mailbox_checkpoint)=8),
    inventory_base bytea NOT NULL CHECK(octet_length(inventory_base)=8),
    inventory_cursor text COLLATE "C" CHECK(inventory_cursor IS NULL OR octet_length(inventory_cursor)=153),
    inventory_complete boolean NOT NULL,
    email_count bigint NOT NULL CHECK(email_count>=0),
    receipt_count bigint NOT NULL CHECK(receipt_count>=0)
);
CREATE TABLE td_mta.replica_pending (
    account bytea PRIMARY KEY REFERENCES td_mta.replica_accounts(account),
    generation bytea NOT NULL CHECK(octet_length(generation)=8 AND generation>decode(repeat('00',8),'hex')),
    page bytea NOT NULL CHECK(octet_length(page)=8 AND page>decode(repeat('00',8),'hex')),
    kind integer NOT NULL CHECK(kind IN (1,2)),
    observed bytea NOT NULL CHECK(octet_length(observed)=8),
    ids bytea NOT NULL CHECK(octet_length(ids)<=16384 AND octet_length(ids)%16=0),
    done bytea NOT NULL CHECK(octet_length(done)=128),
    next_cursor text COLLATE "C" CHECK(next_cursor IS NULL OR octet_length(next_cursor)=153),
    next_state bytea CHECK(next_state IS NULL OR octet_length(next_state)=8),
    more boolean NOT NULL,
    CHECK((kind=1 AND next_state IS NULL AND NOT more AND octet_length(ids)<=4096)
        OR (kind=2 AND next_cursor IS NULL AND next_state IS NOT NULL))
);
CREATE TABLE td_mta.replica_mailbox_receipts (
    account bytea NOT NULL REFERENCES td_mta.replica_accounts(account),
    id bytea NOT NULL CHECK(octet_length(id)=16),
    observed bytea NOT NULL CHECK(octet_length(observed)=8),
    present boolean NOT NULL,
    PRIMARY KEY(account,id)
);
REVOKE ALL ON TABLE td_mta.replica_accounts,td_mta.replica_progress,td_mta.replica_pending,
    td_mta.replica_mailbox_receipts FROM PUBLIC;
