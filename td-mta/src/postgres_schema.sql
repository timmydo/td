CREATE SCHEMA td_mta;
REVOKE ALL ON SCHEMA td_mta FROM PUBLIC;
CREATE TABLE td_mta.schema_migrations (
    version pg_catalog.int4 PRIMARY KEY CHECK (version > 0),
    checksum pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(checksum) = 32),
    store_epoch pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(store_epoch) = 16)
);
REVOKE ALL ON TABLE td_mta.schema_migrations FROM PUBLIC;
