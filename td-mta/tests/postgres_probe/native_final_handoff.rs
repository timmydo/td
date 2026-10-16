use super::{deadline, relay_wire as wire, sql, Fixture, PASSWORD};
use std::sync::{Arc, Mutex};
use td_crypto::TrustStore;
use td_mta::{
    clock::RuntimeClock,
    ids::SubmissionId,
    ports::{Clock, IoProgress, Time, Transport},
    postgres_attempt_phases as phases, postgres_attempts as attempts, postgres_body,
    postgres_dispatch as dispatch,
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_relay_body as transfer, postgres_relay_finish as finish,
    postgres_route_owners::{self as owners, Claim},
    queue_clock::Guard,
    smtp_data_encode::Body,
    smtp_relay_body as body, smtp_relay_command as command,
};
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
) {
    for (n, refuse, mail_refusal, data_case) in [
        (95, false, 0, 0),
        (96, true, 0, 0),
        (97, false, 1, 0),
        (98, false, 2, 0),
        (99, false, 0, 1),
        (100, false, 0, 2),
        (101, false, 0, 3),
        (103, false, 0, 4),
        (104, false, 0, 5),
        (105, false, 0, 6),
        (106, false, 0, 7),
        (107, false, 0, 8),
        (108, false, 0, 9),
        (109, false, 0, 10),
        (110, false, 0, 11),
        (111, false, 0, 12),
        (112, false, 0, 13),
        (113, false, 0, 14),
        (114, true, 0, 0),
        (115, true, 0, 0),
    ] {
        let submission = SubmissionId::from_bytes([n; 16]);
        let sub = format!("decode('{submission}','hex')");
        let src = format!("decode('{source}','hex')");
        sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,1,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},0,'0@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={sub}"));
        if data_case >= 4 {
            sql(fixture,&format!("UPDATE td_mta.submissions SET recipient_count=2 WHERE id={sub}; INSERT INTO td_mta.recipients SELECT account,{sub},1,'1@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={sub}"));
        }
        owners::renew(request, trust, deadline(), claim, generation).unwrap();
        let clock = Arc::new(RuntimeClock::new());
        let guard = Arc::new(Mutex::new(
            Guard::new(Time {
                utc_ms: sql(
                    fixture,
                    "SELECT (extract(epoch FROM clock_timestamp())*1000)::bigint",
                )
                .parse()
                .unwrap(),
                monotonic: clock.sample().unwrap().monotonic,
            })
            .unwrap(),
        ));
        let runtime = Request {
            address: request.address,
            server_name: request.server_name,
            user: "mail_runtime",
            database: request.database,
            password: PASSWORD,
            mode: Mode::Runtime,
        };
        let slots = ReadSlots::new(2).unwrap();
        let pending = dispatch::Pending::default();
        let worker = dispatch::Dispatches {
            connection: dispatch::Connection {
                request: &runtime,
                trust,
                slots: &slots,
                clock: clock.clone(),
            },
            guard,
            pending: &pending,
        };
        let cap = wire::until(&clock);
        let job_limit = clock.deadline_instant(cap).unwrap();
        let mut database = wire::meter(cap);
        let published = worker
            .publish_prepared(
                claim,
                generation,
                dispatch::Input {
                    submission,
                    retry: false,
                    deadline: cap,
                },
                &mut database,
            )
            .unwrap();
        let mut published = published
            .admit_envelope(&mut database)
            .unwrap()
            .reserve_mail()
            .unwrap();
        let attempt = published.attempt();
        let prepared_limit = published.check_current(&mut database).unwrap();
        let envelope = published.envelope();
        assert!(prepared_limit < job_limit);
        assert_eq!(
            envelope.recipients.len(),
            if data_case >= 4 { 2 } else { 1 }
        );
        let (session, mut peer) = wire::authenticated(fixture, clock.clone(), cap);
        let (start, body_request, ordinal, rcpt) = if matches!(n, 95 | 96 | 114 | 115) {
            let completed = wire::accepted_body(published, session, &mut peer, cap, &mut database);
            let ready = completed.reserve_body(&mut database).ok().unwrap();
            let body_request = ready.dispatch().clone();
            (
                BodyStart::Bound(ready),
                body_request,
                0,
                String::from("250 recipient"),
            )
        } else {
            let (session, mail) = wire::command(
                session,
                &mut peer,
                clock.clone(),
                cap,
                command::Command::MailFrom(&envelope.reverse_path),
                format!("MAIL FROM:<{}>\r\n", envelope.reverse_path).as_bytes(),
                if mail_refusal != 0 {
                    b"550 sender refused\r\n"
                } else {
                    b"250 sender\r\n"
                },
            );
            if mail_refusal != 0 {
                assert_eq!(mail, "550 sender refused");
                drop(session.into_buffers().unwrap());
                if mail_refusal == 2 {
                    let short =
                        td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 1000)
                            .unwrap();
                    published.check_current(&mut wire::meter(short)).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(1100));
                    let failure = published.record_refusal(&mail, &mut database).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Admission(td_mta::ports::Error::Deadline)
                    );
                    assert_eq!(failure.attempt(), Some(attempt));
                    let failure = failure.retry(&mut wire::meter(cap)).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Admission(td_mta::ports::Error::Deadline)
                    );
                    assert_eq!(failure.attempt(), Some(attempt));
                    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=2 AND phase=1")), "1");
                    let evidence = failure.into_evidence().unwrap();
                    let until = wire::until(&clock);
                    let outcome = worker
                        .publish_phase(evidence, until, &mut wire::meter(until))
                        .unwrap();
                    let expected =
                        phases::refused_mail(claim, generation, attempt, &[0], &mail).unwrap();
                    assert_eq!(
                        phases::inspect(&runtime, trust, deadline(), &expected).unwrap(),
                        Some(outcome)
                    );
                    assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply IS NULL,data_reply FROM td_mta.recipients WHERE submission_id={sub}")), "5|t|550 sender refused");
                    assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
                } else {
                    sql(fixture,"CREATE FUNCTION public.reject_wire_mail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'wire MAIL refusal'; END $$; CREATE TRIGGER reject_wire_mail BEFORE INSERT ON td_mta.attempt_command_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_wire_mail()");
                    let failure = published.record_refusal(&mail, &mut database).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Phase(phases::Error::Database)
                    );
                    assert_eq!(failure.attempt(), Some(attempt));
                    let mut empty = td_mta::admission::work::Meter::new(cap, Default::default());
                    let failure = failure.retry(&mut empty).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Admission(td_mta::ports::Error::Capacity)
                    );
                    assert_eq!(failure.attempt(), Some(attempt));
                    let first = worker.reserve_final(attempt, cap, &mut database).unwrap();
                    let second = worker.reserve_final(attempt, cap, &mut database).unwrap();
                    let failure = failure.retry(&mut database).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Admission(td_mta::ports::Error::Busy)
                    );
                    drop((first, second));
                    sql(
                        fixture,
                        "DROP TRIGGER reject_wire_mail ON td_mta.attempt_command_receipts",
                    );
                    let outcome = failure.retry(&mut database).unwrap();
                    let value =
                        phases::refused_mail(claim, generation, attempt, &[0], &mail).unwrap();
                    assert_eq!(
                        phases::inspect(&runtime, trust, deadline(), &value).unwrap(),
                        Some(outcome)
                    );
                    assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply IS NULL,data_reply FROM td_mta.recipients WHERE submission_id={sub}")), "5|t|550 sender refused");
                }
                assert_eq!(pending.attempt().unwrap(), None);
                drop(peer.into_buffers().unwrap());
                sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                continue;
            }
            assert_eq!(mail, "250 sender");
            let published = published.accepted(&mail).unwrap();
            if data_case == 2 {
                let recipient = &published.envelope().recipients[0];
                let (session, rcpt) = wire::command(
                    session,
                    &mut peer,
                    clock.clone(),
                    cap,
                    command::Command::RcptTo(&recipient.address),
                    format!("RCPT TO:<{}>\r\n", recipient.address).as_bytes(),
                    b"250 recipient\r\n",
                );

                assert!(matches!(
                    published.reserve_data(&[(999, &rcpt)]),
                    Err(dispatch::Error::Admission(td_mta::ports::Error::Invalid))
                ));
                assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=2 AND phase=1")), "1");
                drop(session.into_buffers().unwrap());
                let cancellation =
                    td_mta::postgres_submission_cancel::prepare(claim, generation, submission)
                        .unwrap();
                td_mta::postgres_submission_cancel::publish(
                    &runtime,
                    trust,
                    deadline(),
                    &cancellation,
                    |_| Ok(()),
                )
                .unwrap();
                assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
                drop(peer.into_buffers().unwrap());
                sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                continue;
            }
            let mut published = published.reserve_rcpt().unwrap();
            assert_eq!(published.attempt(), attempt);
            let mut session = session;
            while published.next_recipient().is_some() {
                if data_case == 6 && published.next_recipient().unwrap().ordinal == 1 {
                    break;
                }
                assert!(published.check_current(&mut database).unwrap() <= prepared_limit);
                let recipient = published.next_recipient().unwrap();
                let ordinal = recipient.ordinal;
                let negative = data_case == 5 || (matches!(data_case, 4 | 7..=14) && ordinal == 1);
                let (next, reply) = wire::command(
                    session,
                    &mut peer,
                    clock.clone(),
                    cap,
                    command::Command::RcptTo(&recipient.address),
                    format!("RCPT TO:<{}>\r\n", recipient.address).as_bytes(),
                    if negative {
                        b"550 recipient refused\r\n"
                    } else {
                        b"250 recipient\r\n"
                    },
                );
                session = next;
                published.record_reply(ordinal, &reply).unwrap();
            }
            let result = published.finish().unwrap();
            assert_eq!(result.attempt(), attempt);
            if data_case == 5 || data_case == 6 {
                let failure = result.reserve_data().err().unwrap();
                assert_eq!(
                    failure.error,
                    dispatch::Error::Admission(td_mta::ports::Error::Invalid)
                );
                let result = failure.into_retained();
                assert_eq!(result.evidence().is_complete(), data_case == 5);
                assert_eq!(
                    result.evidence().accepted().len(),
                    usize::from(data_case == 6)
                );
                drop(session.into_buffers().unwrap());
                let interruption = result.into_interruption().unwrap();
                assert_eq!(interruption.recovery.is_some(), data_case == 6);
                if let Some(refusal) = interruption.refusal {
                    worker.publish_phase(refusal, cap, &mut database).unwrap();
                }
                if let Some(recovery) = interruption.recovery {
                    worker
                        .publish_recovery(recovery, cap, &mut database)
                        .unwrap();
                }
                assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")), if data_case == 5 { "5|550 recipient refused|\n5|550 recipient refused|" } else { "3|250 recipient|\n3||" });
                assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
                assert_eq!(pending.attempt().unwrap(), None);
                drop(peer.into_buffers().unwrap());
                sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                continue;
            }
            assert!(result.evidence().is_complete());
            let (ordinal, rcpt) = result.evidence().accepted().first().unwrap().clone();
            let result = result.reserve_data().unwrap();
            if matches!(data_case, 7..=11) {
                let mut result = result;
                assert!(result.check_current(&mut database).unwrap() <= prepared_limit);
                let (session, data) = wire::command(
                    session,
                    &mut peer,
                    clock.clone(),
                    cap,
                    command::Command::Data,
                    b"DATA\r\n",
                    b"550 data refused\r\n",
                );
                drop(session.into_buffers().unwrap());
                if data_case == 11 {
                    sql(fixture, "CREATE FUNCTION public.delay_extracted_rcpt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_extracted_rcpt AFTER INSERT ON td_mta.attempt_rcpt_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_extracted_rcpt()");
                    let short =
                        td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 1500)
                            .unwrap();
                    let failure = result
                        .record_refusal(&data, &mut wire::meter(short))
                        .unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Phase(phases::Error::Indeterminate)
                    );
                    assert_eq!(failure.attempt(), Some(attempt));
                    let evidence = failure.into_evidence().unwrap();
                    assert!(evidence.refusal.is_some());
                    assert_eq!(pending.attempt().unwrap(), Some(attempt));
                    let until = wire::until(&clock);
                    let mut settlement = wire::meter(until);
                    assert_eq!(
                        worker.publish_phase(evidence.command.clone(), until, &mut settlement),
                        Err(dispatch::Error::Admission(td_mta::ports::Error::Busy))
                    );
                    std::thread::sleep(std::time::Duration::from_secs(4));
                    sql(
                        fixture,
                        "DROP TRIGGER delay_extracted_rcpt ON td_mta.attempt_rcpt_receipts",
                    );
                    let slots = ReadSlots::new(1).unwrap();
                    let connection = dispatch::Connection {
                        request: &runtime,
                        trust,
                        slots: &slots,
                        clock: clock.clone(),
                    };
                    let historical =
                        match pending.settle(&connection, until, &mut settlement).unwrap() {
                            Some(dispatch::Settlement::Phase(outcome)) => outcome,
                            other => panic!("expected RCPT settlement, got {other:?}"),
                        };
                    assert_eq!(pending.attempt().unwrap(), None);
                    let replay = worker
                        .publish_phase(evidence.refusal.unwrap(), until, &mut settlement)
                        .unwrap();
                    assert_eq!(replay, historical);
                    let outcome = worker
                        .publish_phase(evidence.command, until, &mut settlement)
                        .unwrap();
                    let expected = phases::refused_data_command(
                        claim,
                        generation,
                        attempt,
                        &[(ordinal, &rcpt)],
                        &data,
                    )
                    .unwrap();
                    assert_eq!(
                        phases::inspect(&runtime, trust, deadline(), &expected).unwrap(),
                        Some(outcome)
                    );
                    assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")), "5|250 recipient|550 data refused\n5|550 recipient refused|");
                    assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
                    assert_eq!(pending.attempt().unwrap(), None);
                    drop(peer.into_buffers().unwrap());
                    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                    continue;
                }
                let table = if matches!(data_case, 7 | 9) {
                    "attempt_rcpt_receipts"
                } else {
                    "attempt_command_receipts"
                };
                let trigger = format!("reject_mixed_data_{n}");
                sql(fixture, &format!("CREATE FUNCTION public.{trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'mixed DATA fixture'; END $$; CREATE TRIGGER {trigger} BEFORE INSERT ON td_mta.{table} FOR EACH ROW EXECUTE FUNCTION public.{trigger}()"));
                let failure = result.record_refusal(&data, &mut database).unwrap_err();
                assert_eq!(
                    failure.error,
                    dispatch::Error::Phase(phases::Error::Database)
                );
                assert_eq!(failure.attempt(), Some(attempt));
                assert_eq!(pending.attempt().unwrap(), None);
                assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")), if matches!(data_case, 7 | 9) { "2||\n2||" } else { "2||\n5|550 recipient refused|" });
                sql(
                    fixture,
                    &format!("DROP TRIGGER {trigger} ON td_mta.{table}"),
                );
                let mut empty = td_mta::admission::work::Meter::new(cap, Default::default());
                let failure = failure.retry(&mut empty).unwrap_err();
                assert_eq!(
                    failure.error,
                    dispatch::Error::Admission(td_mta::ports::Error::Capacity)
                );
                assert_eq!(failure.attempt(), Some(attempt));
                let outcome = if data_case >= 9 {
                    let first = worker.reserve_final(attempt, cap, &mut database).unwrap();
                    let second = worker.reserve_final(attempt, cap, &mut database).unwrap();
                    let short =
                        td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 1000)
                            .unwrap();
                    let failure = failure.retry(&mut wire::meter(short)).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Admission(td_mta::ports::Error::Busy)
                    );
                    drop((first, second));
                    std::thread::sleep(std::time::Duration::from_millis(1100));
                    let failure = failure.retry(&mut database).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Admission(td_mta::ports::Error::Deadline)
                    );
                    assert_eq!(failure.attempt(), Some(attempt));
                    let evidence = failure.into_evidence().unwrap();
                    assert_eq!(evidence.refusal.is_some(), data_case == 9);
                    let until = wire::until(&clock);
                    let mut settlement = wire::meter(until);
                    if let Some(refusal) = evidence.refusal {
                        worker
                            .publish_phase(refusal, until, &mut settlement)
                            .unwrap();
                    }
                    worker
                        .publish_phase(evidence.command, until, &mut settlement)
                        .unwrap()
                } else {
                    failure.retry(&mut database).unwrap()
                };
                let value = phases::refused_data_command(
                    claim,
                    generation,
                    attempt,
                    &[(ordinal, &rcpt)],
                    &data,
                )
                .unwrap();
                assert_eq!(
                    phases::inspect(&runtime, trust, deadline(), &value).unwrap(),
                    Some(outcome)
                );
                assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")), "5|250 recipient|550 data refused\n5|550 recipient refused|");
                assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
                assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.attempt_rcpt_receipts WHERE attempt_id=decode('{attempt}','hex')")), "1");
                assert_eq!(pending.attempt().unwrap(), None);
                drop(peer.into_buffers().unwrap());
                sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                continue;
            }
            let (body_owner, body_request, session) = if matches!(data_case, 4 | 12..=14) {
                let mut result = result;
                assert_eq!(result.attempt(), attempt);
                assert!(result.check_current(&mut database).unwrap() <= prepared_limit);
                let (session, data) = wire::command(
                    session,
                    &mut peer,
                    clock.clone(),
                    cap,
                    command::Command::Data,
                    b"DATA\r\n",
                    b"354 send body\r\n",
                );
                if data_case >= 12 {
                    let table = if data_case == 12 {
                        "attempt_rcpt_receipts"
                    } else {
                        "attempt_phase_receipts"
                    };
                    let trigger = format!("body_handoff_{n}");
                    if data_case == 14 {
                        sql(fixture, &format!("CREATE FUNCTION public.{trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER {trigger} AFTER INSERT ON td_mta.{table} DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.{trigger}()"));
                    } else {
                        sql(fixture, &format!("CREATE FUNCTION public.{trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'Body handoff fixture'; END $$; CREATE TRIGGER {trigger} BEFORE INSERT ON td_mta.{table} FOR EACH ROW EXECUTE FUNCTION public.{trigger}()"));
                    }
                    let failure = if data_case == 14 {
                        let short =
                            td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 1500)
                                .unwrap();
                        result
                            .publish_body(&data, &mut wire::meter(short))
                            .err()
                            .unwrap()
                    } else {
                        result.publish_body(&data, &mut database).err().unwrap()
                    };
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Phase(if data_case == 14 {
                            phases::Error::Indeterminate
                        } else {
                            phases::Error::Database
                        })
                    );
                    assert_eq!(failure.attempt(), attempt);
                    drop(session.into_buffers().unwrap());
                    let interruption = failure.into_interruption().unwrap();
                    assert_eq!(interruption.refusal.is_some(), data_case == 12);
                    assert!(interruption.recovery.is_some());
                    let until = wire::until(&clock);
                    let mut settlement = wire::meter(until);
                    if data_case == 14 {
                        assert_eq!(pending.attempt().unwrap(), Some(attempt));
                        assert_eq!(
                            worker.publish_recovery(
                                interruption.recovery.as_ref().unwrap().clone(),
                                until,
                                &mut settlement
                            ),
                            Err(dispatch::Error::Admission(td_mta::ports::Error::Busy))
                        );
                        std::thread::sleep(std::time::Duration::from_secs(4));
                        sql(
                            fixture,
                            &format!("DROP TRIGGER {trigger} ON td_mta.{table}"),
                        );
                        let slots = ReadSlots::new(1).unwrap();
                        let connection = dispatch::Connection {
                            request: &runtime,
                            trust,
                            slots: &slots,
                            clock: clock.clone(),
                        };
                        let historical =
                            match pending.settle(&connection, until, &mut settlement).unwrap() {
                                Some(dispatch::Settlement::Phase(outcome)) => outcome,
                                other => panic!("expected Body settlement, got {other:?}"),
                            };
                        let expected =
                            phases::body(claim, generation, attempt, &[(ordinal, &rcpt)]).unwrap();
                        assert_eq!(
                            phases::inspect(&runtime, trust, deadline(), &expected).unwrap(),
                            Some(historical)
                        );
                        assert_eq!(sql(fixture,&format!("SELECT state,phase,rcpt_reply FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=0")), "2|2|250 recipient");
                    } else {
                        sql(
                            fixture,
                            &format!("DROP TRIGGER {trigger} ON td_mta.{table}"),
                        );
                        assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")), if data_case == 12 { "2||\n2||" } else { "2||\n5|550 recipient refused|" });
                    }
                    assert_eq!(pending.attempt().unwrap(), None);
                    if let Some(refusal) = interruption.refusal {
                        worker
                            .publish_phase(refusal, until, &mut settlement)
                            .unwrap();
                    }
                    worker
                        .publish_recovery(interruption.recovery.unwrap(), until, &mut settlement)
                        .unwrap();
                    assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")), "3|250 recipient|\n5|550 recipient refused|");
                    assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
                    assert_eq!(pending.attempt().unwrap(), None);
                    drop(peer.into_buffers().unwrap());
                    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                    continue;
                }
                let (body_owner, body_request) = result.publish_body(&data, &mut database).unwrap();
                assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply IS NULL FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=1")), "5|550 recipient refused|t");
                (body_owner, body_request, session)
            } else {
                let (mut published, refusal) = result.into_parts();
                assert_eq!(published.attempt(), attempt);
                assert!(published.check_current(&mut database).unwrap() <= prepared_limit);
                let (session, data) = wire::command(
                    session,
                    &mut peer,
                    clock.clone(),
                    cap,
                    command::Command::Data,
                    b"DATA\r\n",
                    match data_case {
                        1 => b"550 data refused\r\n",
                        3 => b"250 wrong status\r\n",
                        _ => b"354 send body\r\n",
                    },
                );
                if let Some(refusal) = refusal {
                    let outcome = worker.publish_phase(refusal, cap, &mut database).unwrap();
                    assert_eq!(outcome.recipients, 1);
                    assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply IS NULL FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=1")), "5|550 recipient refused|t");
                }
                if data_case == 1 {
                    assert_eq!(data, "550 data refused");
                    drop(session.into_buffers().unwrap());
                    sql(fixture,"CREATE FUNCTION public.reject_wire_data() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'wire DATA refusal'; END $$; CREATE TRIGGER reject_wire_data BEFORE INSERT ON td_mta.attempt_command_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_wire_data()");
                    let failure = published.record_refusal(&data, &mut database).unwrap_err();
                    assert_eq!(
                        failure.error,
                        dispatch::Error::Phase(phases::Error::Database)
                    );
                    assert_eq!(failure.attempt(), Some(attempt));
                    sql(
                        fixture,
                        "DROP TRIGGER reject_wire_data ON td_mta.attempt_command_receipts",
                    );
                    let outcome = failure.retry(&mut database).unwrap();
                    let value = phases::refused_data_command(
                        claim,
                        generation,
                        attempt,
                        &[(ordinal, &rcpt)],
                        &data,
                    )
                    .unwrap();
                    assert_eq!(
                        phases::inspect(&runtime, trust, deadline(), &value).unwrap(),
                        Some(outcome)
                    );
                    assert_eq!(sql(fixture,&format!("SELECT state,rcpt_reply,data_reply FROM td_mta.recipients WHERE submission_id={sub}")), "5|250 recipient|550 data refused");
                    assert_eq!(pending.attempt().unwrap(), None);
                    drop(peer.into_buffers().unwrap());
                    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                    continue;
                }
                if data_case == 3 {
                    assert_eq!(data, "250 wrong status");
                    assert!(matches!(
                        published.publish_body(&data, &mut database),
                        Err(dispatch::Error::Admission(td_mta::ports::Error::Invalid))
                    ));
                    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=2 AND phase=1")), "1");
                    drop(session.into_buffers().unwrap());
                    let cancellation =
                        td_mta::postgres_submission_cancel::prepare(claim, generation, submission)
                            .unwrap();
                    td_mta::postgres_submission_cancel::publish(
                        &runtime,
                        trust,
                        deadline(),
                        &cancellation,
                        |_| Ok(()),
                    )
                    .unwrap();
                    assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")), "f");
                    drop(peer.into_buffers().unwrap());
                    sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
                    continue;
                }
                assert_eq!(data, "354 send body");
                let (body_owner, body_request) =
                    published.publish_body(&data, &mut database).unwrap();
                (body_owner, body_request, session)
            };
            assert_eq!(body_owner.attempt(), attempt);
            assert!(body_owner.check_current(&mut database).unwrap() <= prepared_limit);
            let plan = body_owner.reserve_final(&mut database).unwrap();
            assert_eq!(plan.attempt(), attempt);
            (
                BodyStart::Trusted(plan, session),
                body_request,
                ordinal,
                rcpt,
            )
        };
        assert_eq!(body_request.attempt(), attempt);
        assert!(start.check_current(&mut database).unwrap() <= prepared_limit);
        let occupied = worker.reserve_final(attempt, cap, &mut database).unwrap();
        assert_eq!(
            start.check_current(&mut database),
            Err(dispatch::Error::Admission(td_mta::ports::Error::Busy))
        );
        drop(occupied);
        start.check_current(&mut database).unwrap();
        let mut transferred = Vec::new();
        let handoff = attempts::with_body(
            &runtime,
            trust,
            prepared_limit,
            &body_request,
            postgres_body::MAX_BODY_BYTES,
            |metadata, source| {
                assert_eq!(metadata.attempt, attempt);
                let mut input = [0; body::INPUT_BYTES];
                let mut output = [0; body::OUTPUT_BYTES];
                let mut work = wire::meter(cap);
                let mut exchange = start
                    .begin(
                        source,
                        body::Scratch {
                            input: &mut input,
                            output: &mut output,
                        },
                        &mut work,
                        &mut database,
                    )
                    .ok()
                    .unwrap();
                for _ in 0..100_000 {
                    let mut bytes = [0; 4096];
                    if let IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
                        transferred.extend_from_slice(&bytes[..n]);
                    }
                    match exchange.advance().ok().unwrap() {
                        transfer::Progress::Pending(next) => exchange = next,
                        transfer::Progress::Ready(done) => return Ok(done),
                    }
                    std::thread::yield_now();
                }
                panic!("native body did not finish");
            },
        )
        .unwrap();
        assert!(!transferred.windows(5).any(|v| v == b"\r\n.\r\n"));
        assert_eq!(
            phases::check_current_body(
                &runtime,
                trust,
                prepared_limit,
                &phases::body(claim, generation, attempt, &[(ordinal, &rcpt)]).unwrap(),
                clock.clone(),
                worker.guard.clone()
            ),
            Err(phases::Error::Conflict)
        );
        if refuse {
            sql(fixture,"CREATE OR REPLACE FUNCTION public.reject_wire_final() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'wire fixture refusal'; END $$; CREATE TRIGGER reject_wire_final BEFORE INSERT ON td_mta.attempt_acceptance_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_wire_final()");
        }
        let (mut input, mut line, mut reply, mut terminator) =
            ([0; 512], [0; 512], [0; 4096], [0; 3]);
        let mut work = wire::meter(cap);
        let mut exchange = handoff
            .begin_final(
                command::Scratch {
                    input: &mut input,
                    line: &mut line,
                    reply: &mut reply,
                    command: &mut terminator,
                },
                &mut work,
                &mut database,
            )
            .ok()
            .unwrap();
        let response = b"250 queued\r\n";
        let mut written = 0;
        let mut turns = 0;
        let mut seen_terminator = false;
        let outcome = loop {
            turns += 1;
            assert!(turns <= 100_000);
            let mut bytes = [0; 4096];
            if let IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
                transferred.extend_from_slice(&bytes[..n]);
            }
            if transferred.ends_with(b"\r\n.\r\n") {
                if !seen_terminator {
                    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=2 AND phase=3")),"1");
                    seen_terminator = true;
                }
                if written < response.len() {
                    if let IoProgress::Bytes(n) = peer.write(&response[written..]).unwrap() {
                        written += n;
                    }
                }
                let _ = peer.flush().unwrap();
            }
            match exchange.advance() {
                Ok(finish::Progress::Pending(next)) => exchange = next,
                Ok(finish::Progress::Ready(done)) => {
                    assert!(!refuse);
                    let result = done.outcome;
                    drop(done.into_buffers().unwrap());
                    break result;
                }
                Err(failure) => {
                    assert!(refuse);
                    drop(failure.buffers.unwrap());
                    let retained = failure.retained.unwrap();
                    assert_eq!(retained.attempt(), Some(attempt));
                    if matches!(n, 114 | 115) {
                        assert_eq!(
                            retained.error,
                            dispatch::Error::Phase(phases::Error::Database)
                        );
                        if n == 114 {
                            let short = td_mta::ports::Deadline::after(
                                clock.sample().unwrap().monotonic,
                                500,
                            )
                            .unwrap();
                            let retained = retained.retry(&mut wire::meter(short)).unwrap_err();
                            assert_eq!(
                                retained.error,
                                dispatch::Error::Phase(phases::Error::Database)
                            );
                            sql(fixture, "DROP TRIGGER reject_wire_final ON td_mta.attempt_acceptance_receipts");
                            std::thread::sleep(std::time::Duration::from_millis(600));
                            let until = wire::until(&clock);
                            let mut settlement = wire::meter(until);
                            let retained = retained.retry(&mut settlement).unwrap_err();
                            assert_eq!(
                                retained.error,
                                dispatch::Error::Admission(td_mta::ports::Error::Deadline)
                            );
                            let other = worker
                                .reserve_final(attempt, until, &mut settlement)
                                .unwrap();
                            assert_eq!(
                                worker
                                    .reserve_final(attempt, until, &mut settlement)
                                    .err()
                                    .unwrap(),
                                dispatch::Error::Admission(td_mta::ports::Error::Busy)
                            );
                            assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.attempt_final_reservations WHERE attempt_id=decode('{attempt}','hex')")), "1");
                            let evidence = retained.into_evidence().unwrap();
                            let released = worker
                                .reserve_final(attempt, until, &mut settlement)
                                .unwrap();
                            drop((other, released));
                            assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.attempt_final_reservations WHERE attempt_id=decode('{attempt}','hex')")), "1");
                            let mut empty =
                                td_mta::admission::work::Meter::new(until, Default::default());
                            assert_eq!(
                                worker.publish_phase(evidence.clone(), until, &mut empty),
                                Err(dispatch::Error::Admission(td_mta::ports::Error::Capacity))
                            );
                            break worker
                                .publish_phase(evidence, until, &mut settlement)
                                .unwrap();
                        }
                        sql(
                            fixture,
                            "DROP TRIGGER reject_wire_final ON td_mta.attempt_acceptance_receipts",
                        );
                        sql(fixture, "CREATE FUNCTION public.delay_extracted_final() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_extracted_final AFTER INSERT ON td_mta.attempt_acceptance_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_extracted_final()");
                        let short =
                            td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 1500)
                                .unwrap();
                        let retained = retained.retry(&mut wire::meter(short)).unwrap_err();
                        assert_eq!(
                            retained.error,
                            dispatch::Error::Phase(phases::Error::Indeterminate)
                        );
                        let evidence = retained.into_evidence().unwrap();
                        assert_eq!(pending.attempt().unwrap(), Some(attempt));
                        let until = wire::until(&clock);
                        let mut settlement = wire::meter(until);
                        assert_eq!(
                            worker.publish_phase(evidence.clone(), until, &mut settlement),
                            Err(dispatch::Error::Admission(td_mta::ports::Error::Busy))
                        );
                        std::thread::sleep(std::time::Duration::from_secs(4));
                        sql(fixture, "DROP TRIGGER delay_extracted_final ON td_mta.attempt_acceptance_receipts");
                        let slots = ReadSlots::new(1).unwrap();
                        let connection = dispatch::Connection {
                            request: &runtime,
                            trust,
                            slots: &slots,
                            clock: clock.clone(),
                        };
                        let historical =
                            match pending.settle(&connection, until, &mut settlement).unwrap() {
                                Some(dispatch::Settlement::Phase(outcome)) => outcome,
                                other => panic!("expected final settlement, got {other:?}"),
                            };
                        assert_eq!(pending.attempt().unwrap(), None);
                        let first = worker
                            .reserve_final(attempt, until, &mut settlement)
                            .unwrap();
                        let second = worker
                            .reserve_final(attempt, until, &mut settlement)
                            .unwrap();
                        drop((first, second));
                        let replay = worker
                            .publish_phase(evidence, until, &mut settlement)
                            .unwrap();
                        assert_eq!(replay, historical);
                        break replay;
                    }
                    sql(
                        fixture,
                        "DROP TRIGGER reject_wire_final ON td_mta.attempt_acceptance_receipts",
                    );
                    break retained.retry(&mut database).unwrap();
                }
            }
            std::thread::yield_now();
        };
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.attempt_final_reservations WHERE attempt_id=decode('{attempt}','hex')")), "0");
        assert_eq!(written, response.len());
        assert_eq!(
            transferred
                .windows(5)
                .filter(|v| *v == b"\r\n.\r\n")
                .count(),
            1
        );
        let final_value =
            phases::accepted(claim, generation, attempt, &[ordinal], "250 queued").unwrap();
        assert_eq!(
            phases::inspect(&runtime, trust, deadline(), &final_value).unwrap(),
            Some(outcome)
        );
        assert_eq!(pending.attempt().unwrap(), None);
        drop(peer.into_buffers().unwrap());
        sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    }
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
}

