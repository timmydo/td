use super::{deadline, sql, sql_command, wait_command, Fixture, ProbeChild};
use std::{
    io::Write,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts, postgres_history_audit as audit, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_history_audit_checks_retained_rows_in_one_snapshot() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let inspect = |limit| {
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            limit,
        )
    };
    assert_eq!(inspect(1000), Err(audit::Error::Missing));
    for selected in [account, AccountId::from_bytes([2; 16])] {
        postgres_accounts::publish(
            &fixture.request(),
            &trust,
            deadline(),
            postgres_accounts::prepare(epoch, selected).unwrap(),
        )
        .unwrap();
    }
    // Invalid history from another account cannot poison this report.
    sql(&fixture,"UPDATE td_mta.changes SET sequence=decode(repeat('00',8),'hex') WHERE account=decode(repeat('02',16),'hex')");
    let initial = inspect(1).unwrap();
    assert_eq!(initial.identity().account, account);
    assert_eq!(initial.identity().epoch, epoch);
    assert_eq!(initial.identity().committed_sequence.number(), 1);
    assert_eq!(initial.identity().history_floor.number(), 0);
    assert_eq!((initial.rows(), initial.retired_rows()), (1, 0));
    assert_eq!(inspect(0), Err(audit::Error::Capacity));
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            account,
            1
        ),
        Err(audit::Error::Epoch)
    );
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            Instant::now(),
            epoch,
            account,
            1
        ),
        Err(audit::Error::Connection(
            td_mta::postgres_probe::Error::Deadline
        ))
    );
    for sequence in ["0000000000000000", "0000000000000002", "ffffffffffffffff"] {
        sql(&fixture,&format!("UPDATE td_mta.changes SET sequence=decode('{sequence}','hex') WHERE account=decode(repeat('01',16),'hex')"));
        assert_eq!(inspect(1), Err(audit::Error::Integrity));
        assert_eq!(sql(&fixture,"SELECT encode(sequence,'hex') FROM td_mta.changes WHERE account=decode(repeat('01',16),'hex')"),sequence);
        sql(&fixture,"UPDATE td_mta.changes SET sequence=decode('0000000000000001','hex') WHERE account=decode(repeat('01',16),'hex')");
        assert_eq!(inspect(1).unwrap(), initial);
    }
    // Operation gaps and absent live objects are not event-completeness checks.
    sql(&fixture,"INSERT INTO td_mta.changes SELECT decode(repeat('01',16),'hex'),decode('0000000000000001','hex'),n,3,1,decode(lpad(to_hex(n),32,'0'),'hex') FROM generate_series(2,130) n");
    assert_eq!(inspect(130).unwrap().rows(), 130);
    assert_eq!(inspect(129), Err(audit::Error::Capacity));
    sql(&fixture,"DELETE FROM td_mta.changes WHERE account=decode(repeat('01',16),'hex') AND operation<>1; UPDATE td_mta.accounts SET floor=decode('0000000000000001','hex') WHERE id=decode(repeat('01',16),'hex')");
    let constraint=sql(&fixture,"SELECT quote_ident(conname) FROM pg_catalog.pg_constraint WHERE conrelid='td_mta.accounts'::regclass AND contype='c' AND pg_get_constraintdef(oid) LIKE '%floor%'");
    assert!(!constraint.is_empty() && !constraint.contains('\n'));
    sql(&fixture,&format!("ALTER TABLE td_mta.accounts DROP CONSTRAINT {constraint}; UPDATE td_mta.accounts SET floor=decode('0000000000000002','hex') WHERE id=decode(repeat('01',16),'hex')"));
    assert_eq!(inspect(1), Err(audit::Error::Integrity));
    sql(&fixture,&format!("UPDATE td_mta.accounts SET floor=decode('0000000000000001','hex') WHERE id=decode(repeat('01',16),'hex'); ALTER TABLE td_mta.accounts ADD CONSTRAINT {constraint} CHECK(octet_length(floor)=8 AND floor<=sequence)"));
    let retired = inspect(1).unwrap();
    assert_eq!((retired.rows(), retired.retired_rows()), (1, 1));

    let mut locker = ProbeChild(
        sql_command(&fixture)
            .env("PGAPPNAME", "td-history-audit-lock")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    writeln!(
        locker.0.stdin.as_mut().unwrap(),
        "BEGIN; LOCK TABLE td_mta.changes IN ACCESS EXCLUSIVE MODE;"
    )
    .unwrap();
    let wait = |query: &str| {
        let end = Instant::now() + Duration::from_secs(5);
        while sql(&fixture, query) != "1" {
            assert!(
                Instant::now() < end,
                "history audit concurrency fixture timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    wait("SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE a.application_name='td-history-audit-lock' AND l.relation='td_mta.changes'::regclass AND l.mode='AccessExclusiveLock' AND l.granted");
    let request = fixture.request();
    let historical = thread::scope(|scope| {
        let running =
            scope.spawn(|| audit::inspect(&request, &trust, deadline(), epoch, account, 1000));
        wait("SELECT count(*) FROM pg_locks WHERE relation='td_mta.changes'::regclass AND mode='AccessShareLock' AND NOT granted");
        sql(&fixture,"UPDATE td_mta.accounts SET sequence=decode('0000000000000002','hex') WHERE id=decode(repeat('01',16),'hex')");
        writeln!(locker.0.stdin.as_mut().unwrap(),"INSERT INTO td_mta.changes VALUES(decode(repeat('01',16),'hex'),decode('0000000000000002','hex'),0,3,1,decode(repeat('09',16),'hex')); COMMIT;").unwrap();
        drop(locker.0.stdin.take());
        wait_command(&mut locker);
        running.join().unwrap().unwrap()
    });
    assert_eq!(historical, retired);
    let current = inspect(2).unwrap();
    assert_eq!((current.rows(), current.retired_rows()), (2, 1));
    assert_eq!(current.identity().committed_sequence.number(), 2);
    // Missing events are outside canonical row validation.
    sql(
        &fixture,
        "DELETE FROM td_mta.changes WHERE account=decode(repeat('01',16),'hex')",
    );
    assert_eq!(inspect(0).unwrap().rows(), 0);
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.devices"), "0");
}
