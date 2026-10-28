-- Current graph and latest observation stay cheap as tombstones accumulate.
CREATE INDEX replica_mailbox_observation ON td_mta.replica_mailbox_receipts(account,observed DESC);
CREATE INDEX replica_mailbox_present ON td_mta.replica_mailbox_receipts(account,id) WHERE present;
