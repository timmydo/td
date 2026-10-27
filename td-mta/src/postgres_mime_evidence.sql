-- Optional interpretation and chunk evidence, bound to the immutable body.
CREATE TABLE td_mta.blob_interpretations(
    account bytea NOT NULL,
    blob_id bytea NOT NULL,
    version integer NOT NULL CHECK(version > 0),
    body_length bigint NOT NULL CHECK(body_length BETWEEN 0 AND 67108864),
    body_digest bytea NOT NULL CHECK(octet_length(body_digest)=32),
    header_bytes bigint NOT NULL CHECK(header_bytes BETWEEN 0 AND 262144),
    part_table bytea NOT NULL CHECK(octet_length(part_table) BETWEEN 56 AND 57344
        AND octet_length(part_table)%56=0),
    chunk_hashes bytea NOT NULL CHECK(octet_length(chunk_hashes) BETWEEN 0 AND 32768
        AND octet_length(chunk_hashes)=((body_length+65535)/65536)*32),
    manifest_digest bytea NOT NULL CHECK(octet_length(manifest_digest)=32),
    PRIMARY KEY(account,blob_id),
    FOREIGN KEY(account,blob_id) REFERENCES td_mta.blobs(account,id) ON DELETE CASCADE
);
REVOKE ALL ON TABLE td_mta.blob_interpretations FROM PUBLIC;
