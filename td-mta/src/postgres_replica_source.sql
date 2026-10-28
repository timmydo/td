-- A persistent application-replica identity; empty means ordinary primary mode.
CREATE TABLE td_mta.replica_source (
    singleton boolean PRIMARY KEY CHECK(singleton),
    origin text COLLATE "C" NOT NULL CHECK(octet_length(origin) BETWEEN 1 AND 4096),
    source_epoch bytea NOT NULL CHECK(octet_length(source_epoch)=16),
    protocol integer NOT NULL CHECK(protocol=1)
);
REVOKE ALL ON TABLE td_mta.replica_source FROM PUBLIC;
