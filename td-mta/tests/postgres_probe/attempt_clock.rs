use super::{deadline, sql, Fixture};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use td_crypto::TrustStore;
use td_mta::{
    ids::SubmissionId,
    ports::{Clock, Tick, Time},
    postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_route_owners::Claim,
    queue_clock::{self, Guard},
};
struct TestClock {
    start: Instant,
    offset: u64,
    jump_at: u64,
    fail: bool,
    samples: AtomicU64,
}
impl Clock for TestClock {
    fn sample(&self) -> Result<Time, td_mta::ports::Error> {
        let n = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
        if self.fail {
            return Err(td_mta::ports::Error::Invalid);
        }
        Ok(Time {
            // Deliberately unrelated: publication must use database UTC.
            utc_ms: -1,
            monotonic: Tick(
                self.offset
                    + self.start.elapsed().as_millis() as u64
                    + if n >= self.jump_at { 600_001 } else { 0 },
            ),
        })
    }
}
fn setup(
    fixture: &Fixture,
    stable: bool,
    jump_at: u64,
    fail: bool,
) -> (Arc<TestClock>, Arc<Mutex<Guard>>) {
    let now: i64 = sql(
        fixture,
        "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
    )
    .parse()
    .unwrap();
    let offset = if stable { 300_000 } else { 0 };
    let clock = Arc::new(TestClock {
        start: Instant::now(),
        offset,
        jump_at,
        fail,
        samples: AtomicU64::new(0),
    });
    let guard = Guard::new(Time {
        utc_ms: now - offset as i64,
        monotonic: Tick(0),
    })
    .unwrap();
    (clock, Arc::new(Mutex::new(guard)))
}
// Existing storage cases independently simulate five elapsed stable minutes.
// Production callers must retain the same process guard across publications.
pub(super) fn publish(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    limit: Instant,
    value: &attempts::Dispatch,
) -> Result<attempts::Outcome, attempts::Error> {
    let (clock, guard) = setup(fixture, true, u64::MAX, false);
    attempts::publish(request, trust, limit, value, clock, guard)
}
pub(super) fn fresh_publish(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    limit: Instant,
    value: &attempts::Dispatch,
) -> Result<attempts::Outcome, attempts::Error> {
    let (clock, guard) = setup(fixture, false, u64::MAX, false);
    attempts::publish(request, trust, limit, value, clock, guard)
}
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
) {
    let fresh = attempts::prepare(claim, generation, submission).unwrap();
    let retry = attempts::retry(claim, generation, submission).unwrap();
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    let recipients = sql(fixture, "SELECT md5(string_agg(row_to_json(r)::text,',' ORDER BY account,submission_id,ordinal)) FROM td_mta.recipients r");
    let (clock, guard) = setup(fixture, false, u64::MAX, false);
    assert_eq!(
        attempts::publish(request, trust, deadline(), &retry, clock, guard),
        Err(attempts::Error::Clock(queue_clock::Error::Unstable))
    );
    for (jump_at, fail) in [(1, false), (2, false), (u64::MAX, true)] {
        let (clock, guard) = setup(fixture, false, jump_at, fail);
        assert_eq!(
            attempts::publish(
                request,
                trust,
                deadline(),
                &fresh,
                clock.clone(),
                guard.clone()
            ),
            Err(attempts::Error::Clock(if fail {
                queue_clock::Error::Invalid
            } else {
                queue_clock::Error::Unstable
            }))
        );
        if jump_at == 2 {
            assert_eq!(clock.samples.load(Ordering::Relaxed), 2);
        }
        assert_eq!(
            attempts::inspect(request, trust, deadline(), &fresh).unwrap(),
            None
        );
        assert_eq!(
            sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts"
            ),
            before
        );
        assert_eq!(sql(fixture, "SELECT md5(string_agg(row_to_json(r)::text,',' ORDER BY account,submission_id,ordinal)) FROM td_mta.recipients r"), recipients);
        if !fail {
            // Elapsed stable time alone cannot clear the discontinuity hold.
            assert_eq!(
                attempts::publish(request, trust, deadline(), &fresh, clock, guard),
                Err(attempts::Error::Clock(queue_clock::Error::Unstable))
            );
        }
    }
    // Seed the previous database clock and lease on either side of current UTC.
    // This models real DB jumps without changing the host's wall clock.
    let owner = sql(fixture, &format!("SELECT last_clock||'|'||expires_at FROM td_mta.route_owners WHERE operation=decode('{}','hex')", claim.operation()));
    let (last, expires) = owner.split_once('|').unwrap();
    for shift in [-600_001, 600_001] {
        let now: i64 = sql(
            fixture,
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
        )
        .parse()
        .unwrap();
        let prior = now + shift;
        let (clock, _) = setup(fixture, false, u64::MAX, false);
        let guard = Arc::new(Mutex::new(
            Guard::new(Time {
                utc_ms: prior,
                monotonic: Tick(0),
            })
            .unwrap(),
        ));
        sql(fixture, &format!("UPDATE td_mta.route_owners SET last_clock={prior},expires_at={} WHERE operation=decode('{}','hex')", prior+30_000, claim.operation()));
        assert_eq!(
            attempts::publish(
                request,
                trust,
                deadline(),
                &fresh,
                clock.clone(),
                guard.clone()
            ),
            Err(attempts::Error::Clock(queue_clock::Error::Unstable))
        );
        sql(fixture, &format!("UPDATE td_mta.route_owners SET last_clock={last},expires_at={expires} WHERE operation=decode('{}','hex')", claim.operation()));
        assert_eq!(
            attempts::publish(request, trust, deadline(), &fresh, clock, guard),
            Err(attempts::Error::Clock(queue_clock::Error::Unstable))
        );
        assert_eq!(
            attempts::inspect(request, trust, deadline(), &fresh).unwrap(),
            None
        );
    }
    let (clock, guard) = setup(fixture, false, u64::MAX, false);
    let locked = guard.lock().unwrap();
    assert_eq!(
        attempts::publish(request, trust, deadline(), &fresh, clock, guard.clone()),
        Err(attempts::Error::Clock(queue_clock::Error::Invalid))
    );
    drop(locked);
}
pub(super) fn replay(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    value: &attempts::Dispatch,
    expected: &attempts::Outcome,
) {
    let (clock, guard) = setup(fixture, false, u64::MAX, true);
    assert_eq!(
        &attempts::publish(request, trust, deadline(), value, clock.clone(), guard).unwrap(),
        expected
    );
    assert_eq!(clock.samples.load(Ordering::Relaxed), 0);
}

