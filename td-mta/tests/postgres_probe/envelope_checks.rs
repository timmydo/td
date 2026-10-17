use super::{
    body_checks::{backend, closed, quiet, CleanupClock},
    deadline, sql, Fixture,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::sync_channel,
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    clock::RuntimeClock,
    ports::{Clock, Time},
    postgres_attempts::{self as attempts, Dispatch, EnvelopeChecks},
    postgres_probe::{self, Request},
    queue_clock::Guard,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    value: &Dispatch,
) {
    let runtime = Request {
        address: request.address,
        server_name: request.server_name,
        user: "mail_runtime",
        database: request.database,
        password: super::PASSWORD,
        mode: postgres_probe::Mode::Runtime,
    };
    let request = &runtime;
    let clock: Arc<dyn Clock> = Arc::new(RuntimeClock::new());
    let guard = Arc::new(Mutex::new(
        Guard::new(Time {
            utc_ms: sql(
                fixture,
                "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            )
            .parse()
            .unwrap(),
            monotonic: clock.sample().unwrap().monotonic,
        })
        .unwrap(),
    ));
    let open = || {
        quiet(fixture);
        EnvelopeChecks::open(
            request,
            trust,
            deadline(),
            value,
            clock.clone(),
            guard.clone(),
        )
        .unwrap()
    };
    let mut checks = open();
    let (first, limit) = checks.check().unwrap();
    assert_eq!(first.recipients.len(), 100);
    let pid = backend(fixture);
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT state||'|'||query FROM pg_catalog.pg_stat_activity WHERE pid={pid}")
        ),
        "idle|ROLLBACK"
    );
    assert!(sql(fixture, &format!("BEGIN; SELECT pg_catalog.pg_try_advisory_xact_lock({}); SELECT id FROM td_mta.accounts FOR UPDATE NOWAIT; SELECT singleton FROM td_mta.config_selection FOR UPDATE NOWAIT; SELECT account FROM td_mta.route_owners FOR UPDATE NOWAIT; ROLLBACK", td_mta::postgres_schema::MIGRATION_KEY)).lines().any(|line| line == "t"));
    let (second, next) = checks.check().unwrap();
    assert_eq!(second, first);
    assert!(next <= limit);
    assert_eq!(backend(fixture), pid);
    drop(checks);
    closed(fixture, &pid);

    let selection = sql(
        fixture,
        "SELECT encode(generation,'hex') FROM td_mta.config_selection",
    );
    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')",
    );
    assert_eq!(checks.check(), Err(attempts::Error::Policy));
    closed(fixture, &pid);
    sql(
        fixture,
        &format!("UPDATE td_mta.config_selection SET generation=decode('{selection}','hex')"),
    );
    assert_eq!(checks.check(), Err(attempts::Error::Policy));

    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    sql(fixture, "ALTER ROLE mail_runtime CREATEDB");
    let role = attempts::Error::Connection(postgres_probe::Error::RuntimeRole);
    assert_eq!(checks.check(), Err(role));
    closed(fixture, &pid);
    sql(fixture, "ALTER ROLE mail_runtime NOCREATEDB");
    assert_eq!(checks.check(), Err(role));

    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    sql(fixture, "GRANT TRUNCATE ON td_mta.blobs TO mail_runtime");
    let privileges = attempts::Error::Connection(postgres_probe::Error::RuntimePrivileges);
    assert_eq!(checks.check(), Err(privileges));
    closed(fixture, &pid);
    sql(fixture, "REVOKE TRUNCATE ON td_mta.blobs FROM mail_runtime");
    assert_eq!(checks.check(), Err(privileges));

    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    sql(
        fixture,
        "UPDATE td_mta.recipients SET phase=2 WHERE state=2 AND phase=1",
    );
    assert_eq!(checks.check(), Err(attempts::Error::Conflict));
    closed(fixture, &pid);
    sql(
        fixture,
        "UPDATE td_mta.recipients SET phase=1 WHERE state=2 AND phase=2",
    );
    assert_eq!(checks.check(), Err(attempts::Error::Conflict));

    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    let attempt = value.attempt();
    let selected = format!("a.attempt_id=decode('{attempt}','hex') AND o.account=a.account AND o.route_name=a.route_name AND o.destination=a.destination");
    let generation = sql(fixture, &format!("SELECT encode(o.generation,'hex') FROM td_mta.route_owners o JOIN td_mta.submission_attempts a ON {selected}"));
    sql(fixture, &format!("UPDATE td_mta.route_owners o SET generation=decode('0000000000000063','hex') FROM td_mta.submission_attempts a WHERE {selected}"));
    let stale = attempts::Error::Owner(td_mta::postgres_route_owners::Error::Stale);
    assert_eq!(checks.check(), Err(stale));
    closed(fixture, &pid);
    sql(
        fixture,
        &format!("UPDATE td_mta.route_owners o SET generation=decode('{generation}','hex') FROM td_mta.submission_attempts a WHERE {selected}"),
    );
    assert_eq!(checks.check(), Err(stale));

    let mut checks = open();
    let (before, _) = checks.check().unwrap();
    sql(fixture, "CREATE TABLE public.envelope_addresses AS SELECT account,submission_id,ordinal,address FROM td_mta.recipients; UPDATE td_mta.recipients SET address='changed@example.test' WHERE state=2 AND phase=1 AND ordinal=0");
    let (after, _) = checks.check().unwrap();
    assert_ne!(after, before);
    assert_eq!(
        after.recipients.first().unwrap().address,
        "changed@example.test"
    );
    drop(checks);
    sql(fixture, "UPDATE td_mta.recipients r SET address=a.address FROM public.envelope_addresses a WHERE r.account=a.account AND r.submission_id=a.submission_id AND r.ordinal=a.ordinal; DROP TABLE public.envelope_addresses");

    let mut checks = open();
    let mut passed = 0;
    let failure = loop {
        match checks.check() {
            Ok(_) => {
                passed += 1;
                assert!(passed < 32, "connection counters were replenished");
            }
            Err(error) => break error,
        }
    };
    assert!(passed >= 2);
    assert!(matches!(
        failure,
        attempts::Error::Database | attempts::Error::Connection(postgres_probe::Error::Database)
    ));
    assert_eq!(checks.check(), Err(failure));

    quiet(fixture);
    let cap = Instant::now() + Duration::from_secs(1);
    let mut checks =
        EnvelopeChecks::open(request, trust, cap, value, clock.clone(), guard.clone()).unwrap();
    assert!(checks.check().unwrap().1 <= cap);
    thread::sleep(cap.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    let failure = checks.check().unwrap_err();
    assert_eq!(
        failure,
        attempts::Error::Connection(postgres_probe::Error::Deadline)
    );
    assert_eq!(checks.check(), Err(failure));

    quiet(fixture);
    let (reached, receive) = sync_channel(1);
    let (proceed, resume) = sync_channel(1);
    let clock = Arc::new(CleanupClock {
        inner: clock,
        samples: AtomicUsize::new(0),
        reached,
        proceed: Mutex::new(resume),
    });
    let mut checks =
        EnvelopeChecks::open(request, trust, deadline(), value, clock.clone(), guard).unwrap();
    let pid = backend(fixture);
    thread::scope(|scope| {
        let kill_pid = pid.clone();
        let killer = scope.spawn(move || {
            receive.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(
                sql(
                    fixture,
                    &format!("SELECT pg_catalog.pg_terminate_backend({kill_pid},1000)")
                ),
                "t"
            );
            proceed.send(()).unwrap();
        });
        let failure = checks.check().unwrap_err();
        assert!(matches!(
            failure,
            attempts::Error::Database
                | attempts::Error::Connection(postgres_probe::Error::Database)
        ));
        assert_eq!(checks.check(), Err(failure));
        killer.join().unwrap();
    });
    assert_eq!(clock.samples.load(Ordering::Relaxed), 2);
    closed(fixture, &pid);
}
