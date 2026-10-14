CREATE TABLE td_mta.account_bootstraps (
    account pg_catalog.bytea PRIMARY KEY REFERENCES td_mta.accounts(id),
    inbox pg_catalog.bytea NOT NULL CHECK (pg_catalog.octet_length(inbox) = 16)
);
REVOKE ALL ON TABLE td_mta.account_bootstraps FROM PUBLIC;
