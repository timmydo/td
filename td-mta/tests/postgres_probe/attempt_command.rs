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
    for case in 0..4 {
        // Seed independent fresh attempts for both command stages and classes.
        sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,phase=0,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.submissions SET completed_at=NULL,notification=0,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        let attempt = super::attempt_clock::publish(
            fixture,
            request,
            trust,
            deadline(),
            &attempts::prepare(claim, generation, submission).unwrap(),
        )
        .unwrap();
        assert_eq!(attempt.ordinals, (0..100).collect::<Vec<_>>());
        let data = case >= 2;
        let temporary = case % 2 == 0;
        let end = if case == 2 { 50 } else { 100 };
        let ordinals: Vec<_> = (0..end).collect();
        let positive: Vec<_> = ordinals
            .iter()
            .map(|n| (*n, format!("250 {n:03}{}", "p".repeat(4089))))
            .collect();
        let rcpts: Vec<_> = positive.iter().map(|(n, s)| (*n, s.as_str())).collect();
        let reply = format!("{} {}", if temporary { 450 } else { 550 }, "r".repeat(4092));
        let reserve = |generation| {
            if data {
                phases::reserve_data_command_reply(claim, generation, attempt.attempt, &rcpts)
            } else {
                phases::reserve_mail_reply(claim, generation, attempt.attempt, &ordinals)
            }
        };
        let prepare = |generation, reply: &str| reserve(generation)?.finish(reply);
        for invalid in [
            vec![],
            vec![1000],
            vec![0, 0],
            vec![1, 0],
            (0..101).collect(),
        ] {
            assert!(
                phases::reserve_mail_reply(claim, generation, attempt.attempt, &invalid).is_err()
            );
        }
        assert!(phases::reserve_data_command_reply(
            claim,
            generation,
            attempt.attempt,
            &[(0, "450 refused")]
        )
        .is_err());
        for bad in ["250 positive", "354 interim", "550 injected\r\n"] {
            assert!(prepare(generation, bad).is_err());
        }
        assert!(prepare(generation, &format!("550 {}", "x".repeat(4093))).is_err());
        assert!(reserve(0).is_err());
        assert!(phases::refused_data_command(
            claim,
            generation,
            attempt.attempt,
            &[(0, "450 refused")],
            &reply
        )
        .is_err());
        let value = prepare(generation, &reply).unwrap();
        let stale = prepare(generation + 1, &reply).unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &stale),
            Err(phases::Error::Owner(owners::Error::Stale))
        );
        let partial =
            phases::refused_mail(claim, generation, attempt.attempt, &[0], &reply).unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &partial),
            Err(phases::Error::Conflict)
        );
        sql(fixture,&format!("UPDATE td_mta.recipients SET uncertain=1 WHERE submission_id=decode('{submission}','hex') AND ordinal=0; UPDATE td_mta.recipients SET rcpt_reply='250 old',data_reply='450 old' WHERE submission_id=decode('{submission}','hex') AND state=2"));
        if temporary {
            sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+1000 WHERE id=decode('{submission}','hex')"));
            super::expiry_wait_snapshot(fixture, || {
                phases::publish(request, trust, deadline(), &value).unwrap();
            });
            sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
        }
        if case == 2 {
            let rejected: Vec<_> = (50..100).map(|n| (n, "550 RCPT refused")).collect();
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::refused_rcpt(claim, generation, attempt.attempt, &rejected).unwrap(),
            )
            .unwrap();
            // MAIL cannot follow an already published RCPT subset.
            let remaining =
                phases::refused_mail(claim, generation, attempt.attempt, &ordinals, &reply)
                    .unwrap();
            assert_eq!(
                phases::publish(request, trust, deadline(), &remaining),
                Err(phases::Error::Conflict)
            );
        }
        if case == 0 {
            let before = sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts",
            );
            sql(fixture,"CREATE FUNCTION public.reject_command() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_command BEFORE INSERT ON td_mta.attempt_command_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_command()");
            assert_eq!(
                phases::publish(request, trust, deadline(), &value),
                Err(phases::Error::Database)
            );
            assert_eq!(
                sql(
                    fixture,
                    "SELECT encode(sequence,'hex') FROM td_mta.accounts"
                ),
                before
            );
            assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=2 AND rcpt_reply='250 old' AND data_reply='450 old'")),"100");
            sql(
                fixture,
                "DROP TRIGGER reject_command ON td_mta.attempt_command_receipts",
            );
        }
        // Outside this attempted batch, all responsibility is already terminal.
        sql(fixture,&format!("UPDATE td_mta.recipients SET state=5,reason=6,next_attempt_at=NULL WHERE submission_id=decode('{submission}','hex') AND ordinal>=100"));
        if !temporary {
            sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint-1 WHERE id=decode('{submission}','hex')"));
        }
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        if case == 3 {
            sql(fixture,"CREATE FUNCTION public.delay_command() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_command AFTER INSERT ON td_mta.attempt_command_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_command()");
            assert_eq!(
                phases::publish(
                    request,
                    trust,
                    Instant::now() + Duration::from_millis(1500),
                    &value
                ),
                Err(phases::Error::Indeterminate)
            );
            thread::sleep(Duration::from_secs(4));
        }
        let outcome = phases::publish(request, trust, deadline(), &value).unwrap();
        assert_eq!(outcome.recipients, end);
        assert_eq!(outcome.new_sequence, outcome.old_sequence + 1);
        assert_eq!(
            phases::inspect(request, trust, deadline(), &value).unwrap(),
            Some(outcome)
        );
        assert_eq!(
            phases::publish(request, trust, deadline(), &value).unwrap(),
            outcome
        );
        let legacy = if data {
            phases::refused_data_command(claim, generation, attempt.attempt, &rcpts, &reply)
        } else {
            phases::refused_mail(claim, generation, attempt.attempt, &ordinals, &reply)
        }
        .unwrap();
        assert_eq!(
            phases::inspect(request, trust, deadline(), &legacy).unwrap(),
            Some(outcome)
        );
        let different = prepare(generation, "550 different").unwrap();
        assert_eq!(
            phases::publish(request, trust, deadline(), &different),
            Err(phases::Error::Conflict)
        );
        if data {
            let changed_rcpts: Vec<_> = rcpts
                .iter()
                .map(|(n, s)| {
                    (
                        *n,
                        if *n == 0 {
                            "250 different recipient"
                        } else {
                            *s
                        },
                    )
                })
                .collect();
            let different_rcpt = phases::refused_data_command(
                claim,
                generation,
                attempt.attempt,
                &changed_rcpts,
                &reply,
            )
            .unwrap();
            assert_eq!(
                phases::inspect(request, trust, deadline(), &different_rcpt),
                Err(phases::Error::Conflict)
            );
        }
        assert_eq!(sql(fixture,&format!("SELECT state,uncertain,next_attempt_at IS NOT NULL FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal IN (0,1) ORDER BY ordinal")),if temporary {"7|1|t\n3|0|t"}else{"7|1|f\n5|0|f"});
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<{end} AND phase=4 AND octet_length(data_reply)=4096 AND {}",if data {"octet_length(rcpt_reply)=4096"}else{"rcpt_reply IS NULL"})),end.to_string());
        assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{}','hex')",attempt.attempt)),"f");
        assert_eq!(sql(fixture,&format!("SELECT completed_at IS NOT NULL,notification FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),if temporary {"f|0"}else{"t|1"});
        if case == 3 {
            sql(
                fixture,
                "DROP TRIGGER delay_command ON td_mta.attempt_command_receipts",
            );
        }
    }
    for body_first in [false, true] {
        sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,phase=0,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.submissions SET completed_at=NULL,notification=0,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        let attempt = super::attempt_clock::publish(
            fixture,
            request,
            trust,
            deadline(),
            &attempts::prepare(claim, generation, submission).unwrap(),
        )
        .unwrap();
        let first: Vec<_> = (0..50).map(|n| (n, "250 accepted")).collect();
        let rest: Vec<_> = (50..100).map(|n| (n, "250 accepted")).collect();
        if body_first {
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::body(claim, generation, attempt.attempt, &first).unwrap(),
            )
            .unwrap();
            let ordinals: Vec<_> = (0..50).collect();
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
        } else {
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::refused_data_command(
                    claim,
                    generation,
                    attempt.attempt,
                    &first,
                    "550 refused",
                )
                .unwrap(),
            )
            .unwrap();
        }
        let conflicting = if body_first {
            phases::refused_data_command(claim, generation, attempt.attempt, &rest, "550 refused")
        } else {
            phases::body(claim, generation, attempt.attempt, &rest)
        }
        .unwrap();
        let before = sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        );
        assert_eq!(
            phases::publish(request, trust, deadline(), &conflicting),
            Err(phases::Error::Conflict)
        );
        assert_eq!(
            sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts"
            ),
            before
        );
        let refused: Vec<_> = (50..100).map(|n| (n, "550 RCPT refused")).collect();
        phases::publish(
            request,
            trust,
            deadline(),
            &phases::refused_rcpt(claim, generation, attempt.attempt, &refused).unwrap(),
        )
        .unwrap();
        assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{}','hex')",attempt.attempt)), "f");
    }
}
