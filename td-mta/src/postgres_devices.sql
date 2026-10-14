CREATE TABLE td_mta.devices (
    account pg_catalog.bytea NOT NULL REFERENCES td_mta.accounts(id),
    id pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(id) = 16),
    salt pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(salt) = 32),
    verifier pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(verifier) = 32),
    label pg_catalog.text COLLATE "C" NOT NULL CHECK (pg_catalog.octet_length(label) BETWEEN 1 AND 128),
    created_at pg_catalog.int8 NOT NULL CHECK (created_at >= 0),
    creation_digest pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(creation_digest) = 32),
    revoked_at pg_catalog.int8 CHECK (revoked_at >= 0),
    revocation_id pg_catalog.bytea CHECK (pg_catalog.octet_length(revocation_id) = 16),
    revocation_digest pg_catalog.bytea CHECK (pg_catalog.octet_length(revocation_digest) = 32),
    CHECK ((revoked_at IS NULL) = (revocation_id IS NULL)
        AND (revoked_at IS NULL) = (revocation_digest IS NULL)),
    PRIMARY KEY (account, id),
    UNIQUE (account, revocation_id)
);
REVOKE ALL ON TABLE td_mta.devices FROM PUBLIC;
