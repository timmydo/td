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
    // Seed an independent three-batch queue after the acceptance assertions.
    sql(fixture, &format!("UPDATE td_mta.recipients SET state=1,phase=0,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.submissions SET completed_at=NULL,notification=0,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
    for batch in 0..3 {
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        let prepared = attempts::prepare(claim, generation, submission).unwrap();
        let attempt = attempts::publish(request, trust, deadline(), &prepared).unwrap();
        assert_eq!(
            attempt.ordinals,
            (batch * 100..batch * 100 + 100).collect::<Vec<_>>()
        );
        let reply = if batch == 0 {
            format!("450 {}", "t".repeat(4092))
        } else {
            "550 refused".to_owned()
        };
        let refused = phases::refused(
            claim,
            generation,
            attempt.attempt,
            &attempt.ordinals,
            &reply,
        )
        .unwrap();
        for invalid in ["250 accepted", "354 continue", "550 bad\rreply"] {
            assert!(phases::refused(
                claim,
                generation,
                attempt.attempt,
                &attempt.ordinals,
                invalid
            )
            .is_err());
        }
        assert_eq!(
            phases::publish(request, trust, deadline(), &refused),
            Err(phases::Error::Conflict)
        );
        let rcpts: Vec<_> = attempt
            .ordinals
            .iter()
            .map(|n| (*n, "250 recipient accepted"))
            .collect();
        phases::publish(
            request,
            trust,
            deadline(),
            &phases::body(claim, generation, attempt.attempt, &rcpts).unwrap(),
        )
        .unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &refused),
            Err(phases::Error::Conflict)
        );
        phases::publish(
            request,
            trust,
            deadline(),
            &phases::acceptance_possible(claim, generation, attempt.attempt, &attempt.ordinals)
                .unwrap(),
        )
        .unwrap();
        let partial =
            phases::refused(claim, generation, attempt.attempt, &[batch * 100], &reply).unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &partial),
            Err(phases::Error::Conflict)
        );
        sql(fixture,&format!("UPDATE td_mta.recipients SET uncertain=1 WHERE submission_id=decode('{submission}','hex') AND ordinal={}",batch*100));
        let earliest = sql(
            fixture,
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
        );
        if batch == 0 {
            let before = sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts",
            );
            let expiry=sql(fixture,&format!("SELECT expires_at FROM td_mta.submissions WHERE id=decode('{submission}','hex')"));
            sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+1000 WHERE id=decode('{submission}','hex')"));
            assert_eq!(
                phases::publish(request, trust, deadline(), &refused),
                Err(phases::Error::ExpiryHold)
            );
            sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at={expiry} WHERE id=decode('{submission}','hex')"));
            sql(fixture,"CREATE FUNCTION public.reject_refusal() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_refusal BEFORE INSERT ON td_mta.attempt_refusal_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_refusal()");
            assert_eq!(
                phases::publish(request, trust, deadline(), &refused),
                Err(phases::Error::Database)
            );
            assert_eq!(
                sql(
                    fixture,
                    "SELECT encode(sequence,'hex') FROM td_mta.accounts"
                ),
                before
            );
            sql(fixture,"DROP TRIGGER reject_refusal ON td_mta.attempt_refusal_receipts; CREATE FUNCTION public.delay_refusal() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_refusal AFTER INSERT ON td_mta.attempt_refusal_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_refusal()");
            owners::renew(request, trust, deadline(), claim, generation).unwrap();
            assert_eq!(
                phases::publish(
                    request,
                    trust,
                    Instant::now() + Duration::from_millis(1500),
                    &refused
                ),
                Err(phases::Error::Indeterminate)
            );
            thread::sleep(Duration::from_secs(4));
        }
        if batch == 2 {
            // Seed terminal earlier batches, then prove final refusal completion.
            sql(fixture,&format!("UPDATE td_mta.recipients SET state=CASE WHEN uncertain=1 THEN 7 ELSE 5 END,reason=2,next_attempt_at=NULL,data_reply='550 prior refusal' WHERE submission_id=decode('{submission}','hex') AND ordinal<200; UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-1 WHERE id=decode('{submission}','hex')"));
        }
        let outcome = phases::publish(request, trust, deadline(), &refused).unwrap();
        assert_eq!(outcome.recipients, 100);
        assert_eq!(
            phases::inspect(request, trust, deadline(), &refused).unwrap(),
            Some(outcome)
        );
        assert_eq!(
            phases::publish(request, trust, deadline(), &refused).unwrap(),
            outcome
        );
        let conflict = phases::refused(
            claim,
            generation,
            attempt.attempt,
            &attempt.ordinals,
            "550 different",
        )
        .unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &conflict),
            Err(phases::Error::Conflict)
        );
        assert_eq!(
            sql(
                fixture,
                "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
            ),
            "0"
        );
        let expected = if batch == 0 { "99|1|100" } else { "0|1|0" };
        assert_eq!(sql(fixture,&format!("SELECT count(*) FILTER (WHERE state=3),count(*) FILTER (WHERE state=7 AND uncertain=1),count(*) FILTER (WHERE next_attempt_at IS NOT NULL) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal BETWEEN {} AND {}",batch*100,batch*100+99)),expected);
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal BETWEEN {} AND {} AND phase=4 AND rcpt_reply='250 recipient accepted' AND reason={} AND data_reply={}",batch*100,batch*100+99,if batch==0 {1}else{2},if batch==0 {"('450 '||repeat('t',4092))"}else{"'550 refused'"})),"100");
        if batch == 0 {
            let latest = sql(
                fixture,
                "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
            );
            assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<100 AND next_attempt_at BETWEEN {earliest}::bigint+300000 AND {latest}::bigint+330000")),"100");
            sql(
                fixture,
                "DROP TRIGGER delay_refusal ON td_mta.attempt_refusal_receipts",
            );
        }
        assert_eq!(sql(fixture,&format!("SELECT completed_at IS NOT NULL,notification FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),if batch==2 {"t|1"} else {"f|0"});
    }
}
