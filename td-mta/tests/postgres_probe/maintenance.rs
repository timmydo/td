use super::{create_runtime_login, deadline, grant_runtime_mail, hold_schema_lock, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts,
    postgres_maintenance::{self as maintenance, Error, ExternalFence},
    postgres_probe::{self, Mode},
    postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_maintenance_entry_is_owner_only_epoch_bound_and_monotone() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let receipt = postgres_schema::initialize(&owner, &trust, deadline()).unwrap();
    let epoch = receipt.store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    postgres_accounts::publish(
        &owner,
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let inspect = || maintenance::inspect(&owner, &trust, deadline(), epoch);
    let enter = || {
        maintenance::enter(
            &owner,
            &trust,
            deadline(),
            epoch,
            ExternalFence::InstancesAndSmtpEgressStopped,
        )
    };
    assert_eq!(inspect(), Ok(false));
    assert_eq!(
        maintenance::enter(
            &owner,
            &trust,
            Instant::now(),
            epoch,
            ExternalFence::InstancesAndSmtpEgressStopped
        ),
        Err(Error::Connection(postgres_probe::Error::Deadline))
    );
    assert_eq!(
        maintenance::enter(
            &owner,
            &trust,
            deadline(),
            StoreEpoch::from_bytes([2; 16]),
            ExternalFence::InstancesAndSmtpEgressStopped
        ),
        Err(Error::Epoch)
    );
    for shared in [true, false] {
        let lock = hold_schema_lock(&fixture, shared);
        assert_eq!(enter(), Err(Error::Schema(postgres_schema::Error::Busy)));
        drop(lock);
        let stop = deadline();
        while sql(&fixture, "SELECT count(*) FROM pg_catalog.pg_locks l JOIN pg_catalog.pg_stat_activity a ON l.pid=a.pid WHERE a.application_name='td-mta-schema-fixture' AND l.locktype='advisory' AND l.granted") != "0" {
            assert!(Instant::now() < stop, "fixture migration lock was not released");
            thread::sleep(Duration::from_millis(10));
        }
    }
    assert_eq!(inspect(), Ok(false));
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    let mut runtime = fixture.request();
    runtime.user = "mail_runtime";
    assert_eq!(
        maintenance::inspect(&runtime, &trust, deadline(), epoch),
        Err(Error::Owner)
    );
    sql(
        &fixture,
        "GRANT UPDATE(blocked) ON td_mta.runtime_barrier TO mail_runtime",
    );
    assert_eq!(
        maintenance::enter(
            &runtime,
            &trust,
            deadline(),
            epoch,
            ExternalFence::InstancesAndSmtpEgressStopped
        ),
        Err(Error::Owner)
    );
    sql(
        &fixture,
        "REVOKE UPDATE(blocked) ON td_mta.runtime_barrier FROM mail_runtime",
    );
    runtime.mode = Mode::Runtime;
    assert_eq!(
        maintenance::enter(
            &runtime,
            &trust,
            deadline(),
            epoch,
            ExternalFence::InstancesAndSmtpEgressStopped
        ),
        Err(Error::Owner)
    );
    assert!(postgres_probe::probe(&runtime, &trust, deadline()).is_ok());
    sql(&fixture,"ALTER ROLE td_probe SET default_transaction_isolation='repeatable read'; CREATE FUNCTION public.require_barrier_settings() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('synchronous_commit') <> 'on' OR current_setting('idle_in_transaction_session_timeout') <> '10s' OR current_setting('transaction_isolation') <> 'read committed' THEN RAISE EXCEPTION 'barrier settings'; END IF; RETURN NEW; END $$; CREATE TRIGGER require_barrier_settings BEFORE UPDATE ON td_mta.runtime_barrier FOR EACH ROW EXECUTE FUNCTION public.require_barrier_settings()");
    assert_eq!(enter(), Ok(()));
    assert_eq!(inspect(), Ok(true));
    assert_eq!(enter(), Ok(()));
    assert_eq!(
        postgres_probe::probe(&runtime, &trust, deadline()),
        Err(postgres_probe::Error::RuntimeMaintenance)
    );
    assert_eq!(
        postgres_schema::inspect(&owner, &trust, deadline()).unwrap(),
        receipt
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*),min(encode(sequence,'hex')) FROM td_mta.accounts"
        ),
        "1|0000000000000001"
    );
    // Simulate only the future release invariant, not a supported restore:
    // clearing the barrier must atomically change every ledger row's epoch.
    sql(&fixture,"DROP TRIGGER require_barrier_settings ON td_mta.runtime_barrier; BEGIN; UPDATE td_mta.schema_migrations SET store_epoch=decode(repeat('22',16),'hex'); UPDATE td_mta.runtime_barrier SET blocked=false; COMMIT");
    assert_eq!(enter(), Err(Error::Epoch));
    let renewed = StoreEpoch::from_bytes([0x22; 16]);
    assert_eq!(
        maintenance::inspect(&owner, &trust, deadline(), renewed),
        Ok(false)
    );
    sql(&fixture, "DELETE FROM td_mta.runtime_barrier");
    assert_eq!(
        maintenance::inspect(&owner, &trust, deadline(), renewed),
        Err(Error::Integrity)
    );
    assert_eq!(
        maintenance::enter(
            &owner,
            &trust,
            deadline(),
            renewed,
            ExternalFence::InstancesAndSmtpEgressStopped
        ),
        Err(Error::Integrity)
    );
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.runtime_barrier"),
        "0"
    );
}

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_maintenance_entry_resolves_uncertain_commit_at_same_epoch() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let epoch = postgres_schema::initialize(&owner, &trust, deadline())
        .unwrap()
        .store_epoch;
    sql(&fixture,"CREATE FUNCTION public.delay_barrier_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(6); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_barrier_commit AFTER UPDATE ON td_mta.runtime_barrier DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_barrier_commit()");
    assert_eq!(
        maintenance::enter(
            &owner,
            &trust,
            Instant::now() + Duration::from_secs(2),
            epoch,
            ExternalFence::InstancesAndSmtpEgressStopped
        ),
        Err(Error::Indeterminate)
    );
    assert_eq!(
        maintenance::inspect(&owner, &trust, deadline(), epoch),
        Err(Error::Schema(postgres_schema::Error::Busy))
    );
    let stop = Instant::now() + Duration::from_secs(8);
    while sql(
        &fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        maintenance::inspect(&owner, &trust, deadline(), epoch),
        Ok(true)
    );
    // A retry observes true without issuing UPDATE or firing the delay again.
    assert_eq!(
        maintenance::enter(
            &owner,
            &trust,
            Instant::now() + Duration::from_secs(2),
            epoch,
            ExternalFence::InstancesAndSmtpEgressStopped
        ),
        Ok(())
    );
}
