ALTER TABLE td_mta.recipients DROP CONSTRAINT recipients_reason_check;
ALTER TABLE td_mta.recipients ADD CONSTRAINT recipients_reason_check
    CHECK(reason BETWEEN 0 AND 10);
