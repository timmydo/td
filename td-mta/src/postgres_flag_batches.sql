-- Existing single-Email receipts always recorded an existing object's success.
ALTER TABLE td_mta.flag_receipts ADD COLUMN results bytea NOT NULL DEFAULT decode('01','hex') CHECK(octet_length(results)<=256);
ALTER TABLE td_mta.flag_receipts ALTER COLUMN results DROP DEFAULT;
