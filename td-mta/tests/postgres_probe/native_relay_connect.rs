#![allow(
    clippy::result_large_err,
    reason = "Fixture retains real inline failure custody."
)]
use super::{deadline, relay_wire as wire, sql, Fixture, PASSWORD};
use std::{
    io::{Read, Write},
    net::{TcpListener, UdpSocket},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use td_crypto::{SystemEntropy, TrustStore};
use td_mta::{
    admission::work::Meter,
    clock::RuntimeClock,
    generations::GenerationSet,
    ids::{BootId, StoreEpoch, SubmissionId},
    ports::{
        self, Clock, Deadline, FlushProgress, Handshake, IoProgress, Time, TlsTransport, Transport,
    },
    postgres_configuration as configuration, postgres_configuration_store as versions,
    postgres_dispatch as dispatch,
    postgres_probe::{Mode, Request},
    postgres_read::ReadSlots,
    postgres_relay_connect as connect, postgres_route_owners as owners,
    postgres_runtime_profile::{self as runtime, PublishedProfile},
    postgres_selection as selection,
    queue_clock::Guard,
    smtp_relay_auth as auth,
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::TlsPolicies,
    transport::TcpTransport,
};
pub(super) struct Setup {
    profile: PublishedProfile,
    policies: wire::Policies,
    socket: TcpListener,
    dns: UdpSocket,
    starttls: bool,
}
impl Setup {
    pub(super) fn connect<'w, 'a, 'r>(
        &self,
        prepared: dispatch::PreparedResult<'w, 'a, 'r>,
        clock: Arc<RuntimeClock>,
        cap: Deadline,
        database: &mut Meter,
        refuse: bool,
        revoke: Option<&Fixture>,
    ) -> (
        Result<
            connect::ConnectedPrepared<'w, 'a, 'r, wire::Buffer>,
            connect::Refusal<'w, 'a, 'r, wire::Buffer>,
        >,
        wire::Peer,
    ) {
        thread::scope(|scope| {
            let dns = scope.spawn(|| {
                for _ in 0..2 {
                    let mut packet = [0; 512];
                    let (size, peer) = self.dns.recv_from(&mut packet).unwrap();
                    let mut answer = packet[..size].to_vec();
                    answer[2] |= 0x80;
                    if packet[size - 3] == 1 {
                        answer[7] = 1;
                        answer.extend_from_slice(&[
                            0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 127, 0, 0, 1,
                        ]);
                    }
                    self.dns.send_to(&answer, peer).unwrap();
                }
            });
            let server = scope.spawn(|| self.peer(clock.clone(), cap, refuse, revoke));
            let result = call(prepared, &self.profile, database, wire::meter(cap));
            dns.join().unwrap();
            (result, server.join().unwrap())
        })
    }
    fn peer(
        &self,
        clock: Arc<RuntimeClock>,
        cap: Deadline,
        refuse: bool,
        revoke: Option<&Fixture>,
    ) -> wire::Peer {
        let socket = loop {
            match self.socket.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(!cap.expired(clock.sample().unwrap().monotonic));
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("relay accept: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut socket = socket;
        if self.starttls {
            socket.write_all(b"220 ready\r\n").unwrap();
            plain_expect(&mut socket, b"EHLO localhost\r\n");
            socket
                .write_all(b"250-localhost\r\n250-STARTTLS\r\n250 AUTH LOGIN\r\n")
                .unwrap();
            plain_expect(&mut socket, b"STARTTLS\r\n");
            socket.write_all(b"220 upgrade\r\n").unwrap();
        }
        let pool = HandshakePool::new(1).unwrap();
        let mut peer = TlsPolicies::reserve_session(
            self.policies.clone(),
            TlsPolicies::listener(&self.policies, "smtp").unwrap(),
            &pool,
            Box::new([0; TLS_WIRE_BYTES]),
            Box::new([0; TLS_WIRE_BYTES]),
        )
        .unwrap()
        .construct()
        .unwrap()
        .handoff(
            TcpTransport::from_stream(socket).unwrap(),
            &[],
            clock.clone(),
            cap,
            cap,
        )
        .unwrap();
        while matches!(peer.handshake(cap).unwrap(), Handshake::Pending) {
            thread::sleep(Duration::from_millis(1));
        }
        if !self.starttls {
            script(&mut peer, &clock, cap, b"", b"220 ready\r\n");
        }
        script(&mut peer, &clock, cap, b"EHLO localhost\r\n", b"");
        if let Some(fixture) = revoke {
            sql(
                fixture,
                "UPDATE td_mta.route_owners SET expires_at=last_clock+1 WHERE owner_boot IS NOT NULL",
            );
            assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.route_owners WHERE owner_boot IS NOT NULL AND expires_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint"), "0");
        }
        script(
            &mut peer,
            &clock,
            cap,
            b"",
            b"250-localhost\r\n250 AUTH PLAIN\r\n",
        );
        if revoke.is_some() {
            return peer;
        }
        script(&mut peer, &clock, cap, b"AUTH PLAIN\r\n", b"334 \r\n");
        script(
            &mut peer,
            &clock,
            cap,
            b"AHJlbGF5LXVzZXIAZGF0YWJhc2UtcGFzc3dvcmQ=\r\n",
            if refuse {
                b"535 credentials refused\r\n"
            } else {
                b"235 authenticated\r\n"
            },
        );
        peer
    }
}
fn plain_expect(socket: &mut std::net::TcpStream, expected: &[u8]) {
    let mut received = vec![0; expected.len()];
    socket.read_exact(&mut received).unwrap();
    assert_eq!(received, expected);
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
            if let IoProgress::Bytes(n) = peer.read(&mut bytes).unwrap() {
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
fn call<'w, 'a, 'r>(
    prepared: dispatch::PreparedResult<'w, 'a, 'r>,
    profile: &PublishedProfile,
    database: &mut Meter,
    mut wire: Meter,
) -> Result<
    connect::ConnectedPrepared<'w, 'a, 'r, wire::Buffer>,
    connect::Refusal<'w, 'a, 'r, wire::Buffer>,
> {
    let (mut input, mut first, mut second, mut third, mut command) =
        ([0; 512], [0; 512], [0; 512], [0; 512], [0; 512]);
    let (mut plain, mut encoded, mut dns) = (
        [0; auth::PLAIN_BYTES],
        [0; auth::RESPONSE_BYTES],
        [0; td_mta::dns_transport::PACKET_BYTES],
    );
    let pool = HandshakePool::new(1).unwrap();
    let result = connect::connect(
        prepared,
        profile,
        &pool,
        Box::new([0; TLS_WIRE_BYTES]),
        Box::new([0; TLS_WIRE_BYTES]),
        connect::Scratch {
            input: &mut input,
            first: &mut first,
            second: &mut second,
            third: &mut third,
            command: &mut command,
            plain: &mut plain,
            encoded: &mut encoded,
            dns: &mut dns,
        },
        SystemEntropy::try_new().unwrap(),
        &mut wire,
        database,
    );
    assert_eq!(pool.available(), 1);
    result
}
pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    epoch: StoreEpoch,
    source: SubmissionId,
) {
    let (original, candidate) = selection::read(request, trust, deadline(), epoch)
        .unwrap()
        .unwrap();
    // Private fixture has no concurrent configuration writer. Restore exact
    // selection rows so the surrounding configuration oracle stays independent.
    sql(fixture, "CREATE TABLE public.connect_selection AS TABLE td_mta.config_selection; CREATE TABLE public.connect_receipts AS TABLE td_mta.config_selection_receipts");
    let source_route = sql(fixture, &format!("SELECT encode(destination,'hex') FROM td_mta.submission_routes WHERE submission_id=decode('{source}','hex')"));
    for starttls in [false, true] {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let dns = UdpSocket::bind("127.0.0.1:0").unwrap();
        dns.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let sources: Vec<_> = (0..candidate.profile_count())
            .map(|i| {
                let profile = candidate.profile(i).unwrap();
                String::from_utf8(profile.source.to_vec())
                    .unwrap()
                    .replace(
                        "port = 465",
                        &format!(
                            "port = {}{}",
                            socket.local_addr().unwrap().port(),
                            if starttls {
                                "\ntransport = \"required_starttls\""
                            } else {
                                ""
                            }
                        ),
                    )
                    .replace("127.0.0.1:53", &dns.local_addr().unwrap().to_string())
            })
            .collect();
        let profiles: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(i, source)| configuration::Profile {
                source: source.as_bytes(),
                ..candidate.profile(i).unwrap()
            })
            .collect();
        let new = configuration::prepare(
            request,
            trust,
            deadline(),
            epoch,
            candidate.shared(),
            &profiles,
        )
        .unwrap();
        let version = versions::Version::from_bytes([if starttls { 172 } else { 171 }; 16]);
        versions::publish(request, trust, deadline(), version, &new).unwrap();
        let selected = selection::read(request, trust, deadline(), epoch)
            .unwrap()
            .unwrap()
            .0;
        selection::publish(
            request,
            trust,
            deadline(),
            selection::prepare(epoch, selected.generation, version, &new).unwrap(),
        )
        .unwrap();
        let mut domain = GenerationSet::at_startup();
        let prepared = runtime::load(
            request,
            trust,
            deadline(),
            epoch,
            "cloud",
            domain.reserve().unwrap(),
        )
        .unwrap();
        let mut wrong = GenerationSet::at_startup();
        let refusal = prepared.publish(&mut wrong).err().unwrap();
        assert!(wrong.current().is_none());
        let (profile, retired) = refusal.into_prepared().publish(&mut domain).ok().unwrap();
        drop(retired);
        let policies = domain.current().unwrap();
        let setup = Setup {
            profile,
            policies,
            socket,
            dns,
            starttls,
        };
        let route = setup
            .profile
            .binding()
            .route()
            .destination()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        sql(fixture, &format!("UPDATE td_mta.submission_routes SET destination=decode('{route}','hex') WHERE submission_id=decode('{source}','hex')"));
        let claim = owners::prepare(
            setup.profile.binding(),
            BootId::from_bytes([if starttls { 172 } else { 171 }; 16]),
        )
        .unwrap();
        let grant = owners::acquire(request, trust, deadline(), &claim).unwrap();
        super::native_final_handoff::connected(
            fixture,
            request,
            trust,
            &claim,
            grant.generation,
            source,
            &setup.policies,
            &setup,
        );
        failures(
            fixture,
            request,
            trust,
            &claim,
            grant.generation,
            source,
            &setup,
        );
        owners::release(request, trust, deadline(), &claim, grant.generation).unwrap();
        sql(
            fixture,
            &format!("DELETE FROM td_mta.route_health WHERE destination=decode('{route}','hex')"),
        );
    }
    sql(fixture, &format!("UPDATE td_mta.submission_routes SET destination=decode('{source_route}','hex') WHERE submission_id=decode('{source}','hex'); DELETE FROM td_mta.config_selection_receipts; INSERT INTO td_mta.config_selection_receipts SELECT * FROM public.connect_receipts; DELETE FROM td_mta.config_selection; INSERT INTO td_mta.config_selection SELECT * FROM public.connect_selection; DROP TABLE public.connect_selection, public.connect_receipts"));
    assert_eq!(
        selection::read(request, trust, deadline(), epoch)
            .unwrap()
            .map(|(selected, _)| selected),
        Some(original)
    );
}
fn failures(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    claim: &owners::Claim,
    generation: u64,
    source: SubmissionId,
    setup: &Setup,
) {
    for n in [167, 168, 170, 171, 172, 169] {
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
        let failure = if n == 171 {
            blocked(fixture, || {
                let short = Deadline::after(clock.sample().unwrap().monotonic, 300).unwrap();
                let began = std::time::Instant::now();
                let failure = call(prepared, &setup.profile, &mut database, wire::meter(short))
                    .err()
                    .unwrap();
                assert!(
                    began.elapsed() < Duration::from_millis(1500),
                    "initial SQL ignored wire cap"
                );
                assert_eq!(failure.stage, connect::Stage::Admission);
                failure
            })
        } else if n == 172 {
            let (connected, mut peer) =
                setup.connect(prepared, clock.clone(), cap, &mut database, false, None);
            let connected = connected.ok().unwrap();
            let failure = blocked(fixture, || {
                let short = Deadline::after(clock.sample().unwrap().monotonic, 300).unwrap();
                let mut wire = wire::meter(short);
                let (mut input, mut line, mut reply, mut command) =
                    ([0; 512], [0; 512], [0; 4096], [0; 512]);
                let began = std::time::Instant::now();
                let failure = connected
                    .begin_mail(
                        td_mta::smtp_data_encode::Body::SevenBit,
                        td_mta::smtp_relay_command::Scratch {
                            input: &mut input,
                            line: &mut line,
                            reply: &mut reply,
                            command: &mut command,
                        },
                        &mut wire,
                        &mut database,
                    )
                    .err()
                    .unwrap();
                assert!(
                    began.elapsed() < Duration::from_millis(1500),
                    "MAIL SQL ignored wire cap"
                );
                assert!(failure.failure.retained.is_none());
                connect::Refusal {
                    stage: connect::Stage::Admission,
                    error: connect::Error::Protocol(ports::Error::Deadline),
                    closed: connect::Closed::Interrupted(failure.closed),
                    buffers: failure.failure.buffers,
                }
            });
            assert!(!matches!(
                peer.read(&mut [0; 512]),
                Ok(IoProgress::Bytes(_))
            ));
            drop(peer.into_buffers().unwrap());
            failure
        } else if n == 169 || n == 170 {
            if n == 170 {
                sql(
                    fixture,
                    "CREATE TABLE public.connect_owner AS TABLE td_mta.route_owners",
                );
            }
            let (result, mut peer) = setup.connect(
                prepared,
                clock.clone(),
                cap,
                &mut database,
                true,
                (n == 170).then_some(fixture),
            );
            let failure = result.err().unwrap();
            // EOF may require one final TLS pump after the client teardown.
            for _ in 0..1000 {
                match peer.read(&mut [0; 512]) {
                    Ok(IoProgress::Closed) | Err(_) => break,
                    Ok(IoProgress::Pending) => {}
                    Ok(IoProgress::Bytes(_)) => panic!("SMTP bytes after closed setup"),
                }
                thread::sleep(Duration::from_millis(1));
            }
            drop(peer.into_buffers().unwrap());
            assert_eq!(failure.stage, connect::Stage::Auth);
            if n == 170 {
                assert!(matches!(failure.error, connect::Error::Publication(_)));
                assert!(matches!(&failure.closed, connect::Closed::Interrupted(_)));
                sql(fixture, "DELETE FROM td_mta.route_owners; INSERT INTO td_mta.route_owners SELECT * FROM public.connect_owner; DROP TABLE public.connect_owner");
            }
            failure
        } else {
            let other_domain = GenerationSet::at_startup();
            let other = runtime::load(
                request,
                trust,
                deadline(),
                setup.profile.binding().epoch(),
                "cloud",
                other_domain.reserve().unwrap(),
            )
            .unwrap();
            let mut other_domain = other_domain;
            let (other, retired) = other.publish(&mut other_domain).ok().unwrap();
            drop(retired);
            let work = if n == 167 {
                Meter::new(cap, Default::default())
            } else {
                wire::meter(cap)
            };
            let failure = call(
                prepared,
                if n == 168 { &other } else { &setup.profile },
                &mut database,
                work,
            )
            .err()
            .unwrap();
            assert_eq!(
                failure.error,
                connect::Error::Protocol(if n == 167 {
                    ports::Error::Capacity
                } else {
                    ports::Error::Forbidden
                })
            );
            assert_eq!(
                setup.socket.accept().err().unwrap().kind(),
                std::io::ErrorKind::WouldBlock
            );
            setup.dns.set_nonblocking(true).unwrap();
            assert_eq!(
                setup.dns.recv_from(&mut [0; 512]).err().unwrap().kind(),
                std::io::ErrorKind::WouldBlock
            );
            setup.dns.set_nonblocking(false).unwrap();
            failure
        };
        drop(failure.buffers.unwrap());
        match failure.closed {
            connect::Closed::Interrupted(closed) => {
                let value = closed.prepare().ok().unwrap();
                worker
                    .publish_recovery(value, cap, &mut wire::meter(cap))
                    .unwrap();
            }
            connect::Closed::Auth(mut failed) => {
                let settlement = ReadSlots::new(1).unwrap();
                assert!(failed
                    .step(cap, &mut wire::meter(cap), &settlement)
                    .unwrap()
                    .is_none());
                assert!(failed
                    .step(cap, &mut wire::meter(cap), &settlement)
                    .unwrap()
                    .is_some());
            }
        }
        assert!(pending.attempt().unwrap().is_none());
        assert_eq!(sql(fixture,&format!("SELECT count(*) FROM td_mta.submission_attempts WHERE submission_id={sub} AND active")), "0");
        sql(fixture,&format!("DELETE FROM td_mta.recipients WHERE submission_id={sub}; DELETE FROM td_mta.submissions WHERE id={sub}"));
    }
}

fn blocked<T>(fixture: &Fixture, run: impl FnOnce() -> T) -> T {
    thread::scope(|scope| {
        let lock = scope.spawn(|| sql(fixture, "BEGIN; SELECT sequence FROM td_mta.accounts FOR UPDATE; SELECT pg_sleep(3) /* connector_short_cap */; ROLLBACK"));
        let mut ready = false;
        for _ in 0..100 {
            if sql(fixture, "SELECT count(*) FROM pg_stat_activity WHERE wait_event='PgSleep' AND query LIKE '%connector_short_cap%'") == "1" {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(ready, "fixture account lock not observed");
        let result = run();
        lock.join().unwrap();
        result
    })
}
