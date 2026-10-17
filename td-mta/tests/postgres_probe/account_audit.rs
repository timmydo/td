use super::{deadline, sql, sql_command, wait_command, Fixture, ProbeChild};
use std::{
    io::Write,
    process::Stdio,
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::AccountId, metadata_sweep, postgres_account_audit as audit, postgres_accounts,
    postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_account_audit_keeps_metadata_counters_and_bodies_in_one_snapshot() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let limits = audit::Limits {
        metadata: metadata_sweep::Limits {
            rows: 100,
            parent_reads: 100,
        },
        blobs: 2,
        bytes: 3,
    };
    let inspect = || {
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            0,
            limits,
        )
    };
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let empty = inspect().unwrap();
    assert_eq!(empty.blobs(), 0);
    assert_eq!(empty.bytes(), 0);
    sql(&fixture,"INSERT INTO td_mta.blob_ids SELECT decode(repeat('01',16),'hex'),decode(repeat(n,16),'hex') FROM unnest(ARRAY['03','04']) n; INSERT INTO td_mta.blobs SELECT account,id,CASE WHEN id=decode(repeat('03',16),'hex') THEN 3 ELSE 0 END,CASE WHEN id=decode(repeat('03',16),'hex') THEN decode('ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad','hex') ELSE decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex') END,0,decode('0000000000000001','hex'),CASE WHEN id=decode(repeat('03',16),'hex') THEN 1 ELSE 0 END FROM td_mta.blob_ids; INSERT INTO td_mta.blob_chunks VALUES(decode(repeat('01',16),'hex'),decode(repeat('03',16),'hex'),0,decode('616263','hex')); UPDATE td_mta.account_usage SET body_bytes=3,blob_count=2");
    let report = inspect().unwrap();
    assert_eq!((report.blobs(), report.bytes()), (2, 3));
    assert_eq!(report.usage().blob_count, 2);
    assert_eq!(report.usage().body_bytes, 3);
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    for bounded in [
        audit::Limits { blobs: 1, ..limits },
        audit::Limits { bytes: 2, ..limits },
    ] {
        assert_eq!(
            audit::inspect(
                &fixture.request(),
                &trust,
                deadline(),
                epoch,
                account,
                0,
                bounded
            ),
            Err(audit::Error::Capacity)
        );
    }
    sql(&fixture, "UPDATE td_mta.mailboxes SET name=chr(10)");
    assert!(matches!(inspect(), Err(audit::Error::Metadata(_))));
    sql(
        &fixture,
        "UPDATE td_mta.mailboxes SET name='Inbox'; UPDATE td_mta.account_usage SET blob_count=3",
    );
    assert!(matches!(inspect(), Err(audit::Error::Usage(_))));
    assert_eq!(
        sql(&fixture, "SELECT blob_count FROM td_mta.account_usage"),
        "3"
    );
    sql(&fixture,"UPDATE td_mta.account_usage SET blob_count=2; UPDATE td_mta.blobs SET digest=decode(repeat('00',32),'hex') WHERE length=0");
    assert!(matches!(inspect(), Err(audit::Error::Body(_))));
    sql(&fixture,"UPDATE td_mta.blobs SET digest=decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex') WHERE length=0");
    assert_eq!(inspect().unwrap(), report);

    // Block the metadata pass after the account snapshot has been captured.
    let mut locker = ProbeChild(
        sql_command(&fixture)
            .env("PGAPPNAME", "td-account-audit-lock")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    writeln!(
        locker.0.stdin.as_mut().unwrap(),
        "BEGIN; LOCK TABLE td_mta.mailboxes IN ACCESS EXCLUSIVE MODE;"
    )
    .unwrap();
    let wait = |query: &str| {
        let end = Instant::now() + Duration::from_secs(5);
        while sql(&fixture, query) != "1" {
            assert!(Instant::now() < end, "audit concurrency fixture timed out");
            thread::sleep(Duration::from_millis(5));
        }
    };
    wait("SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE a.application_name='td-account-audit-lock' AND l.relation='td_mta.mailboxes'::regclass AND l.mode='AccessExclusiveLock' AND l.granted");
    let request = fixture.request();
    let historical = thread::scope(|scope| {
        let running =
            scope.spawn(|| audit::inspect(&request, &trust, deadline(), epoch, account, 0, limits));
        wait("SELECT count(*) FROM pg_locks WHERE relation='td_mta.mailboxes'::regclass AND mode='AccessShareLock' AND NOT granted");
        sql(&fixture,"UPDATE td_mta.blob_chunks SET bytes=decode('78797a','hex'); UPDATE td_mta.account_usage SET body_bytes=99; UPDATE td_mta.accounts SET sequence=decode('0000000000000002','hex')");
        writeln!(locker.0.stdin.as_mut().unwrap(), "ROLLBACK;\n\\q").unwrap();
        drop(locker.0.stdin.take());
        wait_command(&mut locker);
        running.join().unwrap().unwrap()
    });
    assert_eq!(historical, report);
    assert!(matches!(inspect(), Err(audit::Error::Usage(_))));
    sql(&fixture, "UPDATE td_mta.account_usage SET body_bytes=3");
    assert!(matches!(inspect(), Err(audit::Error::Body(_))));
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(bytes,'hex') FROM td_mta.blob_chunks"
        ),
        "78797a"
    );
    sql(
        &fixture,
        "UPDATE td_mta.blob_chunks SET bytes=decode('616263','hex')",
    );
    assert_eq!(
        inspect()
            .unwrap()
            .metadata()
            .references()
            .identity()
            .committed_sequence
            .number(),
        2
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.devices"), "0");
}
