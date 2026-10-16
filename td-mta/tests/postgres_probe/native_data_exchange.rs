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
    for n in 134..=141 {
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
            134 | 137 => b"354 continue\r\n",
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
                assert_eq!(n, 134);
                let (body, dispatch, session, mode) = done.into_parts();
                assert_eq!(mode, Body::SevenBit);
                assert_eq!(body.attempt(), attempt);
                assert_eq!(dispatch.attempt(), attempt);
                body.check_current(&mut settlement).unwrap();
                drop(body.reserve_final(&mut settlement).unwrap());
                drop(session.into_buffers().unwrap());
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