// Focused legacy interruption cases keep their explicit trusted input seam;
// composed envelope cases enter only through the bound DATA owner.
#[allow(clippy::large_enum_variant)]
enum BodyStart<'w, 'a, 'r> {
    Bound(td_mta::postgres_relay_data::BodyReady<'w, 'a, 'r, wire::Buffer>),
    Trusted(
        dispatch::BodyPlan<'w, 'a, 'r>,
        td_mta::smtp_relay_auth::Session<wire::Buffer>,
    ),
}
impl<'w, 'a, 'r> BodyStart<'w, 'a, 'r> {
    fn check_current(
        &self,
        database: &mut td_mta::admission::work::Meter,
    ) -> Result<std::time::Instant, dispatch::Error> {
        match self {
            Self::Bound(ready) => ready.check_current(database),
            Self::Trusted(plan, _) => plan.check_current(database),
        }
    }
    fn begin<'m>(
        self,
        source: &'m mut dyn td_mta::ports::BlobReader,
        scratch: body::Scratch<'m>,
        wire: &'m mut td_mta::admission::work::Meter,
        database: &'m mut td_mta::admission::work::Meter,
    ) -> Result<transfer::Exchange<'m, 'w, 'a, 'r, wire::Buffer>, transfer::Refusal<wire::Buffer>>
    {
        match self {
            Self::Bound(ready) => ready.begin(source, scratch, wire, database),
            Self::Trusted(plan, session) => transfer::begin(
                plan,
                session,
                source,
                Body::SevenBit,
                scratch,
                wire,
                database,
            ),
        }
    }
}
