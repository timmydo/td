use super::{deadline, sql, Fixture};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ports::{Clock, Time},
    postgres_attempt_phases::{self as phases, BodyChecks},
    postgres_probe::{self, Request},
    queue_clock::Guard,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    change: &phases::Change,
    clock: Arc<dyn Clock>,
    guard: Arc<Mutex<Guard>>,
) {
    let open = || {
        quiet(fixture);
        BodyChecks::open(
            request,
            trust,
            deadline(),
            change,
            clock.clone(),
            guard.clone(),
        )
        .unwrap()
    };
    let mut checks = open();
    let first = checks.check().unwrap();
    let pid = backend(fixture);
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT state||'|'||query FROM pg_catalog.pg_stat_activity WHERE pid={pid}")
        ),
        "idle|ROLLBACK"
    );
    // A different session must acquire all conflicting locks while this
    // validator remains alive. Counting pg_locks cannot detect recursion.
    assert!(sql(fixture, &format!("BEGIN; SELECT pg_catalog.pg_try_advisory_xact_lock({}); SELECT id FROM td_mta.accounts FOR UPDATE NOWAIT; SELECT singleton FROM td_mta.config_selection FOR UPDATE NOWAIT; SELECT account FROM td_mta.route_owners FOR UPDATE NOWAIT; ROLLBACK", td_mta::postgres_schema::MIGRATION_KEY)).lines().any(|line| line == "t"));
    assert!(checks.check().unwrap() <= first);
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
    assert_eq!(checks.check(), Err(phases::Error::Policy));
    closed(fixture, &pid);
    sql(
        fixture,
        &format!("UPDATE td_mta.config_selection SET generation=decode('{selection}','hex')"),
    );
    assert_eq!(checks.check(), Err(phases::Error::Policy));

    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    sql(fixture, "ALTER ROLE mail_runtime CREATEDB");
    let role = phases::Error::Connection(postgres_probe::Error::RuntimeRole);
    assert_eq!(checks.check(), Err(role));
    closed(fixture, &pid);
    sql(fixture, "ALTER ROLE mail_runtime NOCREATEDB");
    assert_eq!(checks.check(), Err(role));

    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    sql(fixture, "GRANT TRUNCATE ON td_mta.blobs TO mail_runtime");
    let privileges = phases::Error::Connection(postgres_probe::Error::RuntimePrivileges);
    assert_eq!(checks.check(), Err(privileges));
    closed(fixture, &pid);
    sql(fixture, "REVOKE TRUNCATE ON td_mta.blobs FROM mail_runtime");
    assert_eq!(checks.check(), Err(privileges));

    let mut checks = open();
    checks.check().unwrap();
    let pid = backend(fixture);
    sql(fixture, "CREATE TABLE public.body_turn_replies AS SELECT account,submission_id,ordinal,rcpt_reply FROM td_mta.recipients; UPDATE td_mta.recipients SET rcpt_reply='250 different' WHERE state=2 AND phase=2");
    assert_eq!(checks.check(), Err(phases::Error::Integrity));
    closed(fixture, &pid);
    sql(fixture, "UPDATE td_mta.recipients r SET rcpt_reply=b.rcpt_reply FROM public.body_turn_replies b WHERE r.account=b.account AND r.submission_id=b.submission_id AND r.ordinal=b.ordinal; DROP TABLE public.body_turn_replies");
    assert_eq!(checks.check(), Err(phases::Error::Integrity));

    // The original frame/byte profile is cumulative, not replenished by
    // successful ROLLBACK or a new validation turn.
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
        phases::Error::Database | phases::Error::Connection(postgres_probe::Error::Database)
    ));
    assert_eq!(checks.check(), Err(failure));

    // A completed turn cannot refresh the original absolute deadline.
    quiet(fixture);
    let cap = Instant::now() + Duration::from_secs(1);
    let mut checks =
        BodyChecks::open(request, trust, cap, change, clock.clone(), guard.clone()).unwrap();
    assert!(checks.check().unwrap() <= cap);
    thread::sleep(cap.saturating_duration_since(Instant::now()) + Duration::from_millis(1));
    let failure = checks.check().unwrap_err();
    assert_eq!(
        failure,
        phases::Error::Connection(postgres_probe::Error::Deadline)
    );
    assert_eq!(checks.check(), Err(failure));

    quiet(fixture);
    cleanup_failure(fixture, request, trust, change, clock, guard);
}
fn quiet(fixture: &Fixture) {
    let stop = deadline();
    while sql(
        fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(Instant::now() < stop, "previous backend remained connected");
        thread::sleep(Duration::from_millis(1));
    }
}
fn backend(fixture: &Fixture) -> String {
    let pid = sql(
        fixture,
        "SELECT pid FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    );
    pid.parse::<u32>().unwrap();
    pid
}
fn closed(fixture: &Fixture, pid: &str) {
    let stop = deadline();
    while sql(
        fixture,
        &format!("SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE pid={pid}"),
    ) != "0"
    {
        assert!(
            Instant::now() < stop,
            "refused validator retained its backend"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
struct CleanupClock {
    inner: Arc<dyn Clock>,
    samples: AtomicUsize,
    reached: SyncSender<()>,
    proceed: Mutex<Receiver<()>>,
}
impl Clock for CleanupClock {
    fn sample(&self) -> Result<Time, td_mta::ports::Error> {
        if self.samples.fetch_add(1, Ordering::Relaxed) == 1 {
            self.reached.send(()).unwrap();
            self.proceed
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
        }
        self.inner.sample()
    }
}
fn cleanup_failure(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    change: &phases::Change,
    clock: Arc<dyn Clock>,
    guard: Arc<Mutex<Guard>>,
) {
    let (reached, receive) = sync_channel(1);
    let (proceed, resume) = sync_channel(1);
    let clock = Arc::new(CleanupClock {
        inner: clock,
        samples: AtomicUsize::new(0),
        reached,
        proceed: Mutex::new(resume),
    });
    let mut checks =
        BodyChecks::open(request, trust, deadline(), change, clock.clone(), guard).unwrap();
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
        // The second database-clock observation has completed every live
        // check. The peer now vanishes before ROLLBACK can be acknowledged.
        let failure = checks.check().unwrap_err();
        assert!(matches!(
            failure,
            phases::Error::Database | phases::Error::Connection(postgres_probe::Error::Database)
        ));
        assert_eq!(checks.check(), Err(failure));
        killer.join().unwrap();
    });
    assert_eq!(clock.samples.load(Ordering::Relaxed), 2);
    closed(fixture, &pid);
}
