CREATE TABLE td_mta.config_versions (
    id pg_catalog.bytea PRIMARY KEY CHECK (pg_catalog.octet_length(id) = 16),
    length pg_catalog.int4 NOT NULL CHECK (length BETWEEN 1 AND 2098176),
    checksum pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(checksum) = 32),
    chunk_count pg_catalog.int2 NOT NULL CHECK (chunk_count BETWEEN 1 AND 33),
    CHECK (chunk_count = (length + 65535) / 65536)
);
CREATE TABLE td_mta.config_version_chunks (
    version_id pg_catalog.bytea NOT NULL REFERENCES td_mta.config_versions(id),
    ordinal pg_catalog.int2 NOT NULL CHECK (ordinal BETWEEN 0 AND 32),
    bytes pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(bytes) BETWEEN 1 AND 65536),
    PRIMARY KEY (version_id, ordinal)
);
REVOKE ALL ON TABLE td_mta.config_versions FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.config_version_chunks FROM PUBLIC;
