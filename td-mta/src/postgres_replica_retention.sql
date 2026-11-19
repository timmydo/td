ALTER TABLE td_mta.replica_email_receipts
    ADD COLUMN identity bytea CHECK(octet_length(identity)=40),
    ALTER COLUMN digest DROP NOT NULL;
DELETE FROM td_mta.replica_email_receipts r USING td_mta.replica_progress p
WHERE r.account=p.account AND NOT r.present AND r.observed<p.email_cursor;
UPDATE td_mta.replica_email_receipts r
SET identity=e.blob_id || e.thread_id || int8send(e.received_at)
FROM td_mta.emails e WHERE r.present AND (r.account,r.id)=(e.account,e.id);
UPDATE td_mta.replica_email_receipts SET digest=NULL WHERE NOT present;
ALTER TABLE td_mta.replica_email_receipts ADD CHECK(
    (present AND identity IS NOT NULL AND digest IS NOT NULL)
    OR (NOT present AND digest IS NULL)
);
CREATE INDEX replica_absent_by_observation ON td_mta.replica_email_receipts(account,observed,id)
    WHERE NOT present;
DELETE FROM td_mta.replica_mailbox_receipts r WHERE NOT r.present AND NOT EXISTS
    (SELECT FROM td_mta.mailboxes m WHERE (r.account,r.id)=(m.account,m.id));
UPDATE td_mta.replica_progress p SET receipt_count=
    (SELECT count(*) FROM td_mta.emails e WHERE e.account=p.account)
    + (SELECT count(*) FROM td_mta.blobs b WHERE b.account=p.account);
