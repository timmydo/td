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
    postgres_relay_data as data,
    postgres_route_owners::{self as owners, Claim},
    queue_clock::Guard,
    smtp_relay_command::Scratch,
};
#[allow(clippy::too_many_arguments)]
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &Claim,
    generation: u64,
    source: SubmissionId,
    policies: &wire::Policies,
    setup: Option<&super::native_relay_connect::Setup>,
) {
    for n in 134..=142 {
        if setup.is_some() && n == 139 {
            continue;
        }
        let id = if setup.is_some() { n + 46 } else { n };
        let submission = SubmissionId::from_bytes([id; 16]);
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
        let mut database = if setup.is_some() {
            td_mta::admission::work::Meter::new(
                cap,
                td_mta::admission::work::Charge {
                    io_bytes: 128 * 1024 * 1024,
                    records: 262144,
                    ..Default::default()
                },
            )
        } else {
            wire::meter(cap)
        };
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
        let attempt = published.attempt();
        let (accepted, mut peer) = if let Some(setup) = setup {
            let sender = sql(
                fixture,
                &format!("SELECT reverse_path FROM td_mta.submissions WHERE id={sub}"),
            );
            let (connected, mut peer) =
                setup.connect(published, clock.clone(), cap, &mut database, false, None);
            let accepted = wire::accepted_configured(
                connected.ok().unwrap(),
                &sender,
                fixture,
                &mut peer,
                cap,
                &mut database,
            );
            (accepted, peer)
        } else {
            let published = published
                .admit_envelope(&mut database)
                .unwrap()
                .reserve_mail()
                .unwrap();
            let (session, mut peer) = wire::authenticated(fixture, clock.clone(), cap, policies);
            let accepted = wire::accepted_mail(published, session, &mut peer, cap, &mut database);
            (accepted, peer)
        };
        let backend = setup.map(|_| super::body_checks::backend(fixture));
        let (first, _) = wire::rcpt_command(
            wire::RcptStart::Mail(accepted),
            &mut peer,
            cap,
            &mut database,
            b"RCPT TO:<0@example.test>\r\n",
            b"250 first\r\n",
        );
        let cursor = match first {
            wire::RcptStep::Next(cursor) => cursor,
            _ => panic!("first RCPT did not continue"),
        };
        let (second, _) = wire::rcpt_command(
            wire::RcptStart::Cursor(cursor),
            &mut peer,
            cap,
            &mut database,
            b"RCPT TO:<1@example.test>\r\n",
            b"550 second\r\n",
        );
        let completed = match second {
            wire::RcptStep::Ready(done) => done,
            _ => panic!("mixed RCPT did not complete"),
        };
        if let Some(pid) = &backend {
            assert_eq!(super::body_checks::backend(fixture), *pid);
            assert_eq!(
                sql(
                    fixture,
                    &format!(
                        "SELECT state||'|'||query FROM pg_catalog.pg_stat_activity WHERE pid={pid}"
                    )
                ),
                "idle|ROLLBACK"
            );
            assert!(sql(fixture, &format!("BEGIN; SELECT pg_catalog.pg_try_advisory_xact_lock({}); SELECT id FROM td_mta.accounts FOR UPDATE NOWAIT; SELECT singleton FROM td_mta.config_selection FOR UPDATE NOWAIT; SELECT account FROM td_mta.route_owners FOR UPDATE NOWAIT; ROLLBACK", td_mta::postgres_schema::MIGRATION_KEY)).lines().any(|line| line == "t"));
        }
        let other_slot = setup.map(|_| worker.reserve_final(attempt, cap, &mut database).unwrap());
        let trigger = match n {
            136 => Some("attempt_rcpt_receipts"),
            137 => Some("attempt_phase_receipts"),
            138 => Some("attempt_command_receipts"),
            _ => None,
        };
        if let Some(table) = trigger {
            sql(fixture,&format!("CREATE OR REPLACE FUNCTION public.reject_bound_data() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'bound DATA refusal'; END $$; CREATE TRIGGER reject_bound_data BEFORE INSERT ON td_mta.{table} FOR EACH ROW EXECUTE FUNCTION public.reject_bound_data()"));
        }
        let held = if n == 139 {
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
        let mut exchange = data::begin(
            completed,
            Scratch {
                input: &mut input,
                line: &mut line,
                reply: &mut reply,
                command: &mut command,
            },
            &mut network,
            &mut database,
        )
        .ok()
        .unwrap();
        let response: &[u8] = match n {
            134 | 137 | 142 => b"354 continue\r\n",
            140 => b"250 invalid DATA\r\n",
            141 => b"421 service unavailable\r\n",
            _ => b"550 DATA refused\r\n",
        };
        let mut received = Vec::new();
        let mut sent = 0;
        let result = loop {
            let mut bytes = [0; 512];
            if let IoProgress::Bytes(count) = peer.read(&mut bytes).unwrap() {
                received.extend_from_slice(&bytes[..count]);
            }
            assert!(b"DATA\r\n".starts_with(&received));
            if received == b"DATA\r\n" {
                if sent < response.len() {
                    if let IoProgress::Bytes(count) = peer.write(&response[sent..]).unwrap() {
                        sent += count;
                    }
                }
                let _ = peer.flush().unwrap();
            }
            match exchange.advance() {
                Ok(data::Progress::Pending(next)) => exchange = next,
                Ok(data::Progress::Ready(done)) => break Ok(Some(done)),
                Ok(data::Progress::Rejected(done)) => {
                    assert!(
                        matches!(n, 135 | 141),
                        "unexpected durable rejection in case {n}"
                    );
                    assert_eq!(done.outcome.attempt, attempt);
                    drop(done.buffers.unwrap());
                    break Ok(None);
                }
                Err(failure) => break Err(failure),
            }
            std::thread::yield_now();
        };
        if n == 139 {
            assert!(received.is_empty());
        } else {
            assert_eq!(received, b"DATA\r\n");
            assert_eq!(sent, response.len());
        }
        drop(held);
        drop(other_slot);
        if let Some(pid) = &backend {
            super::body_checks::closed(fixture, pid);
            let first = worker
                .reserve_final(attempt, cap, &mut wire::meter(cap))
                .unwrap();
            let second = worker
                .reserve_final(attempt, cap, &mut wire::meter(cap))
                .unwrap();
            drop((first, second));
        }
        if let Some(table) = trigger {
            sql(
                fixture,
                &format!("DROP TRIGGER reject_bound_data ON td_mta.{table}"),
            );
        }
        let until = wire::until(&clock);
        let mut settlement = wire::meter(until);
        match result {
            Ok(Some(done)) => {
                assert!(matches!(n, 134 | 142));
                if n == 142 {
                    let first = worker.reserve_final(attempt, cap, &mut database).unwrap();
                    let second = worker.reserve_final(attempt, cap, &mut database).unwrap();
                    let failure = done.reserve_body(&mut settlement).err().unwrap();
                    assert_eq!(
                        failure.error,
                        td_mta::postgres_relay_body::Error::Publication(
                            dispatch::Error::Admission(td_mta::ports::Error::Busy)
                        )
                    );
                    drop(failure.buffers.unwrap());
                    drop((first, second));
                    let mut bytes = [0; 512];
                    assert!(!matches!(peer.read(&mut bytes), Ok(IoProgress::Bytes(_))));
                    assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.recipients WHERE submission_id={sub} AND state=2 AND phase=2")), "1");
                } else {
                    let ready = done.reserve_body(&mut settlement).ok().unwrap();
                    assert_eq!(ready.dispatch().attempt(), attempt);
                    ready.check_current(&mut settlement).unwrap();
                    drop(ready.into_buffers().unwrap());
                }
                let recovery = recovery::interrupted_with_body_rcpt(
                    claim,
                    generation,
                    attempt,
                    &[(0, "250 first")],
                )
                .unwrap();
                worker
                    .publish_recovery(recovery, until, &mut settlement)
                    .unwrap();
            }
            Ok(None) => {}
            Err(failure) => {
                drop(failure.buffers.unwrap());
                let expected = match n {
                    136..=138 => {
                        data::Error::Publication(dispatch::Error::Phase(phases::Error::Database))
                    }
                    139 => data::Error::Publication(dispatch::Error::Admission(
                        td_mta::ports::Error::Busy,
                    )),
                    140 => data::Error::Protocol(td_mta::ports::Error::Invalid),
                    _ => panic!("unexpected DATA failure"),
                };
                assert_eq!(failure.error, expected);
                match failure.retained {
                    data::Retained::Command(retained) => {
                        assert!(matches!(n, 136 | 138));
                        assert_eq!(retained.attempt(), Some(attempt));
                        retained.retry(&mut settlement).unwrap();
                    }
                    data::Retained::Body(retained) => {
                        assert!(matches!(n, 137 | 139 | 140));
                        assert_eq!(retained.attempt(), attempt);
                        let interrupted = retained.into_interruption().unwrap();
                        assert_eq!(interrupted.refusal.is_some(), n != 137);
                        if let Some(refusal) = interrupted.refusal {
                            worker
                                .publish_phase(refusal, until, &mut settlement)
                                .unwrap();
                        }
                        worker
                            .publish_recovery(interrupted.recovery.unwrap(), until, &mut settlement)
                            .unwrap();
                    }
                    data::Retained::Planning(_) => panic!("unexpected DATA planning failure"),
                }
            }
        }
        drop(peer.into_buffers().unwrap());
        let expected = match n {
            135 | 136 | 138 => "0|5|250 first|550 DATA refused\n1|5|550 second|",
            141 => "0|3|250 first|421 service unavailable\n1|5|550 second|",
            _ => "0|3|250 first|\n1|5|550 second|",
        };
        assert_eq!(sql(fixture,&format!("SELECT ordinal,state,rcpt_reply,coalesce(data_reply,'') FROM td_mta.recipients WHERE submission_id={sub} ORDER BY ordinal")),expected);
        assert_eq!(pending.attempt().unwrap(), None);
        assert_eq!(sql(fixture,&format!("SELECT active FROM td_mta.submission_attempts WHERE attempt_id=decode('{attempt}','hex')")),"f");
        sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    }
}
