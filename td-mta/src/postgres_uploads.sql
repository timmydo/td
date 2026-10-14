-- Account mutations lock accounts before reading or changing these counters.
CREATE TABLE td_mta.account_usage(
    account bytea PRIMARY KEY REFERENCES td_mta.accounts(id),
    body_bytes bigint NOT NULL CHECK(body_bytes>=0),
    blob_count bigint NOT NULL CHECK(blob_count>=0),
    upload_bytes bigint NOT NULL CHECK(upload_bytes>=0 AND upload_bytes<=body_bytes),
    queue_bytes bigint NOT NULL CHECK(queue_bytes>=0 AND queue_bytes<=body_bytes),
    queue_submissions bigint NOT NULL CHECK(queue_submissions>=0)
);
INSERT INTO td_mta.account_usage
SELECT a.id,
    coalesce((SELECT sum(b.length) FROM td_mta.blobs b WHERE b.account=a.id),0),
    (SELECT count(*) FROM td_mta.blobs b WHERE b.account=a.id),
    coalesce((SELECT sum(b.length) FROM td_mta.blobs b WHERE b.account=a.id AND EXISTS(SELECT 1 FROM td_mta.leases l WHERE l.account=b.account AND l.blob_id=b.id)),0),
    coalesce((SELECT sum(b.length) FROM td_mta.blobs b WHERE b.account=a.id AND EXISTS(SELECT 1 FROM td_mta.submissions s WHERE s.account=b.account AND s.transmitted_blob_id=b.id)),0),
    (SELECT count(*) FROM td_mta.submissions s WHERE s.account=a.id)
FROM td_mta.accounts a;
CREATE TABLE td_mta.upload_receipts(
    account bytea NOT NULL REFERENCES td_mta.accounts(id),
    operation bytea NOT NULL CHECK(octet_length(operation)=16),
    request_digest bytea NOT NULL CHECK(octet_length(request_digest)=32),
    blob_id bytea NOT NULL CHECK(octet_length(blob_id)=16),
    length bigint NOT NULL CHECK(length BETWEEN 0 AND 33554432),
    created_at bigint NOT NULL,
    expires_at bigint NOT NULL CHECK(expires_at>=created_at),
    old_sequence bytea NOT NULL CHECK(octet_length(old_sequence)=8),
    new_sequence bytea NOT NULL CHECK(octet_length(new_sequence)=8 AND new_sequence>old_sequence),
    PRIMARY KEY(account,operation)
);
REVOKE ALL ON TABLE td_mta.account_usage,td_mta.upload_receipts FROM PUBLIC;
