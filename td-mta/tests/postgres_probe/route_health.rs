use super::{deadline, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::{BootId, SubmissionId},
    postgres_attempt_recovery as recovery, postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_route_health as health,
    postgres_route_owners::{self as owners, Claim},
};
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    binding: &td_mta::postgres_runtime_profile::Binding,
    generation: u64,
    submission: SubmissionId,
) {
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000,completed_at=NULL,notification=0 WHERE id=decode('{submission}','hex'); UPDATE td_mta.recipients SET state=1,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,phase=0,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL,diagnostic='' WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    let dispatch = attempts::prepare(claim, generation, submission).unwrap();
    let first = attempts::publish(request, trust, deadline(), &dispatch).unwrap();
    assert_eq!(first.ordinals, vec![0]);
    // Retire a failure without a route-health report, then start a later attempt.
    let abandoned = first;
    recovery::publish(
        request,
        trust,
        deadline(),
        &recovery::interrupted(claim, generation, abandoned.attempt).unwrap(),
    )
    .unwrap();
    sql(fixture, &format!("UPDATE td_mta.recipients SET last_attempt_at=last_attempt_at-1200000,next_attempt_at=next_attempt_at-1200000 WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    let first = attempts::publish(
        request,
        trust,
        deadline(),
        &attempts::retry(claim, generation, submission).unwrap(),
    )
    .unwrap();
    let stale =
        health::prepare(claim, generation, abandoned.attempt, health::Failure::Tls).unwrap();
    assert_eq!(
        health::publish(request, trust, deadline(), &stale),
        Err(health::Error::Conflict)
    );

    let value =
        health::prepare(claim, generation, first.attempt, health::Failure::Network).unwrap();
    assert_eq!(
        health::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    assert_eq!(
        health::publish(
            request,
            trust,
            deadline(),
            &health::prepare(
                claim,
                generation + 1,
                first.attempt,
                health::Failure::Network
            )
            .unwrap()
        ),
        Err(health::Error::Owner(owners::Error::Stale))
    );
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(fixture,"CREATE FUNCTION public.reject_health() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_health BEFORE INSERT ON td_mta.route_health_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_health()");
    assert_eq!(
        health::publish(request, trust, deadline(), &value),
        Err(health::Error::Database)
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.route_health"),
        "0"
    );
    sql(fixture,"DROP TRIGGER reject_health ON td_mta.route_health_receipts; CREATE FUNCTION public.delay_health() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_health AFTER INSERT ON td_mta.route_health_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_health()");
    assert_eq!(
        health::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &value
        ),
        Err(health::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome = health::publish(request, trust, deadline(), &value).unwrap();
    assert_eq!(outcome.old_revision, 0);
    assert_eq!(outcome.state.revision, 1);
    assert_eq!(outcome.state.failure_count, 1);
    assert_eq!(outcome.state.repair_generation, None);
    assert!((300_000..=330_000)
        .contains(&(outcome.state.next_at.unwrap() - outcome.state.failed_at.unwrap())));
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        before
    );
    assert_eq!(
        health::publish(
            request,
            trust,
            deadline(),
            &health::prepare(claim, generation, first.attempt, health::Failure::Network).unwrap()
        )
        .unwrap(),
        outcome
    );
    assert_eq!(
        health::publish(
            request,
            trust,
            deadline(),
            &health::prepare(claim, generation, first.attempt, health::Failure::Tls).unwrap()
        ),
        Err(health::Error::Conflict)
    );
    sql(
        fixture,
        "DROP TRIGGER delay_health ON td_mta.route_health_receipts",
    );
    assert_eq!(
        attempts::publish(request, trust, deadline(), &dispatch).unwrap(),
        abandoned
    );
    assert_eq!(
        attempts::publish(
            request,
            trust,
            deadline(),
            &attempts::prepare(claim, generation, submission).unwrap()
        ),
        Err(attempts::Error::RouteHealth(health::Error::Backoff))
    );
    recovery::publish(
        request,
        trust,
        deadline(),
        &recovery::interrupted(claim, generation, first.attempt).unwrap(),
    )
    .unwrap();
    sql(fixture,&format!("UPDATE td_mta.route_health SET failed_at=failed_at-1200000,next_at=next_at-1200000; UPDATE td_mta.recipients SET last_attempt_at=last_attempt_at-1200000,next_attempt_at=next_attempt_at-1200000 WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    let second = attempts::publish(
        request,
        trust,
        deadline(),
        &attempts::retry(claim, generation, submission).unwrap(),
    )
    .unwrap();
    let auth = health::prepare(claim, generation, second.attempt, health::Failure::Auth).unwrap();
    let second_outcome = health::publish(request, trust, deadline(), &auth).unwrap();
    assert_eq!(second_outcome.old_revision, 1);
    assert_eq!(second_outcome.state.revision, 2);
    assert_eq!(second_outcome.state.failure_count, 2);
    assert_eq!(
        second_outcome.state.repair_generation,
        Some(binding.selection().generation)
    );
    assert!((540_000..=660_000).contains(
        &(second_outcome.state.next_at.unwrap() - second_outcome.state.failed_at.unwrap())
    ));
    sql(
        fixture,
        "UPDATE td_mta.route_health SET failed_at=failed_at-1200000,next_at=next_at-1200000",
    );
    assert_eq!(
        attempts::publish(
            request,
            trust,
            deadline(),
            &attempts::retry(claim, generation, submission).unwrap()
        ),
        Err(attempts::Error::RouteHealth(health::Error::RepairRequired))
    );
    assert_eq!(
        health::inspect(request, trust, deadline(), &value).unwrap(),
        Some(outcome)
    );
    assert_eq!(
        health::publish(request, trust, deadline(), &auth).unwrap(),
        second_outcome
    );
    // A real replacement owner inherits the durable repair hold.
    owners::release(request, trust, deadline(), claim, generation).unwrap();
    let replacement = owners::prepare(binding, BootId::from_bytes([77; 16])).unwrap();
    let grant = owners::acquire(request, trust, deadline(), &replacement).unwrap();
    assert!(grant.generation > generation);
    assert_eq!(
        attempts::publish(
            request,
            trust,
            deadline(),
            &attempts::retry(&replacement, grant.generation, submission).unwrap()
        ),
        Err(attempts::Error::RouteHealth(health::Error::RepairRequired))
    );
    assert_eq!(
        health::publish(request, trust, deadline(), &auth).unwrap(),
        second_outcome
    );
    owners::release(request, trust, deadline(), &replacement, grant.generation).unwrap();
}
