use super::{deadline, relay_wire as wire, sql, Fixture, PASSWORD};
use std::sync::{Arc, Mutex};
use td_crypto::TrustStore;
use td_mta::{
    clock::RuntimeClock,
    ids::SubmissionId,
    ports::{Clock, Time, Transport},
    postgres_attempt_phases as phases, postgres_attempts as attempts,
    postgres_dispatch as dispatch,
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_relay_rcpt as rcpt,
    postgres_route_owners::{self as owners, Claim},
    queue_clock::Guard,
};
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
) {
    for n in 124..=133 {
        let submission = SubmissionId::from_bytes([n; 16]);
        let sub = format!("decode('{submission}','hex')");
        let src = format!("decode('{source}','hex')");
        sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,1,NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},0,'0@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={sub}"));
        sql(fixture,&format!("UPDATE td_mta.submissions SET recipient_count=2 WHERE id={sub}; INSERT INTO td_mta.recipients SELECT account,{sub},1,'1@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions WHERE id={sub}"));
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
        let published = published
            .admit_envelope(&mut database)
            .unwrap()
            .reserve_mail()
            .unwrap();
        let attempt = published.attempt();
        let (session, mut peer) = wire::authenticated(fixture, clock.clone(), cap);
        let accepted = wire::accepted_mail(published, session, &mut peer, cap, &mut database);
        let rcpt_cap = if n == 131 {
            td_mta::ports::Deadline::after(clock.sample().unwrap().monotonic, 1000).unwrap()
        } else {
            cap
        };
        let mut rcpt_database = wire::meter(rcpt_cap);
        let first: &[u8] = match n {
            125 => b"450 first\r\n",
            127 => b"550 first\r\n",
            132 => b"421 first\r\n",
            _ => b"250 first\r\n",
        };
        let (step, received) = command(
            Start::Mail(accepted),
            &mut peer,
            cap,
            &mut rcpt_database,
            b"RCPT TO:<0@example.test>\r\n",
            first,
        );
        assert_eq!(received, b"RCPT TO:<0@example.test>\r\n");
        let closed = if n == 132 {
            match step {
                Step::Closed(closed) => closed,
                _ => panic!("421 retained live RCPT custody"),
            }
        } else {
            let cursor = match step {
                Step::Next(cursor) => cursor,
                _ => panic!("first RCPT did not retain its next recipient"),
            };
            if matches!(n, 126 | 127) {
                cursor.close()
            } else {
                let held = if n == 129 {
                    Some((
                        worker.reserve_final(attempt, cap, &mut database).unwrap(),
                        worker.reserve_final(attempt, cap, &mut database).unwrap(),
                    ))
                } else {
                    None
                };
                if n == 130 {
                    sql(fixture,&format!("UPDATE td_mta.submissions SET reverse_path='changed@example.test' WHERE id={sub}"));
                }
                if n == 131 {
                    std::thread::sleep(std::time::Duration::from_millis(1100));
                }
                let response: &[u8] = if n == 128 {
                    b"354 unexpected\r\n"
                } else if n == 133 {
                    b"421 second\r\n"
                } else {
                    b"550 second\r\n"
                };
                let (step, received) = command(
                    Start::Cursor(cursor),
                    &mut peer,
                    cap,
                    &mut rcpt_database,
                    b"RCPT TO:<1@example.test>\r\n",
                    response,
                );
                if matches!(n, 129..=131) {
                    assert!(received.is_empty());
                } else {
                    assert_eq!(received, b"RCPT TO:<1@example.test>\r\n");
                }
                let closed = match step {
                    Step::Ready(done) => {
                        assert_eq!(n, 124);
                        done.close()
                    }
                    Step::Closed(closed) => {
                        assert!(matches!(n, 125 | 133));
                        closed
                    }
                    Step::Refused(failure) => {
                        let expected = match n {
                            128 => rcpt::Error::Publication(dispatch::Error::Phase(
                                phases::Error::Integrity,
                            )),
                            129 => rcpt::Error::Publication(dispatch::Error::Admission(
                                td_mta::ports::Error::Busy,
                            )),
                            130 => rcpt::Error::Publication(dispatch::Error::Publication(
                                attempts::Error::Integrity,
                            )),
                            131 => rcpt::Error::Protocol(td_mta::ports::Error::Deadline),
                            _ => panic!("unexpected RCPT failure"),
                        };
                        assert_eq!(failure.error, expected);
                        failure.closed
                    }
                    Step::Next(_) => panic!("RCPT continued beyond the admitted batch"),
                };
                drop(held);
                closed
            }
        };
        drop(closed.buffers.unwrap());
        drop(peer.into_buffers().unwrap());
        let result = closed.evidence.unwrap();
        assert_eq!(result.attempt(), attempt);
        assert_eq!(
            result.evidence().is_complete(),
            matches!(n, 124 | 125 | 133)
        );
        assert_eq!(
            result.evidence().accepted().len(),
            if matches!(n, 125 | 127 | 132) { 0 } else { 1 }
        );
        if let Some((ordinal, reply)) = result.evidence().accepted().first() {
            assert_eq!(*ordinal, 0);
            assert_eq!(reply, "250 first");
        }
        let interrupted = result.into_interruption().unwrap();
        assert_eq!(interrupted.recovery.is_none(), n == 125);
        let until = wire::until(&clock);
        let mut settlement = wire::meter(until);
        if let Some(refusal) = interrupted.refusal {
            worker
                .publish_phase(refusal, until, &mut settlement)
                .unwrap();
        }
        if let Some(recovery) = interrupted.recovery {
            worker
                .publish_recovery(recovery, until, &mut settlement)
                .unwrap();
        }
        let expected = match n {
            124 => "0|250 first|t\n1|550 second|t",
            125 => "0|450 first|t\n1|550 second|t",
            127 => "0|550 first|t\n1||t",
            132 => "0|421 first|t\n1||t",
            133 => "0|250 first|t\n1|421 second|t",
            _ => "0|250 first|t\n1||t",
        };
        assert_eq!(sql(fixture,&format!("SELECT ordinal,rcpt_reply,data_reply IS NULL FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")),expected);
        assert_eq!(pending.attempt().unwrap(), None);
        assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")),"f");
        sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "Fixture keeps real TLS owners inline."
)]
enum Start<'w, 'a, 'r> {
    Mail(td_mta::postgres_relay_mail::Accepted<'w, 'a, 'r, wire::Buffer>),
    Cursor(rcpt::Cursor<'w, 'a, 'r, wire::Buffer>),
}
enum Step<'w, 'a, 'r> {
    Next(rcpt::Cursor<'w, 'a, 'r, wire::Buffer>),
    Ready(rcpt::Completed<'w, 'a, 'r, wire::Buffer>),
    Closed(rcpt::Closed<'w, 'a, 'r, wire::Buffer>),
    Refused(rcpt::Refusal<'w, 'a, 'r, wire::Buffer>),
}
fn command<'w, 'a, 'r>(
    start: Start<'w, 'a, 'r>,
    peer: &mut wire::Peer,
    cap: td_mta::ports::Deadline,
    database: &mut td_mta::admission::work::Meter,
    expected: &[u8],
    response: &[u8],
) -> (Step<'w, 'a, 'r>, Vec<u8>) {
    let mut input = [0; 512];
    let mut line = [0; 512];
    let mut reply = [0; 4096];
    let mut output = [0; 512];
    let mut network = wire::meter(cap);
    let scratch = td_mta::smtp_relay_command::Scratch {
        input: &mut input,
        line: &mut line,
        reply: &mut reply,
        command: &mut output,
    };
    let begin = match start {
        Start::Mail(accepted) => rcpt::begin(accepted, scratch, &mut network, database),
        Start::Cursor(cursor) => cursor.begin_next(scratch, &mut network, database),
    };
    let mut exchange = match begin {
        Ok(exchange) => exchange,
        Err(failure) => return (Step::Refused(failure), Vec::new()),
    };
    let mut received = Vec::new();
    let mut sent = 0;
    loop {
        let mut bytes = [0; 512];
        if let td_mta::ports::IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
            received.extend_from_slice(&bytes[..n]);
        }
        assert!(expected.starts_with(&received));
        if received == expected {
            if sent < response.len() {
                if let td_mta::ports::IoProgress::Bytes(n) = peer.write(&response[sent..]).unwrap()
                {
                    sent += n;
                }
            }
            let _ = peer.flush().unwrap();
        }
        let step = match exchange.advance() {
            Ok(rcpt::Progress::Pending(next)) => {
                exchange = next;
                std::thread::yield_now();
                continue;
            }
            Ok(rcpt::Progress::Next(cursor)) => Step::Next(cursor),
            Ok(rcpt::Progress::Ready(done)) => Step::Ready(done),
            Ok(rcpt::Progress::Closed(closed)) => Step::Closed(closed),
            Err(failure) => Step::Refused(failure),
        };
        if !matches!(step, Step::Refused(_)) {
            assert_eq!(received, expected);
            assert_eq!(sent, response.len());
        }
        return (step, received);
    }
}
