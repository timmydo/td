CREATE TABLE td_mta.config_selection (
    singleton pg_catalog.bool PRIMARY KEY CHECK (singleton),
    generation pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(generation) = 8),
    version_id pg_catalog.bytea REFERENCES td_mta.config_versions(id),
    CHECK ((generation = pg_catalog.decode('0000000000000000', 'hex')) = (version_id IS NULL))
);
INSERT INTO td_mta.config_selection VALUES (true, pg_catalog.decode('0000000000000000', 'hex'), NULL);
CREATE TABLE td_mta.config_selection_receipts (
    operation pg_catalog.bytea PRIMARY KEY CHECK (pg_catalog.octet_length(operation) = 16),
    request_digest pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(request_digest) = 32),
    generation pg_catalog.bytea NOT NULL UNIQUE CHECK (pg_catalog.octet_length(generation) = 8 AND generation <> pg_catalog.decode('0000000000000000', 'hex')),
    version_id pg_catalog.bytea NOT NULL REFERENCES td_mta.config_versions(id)
);
REVOKE ALL ON TABLE td_mta.config_selection FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.config_selection_receipts FROM PUBLIC;
