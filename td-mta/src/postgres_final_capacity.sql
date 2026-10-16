-- Resolve old possible-acceptance attempts with the old binary before upgrade.
-- A migration must not promise capacity that old writers never reserved.
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM td_mta.recipients WHERE state=2 AND phase=3) THEN
        RAISE EXCEPTION 'resolve AcceptancePossible attempts before upgrading';
    END IF;
END $$;
CREATE TABLE td_mta.attempt_final_reservations(
    account bytea NOT NULL,
    attempt_id bytea NOT NULL,
    ordinals bytea NOT NULL CHECK(octet_length(ordinals) BETWEEN 4 AND 400 AND octet_length(ordinals)%4=0),
    PRIMARY KEY(account,attempt_id),
    FOREIGN KEY(account,attempt_id) REFERENCES td_mta.submission_attempts(account,attempt_id)
);
REVOKE ALL ON TABLE td_mta.attempt_final_reservations FROM PUBLIC;
