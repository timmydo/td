-- Rebuildable complete search fields; raw bodies remain authoritative.
CREATE TABLE td_mta.blob_search(
    account bytea NOT NULL,
    blob_id bytea NOT NULL,
    version integer NOT NULL CHECK(version > 0),
    body_length bigint NOT NULL CHECK(body_length BETWEEN 0 AND 67108864),
    body_digest bytea NOT NULL CHECK(octet_length(body_digest)=32),
    fields bytea NOT NULL CHECK(octet_length(fields) <= 8650752),
    fields_digest bytea NOT NULL CHECK(octet_length(fields_digest)=32),
    PRIMARY KEY(account,blob_id),
    FOREIGN KEY(account,blob_id) REFERENCES td_mta.blobs(account,id) ON DELETE CASCADE
);
REVOKE ALL ON TABLE td_mta.blob_search FROM PUBLIC;
