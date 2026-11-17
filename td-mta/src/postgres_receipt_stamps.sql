-- HTTP write receipts expire by database-clock age. Rows an older binary
-- left carry no stamp and expire on the account's next publication.
ALTER TABLE td_mta.flag_receipts ADD COLUMN created_at bigint NOT NULL DEFAULT 0;
ALTER TABLE td_mta.flag_receipts ALTER COLUMN created_at DROP DEFAULT;
ALTER TABLE td_mta.mailbox_create_receipts ADD COLUMN created_at bigint NOT NULL DEFAULT 0;
ALTER TABLE td_mta.mailbox_create_receipts ALTER COLUMN created_at DROP DEFAULT;
ALTER TABLE td_mta.submission_cancel_receipts ADD COLUMN created_at bigint NOT NULL DEFAULT 0;
ALTER TABLE td_mta.submission_cancel_receipts ALTER COLUMN created_at DROP DEFAULT;
