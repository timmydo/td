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
        let attempt = super::attempt_clock::publish(
            fixture,
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
        let mut collector =
            phases::reserve_rcpt_replies(claim, generation, attempt.attempt, &attempt.ordinals)
                .unwrap();
        let first = batch * 100;
        for invalid in [
            String::new(),
            "354 not RCPT".to_owned(),
            "550 bad\ntext".to_owned(),
            format!("550 {}", "x".repeat(4093)),
        ] {
            assert_eq!(
                collector.record(first, &invalid),
                Err(phases::Error::Integrity)
            );
            assert_eq!(collector.next_ordinal(), Some(first));
        }
        assert_eq!(
            collector.record(first + 1, "550 wrong ordinal"),
            Err(phases::Error::Integrity)
        );
        for ordinal in &attempt.ordinals {
            if let Some((_, reply)) = borrowed.iter().find(|(n, _)| n == ordinal) {
                collector.record(*ordinal, reply).unwrap();
            } else {
                collector
                    .record(*ordinal, &format!("250 {ordinal:03}{}", "p".repeat(4089)))
                    .unwrap();
            }
        }
        assert_eq!(collector.next_ordinal(), None);
        assert_eq!(
            collector.record(first, "550 duplicate"),
            Err(phases::Error::Integrity)
        );
        let evidence = collector.finish().unwrap();
        assert!(evidence.is_complete());
        assert_eq!(evidence.accepted().len(), if batch == 0 { 50 } else { 0 });
        let refusal = evidence.refusal().unwrap();
        let legacy = phases::refused_rcpt(claim, generation, attempt.attempt, &borrowed).unwrap();
        if batch == 0 {
            assert!(phases::reserve_rcpt_replies(claim, generation, attempt.attempt, &[]).is_err());
            assert!(phases::reserve_rcpt_replies(claim, 0, attempt.attempt, &[0]).is_err());
            assert!(
                phases::reserve_rcpt_replies(claim, generation, attempt.attempt, &[0, 0]).is_err()
            );
            for positive in [true, false] {
                let mut partial =
                    phases::reserve_rcpt_replies(claim, generation, attempt.attempt, &[0, 1])
                        .unwrap();
                partial
                    .record(
                        0,
                        if positive {
                            "250 retained"
                        } else {
                            "550 retained"
                        },
                    )
                    .unwrap();
                assert_eq!(
                    partial.record(1, "354 invalid"),
                    Err(phases::Error::Integrity)
                );
                assert_eq!(partial.next_ordinal(), Some(1));
                let partial = partial.finish().unwrap();
                assert!(!partial.is_complete());
                assert_eq!(partial.accepted().len(), usize::from(positive));
                assert_eq!(partial.refusal().is_some(), !positive);
                if positive {
                    assert_eq!(partial.accepted(), &[(0, "250 retained".to_owned())]);
                }
            }
            let mut all_positive =
                phases::reserve_rcpt_replies(claim, generation, attempt.attempt, &[0, 1]).unwrap();
            all_positive.record(0, "250 first").unwrap();
            all_positive.record(1, "251 second").unwrap();
            let all_positive = all_positive.finish().unwrap();
            assert!(all_positive.is_complete());
            assert!(all_positive.refusal().is_none());
            assert_eq!(
                all_positive.accepted(),
                &[(0, "250 first".to_owned()), (1, "251 second".to_owned())]
            );
        }
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
                phases::publish(request, trust, deadline(), refusal),
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
                    refusal
                ),
                Err(phases::Error::Indeterminate)
            );
            thread::sleep(Duration::from_secs(4));
        }
        if batch == 2 {
            sql(fixture,&format!("UPDATE td_mta.recipients SET state=CASE WHEN uncertain=1 THEN 7 ELSE 5 END,reason=2,next_attempt_at=NULL,rcpt_reply='550 prior refused' WHERE submission_id=decode('{submission}','hex') AND ordinal<50; UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-1 WHERE id=decode('{submission}','hex')"));
        }
        let result = phases::publish(request, trust, deadline(), refusal).unwrap();
        assert_eq!(result.recipients, if batch == 0 { 50 } else { 100 });
        assert_eq!(
            phases::inspect(request, trust, deadline(), &legacy).unwrap(),
            Some(result)
        );
        assert_eq!(
            phases::inspect(request, trust, deadline(), refusal).unwrap(),
            Some(result)
        );
        assert_eq!(
            phases::publish(request, trust, deadline(), refusal).unwrap(),
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
            let positives: Vec<_> = evidence
                .accepted()
                .iter()
                .map(|(n, s)| (*n, s.as_str()))
                .collect();
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
    super::attempt_command::exercise(fixture, request, trust, claim, generation, submission);
    super::attempt_interrupted::exercise(fixture, request, trust, claim, generation, submission);
    super::attempt_retry::exercise(fixture, request, trust, claim, generation, submission);
}