// A rejected new phase must roll back recipients, sequence and durable claims,
// while its process-local clock hold survives that rollback.
pub(super) fn possible(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    value: &td_mta::postgres_attempt_phases::Change,
) {
    use td_mta::postgres_attempt_phases as phases;
    let snapshot = || {
        sql(fixture, "SELECT encode(sequence,'hex')||':'||(SELECT count(*) FROM td_mta.attempt_final_reservations)::text||':'||(SELECT md5(string_agg(row_to_json(r)::text,',' ORDER BY account,submission_id,ordinal)) FROM td_mta.recipients r) FROM td_mta.accounts")
    };
    let before = snapshot();
    for (jump_at, fail) in [(1, false), (2, false), (u64::MAX, true)] {
        let (clock, guard) = setup(fixture, false, jump_at, fail);
        assert_eq!(
            phases::publish_new_possible_observed(
                request,
                trust,
                deadline(),
                value,
                clock.clone(),
                guard.clone()
            ),
            Err(phases::Error::Clock(if fail {
                queue_clock::Error::Invalid
            } else {
                queue_clock::Error::Unstable
            }))
        );
        if jump_at == 2 {
            assert_eq!(clock.samples.load(Ordering::Relaxed), 2);
        }
        assert_eq!(
            phases::inspect(request, trust, deadline(), value).unwrap(),
            None
        );
        assert_eq!(snapshot(), before);
        if !fail {
            assert_eq!(
                phases::publish_new_possible_observed(
                    request,
                    trust,
                    deadline(),
                    value,
                    clock,
                    guard
                ),
                Err(phases::Error::Clock(queue_clock::Error::Unstable))
            );
        }
    }
    // Seed the previous database clock and lease on either side of current UTC.
    // This models real DB jumps without changing the host's wall clock.
    let owner = sql(fixture, &format!("SELECT last_clock||'|'||expires_at FROM td_mta.route_owners WHERE operation=decode('{}','hex')", claim.operation()));
    let (last, expires) = owner.split_once('|').unwrap();
    for shift in [-600_001, 600_001] {
        let now: i64 = sql(
            fixture,
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
        )
        .parse()
        .unwrap();
        let prior = now + shift;
        let (clock, _) = setup(fixture, false, u64::MAX, false);
        let guard = Arc::new(Mutex::new(
            Guard::new(Time {
                utc_ms: prior,
                monotonic: Tick(0),
            })
            .unwrap(),
        ));
        sql(fixture, &format!("UPDATE td_mta.route_owners SET last_clock={prior},expires_at={} WHERE operation=decode('{}','hex')", prior+30_000, claim.operation()));
        assert_eq!(
            phases::publish_new_possible_observed(
                request,
                trust,
                deadline(),
                value,
                clock.clone(),
                guard.clone()
            ),
            Err(phases::Error::Clock(queue_clock::Error::Unstable))
        );
        sql(fixture, &format!("UPDATE td_mta.route_owners SET last_clock={last},expires_at={expires} WHERE operation=decode('{}','hex')", claim.operation()));
        assert_eq!(
            phases::publish_new_possible_observed(request, trust, deadline(), value, clock, guard),
            Err(phases::Error::Clock(queue_clock::Error::Unstable))
        );
        assert_eq!(
            phases::inspect(request, trust, deadline(), value).unwrap(),
            None
        );
    }
    let (clock, guard) = setup(fixture, false, u64::MAX, false);
    let locked = guard.lock().unwrap();
    assert_eq!(
        phases::publish_new_possible_observed(
            request,
            trust,
            deadline(),
            value,
            clock,
            guard.clone()
        ),
        Err(phases::Error::Clock(queue_clock::Error::Invalid))
    );
    drop(locked);
    assert_eq!(snapshot(), before);
}
pub(super) fn publish_possible(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    limit: Instant,
    value: &td_mta::postgres_attempt_phases::Change,
) -> Result<td_mta::postgres_attempt_phases::Outcome, td_mta::postgres_attempt_phases::Error> {
    let (clock, guard) = setup(fixture, false, u64::MAX, false);
    td_mta::postgres_attempt_phases::publish_new_possible_observed(
        request, trust, limit, value, clock, guard,
    )
}
