use super::{deadline, relay_wire as wire, sql, Fixture, PASSWORD};
use std::sync::{Arc, Mutex};
use td_crypto::TrustStore;
use td_mta::{
    clock::RuntimeClock,
    ids::SubmissionId,
    ports::{Clock, IoProgress, Time, Transport},
    postgres_attempt_phases as phases, postgres_attempt_recovery as recovery,
    postgres_dispatch as dispatch,
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_relay_mail as mail,
    postgres_route_owners::{self as owners, Claim},
    queue_clock::Guard,
    smtp_data_encode::Body,
    smtp_relay_command::Scratch,
};
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
) {
    for n in 116..=123 {
        let submission = SubmissionId::from_bytes([n; 16]);
        let sub = format!("decode('{submission}','hex')");
        let src = format!("decode('{source}','hex')");
        sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,1,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},0,'0@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={sub}"));
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
        let expected = format!("MAIL FROM:<{}>\r\n", published.envelope().reverse_path);
        let (session, mut peer) = wire::authenticated(fixture, clock.clone(), cap);
        if n == 123 {
            sql(fixture, &format!("UPDATE td_mta.submissions SET reverse_path='changed@example.test' WHERE id={sub}"));
        }
        if n == 120 {
            let short =
                td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 1000).unwrap();
            published.check_current(&mut wire::meter(short)).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        if n == 118 {
            sql(fixture, "CREATE FUNCTION public.reject_bound_mail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'bound MAIL refusal'; END $$; CREATE TRIGGER reject_bound_mail BEFORE INSERT ON td_mta.attempt_command_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_bound_mail()");
        }
        let held = if n == 119 {
            Some((
                worker.reserve_final(attempt, cap, &mut database).unwrap(),
                worker.reserve_final(attempt, cap, &mut database).unwrap(),
            ))
        } else {
            None
        };
        let mut input = [0; 512];
        let mut line = [0; 512];
        let mut reply = [0; 4096];
        let mut command = [0; 512];
        let mut network = wire::meter(cap);
        let mode = if n == 121 {
            Body::EightBit
        } else {
            Body::SevenBit
        };
        let begin = mail::begin(
            published,
            session,
            mode,
            Scratch {
                input: &mut input,
                line: &mut line,
                reply: &mut reply,
                command: &mut command,
            },
            &mut network,
            &mut database,
        );
        let mut terminal = false;
        if matches!(n, 120 | 121) {
            let failure = begin.err().unwrap();
            assert_eq!(
                failure.error,
                mail::Error::Protocol(if n == 120 {
                    td_mta::ports::Error::Deadline
                } else {
                    td_mta::ports::Error::Forbidden
                })
            );
            assert!(failure.retained.is_none());
            drop(failure.buffers.unwrap());
        } else {
            let mut exchange = begin.ok().unwrap();
            let response: &[u8] = if matches!(n, 117 | 118) {
                b"550 sender refused\r\n".as_slice()
            } else if n == 122 {
                b"354 unexpected\r\n"
            } else {
                b"250 sender\r\n"
            };
            let mut received = Vec::new();
            let mut written = 0;
            loop {
                let mut bytes = [0; 512];
                if let IoProgress::Bytes(count) = peer.read(&mut bytes).unwrap() {
                    received.extend_from_slice(&bytes[..count]);
                }
                assert!(expected.as_bytes().starts_with(&received));
                if received == expected.as_bytes() {
                    if written < response.len() {
                        if let IoProgress::Bytes(count) = peer.write(&response[written..]).unwrap()
                        {
                            written += count;
                        }
                    }
                    let _ = peer.flush().unwrap();
                }
                match exchange.advance() {
                    Ok(mail::Progress::Pending(next)) => exchange = next,
                    Ok(mail::Progress::Accepted(accepted)) => {
                        assert_eq!(n, 116);
                        let (envelope, session, retained_mode) = accepted.into_parts();
                        assert_eq!(envelope.attempt(), attempt);
                        assert_eq!(retained_mode, Body::SevenBit);
                        assert_eq!(
                            envelope.envelope().reverse_path,
                            expected
                                .trim_start_matches("MAIL FROM:<")
                                .trim_end_matches(">\r\n")
                        );
                        drop(session.into_buffers().unwrap());
                        break;
                    }
                    Ok(mail::Progress::Rejected(rejected)) => {
                        assert_eq!(n, 117);
                        let value = phases::refused_mail(
                            claim,
                            generation,
                            attempt,
                            &[0],
                            "550 sender refused",
                        )
                        .unwrap();
                        assert_eq!(
                            phases::inspect(&runtime, trust, deadline(), &value).unwrap(),
                            Some(rejected.outcome)
                        );
                        drop(rejected.buffers.unwrap());
                        terminal = true;
                        break;
                    }
                    Err(failure) => {
                        drop(failure.buffers.unwrap());
                        if n == 118 {
                            assert_eq!(
                                failure.error,
                                mail::Error::Publication(dispatch::Error::Phase(
                                    phases::Error::Database
                                ))
                            );
                            let retained = failure.retained.unwrap();
                            assert_eq!(retained.attempt(), Some(attempt));
                            sql(
                                fixture,
                                "DROP TRIGGER reject_bound_mail ON td_mta.attempt_command_receipts",
                            );
                            let until = wire::until(&clock);
                            let outcome = retained.retry(&mut wire::meter(until)).unwrap();
                            let value = phases::refused_mail(
                                claim,
                                generation,
                                attempt,
                                &[0],
                                "550 sender refused",
                            )
                            .unwrap();
                            assert_eq!(
                                phases::inspect(&runtime, trust, deadline(), &value).unwrap(),
                                Some(outcome)
                            );
                            terminal = true;
                        } else {
                            assert!(failure.retained.is_none());
                            assert_eq!(
                                failure.error,
                                if n == 119 {
                                    mail::Error::Publication(dispatch::Error::Admission(
                                        td_mta::ports::Error::Busy,
                                    ))
                                } else if n == 123 {
                                    mail::Error::Publication(dispatch::Error::Publication(
                                        td_mta::postgres_attempts::Error::Integrity,
                                    ))
                                } else {
                                    assert_eq!(n, 122);
                                    mail::Error::Protocol(td_mta::ports::Error::Invalid)
                                }
                            );
                        }
                        break;
                    }
                }
                std::thread::yield_now();
            }
            if matches!(n, 119 | 123) {
                assert!(received.is_empty());
            } else {
                assert_eq!(received, expected.as_bytes());
                assert_eq!(written, response.len());
            }
        }
        drop(held);
        drop(peer.into_buffers().unwrap());
        if !terminal {
            let until = wire::until(&clock);
            let value = recovery::interrupted(claim, generation, attempt).unwrap();
            worker
                .publish_recovery(value, until, &mut wire::meter(until))
                .unwrap();
        } else {
            assert_eq!(
                sql(
                    fixture,
                    &format!(
                        "SELECT state,data_reply FROM td_mta.recipients WHERE submission_id={sub}"
                    )
                ),
                "5|550 sender refused"
            );
        }
        assert_eq!(pending.attempt().unwrap(), None);
        assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")),"f");
        sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    }
}
