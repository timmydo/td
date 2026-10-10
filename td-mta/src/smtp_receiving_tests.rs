use super::*;
use crate::{
    admission::{
        logical::Cell, quota::Kind as QuotaKind, timers::NetworkLimits, DiskLimits, ViewMode,
        WorkLimits,
    },
    format::{
        key::Key,
        operation::Operation,
        row::{EmailOrigin, MailboxRow, ReceiptTls, Row},
        Sequence, Table,
    },
    ids::{AccountId, MailboxId, StoreEpoch},
    limits::Limits,
    ownership::SlotState,
    ports::{Deadline, FlushProgress, Handshake, IoProgress, ReadView, TlsTransport, Transport},
    smtp_receiving::{BoundListener, Control, Receiving, State},
    store_fs::{
        tests::Fixture, AuxiliaryUsage, CommitRequest, IndexStore, IngressSpool, StoreCoordinator,
    },
    transport::TcpTransport,
};
use std::{
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

const ACCOUNT: AccountId = AccountId::from_bytes([0xaa; 16]);
const INBOX: MailboxId = MailboxId::from_bytes([2; 16]);
struct ClockNow(
    Instant,
    AtomicBool,
    std::sync::atomic::AtomicU64,
    AtomicBool,
);
impl Clock for ClockNow {
    fn sample(&self) -> Result<Time, Error> {
        if self.1.load(Ordering::Relaxed) && std::thread::current().name() == Some("smtp-storage") {
            panic!("injected receiving worker failure");
        }
        if self.3.load(Ordering::Relaxed) && std::thread::current().name() == Some("smtp-test-main")
        {
            panic!("injected receiving main failure");
        }
        Ok(Time {
            utc_ms: 1_800_000_000_000,
            monotonic: Tick(
                u64::try_from(self.0.elapsed().as_millis()).unwrap()
                    + self.2.load(Ordering::Relaxed),
            ),
        })
    }
}
struct Stop<'a>(&'a Control);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}
fn cap() -> Deadline {
    Deadline::after(Tick(0), 1_000_000_000).unwrap()
}
fn wait(mut done: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < until, "runtime progress timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn send(wire: &mut impl Transport, bytes: &[u8]) {
    let mut used = 0;
    wait(|| {
        if used < bytes.len() {
            match wire.write(&bytes[used..]).unwrap() {
                IoProgress::Bytes(n) => used += n,
                IoProgress::Pending => (),
                IoProgress::Closed => panic!("closed while sending"),
            }
            false
        } else {
            wire.flush().unwrap() == FlushProgress::Complete
        }
    });
}
fn reply(wire: &mut impl Transport, code: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    let mut bytes = [0; 2048];
    wait(|| {
        match wire.read(&mut bytes).unwrap() {
            IoProgress::Bytes(n) => result.extend_from_slice(&bytes[..n]),
            IoProgress::Pending => (),
            IoProgress::Closed => panic!("closed before reply: {result:?}"),
        }
        result.ends_with(b"\r\n")
            && result
                .split(|b| *b == b'\n')
                .rev()
                .nth(1)
                .is_some_and(|line| line.starts_with(code) && line.get(3) == Some(&b' '))
    });
    assert!(result.starts_with(code), "unexpected reply {result:?}");
    result
}
fn command(wire: &mut impl Transport, bytes: &[u8], code: &[u8]) {
    send(wire, bytes);
    reply(wire, code);
}
fn deliver(wire: &mut impl Transport) {
    command(wire, b"EHLO sender.test\r\n", b"250");
    command(wire, b"MAIL FROM:<>\r\n", b"250");
    command(wire, b"RCPT TO:<main@example.test>\r\n", b"250");
    command(wire, b"DATA\r\n", b"354");
    command(wire, b"Subject: runtime\r\n\r\n..body\r\n.\r\n", b"250");
}

