CREATE TABLE td_mta.config_materials (
    id pg_catalog.bytea PRIMARY KEY CHECK (pg_catalog.octet_length(id) = 16),
    kind pg_catalog.int2 NOT NULL CHECK (kind BETWEEN 1 AND 6),
    length pg_catalog.int4 NOT NULL CHECK (length BETWEEN 0 AND 131072),
    checksum pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(checksum) = 32),
    chunk_count pg_catalog.int2 NOT NULL CHECK (chunk_count BETWEEN 0 AND 2),
    CHECK (chunk_count = (length + 65535) / 65536),
    CHECK (length > 0 OR kind IN (5, 6)),
    CHECK (length <= CASE kind WHEN 1 THEN 1026 WHEN 2 THEN 65536
        WHEN 3 THEN 16384 WHEN 4 THEN 131072 ELSE 16384 END)
);
CREATE TABLE td_mta.config_material_chunks (
    material_id pg_catalog.bytea NOT NULL REFERENCES td_mta.config_materials(id),
    ordinal pg_catalog.int2 NOT NULL CHECK (ordinal BETWEEN 0 AND 1),
    bytes pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(bytes) BETWEEN 1 AND 65536),
    PRIMARY KEY (material_id, ordinal)
);
REVOKE ALL ON TABLE td_mta.config_materials FROM PUBLIC;
REVOKE ALL ON TABLE td_mta.config_material_chunks FROM PUBLIC;
