use super::{deadline, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AttemptId, BootId, SubmissionId},
    postgres_attempt_phases as phases, postgres_attempt_recovery as recovery,
    postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_route_health::{self as health, Error},
    postgres_route_owners as owners, postgres_route_success as success,
    postgres_runtime_profile::Binding,
};
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    submission: SubmissionId,
) {
    let claim = owners::prepare(binding, BootId::from_bytes([90; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
    let previous = AttemptId::parse(&sql(
        fixture,
        "SELECT encode(attempt_id,'hex') FROM td_mta.submission_attempts WHERE active",
    ))
    .unwrap();
    recovery::publish(
        request,
        trust,
        deadline(),
        &recovery::prepare(&claim, grant.generation, previous).unwrap(),
    )
    .unwrap();
    sql(fixture,&format!("UPDATE td_mta.route_health SET failed_at=failed_at-3600000,next_at=next_at-3600000; UPDATE td_mta.recipients SET last_attempt_at=last_attempt_at-3600000,next_attempt_at=next_attempt_at-3600000 WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    let attempt = attempts::publish(
        request,
        trust,
        deadline(),
        &attempts::retry(&claim, grant.generation, submission).unwrap(),
    )
    .unwrap();
    assert_eq!(attempt.ordinals, vec![0]);
    let abandoned = attempt;
    recovery::publish(
        request,
        trust,
        deadline(),
        &recovery::interrupted(&claim, grant.generation, abandoned.attempt).unwrap(),
    )
    .unwrap();
    sql(fixture, &format!("UPDATE td_mta.recipients SET last_attempt_at=last_attempt_at-3600000,next_attempt_at=next_attempt_at-3600000 WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    let attempt = attempts::publish(
        request,
        trust,
        deadline(),
        &attempts::retry(&claim, grant.generation, submission).unwrap(),
    )
    .unwrap();

    let value = success::prepare(&claim, grant.generation, attempt.attempt).unwrap();
    assert_eq!(
        success::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    assert_eq!(
        success::publish(request, trust, deadline(), &value),
        Err(Error::Conflict)
    );
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::body(
            &claim,
            grant.generation,
            attempt.attempt,
            &[(0, "250 recipient")],
        )
        .unwrap(),
    )
    .unwrap();
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::acceptance_possible(&claim, grant.generation, attempt.attempt, &[0]).unwrap(),
    )
    .unwrap();
    assert_eq!(
        success::publish(request, trust, deadline(), &value),
        Err(Error::Conflict)
    );
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::accepted(
            &claim,
            grant.generation,
            attempt.attempt,
            &[0],
            "250 queued",
        )
        .unwrap(),
    )
    .unwrap();
    let before = sql(
        fixture,
        "SELECT encode(revision,'hex'),failure_count FROM td_mta.route_health",
    );
    let account = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(
        fixture,
        "UPDATE td_mta.route_health SET repair_generation=decode('0000000000000001','hex')",
    );
    assert_eq!(
        success::publish(request, trust, deadline(), &value),
        Err(Error::RepairRequired)
    );
    sql(fixture,"UPDATE td_mta.route_health SET repair_generation=NULL; CREATE FUNCTION public.reject_success() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action=3 THEN RAISE EXCEPTION 'fixture refusal'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_success BEFORE INSERT ON td_mta.route_health_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_success()");
    assert_eq!(
        success::publish(request, trust, deadline(), &value),
        Err(Error::Database)
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(revision,'hex'),failure_count FROM td_mta.route_health"
        ),
        before
    );
    sql(fixture,"DROP TRIGGER reject_success ON td_mta.route_health_receipts; CREATE FUNCTION public.delay_success() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.action=3 THEN PERFORM pg_sleep(3); END IF; RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_success AFTER INSERT ON td_mta.route_health_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_success()");
    assert_eq!(
        success::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &value
        ),
        Err(Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome = success::publish(request, trust, deadline(), &value).unwrap();
    assert_eq!(outcome.state.revision, outcome.old_revision + 1);
    assert_eq!(outcome.state.failure_count, 0);
    assert_eq!(outcome.state.reason, 0);
    assert_eq!(outcome.state.failed_at, None);
    assert_eq!(outcome.state.next_at, None);
    assert_eq!(outcome.state.repair_generation, None);
    // A delayed report for the retired older attempt cannot undo recovery.
    assert_eq!(
        health::publish(
            request,
            trust,
            deadline(),
            &health::prepare(
                &claim,
                grant.generation,
                abandoned.attempt,
                health::Failure::Tls
            )
            .unwrap()
        ),
        Err(Error::Conflict)
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT failure_count,repair_generation IS NULL FROM td_mta.route_health"
        ),
        "0|t"
    );

    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        account
    );
    assert_eq!(
        health::publish(
            request,
            trust,
            deadline(),
            &health::prepare(
                &claim,
                grant.generation,
                attempt.attempt,
                health::Failure::Network
            )
            .unwrap()
        ),
        Err(Error::Conflict)
    );
    owners::release(request, trust, deadline(), &claim, grant.generation).unwrap();
    assert_eq!(
        success::publish(request, trust, deadline(), &value).unwrap(),
        outcome
    );
    sql(
        fixture,
        "DROP TRIGGER delay_success ON td_mta.route_health_receipts",
    );
}