#[test]
fn receiving_workers_deliver_beside_idle_peer_and_enforce_peer_limit() {
    run(Case::Plain);
}
#[test]
fn receiving_workers_upgrade_real_tls_and_record_actual_peer_evidence() {
    run(Case::Tls);
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Case {
    Plain,
    Tls,
    BadTls,
    WorkerFailure,
    MainFailure,
    MissingInbox,
    BadPool,
    BadSettings,
    DuplicateListener,
    TotalLimit,
}
#[test]
fn receiving_failed_tls_closes_and_frees_its_slot_without_plaintext_fallback() {
    run(Case::BadTls);
}
#[test]
fn receiving_worker_failure_stops_admission_and_fails_health() {
    run(Case::WorkerFailure);
}
#[test]
fn receiving_main_unwind_joins_workers_and_cleans_live_delivery() {
    run(Case::MainFailure);
}
#[test]
fn receiving_startup_refuses_invalid_store_limits_and_bindings() {
    for case in [
        Case::MissingInbox,
        Case::BadPool,
        Case::BadSettings,
        Case::DuplicateListener,
        Case::TotalLimit,
    ] {
        run(case);
    }
}
fn run(case: Case) {
    let tls = case == Case::Tls;
    let _serial = serial();
    let material = Material::new();
    let listener = TcpListener::bind("127.0.0.2:0").unwrap();
    let address = listener.local_addr().unwrap();
    let source = source(false)
        .replace("127.0.0.1:25", &address.to_string())
        .replace("session_limit = 1", "session_limit = 3")
        .replace("per_peer_limit = 1", "per_peer_limit = 2");
    let policies = material.published(&source);
    let config = resolved(&source);
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool_fixture = Fixture::new();
    let mut spool_root = spool_fixture.locked();
    let clock = Arc::new(ClockNow(
        Instant::now(),
        AtomicBool::new(false),
        std::sync::atomic::AtomicU64::new(0),
        AtomicBool::new(false),
    ));
    let resources = Limits {
        message_bytes: if case == Case::BadSettings {
            512
        } else {
            32768
        },
        header_bytes: if case == Case::BadSettings { 256 } else { 8192 },
        smtp_sessions: if case == Case::TotalLimit { 2 } else { 8 },
        ..Limits::default()
    }
    .plan()
    .unwrap();
    let plan = DiskLimits::default()
        .plan(
            &resources,
            WorkLimits::default(),
            ViewMode::OnlineBackground,
        )
        .unwrap();
    let timeouts = NetworkLimits::default().plan(&plan).unwrap();
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([7; 16]),
        clock.clone(),
        2,
        cap(),
    )
    .unwrap();
    store.create_account(ACCOUNT, cap()).unwrap();
    let mut row = [0; 128];
    let n = Row::Mailbox(MailboxRow {
        name: "Inbox",
        parent: None,
        role: Some("inbox"),
        sort_order: 0,
        subscribed: true,
    })
    .encode(&mut row)
    .unwrap();
    if case != Case::MissingInbox {
        store
            .commit(
                &Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: store.epoch(),
                    expected: Sequence::default(),
                    utc_ms: 1_800_000_000_000,
                    deadline: cap(),
                },
                &[Operation::put(Table::Mailboxes, INBOX.as_bytes(), &row[..n]).unwrap()],
                &mut [],
            )
            .unwrap();
    }
    let spool = IngressSpool::open(&mut spool_root, &resources, clock.clone(), cap()).unwrap();
    let mut states = [const { SlotState::EMPTY }; 8];
    let mut cells = [const { Cell::EMPTY }; 8];
    let coordinator = StoreCoordinator::new(
        store,
        &plan,
        AuxiliaryUsage {
            sort_bytes: 0,
            response_bytes: 0,
            cache_bytes: 0,
            log_bytes: 0,
            cold_bytes: 0,
        },
        &mut states,
        &mut cells,
        (),
        cap(),
    )
    .unwrap();
    let pool = HandshakePool::new(if case == Case::BadPool {
        1
    } else {
        resources.limits().tls_handshakes
    })
    .unwrap();
    let mut negotiated = ReceiptTls::Plain;
    config
        .candidate()
        .with_graph(|records, graph| {
            let row = records
                .view(graph)
                .unwrap()
                .listeners()
                .unwrap()
                .listener(0)
                .unwrap()
                .unwrap();
            let mut wrong = row;
            wrong.per_peer_limit = Some(1);
            assert!(BoundListener::new(listener.try_clone().unwrap(), wrong, &policies).is_err());
            assert!(
                BoundListener::new(TcpListener::bind("127.0.0.1:0").unwrap(), row, &policies)
                    .is_err()
            );
            let mut listeners = Vec::new();
            if case == Case::DuplicateListener {
                listeners.push(
                    BoundListener::new(listener.try_clone().unwrap(), row, &policies).unwrap(),
                );
            }
            listeners.push(BoundListener::new(listener, row, &policies).unwrap());
            let receiving = Receiving {
                coordinator: &coordinator,
                spool: &spool,
                crypto: &Provider,
                routes: records.routes(),
                resources: &resources,
                admission: &plan,
                timeouts: &timeouts,
                policies: &policies,
                handshakes: &pool,
                listeners: &listeners,
                clock: clock.clone(),
            };
            let control = Control::default();
            if matches!(
                case,
                Case::MissingInbox
                    | Case::BadPool
                    | Case::BadSettings
                    | Case::DuplicateListener
                    | Case::TotalLimit
            ) {
                assert!(receiving.run(&control).is_err());
                assert_eq!(control.state(), State::Failed);
                return;
            }
            std::thread::scope(|scope| {
                let server = std::thread::Builder::new()
                    .name("smtp-test-main".into())
                    .spawn_scoped(scope, || {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            receiving.run(&control)
                        }))
                    })
                    .unwrap();
                let stop = Stop(&control);
                wait(|| control.state() != State::Starting);
                assert_eq!(control.state(), State::Ready);
                let mut idle =
                    TcpTransport::from_stream(TcpStream::connect(address).unwrap()).unwrap();
                reply(&mut idle, b"220");
                let mut active =
                    TcpTransport::from_stream(TcpStream::connect(address).unwrap()).unwrap();
                reply(&mut active, b"220");
                let mut excess =
                    TcpTransport::from_stream(TcpStream::connect(address).unwrap()).unwrap();
                reply(&mut excess, b"421");
                if matches!(case, Case::WorkerFailure | Case::MainFailure) {
                    command(&mut active, b"EHLO sender.test\r\n", b"250");
                    command(&mut active, b"MAIL FROM:<>\r\n", b"250");
                    command(&mut active, b"RCPT TO:<main@example.test>\r\n", b"250");
                    command(&mut active, b"DATA\r\n", b"354");
                    assert_eq!(spool.status().unwrap().occupied_slots, 1);
                    if case == Case::WorkerFailure {
                        clock.1.store(true, Ordering::Relaxed);
                        send(&mut active, b"Subject: failure\r\n");
                    } else {
                        clock.3.store(true, Ordering::Relaxed);
                    }
                    wait(|| control.state() == State::Failed);
                    let result = server.join().unwrap();
                    if case == Case::WorkerFailure {
                        assert!(result.unwrap().is_err());
                    } else {
                        assert!(result.is_err());
                    }
                    return;
                }
                if case == Case::BadTls {
                    command(&mut active, b"EHLO sender.test\r\n", b"250");
                    command(&mut active, b"STARTTLS\r\n", b"220");
                    send(&mut active, b"MAIL FROM:<>\r\n");
                    wait(|| match active.read(&mut [0; 1]) {
                        Ok(IoProgress::Closed) | Err(_) => true,
                        Ok(IoProgress::Pending) => false,
                        other => panic!("plaintext after failed TLS: {other:?}"),
                    });
                    active =
                        TcpTransport::from_stream(TcpStream::connect(address).unwrap()).unwrap();
                    reply(&mut active, b"220");
                }
                if tls {
                    command(&mut active, b"EHLO sender.test\r\n", b"250");
                    let held: Vec<_> = (0..pool.capacity())
                        .map(|_| pool.reserve().unwrap())
                        .collect();
                    send(&mut active, b"STARTTLS\r\n");
                    std::thread::sleep(Duration::from_millis(30));
                    assert_eq!(active.read(&mut [0; 256]).unwrap(), IoProgress::Pending);
                    assert_eq!(control.state(), State::Ready);
                    drop(held);
                    reply(&mut active, b"220");
                    let client_pool = HandshakePool::new(1).unwrap();
                    let (input, output) = buffers();
                    let prepared = TlsPolicies::reserve_session(
                        policies.clone(),
                        TlsPolicies::relay(&policies).unwrap(),
                        &client_pool,
                        input,
                        output,
                    )
                    .unwrap()
                    .construct()
                    .unwrap();
                    let mut active = prepared
                        .handoff(active, &[], clock.clone(), cap(), cap())
                        .unwrap();
                    wait(|| match active.handshake(cap()).unwrap() {
                        Handshake::Pending => false,
                        Handshake::Complete(info) => {
                            negotiated = match info.version {
                                crate::ports::TlsVersion::V12 => ReceiptTls::Tls12,
                                crate::ports::TlsVersion::V13 => ReceiptTls::Tls13,
                            };
                            true
                        }
                    });
                    command(&mut active, b"MAIL FROM:<>\r\n", b"503");
                    deliver(&mut active);
                    command(&mut active, b"QUIT\r\n", b"221");
                } else {
                    deliver(&mut active);
                    command(&mut active, b"QUIT\r\n", b"221");
                    active.abort();
                }
                if case == Case::Plain {
                    clock.2.store(300_001, Ordering::Relaxed);
                    reply(&mut idle, b"421");
                    idle.abort();
                    wait(|| {
                        let mut fresh =
                            TcpTransport::from_stream(TcpStream::connect(address).unwrap())
                                .unwrap();
                        let mut bytes = [0; 256];
                        let mut n = 0;
                        wait(|| match fresh.read(&mut bytes).unwrap() {
                            IoProgress::Bytes(count) => {
                                n = count;
                                true
                            }
                            IoProgress::Pending => false,
                            IoProgress::Closed => panic!("closed before retry banner"),
                        });
                        let admitted = bytes[..n].starts_with(b"220 ");
                        fresh.abort();
                        admitted
                    });
                    assert_eq!(control.state(), State::Ready);
                }
                drop(stop);
                assert!(server.join().unwrap().unwrap().is_ok());
                assert_eq!(control.state(), State::Stopped);
            });
        })
        .unwrap();
    assert_eq!(spool.status().unwrap().occupied_slots, 0);
    if matches!(case, Case::WorkerFailure | Case::MainFailure) {
        assert_eq!(coordinator.used(QuotaKind::BodyBytes).unwrap(), 0);
        assert_eq!(coordinator.used(QuotaKind::BlobCount).unwrap(), 0);
    }
    drop(coordinator);
    // Leases::new refuses backing cells still holding any pending record.
    let released = crate::admission::logical::Leases::new(
        &plan,
        crate::admission::quota::Usage::default(),
        &mut states,
        &mut cells,
    )
    .unwrap();
    assert_eq!(released.available_cells(), 8);
    drop(released);
    let store = IndexStore::open(&mut root, clock, 2, cap()).unwrap();
    store.validate_integrity(cap()).unwrap();
    let mut view = store.view(ACCOUNT, cap()).unwrap();
    let mut key = [0; 16];
    let mut scratch = [0; 65536];
    if !matches!(case, Case::Plain | Case::Tls | Case::BadTls) {
        assert!(view
            .next(Table::Emails, None, &mut key, &mut scratch)
            .unwrap()
            .is_none());
        return;
    }
    let record = view
        .next(Table::Emails, None, &mut key, &mut scratch)
        .unwrap()
        .unwrap();
    let Key::Email(id) = record.key else {
        panic!("email key");
    };
    let Row::Email(email) = record.row else {
        panic!("email row");
    };
    let blob = email.blob;
    let EmailOrigin::Smtp(receipt) = email.origin else {
        panic!("SMTP receipt");
    };
    assert_eq!(
        receipt.peer,
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_eq!(receipt.gateway, None);
    assert_eq!(receipt.tls, negotiated);
    assert!(view
        .next(Table::Emails, Some(id.as_bytes()), &mut key, &mut scratch)
        .unwrap()
        .is_none());
    assert!(view
        .get(Key::Membership(id, INBOX), &mut scratch)
        .unwrap()
        .is_some());
    let mut input = view.open_blob_input(&Provider, blob, 32768).unwrap();
    let mut raw = vec![0; 32768];
    let mut used = 0;
    loop {
        let n = input.read(&mut raw[used..]).unwrap();
        if n == 0 {
            break;
        }
        used += n;
    }
    input.finish().unwrap();
    assert!(raw[..used].ends_with(b"Subject: runtime\r\n\r\n.body\r\n"));
    if tls {
        let comment: &[u8] = match negotiated {
            ReceiptTls::Tls12 => b"with ESMTPS (TLSv1.2)",
            ReceiptTls::Tls13 => b"with ESMTPS (TLSv1.3)",
            _ => panic!("missing TLS evidence"),
        };
        assert!(raw[..used]
            .windows(comment.len())
            .any(|part| part == comment));
    }
    assert!(raw[..used].starts_with(b"Return-Path: <>\r\nReceived: from sender.test ([127.0.0.1])"));
}
