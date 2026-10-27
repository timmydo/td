-- Rebuildable immutable header interpretations, never body-read authority.
CREATE TABLE td_mta.blob_summaries(
    account bytea NOT NULL,
    blob_id bytea NOT NULL,
    version integer NOT NULL CHECK(version > 0),
    body_length bigint NOT NULL CHECK(body_length BETWEEN 0 AND 67108864),
    body_digest bytea NOT NULL CHECK(octet_length(body_digest)=32),
    fields bytea[] NOT NULL CHECK(array_ndims(fields)=1 AND array_lower(fields,1)=1
        AND cardinality(fields)=4 AND array_position(fields,NULL) IS NULL
        AND octet_length(fields[1]) BETWEEN 1 AND 16384
        AND octet_length(fields[2]) BETWEEN 1 AND 16384
        AND octet_length(fields[3]) BETWEEN 1 AND 16384
        AND octet_length(fields[4]) BETWEEN 1 AND 16384),
    PRIMARY KEY(account,blob_id),
    FOREIGN KEY(account,blob_id) REFERENCES td_mta.blobs(account,id) ON DELETE CASCADE
);
REVOKE ALL ON TABLE td_mta.blob_summaries FROM PUBLIC;
