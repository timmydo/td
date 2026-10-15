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
    // Seed an independent queue for RCPT results after final-DATA assertions.
    sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,phase=0,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.submissions SET completed_at=NULL,notification=0,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
    for batch in 0..3 {
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        let attempt = attempts::publish(
            request,
            trust,
            deadline(),
            &attempts::prepare(claim, generation, submission).unwrap(),
        )
        .unwrap();
        assert_eq!(
            attempt.ordinals,
            (batch * 100..batch * 100 + 100).collect::<Vec<_>>()
        );
        let end = if batch == 0 { 50 } else { batch * 100 + 100 };
        let replies: Vec<_> = (batch * 100..end)
            .map(|n| {
                let code = if batch == 0 && n % 2 == 0 { 450 } else { 550 };
                (n, format!("{code} {n:03}{}", "r".repeat(4089)))
            })
            .collect();
        let borrowed: Vec<_> = replies.iter().map(|(n, s)| (*n, s.as_str())).collect();
        let refusal = phases::refused_rcpt(claim, generation, attempt.attempt, &borrowed).unwrap();
        assert!(
            phases::refused_rcpt(claim, generation, attempt.attempt, &[(0, "250 accepted")])
                .is_err()
        );
        assert!(phases::refused_rcpt(
            claim,
            generation,
            attempt.attempt,
            &[(0, "550 refused"), (0, "550 duplicate")]
        )
        .is_err());
        let foreign =
            phases::refused_rcpt(claim, generation, attempt.attempt, &[(999, "550 refused")])
                .unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &foreign),
            Err(phases::Error::Integrity)
        );
        // Prepared can retain prior replies; a fresh RCPT replaces that history.
        sql(fixture,&format!("UPDATE td_mta.recipients SET rcpt_reply='250 old recipient',data_reply='450 old DATA' WHERE submission_id=decode('{submission}','hex') AND attempt_id=decode('{}','hex'); UPDATE td_mta.recipients SET uncertain=1 WHERE submission_id=decode('{submission}','hex') AND ordinal IN (0,1)",attempt.attempt));
        if batch == 0 {
            let before = sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts",
            );
            sql(fixture,"CREATE FUNCTION public.reject_rcpt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_rcpt BEFORE INSERT ON td_mta.attempt_rcpt_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_rcpt()");
            assert_eq!(
                phases::publish(request, trust, deadline(), &refusal),
                Err(phases::Error::Database)
            );
            assert_eq!(
                sql(
                    fixture,
                    "SELECT encode(sequence,'hex') FROM td_mta.accounts"
                ),
                before
            );
            sql(
                fixture,
                "DROP TRIGGER reject_rcpt ON td_mta.attempt_rcpt_receipts",
            );
        }
        if batch == 1 {
            sql(fixture,"CREATE FUNCTION public.delay_rcpt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_rcpt AFTER INSERT ON td_mta.attempt_rcpt_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_rcpt()");
            owners::renew(request, trust, deadline(), claim, generation).unwrap();
            assert_eq!(
                phases::publish(
                    request,
                    trust,
                    Instant::now() + Duration::from_millis(1500),
                    &refusal
                ),
                Err(phases::Error::Indeterminate)
            );
            thread::sleep(Duration::from_secs(4));
        }
        if batch == 2 {
            sql(fixture,&format!("UPDATE td_mta.recipients SET state=CASE WHEN uncertain=1 THEN 7 ELSE 5 END,reason=2,next_attempt_at=NULL,rcpt_reply='550 prior refused' WHERE submission_id=decode('{submission}','hex') AND ordinal<50; UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-1 WHERE id=decode('{submission}','hex')"));
        }
        let result = phases::publish(request, trust, deadline(), &refusal).unwrap();
        assert_eq!(result.recipients, if batch == 0 { 50 } else { 100 });
        assert_eq!(
            phases::inspect(request, trust, deadline(), &refusal).unwrap(),
            Some(result)
        );
        assert_eq!(
            phases::publish(request, trust, deadline(), &refusal).unwrap(),
            result
        );
        let different = phases::refused_rcpt(
            claim,
            generation,
            attempt.attempt,
            &[(batch * 100, "550 different")],
        )
        .unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &different),
            Err(phases::Error::Conflict)
        );
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal>={} AND ordinal<{end} AND phase=4 AND data_reply IS NULL AND length(rcpt_reply)=4096",batch*100)),if batch==0 {"50"}else{"100"});
        if batch == 0 {
            assert_eq!(sql(fixture,&format!("SELECT state,uncertain,reason,next_attempt_at IS NOT NULL FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal IN (0,1,2,3) ORDER BY ordinal")),"7|1|1|t\n7|1|2|f\n3|0|1|t\n5|0|2|f");
            assert_eq!(
                sql(
                    fixture,
                    "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
                ),
                "1"
            );
            let positives: Vec<_> = (50..100).map(|n| (n, "250 recipient accepted")).collect();
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::body(claim, generation, attempt.attempt, &positives).unwrap(),
            )
            .unwrap();
            let ordinals: Vec<_> = (50..100).collect();
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::acceptance_possible(claim, generation, attempt.attempt, &ordinals)
                    .unwrap(),
            )
            .unwrap();
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::accepted(claim, generation, attempt.attempt, &ordinals, "250 queued")
                    .unwrap(),
            )
            .unwrap();
        }
        if batch == 1 {
            sql(
                fixture,
                "DROP TRIGGER delay_rcpt ON td_mta.attempt_rcpt_receipts",
            );
        }
        assert_eq!(
            sql(
                fixture,
                "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
            ),
            "0"
        );
        assert_eq!(sql(fixture,&format!("SELECT completed_at IS NOT NULL,notification FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),if batch==2 {"t|1"}else{"f|0"});
    }
}
