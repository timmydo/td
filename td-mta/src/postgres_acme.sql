CREATE TABLE td_mta.acme_profiles (
    slot pg_catalog.int2 PRIMARY KEY CHECK (slot BETWEEN 0 AND 15),
    profile pg_catalog.text NOT NULL UNIQUE CHECK (profile ~ '^[a-z][a-z0-9_-]{0,63}$'),
    settings_hash pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(settings_hash)=32),
    revision pg_catalog.int8 NOT NULL CHECK (revision>0),
    owner pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(owner)=16),
    lease_until pg_catalog.timestamptz NOT NULL,
    state pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(state) BETWEEN 1 AND 32768),
    active_generation pg_catalog.int8 NOT NULL DEFAULT 0 CHECK (active_generation>=0),
    chain pg_catalog.bytea NOT NULL DEFAULT '' CHECK (pg_catalog.octet_length(chain)<=65536),
    key pg_catalog.bytea NOT NULL DEFAULT '' CHECK (pg_catalog.octet_length(key)<=1024),
    CHECK ((active_generation=0 AND pg_catalog.octet_length(chain)=0 AND pg_catalog.octet_length(key)=0)
        OR (active_generation>0 AND pg_catalog.octet_length(chain)>0 AND pg_catalog.octet_length(key)>0))
);
REVOKE ALL ON TABLE td_mta.acme_profiles FROM PUBLIC;
