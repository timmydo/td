-- Trusted PostgreSQL 17 extension, explicitly approved for indexed search.
CREATE SCHEMA td_mta_trgm;
REVOKE ALL ON SCHEMA td_mta_trgm FROM PUBLIC;
CREATE EXTENSION pg_trgm WITH SCHEMA td_mta_trgm VERSION '1.6';
ALTER TABLE td_mta.blob_search ADD COLUMN candidate_text text COLLATE "C"
    CHECK(candidate_text IS NULL OR octet_length(candidate_text) <= 8454144);
CREATE INDEX blob_search_candidates ON td_mta.blob_search
    USING gin(candidate_text td_mta_trgm.gin_trgm_ops);
