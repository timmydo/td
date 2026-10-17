use super::{deadline, native_relay_connect::Setup, relay_wire as wire, sql, Fixture, PASSWORD};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use td_crypto::{Crypto, Digest, Provider, TrustStore};
use td_mta::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    ids::SubmissionId,
    ports::{Clock, Deadline, FlushProgress, IoProgress, Time, Transport},
    postgres_dispatch as dispatch,
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_relay_attempt as attempt_driver,
    postgres_route_owners::{self as owners, Claim},
    queue_clock::Guard,
    smtp_data_encode::Body,
};

#[allow(clippy::too_many_arguments)]
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
    setup: &Setup,
) {
    for n in [173u8, 174, 175, 176, 177, 178] {
        let submission = SubmissionId::from_bytes([n; 16]);
        let sub = format!("decode('{submission}','hex')");
        let src = format!("decode('{source}','hex')");
        let lines = match n {
            174 => 144,
            178 => 333334,
            _ => 64,
        };
        let count = if n == 175 { 3 } else { 2 };
        let last = count - 1;
        let (body, body_sql) = if n == 178 {
            (
                ".\r\n".repeat(lines),
                format!("repeat('.'||chr(13)||chr(10),{lines})"),
            )
        } else {
            (
                format!(".{}\r\n", "x".repeat(997)).repeat(lines),
                format!("repeat('.'||repeat('x',997)||chr(13)||chr(10),{lines})"),
            )
        };
        let length = body.len();
        let chunks = length.div_ceil(65536);
        let mut hash = Provider.sha256().unwrap();
        hash.update(body.as_bytes()).unwrap();
        let digest = hash
            .finish()
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        sql(fixture,&format!("INSERT INTO td_mta.submissions SELECT account,{sub},email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,{count},NULL,0,NULL,changed FROM td_mta.submissions WHERE id={src}; INSERT INTO td_mta.submission_routes SELECT account,{sub},submitting_profile,route_name,destination FROM td_mta.submission_routes WHERE submission_id={src}; INSERT INTO td_mta.recipients SELECT account,{sub},i,i::text||'@example.test',1,0,NULL,0,NULL,0,send_at,NULL,NULL,0,'',changed FROM td_mta.submissions CROSS JOIN generate_series(0,{last}) i WHERE id={sub}; INSERT INTO td_mta.blob_ids SELECT account,id FROM td_mta.submissions WHERE id={sub}; INSERT INTO td_mta.blobs SELECT account,id,{length},decode('{digest}','hex'),0,changed,{chunks} FROM td_mta.submissions WHERE id={sub}; INSERT INTO td_mta.blob_chunks SELECT account,id,i,substring(convert_to({body_sql},'UTF8') from i*65536+1 for 65536) FROM td_mta.submissions CROSS JOIN generate_series(0,{chunks}-1) i WHERE id={sub}; UPDATE td_mta.submissions SET transmitted_blob_id=id WHERE id={sub}"));
        let reverse = sql(
            fixture,
            &format!("SELECT reverse_path FROM td_mta.submissions WHERE id={sub}"),
        );
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
        let cap = if n == 177 {
            Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap()
        } else {
            wire::until(&clock)
        };
        let mut database = Meter::new(
            cap,
            Charge {
                io_bytes: if n == 178 { 256 } else { 128 } * 1024 * 1024,
                records: match n {
                    174 => 131072,
                    178 => 1048576,
                    _ => 262144,
                },
                ..Default::default()
            },
        );
        let started = Instant::now();
        let prepared = worker
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
        let attempt = prepared.attempt();
        if n == 176 {
            sql(fixture, &format!("CREATE FUNCTION public.stop_driver_source() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE target integer; matches bigint; BEGIN IF NEW.phase=3 AND NEW.attempt_id=decode('{attempt}','hex') THEN SELECT min(pid),count(*) INTO target,matches FROM pg_catalog.pg_stat_activity WHERE usename='mail_runtime' AND application_name='td-mta-postgres' AND state='idle in transaction' AND query LIKE 'SELECT ordinal,bytes FROM td_mta.blob_chunks%'; IF matches<>1 THEN RAISE EXCEPTION 'expected one owned immutable source'; END IF; IF NOT pg_terminate_backend(target,1000) THEN RAISE EXCEPTION 'source termination failed'; END IF; END IF; RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER stop_driver_source AFTER INSERT ON td_mta.attempt_phase_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.stop_driver_source()"));
        }
        if n == 177 {
            sql(fixture, "CREATE FUNCTION public.delay_driver_final() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(3); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_driver_final AFTER INSERT ON td_mta.attempt_acceptance_receipts DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_driver_final()");
        }
        let mut work = wire::meter(cap);
        let (connected, mut peer) = setup.connect_with_work(
            prepared,
            clock.clone(),
            cap,
            &mut database,
            &mut work,
            false,
            None,
        );
        let connected = connected.ok().unwrap();
        let source_slots = ReadSlots::new(2).unwrap();
        let mut source_work = Meter::new(
            cap,
            Charge {
                io_bytes: 131 * 1024 * 1024,
                records: 32768,
                ..Default::default()
            },
        );
        let (mut input, mut line, mut reply, mut command, mut body_input, mut body_output) = (
            [0; 512], [0; 512], [0; 4096], [0; 512], [0; 4096], [0; 16384],
        );

        let (result, transferred) = thread::scope(|scope| {
            let server = scope.spawn(|| {
                script(
                    &mut peer,
                    &clock,
                    cap,
                    format!("MAIL FROM:<{reverse}>\r\n").as_bytes(),
                    b"250 sender\r\n",
                );
                script(
                    &mut peer,
                    &clock,
                    cap,
                    b"RCPT TO:<0@example.test>\r\n",
                    b"250 recipient\r\n",
                );
                script(
                    &mut peer,
                    &clock,
                    cap,
                    b"RCPT TO:<1@example.test>\r\n",
                    b"550 refused\r\n",
                );
                if n == 175 {
                    script(&mut peer, &clock, cap, b"RCPT TO:<2@example.test>\r\n", b"");
                    while peer.close().unwrap() == FlushProgress::Pending {
                        assert!(!cap.expired(clock.sample().unwrap().monotonic));
                        thread::sleep(Duration::from_millis(1));
                    }
                    return Vec::new();
                }
                script(&mut peer, &clock, cap, b"DATA\r\n", b"354 send\r\n");
                let mut received = Vec::new();
                loop {
                    assert!(!cap.expired(clock.sample().unwrap().monotonic));
                    let mut bytes = [0; 16384];
                    match peer.read(&mut bytes) {
                        Ok(IoProgress::Bytes(size)) => received.extend_from_slice(&bytes[..size]),
                        Ok(IoProgress::Closed) | Err(_) => break,
                        Ok(IoProgress::Pending) => {}
                    }
                    if received.ends_with(b"\r\n.\r\n") {
                        assert!(matches!(n, 173 | 177 | 178));
                        if n == 177 {
                            let answer_at =
                                clock.deadline_instant(cap).unwrap() - Duration::from_millis(1500);
                            while std::time::Instant::now() < answer_at {
                                thread::sleep(Duration::from_millis(1));
                            }
                        }
                        script(&mut peer, &clock, cap, b"", b"250 queued\r\n");
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                received
            });
            let result = attempt_driver::drive(
                connected,
                Body::SevenBit,
                attempt_driver::Scratch {
                    input: &mut input,
                    line: &mut line,
                    reply: &mut reply,
                    command: &mut command,
                    body_input: &mut body_input,
                    body_output: &mut body_output,
                },
                &mut work,
                &mut database,
                &mut source_work,
                &source_slots,
                td_mta::postgres_body::MAX_BODY_BYTES,
            );
            (result, server.join().unwrap())
        });
        if matches!(n, 173 | 178) {
            let done = result.unwrap_or_else(|stopped| {
                let error = match stopped.evidence {
                    attempt_driver::Evidence::Mail(value) => format!("MAIL {:?}", value.error),
                    attempt_driver::Evidence::Rcpt { error, .. } => format!("RCPT {error:?}"),
                    attempt_driver::Evidence::Data(value) => format!("DATA {:?}", value.error),
                    attempt_driver::Evidence::Body(value) => format!("Body {:?}", value.error),
                    attempt_driver::Evidence::Source(value) => format!("source {:?}", value.error),
                    attempt_driver::Evidence::Final(value) => format!("final {:?}", value.error),
                };
                panic!(
                    "native attempt {n} stopped after {:?}, {} body bytes: {error}",
                    started.elapsed(),
                    transferred.len()
                );
            });
            assert_eq!(done.stage, attempt_driver::Stage::Final);
            assert_eq!(done.outcome.attempt, attempt);
            assert_eq!(done.outcome.recipients, 1);
            drop(done.buffers.unwrap());
            let expected = if n == 178 {
                "..\r\n".repeat(lines)
            } else {
                format!("..{}\r\n", "x".repeat(997)).repeat(lines)
            } + ".\r\n";
            assert_eq!(transferred, expected.as_bytes());
            if n == 178 {
                assert!(started.elapsed() < Duration::from_secs(10));
                eprintln!("native source-sized relay: {length} source bytes, {} encoded bytes, {:?} elapsed", expected.len() - 3, started.elapsed());
            }
        } else if n == 177 {
            let stopped = result.err().unwrap();
            let attempt_driver::Evidence::Final(failure) = stopped.evidence else {
                panic!("expected unknown final publication");
            };
            assert_eq!(
                failure.error,
                td_mta::postgres_relay_finish::Error::Publication(dispatch::Error::Phase(
                    td_mta::postgres_attempt_phases::Error::Indeterminate
                ))
            );
            drop(failure.buffers.unwrap());
            let evidence = failure.retained.unwrap().into_evidence().unwrap();
            assert_eq!(pending.attempt().unwrap(), Some(attempt));
            // Dropping the owned trigger waits for its transaction to finish.
            sql(fixture,"DROP TRIGGER delay_driver_final ON td_mta.attempt_acceptance_receipts; DROP FUNCTION public.delay_driver_final()");
            let settlement_slots = ReadSlots::new(1).unwrap();
            let connection = dispatch::Connection {
                request: &runtime,
                trust,
                slots: &settlement_slots,
                clock: clock.clone(),
            };
            let until = wire::until(&clock);
            let mut settlement = wire::meter(until);
            let Some(dispatch::Settlement::Phase(outcome)) =
                pending.settle(&connection, until, &mut settlement).unwrap()
            else {
                panic!("missing final receipt")
            };
            assert_eq!(outcome.attempt, attempt);
            assert_eq!(
                worker
                    .publish_phase(evidence, until, &mut settlement)
                    .unwrap(),
                outcome
            );
            assert_eq!(
                transferred
                    .windows(5)
                    .filter(|v| *v == b"\r\n.\r\n")
                    .count(),
                1
            );
        } else if n == 175 {
            let stopped = result.err().unwrap();
            let attempt_driver::Evidence::Rcpt { closed, .. } = stopped.evidence else {
                panic!("expected RCPT interruption");
            };
            drop(closed.buffers.unwrap());
            let retained = closed.evidence.unwrap().into_interruption().unwrap();
            worker
                .publish_phase(retained.refusal.unwrap(), cap, &mut wire::meter(cap))
                .unwrap();
            worker
                .publish_recovery(retained.recovery.unwrap(), cap, &mut wire::meter(cap))
                .unwrap();
            assert!(transferred.is_empty());
        } else {
            let stopped = result.err().unwrap();
            let attempt_driver::Evidence::Source(failure) = stopped.evidence else {
                panic!("expected source refusal")
            };
            if n == 176 {
                assert!(matches!(
                    failure.error,
                    td_mta::postgres_relay_data::SourceError::Read(_)
                ));
                assert_eq!(sql(fixture, &format!("SELECT phase FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=0")), "3");
                sql(fixture,"DROP TRIGGER stop_driver_source ON td_mta.attempt_phase_receipts; DROP FUNCTION public.stop_driver_source()");
            } else {
                assert_eq!(
                    failure.error,
                    td_mta::postgres_relay_data::SourceError::Transfer(
                        td_mta::postgres_relay_body::Error::Publication(
                            dispatch::Error::Admission(td_mta::ports::Error::Capacity)
                        )
                    )
                );
            }
            drop(failure.buffers.unwrap());
            assert!(!transferred.windows(5).any(|v| v == b"\r\n.\r\n"));
            assert!(pending.attempt().unwrap().is_none());
            if n == 174 {
                assert!(transferred.is_empty());
                assert_eq!(sql(fixture, &format!("SELECT phase FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=0")), "2");
            }
            let value = stopped.interruption.prepare().ok().unwrap();
            worker
                .publish_recovery(value, cap, &mut wire::meter(cap))
                .unwrap();
        }
        if n == 176 {
            assert_eq!(sql(fixture, &format!("SELECT state,uncertain,phase FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=0")), "7|1|4");
        }
        if n == 174 {
            assert_eq!(sql(fixture, &format!("SELECT state,uncertain,phase FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=0")), "3|0|4");
        }
        if n == 175 {
            assert_eq!(sql(fixture, &format!("SELECT rcpt_reply FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=0")), "250 recipient");
            assert_eq!(sql(fixture, &format!("SELECT rcpt_reply IS NULL FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=2")), "t");
        }
        assert!(pending.attempt().unwrap().is_none());
        if matches!(n, 173 | 177 | 178) {
            assert_eq!(sql(fixture, &format!("SELECT state,phase,data_reply FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=0")), "4|4|250 queued");
        }
        assert_eq!(
            sql(
                fixture,
                &format!(
                    "SELECT state FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=1"
                )
            ),
            "5"
        );
        for pool in [&slots, &source_slots] {
            let checked = dispatch::Dispatches {
                connection: dispatch::Connection {
                    request: &runtime,
                    trust,
                    slots: pool,
                    clock: clock.clone(),
                },
                guard: worker.guard.clone(),
                pending: &pending,
            };
            let fresh = wire::until(&clock);
            let first = checked
                .reserve_final(attempt, fresh, &mut wire::meter(fresh))
                .unwrap();
            let second = checked
                .reserve_final(attempt, fresh, &mut wire::meter(fresh))
                .unwrap();
            drop((first, second));
        }

        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex') AND active")),"0");
        assert_eq!(sql(fixture,&format!("SELECT rcpt_reply FROM td_mta.recipients WHERE submission_id={sub} AND ordinal=1")),"550 refused");
        drop(peer.into_buffers().unwrap());
        sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}; DELETE FROM td_mta.blobs WHERE id={sub}; DELETE FROM td_mta.blob_ids WHERE id={sub}"));
    }
}
fn script(
    peer: &mut wire::Peer,
    clock: &RuntimeClock,
    cap: Deadline,
    expected: &[u8],
    reply: &[u8],
) {
    let mut received = Vec::new();
    let mut sent = 0;
    loop {
        assert!(!cap.expired(clock.sample().unwrap().monotonic));
        if received.len() < expected.len() {
            let mut bytes = [0; 512];
            let size = bytes.len().min(expected.len() - received.len());
            if let IoProgress::Bytes(n) = peer.read(&mut bytes[..size]).unwrap() {
                received.extend_from_slice(&bytes[..n]);
            }
            assert!(expected.starts_with(&received));
        } else {
            if sent < reply.len() {
                if let IoProgress::Bytes(n) = peer.write(&reply[sent..]).unwrap() {
                    sent += n;
                }
            }
            if peer.flush().unwrap() == FlushProgress::Complete && sent == reply.len() {
                return;
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
}
