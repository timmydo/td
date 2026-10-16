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
    for (n, refuse) in [(95, false), (96, true)] {
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
        let job_limit = clock.deadline_instant(cap).unwrap();
        let mut database = wire::meter(cap);
        let published = worker
            .publish(
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
        let envelope = attempts::envelope(&runtime, trust, job_limit, &published.dispatch).unwrap();
        assert_eq!(envelope.recipients.len(), 1);
        let (session, mut peer) = wire::authenticated(fixture, clock.clone(), cap);
        let (session, mail) = wire::command(
            session,
            &mut peer,
            clock.clone(),
            cap,
            command::Command::MailFrom(&envelope.reverse_path),
            format!("MAIL FROM:<{}>\r\n", envelope.reverse_path).as_bytes(),
            b"250 sender\r\n",
        );
        assert_eq!(mail, "250 sender");
        let recipient = &envelope.recipients[0];
        let (session, rcpt) = wire::command(
            session,
            &mut peer,
            clock.clone(),
            cap,
            command::Command::RcptTo(&recipient.address),
            format!("RCPT TO:<{}>\r\n", recipient.address).as_bytes(),
            b"250 recipient\r\n",
        );
        let (session, data) = wire::command(
            session,
            &mut peer,
            clock.clone(),
            cap,
            command::Command::Data,
            b"DATA\r\n",
            b"354 send body\r\n",
        );
        assert_eq!(data, "354 send body");
        let body_owner = worker
            .publish_body(
                claim,
                generation,
                published.dispatch.attempt(),
                &[(recipient.ordinal, &rcpt)],
                cap,
                &mut database,
            )
            .unwrap();
        assert_eq!(body_owner.attempt(), published.dispatch.attempt());
        body_owner.check_current(&mut database).unwrap();
        let plan = body_owner.reserve_final(&mut database).unwrap();
        assert_eq!(plan.attempt(), published.dispatch.attempt());
        let occupied = worker
            .reserve_final(published.dispatch.attempt(), cap, &mut database)
            .unwrap();
        assert_eq!(
            plan.check_current(&mut database),
            Err(dispatch::Error::Admission(td_mta::ports::Error::Busy))
        );
        drop(occupied);
        plan.check_current(&mut database).unwrap();
        let mut transferred = Vec::new();
        let handoff = attempts::with_body(
            &runtime,
            trust,
            job_limit,
            &published.dispatch,
            postgres_body::MAX_BODY_BYTES,
            |metadata, source| {
                assert_eq!(metadata.attempt, published.dispatch.attempt());
                let mut input = [0; body::INPUT_BYTES];
                let mut output = [0; body::OUTPUT_BYTES];
                let mut work = wire::meter(cap);
                let mut exchange = transfer::begin(
                    plan,
                    session,
                    source,
                    Body::SevenBit,
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
                job_limit,
                &phases::body(
                    claim,
                    generation,
                    published.dispatch.attempt(),
                    &[(recipient.ordinal, &rcpt)]
                )
                .unwrap(),
                clock.clone(),
                worker.guard.clone()
            ),
            Err(phases::Error::Conflict)
        );
        if refuse {
            sql(fixture,"CREATE FUNCTION public.reject_wire_final() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'wire fixture refusal'; END $$; CREATE TRIGGER reject_wire_final BEFORE INSERT ON td_mta.attempt_acceptance_receipts FOR EACH ROW EXECUTE FUNCTION public.reject_wire_final()");
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
                    assert_eq!(retained.attempt(), Some(published.dispatch.attempt()));
                    sql(
                        fixture,
                        "DROP TRIGGER reject_wire_final ON td_mta.attempt_acceptance_receipts",
                    );
                    break retained.retry(&mut database).unwrap();
                }
            }
            std::thread::yield_now();
        };
        assert_eq!(written, response.len());
        assert_eq!(
            transferred
                .windows(5)
                .filter(|v| *v == b"\r\n.\r\n")
                .count(),
            1
        );
        let final_value = phases::accepted(
            claim,
            generation,
            published.dispatch.attempt(),
            &[recipient.ordinal],
            "250 queued",
        )
        .unwrap();
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
