use super::{deadline, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AttemptId, SubmissionId},
    postgres_attempt_phases as phases, postgres_attempts as attempts,
    postgres_probe::Request,
    postgres_route_owners::{self as owners, Claim},
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    submission: SubmissionId,
    attempt: AttemptId,
) {
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let ordinals: Vec<_> = (100..200).collect();
    assert!(phases::accepted(claim, generation, attempt, &ordinals, "450 refused").is_err());
    let data_reply = format!("250 {}", "y".repeat(4092));
    let accepted = phases::accepted(claim, generation, attempt, &ordinals, &data_reply).unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &accepted),
        Err(phases::Error::Conflict)
    );
    let rcpts: Vec<_> = ordinals
        .iter()
        .map(|n| (*n, "250 recipient accepted"))
        .collect();
    let body = phases::body(claim, generation, attempt, &rcpts).unwrap();
    phases::publish(request, trust, deadline(), &body).unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &accepted),
        Err(phases::Error::Conflict)
    );
    let terminator = phases::acceptance_possible(claim, generation, attempt, &ordinals).unwrap();
    phases::publish(request, trust, deadline(), &terminator).unwrap();
    let partial = phases::accepted(claim, generation, attempt, &[100], "250 queued").unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &partial),
        Err(phases::Error::Conflict)
    );
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(fixture,"CREATE FUNCTION public.reject_acceptance() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_acceptance BEFORE INSERT ON td_mta.attempt_acceptance_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_acceptance()");
    assert_eq!(
        phases::publish(request, trust, deadline(), &accepted),
        Err(phases::Error::Database)
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        before
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=4")),"0");
    sql(fixture,"DROP TRIGGER reject_acceptance ON td_mta.attempt_acceptance_receipts; CREATE FUNCTION public.delay_acceptance() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_acceptance AFTER INSERT ON td_mta.attempt_acceptance_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_acceptance()");
    // A prior duplicate-risk latch survives a confirmed handoff.
    sql(fixture,&format!("UPDATE td_mta.recipients SET uncertain=1 WHERE submission_id=decode('{submission}','hex') AND ordinal=100"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    assert_eq!(
        phases::publish(
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &accepted
        ),
        Err(phases::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome = phases::inspect(request, trust, deadline(), &accepted)
        .unwrap()
        .unwrap();
    assert_eq!(outcome.recipients, 100);
    assert_eq!(
        phases::publish(request, trust, deadline(), &accepted).unwrap(),
        outcome
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=4 AND phase=4 AND rcpt_reply='250 recipient accepted' AND data_reply=('250 '||repeat('y',4092)) AND reason=0 AND next_attempt_at IS NULL")),"100");
    assert_eq!(sql(fixture,&format!("SELECT uncertain FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=100")),"1");
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
        ),
        "0"
    );
    assert_eq!(sql(fixture,&format!("SELECT completed_at IS NULL FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),"t");
    let conflict =
        phases::accepted(claim, generation, attempt, &ordinals, "250 different").unwrap();
    assert_eq!(
        phases::publish(request, trust, deadline(), &conflict),
        Err(phases::Error::Conflict)
    );
    sql(
        fixture,
        "DROP TRIGGER delay_acceptance ON td_mta.attempt_acceptance_receipts",
    );
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let next = attempts::prepare(claim, generation, submission).unwrap();
    let next = attempts::publish(request, trust, deadline(), &next).unwrap();
    assert_eq!(next.ordinals, (200..300).collect::<Vec<_>>());
    let rcpts: Vec<_> = next
        .ordinals
        .iter()
        .map(|n| (*n, "250 recipient accepted"))
        .collect();
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::body(claim, generation, next.attempt, &rcpts).unwrap(),
    )
    .unwrap();
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::acceptance_possible(claim, generation, next.attempt, &next.ordinals).unwrap(),
    )
    .unwrap();
    // Seed a valid final-batch fixture: earlier recipients have all handed off.
    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal>=300; UPDATE td_mta.recipients SET state=4,phase=4,next_attempt_at=NULL,reason=0,rcpt_reply='250 prior recipient',data_reply='250 prior queued' WHERE submission_id=decode('{submission}','hex') AND ordinal<100; UPDATE td_mta.submissions SET recipient_count=300,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-1 WHERE id=decode('{submission}','hex')"));
    let final_result = phases::accepted(
        claim,
        generation,
        next.attempt,
        &next.ordinals,
        "250 final queued",
    )
    .unwrap();
    let final_outcome = phases::publish(request, trust, deadline(), &final_result).unwrap();
    assert_eq!(sql(fixture,&format!("SELECT completed_at IS NOT NULL AND notification=0 FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),"t");
    let completed = sql(
        fixture,
        &format!(
            "SELECT completed_at FROM td_mta.submissions WHERE id=decode('{submission}','hex')"
        ),
    );
    assert_eq!(
        phases::publish(request, trust, deadline(), &final_result).unwrap(),
        final_outcome
    );
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT completed_at FROM td_mta.submissions WHERE id=decode('{submission}','hex')"
            )
        ),
        completed
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
        ),
        "0"
    );
    super::attempt_refusal::exercise(fixture, request, trust, claim, generation, submission);
}
