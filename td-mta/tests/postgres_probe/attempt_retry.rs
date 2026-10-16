use super::{deadline, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::SubmissionId,
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
) {
    // Independently seed elapsed due times; no fixture waits five real minutes.
    let previous = sql(fixture,&format!("SELECT encode(attempt_id,'hex') FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=5,phase=4,uncertain=0,attempt_id=decode('{previous}','hex'),attempt_count=1,last_attempt_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-600000,next_attempt_at=NULL,reason=2,rcpt_reply='250 earlier RCPT',data_reply='550 earlier DATA',diagnostic='earlier diagnostic' WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.submissions SET completed_at=NULL,notification=0,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex'); UPDATE td_mta.recipients SET state=3,reason=1,data_reply='450 earlier DATA',next_attempt_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-1 WHERE submission_id=decode('{submission}','hex') AND ordinal<200; UPDATE td_mta.recipients SET state=7,uncertain=1,reason=9 WHERE submission_id=decode('{submission}','hex') AND ordinal IN (0,100); UPDATE td_mta.recipients SET state=4,reason=0,data_reply='250 accepted' WHERE submission_id=decode('{submission}','hex') AND ordinal=200; UPDATE td_mta.recipients SET state=7,uncertain=1 WHERE submission_id=decode('{submission}','hex') AND ordinal=201; UPDATE td_mta.recipients SET state=1,phase=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL WHERE submission_id=decode('{submission}','hex') AND ordinal=202; UPDATE td_mta.recipients SET state=3,reason=1,data_reply='450 later',next_attempt_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE submission_id=decode('{submission}','hex') AND ordinal=203; UPDATE td_mta.recipients SET state=7,uncertain=1,reason=6 WHERE submission_id=decode('{submission}','hex') AND ordinal=204"));
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let value = attempts::retry(claim, generation, submission).unwrap();
    assert_eq!(
        attempts::inspect(request, trust, deadline(), &value).unwrap(),
        None
    );
    assert_eq!(
        super::attempt_clock::publish(
            fixture,
            request,
            trust,
            deadline(),
            &attempts::retry(claim, generation + 1, submission).unwrap()
        ),
        Err(attempts::Error::Owner(owners::Error::Stale))
    );
    sql(fixture,&format!("UPDATE td_mta.recipients SET attempt_count=4294967295 WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    assert_eq!(
        super::attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::Capacity)
    );
    sql(fixture,&format!("UPDATE td_mta.recipients SET attempt_count=1,next_attempt_at=last_attempt_at+299999 WHERE submission_id=decode('{submission}','hex') AND ordinal=0"));
    assert_eq!(
        super::attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::Integrity)
    );
    sql(fixture,&format!("UPDATE td_mta.recipients SET next_attempt_at=last_attempt_at+300000 WHERE submission_id=decode('{submission}','hex') AND ordinal=0; UPDATE td_mta.submissions SET expires_at=send_at WHERE id=decode('{submission}','hex')"));
    assert_eq!(
        super::attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::NotDue)
    );
    sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
    let unchanged=sql(fixture,&format!("SELECT md5(string_agg(concat_ws('|',ordinal,state,uncertain,phase,attempt_count,rcpt_reply,data_reply),',' ORDER BY ordinal)) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal>=200"));
    let frozen=sql(fixture,&format!("SELECT encode(transmitted_blob_id,'hex'),reverse_path FROM td_mta.submissions WHERE id=decode('{submission}','hex')"));
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(fixture,"CREATE FUNCTION public.reject_retry() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_retry BEFORE INSERT ON td_mta.submission_attempts FOR EACH ROW EXECUTE FUNCTION public.reject_retry()");
    assert_eq!(
        super::attempt_clock::publish(fixture, request, trust, deadline(), &value),
        Err(attempts::Error::Database)
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        before
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=2")),"0");
    sql(fixture,"DROP TRIGGER reject_retry ON td_mta.submission_attempts; CREATE FUNCTION public.delay_retry() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_retry AFTER INSERT ON td_mta.submission_attempts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_retry()");
    assert_eq!(
        super::attempt_clock::publish(
            fixture,
            request,
            trust,
            Instant::now() + Duration::from_millis(1500),
            &value
        ),
        Err(attempts::Error::Indeterminate)
    );
    thread::sleep(Duration::from_secs(4));
    let outcome =
        super::attempt_clock::publish(fixture, request, trust, deadline(), &value).unwrap();
    assert_eq!(outcome.ordinals, (0..100).collect::<Vec<_>>());
    assert_eq!(outcome.new_sequence, outcome.old_sequence + 1);
    assert_eq!(
        attempts::inspect(request, trust, deadline(), &value).unwrap(),
        Some(outcome.clone())
    );
    assert_eq!(
        super::attempt_clock::publish(fixture, request, trust, deadline(), &value).unwrap(),
        outcome
    );
    sql(
        fixture,
        "DROP TRIGGER delay_retry ON td_mta.submission_attempts",
    );
    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<100 AND state=2 AND phase=1 AND attempt_count=2 AND last_attempt_at={} AND attempt_id=decode('{}','hex') AND next_attempt_at IS NULL AND reason=0 AND diagnostic='' AND rcpt_reply='250 earlier RCPT' AND data_reply='450 earlier DATA'",outcome.prepared_at,outcome.attempt)),"100");
    assert_eq!(sql(fixture,&format!("SELECT uncertain FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=0")),"1");
    // Retry preparation retains previous replies; neither envelope reader may
    // mistake that durable history for progress in the new attempt.
    let envelope = attempts::envelope(request, trust, deadline(), &value).unwrap();
    assert_eq!(envelope.attempt, outcome.attempt);
    assert_eq!(envelope.recipients.len(), 100);
    assert!(envelope.recipients.first().unwrap().uncertain);
    let (observed, limit) =
        super::attempt_clock::fresh_envelope(fixture, request, trust, &value).unwrap();
    assert_eq!(observed, envelope);
    assert!(limit > Instant::now());
    let next = attempts::retry(claim, generation, submission).unwrap();
    assert_eq!(
        super::attempt_clock::publish(fixture, request, trust, deadline(), &next),
        Err(attempts::Error::Busy)
    );
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::refused_mail(
            claim,
            generation,
            outcome.attempt,
            &outcome.ordinals,
            "550 stopped",
        )
        .unwrap(),
    )
    .unwrap();
    let second = super::attempt_clock::publish(fixture, request, trust, deadline(), &next).unwrap();
    assert_eq!(second.ordinals, (100..200).collect::<Vec<_>>());
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::refused_mail(
            claim,
            generation,
            second.attempt,
            &second.ordinals,
            "550 stopped",
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        super::attempt_clock::publish(
            fixture,
            request,
            trust,
            deadline(),
            &attempts::retry(claim, generation, submission).unwrap()
        ),
        Err(attempts::Error::NotDue)
    );
    assert_eq!(sql(fixture,&format!("SELECT md5(string_agg(concat_ws('|',ordinal,state,uncertain,phase,attempt_count,rcpt_reply,data_reply),',' ORDER BY ordinal)) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal>=200")),unchanged);
    assert_eq!(sql(fixture,&format!("SELECT encode(transmitted_blob_id,'hex'),reverse_path FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),frozen);
    // The explicit first-attempt API still selects only never-attempted work.
    let fresh = super::attempt_clock::publish(
        fixture,
        request,
        trust,
        deadline(),
        &attempts::prepare(claim, generation, submission).unwrap(),
    )
    .unwrap();
    assert_eq!(fresh.ordinals, vec![202]);
    phases::publish(
        request,
        trust,
        deadline(),
        &phases::refused_mail(
            claim,
            generation,
            fresh.attempt,
            &fresh.ordinals,
            "550 stopped",
        )
        .unwrap(),
    )
    .unwrap();
    super::queue_expiry::exercise(fixture, request, trust, claim, generation, submission);
}
