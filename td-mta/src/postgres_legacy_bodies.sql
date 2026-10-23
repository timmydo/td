-- Owner imports may preserve legacy messages above the network admission cap.
ALTER TABLE td_mta.blobs DROP CONSTRAINT blobs_length_check;
ALTER TABLE td_mta.blobs ADD CONSTRAINT blobs_length_check
    CHECK(length BETWEEN 0 AND 67108864);
ALTER TABLE td_mta.blob_chunks DROP CONSTRAINT blob_chunks_ordinal_check;
ALTER TABLE td_mta.blob_chunks ADD CONSTRAINT blob_chunks_ordinal_check
    CHECK(ordinal BETWEEN 0 AND 1023);
