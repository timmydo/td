use super::{create_runtime_login, deadline, grant_runtime_mail, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::{BootId, StoreEpoch},
    observability::{Context, Event, Kind},
    postgres_logs as logs, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL 17 tools and private trusted TMPDIR required"]
pub(super) fn real_postgres_logs_bound_retention_rate_wrap_and_inspection() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    sql(&fixture,"DROP TABLE td_mta.application_logs,td_mta.application_log_state; DELETE FROM td_mta.schema_migrations WHERE version=29");
    let upgraded = postgres_schema::upgrade(&fixture.request(), &trust, deadline()).unwrap();
    assert_eq!(upgraded.version, 29);
    assert_eq!(upgraded.store_epoch, epoch);
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    let mut request = fixture.request();
    request.user = "mail_runtime";
    request.mode = td_mta::postgres_probe::Mode::Runtime;
    let event = Event {
        kind: Kind::BootStarted,
        context: Context {
            boot: BootId::from_bytes([1; 16]),
            utc_ms: Some(0),
            config_generation: 1,
            connection: None,
            request: None,
            transaction: None,
            submission: None,
        },
    };
    let append =
        |events: &[Event]| logs::append(&request, &trust, deadline(), epoch, "frontend", events);
    let page = |before| logs::inspect(&request, &trust, deadline(), epoch, before, 100).unwrap();
    let connection = fixture.root.join("log-connection");
    std::fs::write(
        &connection,
        format!(
            "postgresql://mail_runtime:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(super::PASSWORD).unwrap(),
            fixture.port,
            fixture.root.join("sql-ca.pem").display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&connection, std::fs::Permissions::from_mode(0o600)).unwrap();
    let loaded =
        td_mta::postgres_input::Loaded::open(connection.to_str().unwrap(), "frontend").unwrap();
    assert_eq!(
        logs::append_resolved(
            &loaded,
            std::path::Path::new(env!("CARGO_BIN_EXE_td-mta")),
            std::time::Instant::now() + std::time::Duration::from_secs(2),
            epoch,
            "frontend",
            &[event]
        )
        .unwrap()
        .stored,
        1
    );
    let first = page(None);
    assert_eq!(first.records.len(), 1);
    assert_eq!(first.records[0].id, 1);
    assert!(first.records[0].recorded_ms > 0);
    assert!(first.records[0].event.contains("boot_started"));
    assert_eq!(
        logs::append(
            &request,
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            "frontend",
            &[event]
        ),
        Err(logs::Error::Epoch)
    );
    assert_eq!(
        logs::append(
            &request,
            &trust,
            deadline(),
            epoch,
            "bad\nprofile",
            &[event]
        ),
        Err(logs::Error::Invalid)
    );
    assert_eq!(append(&[event; 17]), Err(logs::Error::Invalid));
    assert!(logs::inspect(&request, &trust, deadline(), epoch, None, 101).is_err());
    sql(&fixture,"UPDATE td_mta.application_log_state SET used=599,window_ms=(extract(epoch FROM clock_timestamp())*1000)::bigint");
    let done = append(&[event; 16]).unwrap();
    assert_eq!((done.stored, done.suppressed), (1, 15));
    assert_eq!(append(&[event]).unwrap().suppressed, 1);
    assert_eq!(page(None).suppressed, 16);
    // Concurrent instances share the same persisted rate allowance.
    sql(&fixture,"UPDATE td_mta.application_log_state SET used=599,window_ms=(extract(epoch FROM clock_timestamp())*1000)::bigint");
    std::thread::scope(|scope| {
        let a = scope.spawn(|| append(&[event; 16]));
        let b = scope.spawn(|| append(&[event; 16]));
        let results = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(
            results
                .iter()
                .filter_map(|r| r.as_ref().ok())
                .map(|r| r.stored)
                .sum::<usize>(),
            1
        );
    });
    assert_eq!(
        sql(&fixture, "SELECT used FROM td_mta.application_log_state"),
        "600"
    );
    // Keep the ring-wrap expectation independent of concurrent fixture rows.
    sql(&fixture, "DELETE FROM td_mta.application_logs WHERE id>2");
    // Fixed slots replace an old live row; counter records capacity eviction.
    sql(
        &fixture,
        "UPDATE td_mta.application_log_state SET used=0,window_ms=0,next_id=65537",
    );
    assert_eq!(append(&[event; 2]).unwrap().stored, 2);
    let wrapped = page(None);
    assert_eq!(wrapped.evicted, 2);
    assert_eq!(
        wrapped.records.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![65538, 65537]
    );
    assert_eq!(page(Some(65538)).records[0].id, 65537);
    // A wall-clock jump forward expires rows, and an idle flush reclaims them.
    sql(&fixture, "UPDATE td_mta.application_logs SET recorded_ms=0");
    assert!(page(None).records.is_empty());
    append(&[]).unwrap();
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.application_logs"),
        "0"
    );
    // Exhaustion refuses instead of reusing IDs.
    sql(
        &fixture,
        "UPDATE td_mta.application_log_state SET used=0,next_id=9223372036854775807",
    );
    assert_eq!(append(&[event]), Err(logs::Error::Integrity));
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.application_logs"),
        "0"
    );
    // Bound the complete live ring, then simulate retained dead tuples. The
    // physical high-water stop must refuse inserts even with logical capacity.
    sql(&fixture,"TRUNCATE td_mta.application_logs; ALTER TABLE td_mta.application_logs SET(autovacuum_enabled=false); INSERT INTO td_mta.application_logs SELECT n,n+1,(extract(epoch FROM clock_timestamp())*1000)::bigint,'frontend',repeat(' ',1022)||'{}' FROM generate_series(0,65535) n; UPDATE td_mta.application_logs SET event=repeat(' ',1021)||'{} '; UPDATE td_mta.application_log_state SET used=0,next_id=65537");
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.application_logs"),
        "65536"
    );
    let size:i64=sql(&fixture,"SELECT pg_total_relation_size('td_mta.application_logs')+pg_total_relation_size('td_mta.application_log_state')").parse().unwrap();
    assert!(
        size >= logs::MAX_RELATION_BYTES,
        "fixture did not create retained bloat: {size}"
    );
    let paused = append(&[event]).unwrap();
    assert_eq!((paused.stored, paused.suppressed), (0, 1));
    assert_eq!(
        sql(&fixture, "SELECT next_id FROM td_mta.application_log_state"),
        "65537"
    );
}
