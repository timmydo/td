use super::{deadline, sql, Fixture};
use std::{
    thread,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
use td_mta::{
    ids::SubmissionId,
    postgres_attempt_phases as phases, postgres_attempt_recovery as recovery,
    postgres_attempts as attempts,
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
    for phase in 1..=3 {
        sql(fixture,&format!("UPDATE td_mta.recipients SET state=1,phase=0,uncertain=0,attempt_id=NULL,attempt_count=0,last_attempt_at=NULL,next_attempt_at=0,reason=0,rcpt_reply=NULL,data_reply=NULL WHERE submission_id=decode('{submission}','hex'); UPDATE td_mta.submissions SET completed_at=NULL,notification=0,expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        let attempt = attempts::publish(
            request,
            trust,
            deadline(),
            &attempts::prepare(claim, generation, submission).unwrap(),
        )
        .unwrap();
        sql(fixture,&format!("UPDATE td_mta.recipients SET uncertain=1 WHERE submission_id=decode('{submission}','hex') AND ordinal=0; UPDATE td_mta.recipients SET rcpt_reply='250 earlier RCPT',data_reply='450 earlier DATA' WHERE submission_id=decode('{submission}','hex') AND state=2"));
        phases::publish(
            request,
            trust,
            deadline(),
            &phases::refused_rcpt(claim, generation, attempt.attempt, &[(99, "550 refused")])
                .unwrap(),
        )
        .unwrap();
        if phase >= 2 {
            let accepted: Vec<_> = (0..50).map(|n| (n, "250 current RCPT")).collect();
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::body(claim, generation, attempt.attempt, &accepted).unwrap(),
            )
            .unwrap();
        }
        if phase == 3 {
            let ordinals: Vec<_> = (0..50).collect();
            phases::publish(
                request,
                trust,
                deadline(),
                &phases::acceptance_possible(claim, generation, attempt.attempt, &ordinals)
                    .unwrap(),
            )
            .unwrap();
        }
        let value = recovery::interrupted(claim, generation, attempt.attempt).unwrap();
        let stale = recovery::interrupted(claim, generation + 1, attempt.attempt).unwrap();
        assert_eq!(
            recovery::publish(request, trust, deadline(), &stale),
            Err(recovery::Error::Owner(owners::Error::Stale))
        );
        assert_eq!(
            recovery::publish(
                request,
                trust,
                deadline(),
                &recovery::prepare(claim, generation, attempt.attempt).unwrap()
            ),
            Err(recovery::Error::CurrentOwner)
        );
        sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+1000 WHERE id=decode('{submission}','hex')"));
        assert_eq!(
            recovery::publish(request, trust, deadline(), &value),
            Err(recovery::Error::ExpiryHold)
        );
        sql(fixture,&format!("UPDATE td_mta.submissions SET expires_at=(extract(epoch FROM clock_timestamp())*1000)::bigint+86400000 WHERE id=decode('{submission}','hex')"));
        if phase == 1 {
            let before = sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts",
            );
            sql(fixture,"CREATE FUNCTION public.reject_interrupted() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture refusal'; END $$; CREATE TRIGGER reject_interrupted BEFORE INSERT ON td_mta.attempt_recovery_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_interrupted()");
            assert_eq!(
                recovery::publish(request, trust, deadline(), &value),
                Err(recovery::Error::Database)
            );
            assert_eq!(
                sql(
                    fixture,
                    "SELECT encode(sequence,'hex') FROM td_mta.accounts"
                ),
                before
            );
            assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND state=2")),"99");
            sql(
                fixture,
                "DROP TRIGGER reject_interrupted ON td_mta.attempt_recovery_receipts",
            );
        }
        let earliest = sql(
            fixture,
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
        );
        if phase == 3 {
            sql(fixture,"CREATE FUNCTION public.delay_interrupted() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_interrupted AFTER INSERT ON td_mta.attempt_recovery_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_interrupted()");
            assert_eq!(
                recovery::publish(
                    request,
                    trust,
                    Instant::now() + Duration::from_millis(1500),
                    &value
                ),
                Err(recovery::Error::Indeterminate)
            );
            thread::sleep(Duration::from_secs(4));
        }
        let outcome = recovery::publish(request, trust, deadline(), &value).unwrap();
        assert_eq!(outcome.recipients, 99);
        assert_eq!(outcome.new_sequence, outcome.old_sequence + 1);
        assert_eq!(
            recovery::inspect(request, trust, deadline(), &value).unwrap(),
            Some(outcome)
        );
        let retained=sql(fixture,&format!("SELECT md5(string_agg(concat_ws('|',ordinal,state,uncertain,phase,rcpt_reply,data_reply,next_attempt_at,reason),',' ORDER BY ordinal)) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<100"));
        assert_eq!(
            recovery::publish(request, trust, deadline(), &value).unwrap(),
            outcome
        );
        assert_eq!(sql(fixture,&format!("SELECT md5(string_agg(concat_ws('|',ordinal,state,uncertain,phase,rcpt_reply,data_reply,next_attempt_at,reason),',' ORDER BY ordinal)) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<100")),retained);
        let latest = sql(
            fixture,
            "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
        );
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<99 AND phase=4 AND next_attempt_at BETWEEN {earliest}::bigint+300000 AND {latest}::bigint+330000")),"99");
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<99 AND state=7 AND uncertain=1")),if phase==3 {"50"}else{"1"});
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal<99 AND reason=9")),if phase==3 {"50"}else{"0"});
        assert_eq!(sql(fixture,&format!("SELECT rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=50")),"250 earlier RCPT|450 earlier DATA");
        assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,next_attempt_at IS NULL FROM td_mta.recipients WHERE submission_id=decode('{submission}','hex') AND ordinal=99")),"5|550 refused|t");
        assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{}','hex')",attempt.attempt)),"f");
        assert_eq!(sql(fixture,&format!("SELECT completed_at IS NULL,notification FROM td_mta.submissions WHERE id=decode('{submission}','hex')")),"t|0");
        assert_eq!(
            recovery::publish(
                request,
                trust,
                deadline(),
                &recovery::interrupted(claim, generation, attempt.attempt).unwrap()
            ),
            Err(recovery::Error::Missing)
        );
        if phase == 3 {
            sql(
                fixture,
                "DROP TRIGGER delay_interrupted ON td_mta.attempt_recovery_receipts",
            );
        }
    }
}
