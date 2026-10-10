#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{DiskLimits, ViewMode, WorkLimits},
    config::{
        routing::{AliasSlot, Builder, DomainSlot, Routing},
        syntax::Location,
    },
    format::row::MailboxRow,
    limits::Limits,
    smtp_session::{Pending, Settings},
    store_fs::tests::Fixture,
};
use std::{
    num::NonZeroU32,
    sync::{
        atomic::{AtomicU64, Ordering},
        MutexGuard,
    },
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const INBOX: MailboxId = MailboxId::from_bytes([2; 16]);
const MAXIMUM: usize = 32768;
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
struct DevicePolicy;
struct DeviceGuard(Access);
impl UploadGuard for DeviceGuard {
    fn access(&self) -> Access {
        self.0
    }
}
impl UploadAuthorization for DevicePolicy {
    type Guard<'a> = DeviceGuard;
    fn authorize(
        &self,
        account: AccountId,
        device: DeviceId,
        _: Deadline,
    ) -> Result<DeviceGuard, ports::Error> {
        if account != ACCOUNT || device != DeviceId::from_bytes([3; 16]) {
            return Err(ports::Error::Forbidden);
        }
        Ok(DeviceGuard(Access {
            account,
            principal: Principal::Device(device),
            config_generation: 1,
        }))
    }
}

struct Policy(Arc<Mutex<bool>>, IpAddr);
struct Guard<'a>(MutexGuard<'a, bool>, IpAddr);
impl DeliveryGuard for Guard<'_> {
    fn access(&self) -> Access {
        assert!(*self.0);
        Access {
            account: ACCOUNT,
            principal: Principal::Smtp,
            config_generation: 1,
        }
    }
    fn peer(&self) -> DeliveryPeer<'_> {
        DeliveryPeer {
            peer: self.1,
            tls: ReceiptTls::Plain,
            gateway: None,
        }
    }
}
impl DeliveryAuthorization for Policy {
    type Guard<'a> = Guard<'a>;
    fn authorize(&self, account: AccountId, _: Deadline) -> Result<Guard<'_>, ports::Error> {
        let state = self.0.try_lock().map_err(|_| ports::Error::Busy)?;
        if !*state || account != ACCOUNT {
            return Err(ports::Error::Forbidden);
        }
        Ok(Guard(state, self.1))
    }
}
struct Random(u8);
impl Entropy for Random {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
        self.0 = self.0.checked_add(1).ok_or(td_crypto::Error::Entropy)?;
        bytes.fill(self.0);
        Ok(())
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn request() -> DeliveryRequest {
    DeliveryRequest {
        header_bytes: 8192,
        deadline: deadline(),
    }
}
struct Expected {
    result: DeliveryCompletion,
    tail: String,
}
fn fixture(
    run: impl for<'r, 'a, 's> FnOnce(
        &mut StoreCoordinator<'r, 'a, DevicePolicy>,
        &'s IngressSpool<'r>,
        &Policy,
        &Arc<Timer>,
        &Routing<'_>,
    ) -> Vec<Expected>,
) {
    fixture_with_limits(MAXIMUM, 8192, run)
}
fn fixture_with_limits(
    maximum: usize,
    headers: usize,
    run: impl for<'r, 'a, 's> FnOnce(
        &mut StoreCoordinator<'r, 'a, DevicePolicy>,
        &'s IngressSpool<'r>,
        &Policy,
        &Arc<Timer>,
        &Routing<'_>,
    ) -> Vec<Expected>,
) {
    fixture_with_peer(maximum, headers, "192.0.2.7".parse().unwrap(), run)
}
fn fixture_with_peer(
    maximum: usize,
    headers: usize,
    peer: IpAddr,
    run: impl for<'r, 'a, 's> FnOnce(
        &mut StoreCoordinator<'r, 'a, DevicePolicy>,
        &'s IngressSpool<'r>,
        &Policy,
        &Arc<Timer>,
        &Routing<'_>,
    ) -> Vec<Expected>,
) {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool_fixture = Fixture::new();
    let mut spool_root = spool_fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let resources = Limits {
        message_bytes: maximum,
        header_bytes: headers,
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
    let store = IndexStore::create(
        &mut root,
        StoreEpoch::from_bytes([7; 16]),
        clock.clone(),
        2,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
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
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: deadline(),
            },
            &[Operation::put(Table::Mailboxes, INBOX.as_bytes(), &row[..n]).unwrap()],
            &mut [],
        )
        .unwrap();
    let spool = IngressSpool::open(&mut spool_root, &resources, clock.clone(), deadline()).unwrap();
    let mut states = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { LeaseCell::EMPTY }; 4];
    let mut coordinator = StoreCoordinator::new(
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
        DevicePolicy,
        deadline(),
    )
    .unwrap();
    let policy = Policy(Arc::new(Mutex::new(true)), peer);
    let mut text = [0; 1024];
    let mut domains = [DomainSlot::EMPTY; 1];
    let mut aliases = [AliasSlot::EMPTY; 2];
    let at = Location {
        line: NonZeroU32::new(1).unwrap(),
        column: 1,
    };
    let mut routes = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
    routes.account(ACCOUNT, at).unwrap();
    routes.domain("example.test", at).unwrap();
    routes.alias("Alice@example.test", ACCOUNT, at).unwrap();
    routes.alias("alias@example.test", ACCOUNT, at).unwrap();
    let expected = run(
        &mut coordinator,
        &spool,
        &policy,
        &clock,
        &routes.finish().unwrap(),
    );
    drop(coordinator);
    clock.0.store(1, Ordering::Relaxed);
    let store = IndexStore::open(&mut root, clock, 2, deadline()).unwrap();
    store.validate_integrity(deadline()).unwrap();
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    let mut scratch = [0; 65536];
    for expected in &expected {
        assert!(expected.result.outcome.is_ok());
        let result = &expected.result;
        let Some((Row::Email(email), _)) =
            view.get(Key::Email(result.email), &mut scratch).unwrap()
        else {
            panic!("recovered email");
        };
        assert_eq!(email.blob, result.blob);
        assert_eq!(email.thread, result.thread);
        assert_eq!(email.received_at, 0);
        let EmailOrigin::Smtp(receipt) = email.origin else {
            panic!("receipt")
        };
        assert_eq!(receipt.peer, peer);
        assert_eq!(receipt.reverse_path, "");
        assert_eq!(
            receipt
                .recipients
                .iter()
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            ["Alice@example.test", "alias@example.test"]
        );
        assert_eq!(
            view.get(Key::Membership(result.email, INBOX), &mut scratch)
                .unwrap()
                .unwrap()
                .0,
            Row::Membership
        );
        let mut body = view
            .open_blob_input(&td_crypto::Provider, result.blob, maximum as u64)
            .unwrap();
        let mut bytes = vec![0; maximum];
        let mut used = 0;
        loop {
            let n = body.read(&mut bytes[used..]).unwrap();
            if n == 0 {
                break;
            }
            used += n;
        }
        body.finish().unwrap();
        let prefix = format!("Return-Path: <>\r\nReceived: from sender.test ([{peer}])\r\n\tby mx.example.test with ESMTP id {};\r\n\t1 Jan 1970 00:00:00 +0000\r\n", result.email);
        assert_eq!(
            &bytes[..used],
            format!("{prefix}{}", expected.tail).as_bytes()
        );
    }
    let mut key = [0; 16];
    let mut after = None;
    let mut count = 0;
    while let Some(record) = view
        .next(
            Table::Emails,
            after.as_ref().map(|id: &EmailId| id.as_bytes().as_slice()),
            &mut key,
            &mut scratch,
        )
        .unwrap()
    {
        let Key::Email(email) = record.key else {
            panic!("email key")
        };
        after = Some(email);
        count += 1;
    }
    assert_eq!(count, expected.len());
    assert_eq!(spool.status().unwrap().occupied_slots, 0);
}
fn command(session: &mut Session<'_>, bytes: &[u8], code: &[u8]) {
    assert_eq!(session.feed(bytes).unwrap(), bytes.len());
    let Pending::Reply {
        bytes,
        close: false,
    } = session.pending()
    else {
        panic!("reply")
    };
    assert!(bytes.starts_with(code), "{bytes:?}");
    session.reply_sent().unwrap();
}
fn session<'a>(routes: &'a Routing<'a>) -> Session<'a> {
    session_with_limit(routes, MAXIMUM)
}
fn session_with_limit<'a>(routes: &'a Routing<'a>, maximum: usize) -> Session<'a> {
    let mut session = Session::new(
        routes,
        Settings {
            hostname: "mx.example.test",
            message_bytes: maximum,
            trace_bytes: smtp_trace_allowance("mx.example.test").unwrap(),
            recipients: 100,
            starttls: false,
        },
    )
    .unwrap();
    session.reply_sent().unwrap();
    command(&mut session, b"EHLO sender.test\r\n", b"250");
    command(&mut session, b"MAIL FROM:<> BODY=8BITMIME\r\n", b"250");
    command(&mut session, b"RCPT TO:<Alice@example.test>\r\n", b"250");
    command(&mut session, b"RCPT TO:<alias@example.test>\r\n", b"250");
    assert_eq!(session.feed(b"DATA\r\n").unwrap(), 6);
    session
}
fn deliver<'r, 'a>(
    coordinator: &StoreCoordinator<'r, 'a, DevicePolicy>,
    spool: &IngressSpool<'r>,
    policy: &Policy,
    routes: &Routing<'_>,
    random: &mut Random,
    source: &[u8],
) -> DeliveryCompletion {
    let mut session = session(routes);
    let request = request();
    let mut delivery = retry_reservation(|| {
        coordinator.reserve_delivery(
            &td_crypto::Provider,
            random,
            spool,
            policy,
            &session,
            request,
        )
    })
    .unwrap();
    session.data_ready(Ok(())).unwrap();
    session.reply_sent().unwrap();
    // Actual wire data is deliberately fragmented down to individual octets.
    for byte in source {
        assert_eq!(session.feed(&[*byte]).unwrap(), 1);
        if let Pending::Data(line) = session.pending() {
            delivery.write(line).unwrap();
            session.data_written(Ok(())).unwrap();
        }
    }
    assert_eq!(session.feed(b".\r\n").unwrap(), 3);
    assert_eq!(session.pending(), Pending::Commit);
    delivery.prepare().unwrap();
    let completion = delivery.commit().unwrap();
    session.committed(completion.outcome).unwrap();
    assert!(
        matches!(session.pending(), Pending::Reply { bytes, close: false } if bytes.starts_with(b"250 "))
    );
    completion
}
#[test]
fn accepted_transcript_reopens_one_email_with_truthful_trace_and_alias_receipt() {
    fixture(|coordinator, spool, policy, _, routes| {
        let source = b"Return-Path: <forged@example.test>\r\n\tcontinued\r\nSubject: retained\r\nreturn-PATH : <>\r\nMessage-ID: <a@example.test>\r\n\r\n..dot\r\nbody\r\n";
        let result = deliver(coordinator, spool, policy, routes, &mut Random(10), source);
        assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 1);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 0);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.thread_anchor("a@example.test", &mut [0; 65536])
                .unwrap(),
            Some((result.email, result.thread))
        );
        let mut cursor = ChangeCursor {
            sequence: Sequence::default(),
            operation: u32::MAX,
        };
        for kind in [ObjectType::Email, ObjectType::Thread, ObjectType::Mailbox] {
            let ChangeStep::Record(record) = view.next_change(cursor, kind).unwrap() else {
                panic!("history")
            };
            assert_eq!(record.cursor.sequence, result.outcome.unwrap().sequence);
            cursor.sequence = Sequence::default();
        }
        vec![Expected {
            result,
            tail: "Subject: retained\r\nMessage-ID: <a@example.test>\r\n\r\n.dot\r\nbody\r\n"
                .into(),
        }]
    });
}
#[test]
fn malformed_header_boundary_and_header_only_eof_preserve_the_raw_tail() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut random = Random(10);
        [
            "Subject: header only\r\n",
            "\t(attacker trace text)\r\nSubject: still body\r\n\r\nbody\r\n",
            " orphan continuation\r\nMessage-ID: <not-an-anchor@x>\r\n",
            "Subject: first\r\nnot a header\r\nReturn-Path: body text\r\n",
            "",
        ]
        .into_iter()
        .map(|tail| {
            let result = deliver(
                coordinator,
                spool,
                policy,
                routes,
                &mut random,
                tail.as_bytes(),
            );
            Expected {
                result,
                tail: if tail.starts_with([' ', '\t']) {
                    format!("\r\n{tail}")
                } else {
                    tail.into()
                },
            }
        })
        .collect()
    });
}

#[test]
fn threading_uses_complete_lists_precedence_and_exact_ids_without_late_merges() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut random = Random(10);
        let mut results = Vec::new();
        for source in [
            "Message-ID: <Root@Example>\r\n\r\na\r\n",
            "Message-ID: <Other@Example>\r\n\r\nb\r\n",
            "References: <Root@Example> <broken\r\nIn-Reply-To: old phrase <Other@Example>\r\nMessage-ID: <third@x>\r\n\r\nc\r\n",
            "References: <Other@Example>\r\nReferences: old <Root@Example> newest <missing@x>\r\nIn-Reply-To: <Other@Example>\r\nMessage-ID: <fourth@x>\r\n\r\nd\r\n",
            "Message-ID: <Root@Example>\r\n\r\nduplicate\r\n",
            "References: <root@example>\r\nMessage-ID: <sixth@x>\r\n\r\ncase\r\n",
            "Message-ID: <é@x>\r\n\r\nunicode\r\n",
            "References: <é@x>\r\nMessage-ID: <eight@x>\r\n\r\nreply\r\n",
        ] {
            let result = deliver(coordinator, spool, policy, routes, &mut random, source.as_bytes());
            results.push(Expected { result, tail: source.into() });
        }
        assert_ne!(results[0].result.thread, results[1].result.thread);
        assert_eq!(results[2].result.thread, results[1].result.thread);
        assert_eq!(results[3].result.thread, results[0].result.thread);
        assert_eq!(results[4].result.thread, results[0].result.thread);
        assert_ne!(results[5].result.thread, results[0].result.thread);
        assert_eq!(results[7].result.thread, results[6].result.thread);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.thread_anchor("Root@Example", &mut [0; 65536]).unwrap(),
            Some((results[0].result.email, results[0].result.thread))
        );
        results
    });
}
#[test]
fn oversize_identifiers_occupy_lookback_and_block_own_id_fallback() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut random = Random(10);
        let base = "Message-ID: <root@x>\r\n\r\nbase\r\n";
        let first = deliver(
            coordinator,
            spool,
            policy,
            routes,
            &mut random,
            base.as_bytes(),
        );
        let oversized = format!("<\"{}\r\n {}\"@x>", "a".repeat(970), "b".repeat(60));
        let mut references = String::from("References: <root@x>\r\n");
        for n in 0..31 {
            references.push_str(&format!(" <{n}@missing>\r\n"));
        }
        references.push_str(&format!(" {oversized}\r\n\r\nlookback\r\n"));
        let second = deliver(
            coordinator,
            spool,
            policy,
            routes,
            &mut random,
            references.as_bytes(),
        );
        let own = format!("Message-ID:\r\n {oversized}\r\nMessage-ID: <root@x>\r\n\r\nown\r\n");
        let third = deliver(
            coordinator,
            spool,
            policy,
            routes,
            &mut random,
            own.as_bytes(),
        );
        assert_ne!(first.thread, second.thread);
        assert_ne!(first.thread, third.thread);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        let mut key = [0; 1024];
        let mut value = [0; 65536];
        let row = view
            .next(Table::ThreadAnchors, None, &mut key, &mut value)
            .unwrap()
            .unwrap();
        assert_eq!(row.key, Key::ThreadAnchor("root@x", first.email));
        let after = key.to_vec();
        let length = Key::ThreadAnchor("root@x", first.email)
            .encode(&mut key)
            .unwrap();
        assert!(view
            .next(
                Table::ThreadAnchors,
                Some(&after[..length]),
                &mut key,
                &mut value
            )
            .unwrap()
            .is_none());
        vec![
            Expected {
                result: first,
                tail: base.into(),
            },
            Expected {
                result: second,
                tail: references,
            },
            Expected {
                result: third,
                tail: own,
            },
        ]
    });
}
#[test]
fn header_limit_is_permanent_and_work_exhaustion_is_temporary() {
    for resource in [false, true] {
        fixture(|coordinator, spool, policy, _, routes| {
            let mut session = session(routes);
            let mut delivery = coordinator
                .reserve_delivery(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    policy,
                    &session,
                    DeliveryRequest {
                        header_bytes: 512,
                        ..request()
                    },
                )
                .unwrap();
            if resource {
                delivery.meter = Meter::new(deadline(), WorkCharge::default());
            }
            session.data_ready(Ok(())).unwrap();
            session.reply_sent().unwrap();
            let line = format!("X: {}\r\n", "a".repeat(600));
            assert_eq!(session.feed(line.as_bytes()).unwrap(), line.len());
            let Pending::Data(line) = session.pending() else {
                panic!("data")
            };
            let error = delivery.write(line).unwrap_err();
            if resource {
                assert!(matches!(
                    error,
                    DeliveryError::Storage(UploadError::Store(ports::Error::Capacity))
                ));
                session.data_written(Err(ports::Error::Capacity)).unwrap();
            } else {
                assert!(matches!(error, DeliveryError::HeaderLimit));
                session.message_too_large().unwrap();
            }
            assert!(
                matches!(session.pending(), Pending::Reply { bytes, close:true } if bytes.starts_with(if resource { b"451" } else { b"552" }))
            );
            assert!(delivery.prepare().is_err());
            delivery.discard().unwrap();
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                0
            );
            Vec::new()
        });
    }
}
#[test]
fn quotas_refuse_before_354_and_revocation_refuses_before_commit() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut session = session(routes);
        let remaining = coordinator
            .state
            .lock()
            .unwrap()
            .ledger
            .remaining_capacity(Kind::BodyBytes)
            .unwrap();
        let held = coordinator
            .state
            .lock()
            .unwrap()
            .ledger
            .reserve(
                &[[
                    Charge {
                        kind: Kind::BodyBytes,
                        amount: remaining,
                    },
                    Charge::ZERO,
                    Charge::ZERO,
                    Charge::ZERO,
                ]],
                deadline(),
                Tick(1),
            )
            .unwrap();
        assert!(coordinator
            .reserve_delivery(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                policy,
                &session,
                request()
            )
            .is_err());
        assert!(matches!(session.pending(), Pending::BeginData { .. }));
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        coordinator
            .state
            .lock()
            .unwrap()
            .ledger
            .cancel(held)
            .unwrap();
        let mut delivery = coordinator
            .reserve_delivery(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                policy,
                &session,
                request(),
            )
            .unwrap();
        session.data_ready(Ok(())).unwrap();
        session.reply_sent().unwrap();
        assert_eq!(session.feed(b"\r\n").unwrap(), 2);
        let Pending::Data(bytes) = session.pending() else {
            panic!("data")
        };
        delivery.write(bytes).unwrap();
        session.data_written(Ok(())).unwrap();
        assert_eq!(session.feed(b".\r\n").unwrap(), 3);
        delivery.prepare().unwrap();
        *policy.0.lock().unwrap() = false;
        assert!(matches!(
            delivery.commit(),
            Err(DeliveryError::Storage(UploadError::Store(
                ports::Error::Forbidden
            )))
        ));
        *policy.0.lock().unwrap() = true;
        assert!(matches!(
            delivery.commit(),
            Err(DeliveryError::Storage(UploadError::Store(
                ports::Error::Invalid
            )))
        ));
        session
            .committed(Err(CommitError::Rejected(ports::Error::Forbidden)))
            .unwrap();
        assert!(
            matches!(session.pending(),Pending::Reply{bytes,close:false} if bytes.starts_with(b"451"))
        );
        delivery.discard().unwrap();
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
        Vec::new()
    });
}
#[test]
fn commit_uncertainty_never_accepts_and_known_success_survives_accounting_failure() {
    for deny in [false, true] {
        fixture(|coordinator, spool, policy, clock, routes| {
            let mut session = session(routes);
            let timer = clock.clone();
            lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
                .authorizer(Some(move |context:AuthContext<'_>| {
                    if matches!(context.action,AuthAction::Transaction{operation} if !matches!(operation,TransactionOperation::Begin|TransactionOperation::Rollback)) {
                        if deny {return Authorization::Deny;}
                        timer.0.store(100,Ordering::Relaxed);
                    }
                    Authorization::Allow
                })).unwrap();
            let mut delivery = coordinator
                .reserve_delivery(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    policy,
                    &session,
                    request(),
                )
                .unwrap();
            session.data_ready(Ok(())).unwrap();
            session.reply_sent().unwrap();
            assert_eq!(session.feed(b"\r\n").unwrap(), 2);
            let Pending::Data(bytes) = session.pending() else {
                panic!("data")
            };
            delivery.write(bytes).unwrap();
            session.data_written(Ok(())).unwrap();
            assert_eq!(session.feed(b".\r\n").unwrap(), 3);
            delivery.prepare().unwrap();
            let result = delivery.commit().unwrap();
            session.committed(result.outcome).unwrap();
            assert!(result.admission_stopped);
            drop(delivery);
            assert!(coordinator.used(Kind::BodyBytes).unwrap() > 0);
            assert!(coordinator.admission_stopped());
            if deny {
                assert!(matches!(result.outcome, Err(CommitError::Indeterminate(_))));
                assert_eq!(session.pending(), Pending::Closed);
                Vec::new()
            } else {
                assert!(
                    matches!(session.pending(),Pending::Reply{bytes,close:false} if bytes.starts_with(b"250"))
                );
                vec![Expected {
                    result,
                    tail: "\r\n".into(),
                }]
            }
        });
    }
}

#[test]
fn exactly_one_provisioned_inbox_is_required_before_data_admission() {
    for duplicate in [false, true] {
        fixture(|coordinator, spool, policy, _, routes| {
            let mut bytes = [0; 128];
            let n = Row::Mailbox(MailboxRow {
                name: "Second Inbox",
                parent: None,
                role: Some("inbox"),
                sort_order: 0,
                subscribed: true,
            })
            .encode(&mut bytes)
            .unwrap();
            let second = MailboxId::from_bytes([3; 16]);
            let operation = if duplicate {
                Operation::put(Table::Mailboxes, second.as_bytes(), &bytes[..n]).unwrap()
            } else {
                Operation::delete(Table::Mailboxes, INBOX.as_bytes()).unwrap()
            };
            coordinator
                .store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: coordinator.store.epoch(),
                        expected: Sequence::from_u64(1),
                        utc_ms: 0,
                        deadline: deadline(),
                    },
                    &[operation],
                    &mut [],
                )
                .unwrap();
            let session = session(routes);
            assert!(
                matches!(coordinator.reserve_delivery(&td_crypto::Provider,&mut Random(10),spool,policy,&session,request()),
                Err(DeliveryError::Storage(UploadError::Store(error))) if error == if duplicate {ports::Error::Conflict} else {ports::Error::NotFound})
            );
            assert_eq!(spool.status().unwrap().occupied_slots, 0);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                0
            );
            Vec::new()
        });
    }
}
#[test]
fn receiving_guard_spans_native_publication_and_sql_failure_never_accepts() {
    for deny in [false, true] {
        fixture(|coordinator, spool, policy, _, routes| {
            let allowed = policy.0.clone();
            let captured = allowed.clone();
            let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let check = observed.clone();
            lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
                .authorizer(Some(move |context:AuthContext<'_>| {
                    if matches!(context.action,AuthAction::Insert{table_name} if table_name=="emails") {
                        assert!(matches!(captured.try_lock(),Err(TryLockError::WouldBlock)));
                        check.store(true,Ordering::Relaxed);
                        if deny {return Authorization::Deny;}
                    }
                    Authorization::Allow
                })).unwrap();
            let mut session = session(routes);
            let mut delivery = coordinator
                .reserve_delivery(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    policy,
                    &session,
                    request(),
                )
                .unwrap();
            session.data_ready(Ok(())).unwrap();
            session.reply_sent().unwrap();
            assert_eq!(session.feed(b".\r\n").unwrap(), 3);
            delivery.prepare().unwrap();
            let result = delivery.commit().unwrap();
            session.committed(result.outcome).unwrap();
            drop(delivery);
            assert!(observed.load(Ordering::Relaxed));
            assert!(allowed.try_lock().is_ok());
            if deny {
                assert!(matches!(result.outcome, Err(CommitError::Rejected(_))));
                assert!(
                    matches!(session.pending(),Pending::Reply{bytes,close:false} if bytes.starts_with(b"451"))
                );
                assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
                Vec::new()
            } else {
                vec![Expected {
                    result,
                    tail: String::new(),
                }]
            }
        });
    }
}

#[test]
fn large_folded_field_streams_through_spool_and_reopens_exactly() {
    fixture_with_limits(
        512 * 1024,
        256 * 1024,
        |coordinator, spool, policy, _, routes| {
            let mut session = session_with_limit(routes, 512 * 1024);
            let mut random = Random(10);
            let mut delivery = coordinator
                .reserve_delivery(
                    &td_crypto::Provider,
                    &mut random,
                    spool,
                    policy,
                    &session,
                    DeliveryRequest {
                        header_bytes: 256 * 1024,
                        deadline: deadline(),
                    },
                )
                .unwrap();
            session.data_ready(Ok(())).unwrap();
            session.reply_sent().unwrap();
            let mut source = String::from("Subject: a long valid folded field\r\n");
            for _ in 0..100 {
                source.push_str(&format!(" {}\r\n", "x".repeat(990)));
            }
            source.push_str("\r\nbody\r\n");
            for line in source.split_inclusive("\r\n") {
                assert_eq!(session.feed(line.as_bytes()).unwrap(), line.len());
                let Pending::Data(bytes) = session.pending() else {
                    panic!("DATA line");
                };
                delivery.write(bytes).unwrap();
                session.data_written(Ok(())).unwrap();
            }
            session.feed(b".\r\n").unwrap();
            delivery.prepare().unwrap();
            let result = delivery.commit().unwrap();
            session.committed(result.outcome).unwrap();
            assert!(
                matches!(session.pending(), Pending::Reply { bytes, close: false } if bytes.starts_with(b"250 "))
            );
            vec![Expected {
                result,
                tail: source,
            }]
        },
    );
}

#[test]
fn overnested_identifier_fields_do_not_contribute_partial_thread_candidates() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut random = Random(10);
        let base = "Message-ID: <root@x>\r\n\r\nbase\r\n";
        let first = deliver(
            coordinator,
            spool,
            policy,
            routes,
            &mut random,
            base.as_bytes(),
        );
        let comment = format!("{}x{}", "(".repeat(33), ")".repeat(33));
        let source = format!("References: <root@x> {comment}\r\nMessage-ID: <root@x> {comment}\r\nMessage-ID: <child@x>\r\n\r\nchild\r\n");
        let second = deliver(
            coordinator,
            spool,
            policy,
            routes,
            &mut random,
            source.as_bytes(),
        );
        assert_ne!(first.thread, second.thread);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.thread_anchor("child@x", &mut [0; 65536]).unwrap(),
            Some((second.email, second.thread))
        );
        vec![
            Expected {
                result: first,
                tail: base.into(),
            },
            Expected {
                result: second,
                tail: source,
            },
        ]
    });
}
#[test]
fn streaming_short_body_lines_do_not_consume_object_records() {
    fixture(|coordinator, spool, policy, _, routes| {
        coordinator.work.foreground_records = 32;
        let source = "\r\n".repeat(2048);
        let result = deliver(
            coordinator,
            spool,
            policy,
            routes,
            &mut Random(10),
            source.as_bytes(),
        );
        vec![Expected {
            result,
            tail: source,
        }]
    });
}
#[test]
fn eof_trace_header_limit_is_a_permanent_commit_state_refusal() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut session = session(routes);
        let mut random = Random(10);
        let mut delivery = coordinator
            .reserve_delivery(
                &td_crypto::Provider,
                &mut random,
                spool,
                policy,
                &session,
                DeliveryRequest {
                    header_bytes: 512,
                    ..request()
                },
            )
            .unwrap();
        session.data_ready(Ok(())).unwrap();
        session.reply_sent().unwrap();
        let source = format!("Subject: {}\r\n", "x".repeat(400));
        session.feed(source.as_bytes()).unwrap();
        let Pending::Data(bytes) = session.pending() else {
            panic!("data");
        };
        delivery.write(bytes).unwrap();
        session.data_written(Ok(())).unwrap();
        session.feed(b".\r\n").unwrap();
        assert_eq!(session.pending(), Pending::Commit);
        assert!(matches!(
            delivery.prepare(),
            Err(DeliveryError::HeaderLimit)
        ));
        session.message_too_large().unwrap();
        assert!(
            matches!(session.pending(), Pending::Reply { bytes, close: true } if bytes.starts_with(b"552 "))
        );
        delivery.discard().unwrap();
        Vec::new()
    });
}
#[test]
fn trace_covers_tls_ipv6_helo_gateway_and_worst_case_allowance() {
    let hostname = format!(
        "{}.{}.{}.{}",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61)
    );
    let allowance = smtp_trace_allowance(&hostname).unwrap();
    let mut receipt = Receipt {
        account: ACCOUNT,
        peer: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap(),
        tls: ReceiptTls::Plain,
        gateway: Some("gateway-private-name".into()),
        hello: "h".repeat(crate::format::row::MAX_EHLO),
        reverse: "r".repeat(crate::format::row::MAX_ADDRESS),
        recipients: Vec::new(),
    };
    for (extended, tls, protocol) in [
        (true, ReceiptTls::Plain, "with ESMTP id"),
        (true, ReceiptTls::Tls12, "with ESMTPS (TLSv1.2) id"),
        (true, ReceiptTls::Tls13, "with ESMTPS (TLSv1.3) id"),
        (false, ReceiptTls::Plain, "with SMTP id"),
        (false, ReceiptTls::Tls13, "with SMTP (TLSv1.3) id"),
    ] {
        receipt.tls = tls;
        let value = trace(
            &receipt,
            &hostname,
            extended,
            EmailId::from_bytes([255; 16]),
            253402300799000,
            allowance,
        )
        .unwrap();
        assert!(value.len() + 2 <= allowance);
        assert!(value.contains(protocol));
        assert!(value.contains("[IPv6:ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]"));
        assert!(value.ends_with("31 Dec 9999 23:59:59 +0000\r\n"));
        assert!(!value.contains("gateway-private-name"));
    }
}

#[test]
fn local_trace_must_fit_the_header_limit_before_data_admission() {
    fixture(|coordinator, spool, policy, _, routes| {
        let session = session(routes);
        assert!(matches!(
            coordinator.reserve_delivery(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                policy,
                &session,
                DeliveryRequest {
                    header_bytes: 1,
                    ..request()
                },
            ),
            Err(DeliveryError::Storage(UploadError::Store(
                ports::Error::Invalid
            )))
        ));
        assert!(matches!(session.pending(), Pending::BeginData { .. }));
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::BodyBytes)
                .unwrap(),
            0
        );
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        Vec::new()
    });
}

#[test]
fn invalid_gateway_receipt_text_refuses_before_data_admission() {
    struct InvalidGateway;
    impl DeliveryGuard for InvalidGateway {
        fn access(&self) -> Access {
            Access {
                account: ACCOUNT,
                principal: Principal::Smtp,
                config_generation: 1,
            }
        }
        fn peer(&self) -> DeliveryPeer<'_> {
            DeliveryPeer {
                peer: "192.0.2.7".parse().unwrap(),
                tls: ReceiptTls::Tls13,
                gateway: Some("gateway\u{85}control"),
            }
        }
    }
    impl DeliveryAuthorization for InvalidGateway {
        type Guard<'a> = InvalidGateway;
        fn authorize(&self, _: AccountId, _: Deadline) -> Result<Self::Guard<'_>, ports::Error> {
            Ok(InvalidGateway)
        }
    }
    fixture(|coordinator, spool, _, _, routes| {
        let session = session(routes);
        assert!(matches!(
            coordinator.reserve_delivery(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                &InvalidGateway,
                &session,
                request()
            ),
            Err(DeliveryError::Storage(UploadError::Store(
                ports::Error::Invalid
            )))
        ));
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::BodyBytes)
                .unwrap(),
            0
        );
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        Vec::new()
    });
}
#[test]
fn delivery_rechecks_inbox_after_body_preparation() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut session = session(routes);
        let mut random = Random(10);
        let mut delivery = coordinator
            .reserve_delivery(
                &td_crypto::Provider,
                &mut random,
                spool,
                policy,
                &session,
                request(),
            )
            .unwrap();
        session.data_ready(Ok(())).unwrap();
        session.reply_sent().unwrap();
        session.feed(b"\r\n").unwrap();
        let Pending::Data(bytes) = session.pending() else {
            panic!("data");
        };
        delivery.write(bytes).unwrap();
        session.data_written(Ok(())).unwrap();
        session.feed(b".\r\n").unwrap();
        delivery.prepare().unwrap();
        // Inject an intervening native mutation; the public coordinator does not
        // currently expose another mutation while this job is outstanding.
        delivery
            .coordinator
            .store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: delivery.epoch,
                    expected: Sequence::from_u64(1),
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::delete(Table::Mailboxes, INBOX.as_bytes()).unwrap()],
                &mut [],
            )
            .unwrap();
        assert!(matches!(
            delivery.commit(),
            Err(DeliveryError::Storage(UploadError::Store(
                ports::Error::NotFound
            )))
        ));
        session
            .committed(Err(CommitError::Rejected(ports::Error::NotFound)))
            .unwrap();
        assert!(
            matches!(session.pending(), Pending::Reply { bytes, close: false } if bytes.starts_with(b"451 "))
        );
        delivery.discard().unwrap();
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::BodyBytes)
                .unwrap(),
            0
        );
        Vec::new()
    });
}

#[test]
fn idle_delivery_does_not_block_a_worker_and_commit_uses_new_thread_anchor() {
    fixture(|coordinator, spool, policy, _, routes| {
        let coordinator = &*coordinator;
        let waiting_session = session(routes);
        let mut waiting = coordinator
            .reserve_delivery(
                &td_crypto::Provider,
                &mut Random(40),
                spool,
                policy,
                &waiting_session,
                request(),
            )
            .unwrap();
        waiting.write(b"References: <new@x>\r\n").unwrap();
        waiting.write(b"\r\n").unwrap();
        // The first sender retains its unfinished spool while another worker
        // completes real delivery through the SMTP session.
        let first = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    deliver(
                        coordinator,
                        spool,
                        policy,
                        routes,
                        &mut Random(10),
                        b"Message-ID: <new@x>\r\n\r\nfirst\r\n",
                    )
                })
                .join()
                .unwrap()
        });
        assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 1);
        assert_eq!(spool.status().unwrap().occupied_slots, 1);
        assert_eq!(coordinator.checkpoint(deadline()).unwrap().outcome, Ok(()));
        assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), 0);
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::BodyBytes)
                .unwrap(),
            MAXIMUM as u64
        );
        waiting.write(b"second\r\n").unwrap();
        waiting.prepare().unwrap();
        let second = waiting.commit().unwrap();
        assert_eq!(second.thread, first.thread);
        assert!(second.outcome.unwrap().sequence > first.outcome.unwrap().sequence);
        drop(waiting);
        assert_eq!(
            coordinator.state.lock().unwrap().ledger.available_cells(),
            4
        );
        vec![
            Expected {
                result: first,
                tail: "Message-ID: <new@x>\r\n\r\nfirst\r\n".into(),
            },
            Expected {
                result: second,
                tail: "References: <new@x>\r\n\r\nsecond\r\n".into(),
            },
        ]
    });
}

#[test]
fn busy_publication_can_be_rescheduled_without_losing_prepared_delivery() {
    fixture(|coordinator, spool, policy, _, routes| {
        let session = session(routes);
        let mut job = coordinator
            .reserve_delivery(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                policy,
                &session,
                request(),
            )
            .unwrap();
        job.write(b"\r\n").unwrap();
        job.prepare().unwrap();
        let guard = coordinator.state.lock().unwrap();
        let (mut job, result) = std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let result = job.commit();
                    (job, result)
                })
                .join()
                .unwrap()
        });
        assert!(matches!(result, Err(DeliveryError::CoordinationBusy)));
        assert!(!job.failed);
        drop(guard);
        let result = job.commit().unwrap();
        assert!(result.outcome.is_ok());
        {
            let _guard = coordinator.state.lock().unwrap();
            assert!(matches!(
                job.commit(),
                Err(DeliveryError::Storage(UploadError::Store(
                    ports::Error::Invalid
                )))
            ));
        }
        vec![Expected {
            result,
            tail: "\r\n".into(),
        }]
    });
}

#[test]
fn smtp_and_upload_share_accounting_without_sharing_ingress_custody() {
    fixture(|coordinator, spool, policy, _, routes| {
        let mut upload = coordinator
            .reserve(
                &td_crypto::Provider,
                &mut Random(70),
                spool,
                UploadRequest {
                    account: ACCOUNT,
                    device: DeviceId::from_bytes([3; 16]),
                    maximum: 8,
                    deadline: deadline(),
                },
            )
            .unwrap();
        upload.write(b"body").unwrap();
        let result = deliver(
            coordinator,
            spool,
            policy,
            routes,
            &mut Random(10),
            b"\r\nmail\r\n",
        );
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 0);
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::UploadBytes)
                .unwrap(),
            8
        );
        upload.prepare().unwrap();
        assert!(matches!(
            upload.commit().unwrap(),
            UploadAttempt::SequenceConflict
        ));
        upload.replan().unwrap();
        let UploadAttempt::Complete(receipt) = upload.commit().unwrap() else {
            panic!("second conflict")
        };
        assert!(receipt.outcome.is_ok());
        drop(upload);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
        assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 2);
        vec![Expected {
            result,
            tail: "\r\nmail\r\n".into(),
        }]
    });
}

#[test]
fn policy_busy_is_a_failed_delivery_not_a_coordination_retry() {
    fixture(|coordinator, spool, policy, _, routes| {
        let session = session(routes);
        let mut job = coordinator
            .reserve_delivery(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                policy,
                &session,
                request(),
            )
            .unwrap();
        job.write(b"\r\n").unwrap();
        job.prepare().unwrap();
        let guard = policy.0.lock().unwrap();
        assert!(matches!(
            job.commit(),
            Err(DeliveryError::Storage(UploadError::Store(
                ports::Error::Busy
            )))
        ));
        assert!(job.failed);
        drop(guard);
        assert!(matches!(
            job.commit(),
            Err(DeliveryError::Storage(UploadError::Store(
                ports::Error::Invalid
            )))
        ));
        drop(job);
        assert_eq!(
            coordinator.state.lock().unwrap().ledger.available_cells(),
            4
        );
        Vec::new()
    });
}

#[test]
fn delivery_spool_creation_runs_outside_coordination_and_rolls_back_failure() {
    for fail in [false, true] {
        fixture(|coordinator, spool, policy, _, routes| {
            let session = session(routes);
            let crypto = super::super::tests::CheckingCrypto(|| {
                let state = coordinator.try_state().unwrap();
                assert_eq!(
                    state.ledger.pending(Kind::BodyBytes).unwrap(),
                    MAXIMUM as u64
                );
                if fail {
                    Err(td_crypto::Error::Crypto)
                } else {
                    Ok(())
                }
            });
            let retired = Arc::new(AtomicU64::new(0));
            let authorization = OwnedPolicy {
                policy: Policy(policy.0.clone(), policy.1),
                retired: retired.clone(),
                retiring: || {
                    assert_eq!(coordinator.try_state().unwrap().ledger.available_cells(), 4);
                    assert_eq!(spool.status().unwrap().occupied_slots, 0);
                },
            };
            let result = coordinator.reserve_delivery(
                &crypto,
                &mut Random(10),
                spool,
                authorization,
                &session,
                request(),
            );
            if fail {
                assert!(matches!(
                    result,
                    Err(DeliveryError::Storage(UploadError::Store(
                        ports::Error::Crypto
                    )))
                ));
            } else {
                result.unwrap().discard().unwrap();
            }
            assert_eq!(retired.load(Ordering::SeqCst), 1);
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                4
            );
            assert_eq!(spool.status().unwrap().occupied_slots, 0);
            Vec::new()
        });
    }
}

#[test]
fn real_tcp_delivery_acknowledges_only_recoverable_mail() {
    use crate::{
        smtp_network::{Network, Progress},
        transport::TcpTransport,
    };
    use std::{
        io::{BufRead, BufReader, Write},
        net::{Shutdown, TcpListener, TcpStream},
        time::{Duration, Instant},
    };
    fn reply(reader: &mut BufReader<TcpStream>, code: &str) {
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            assert!(line.starts_with(code), "{line}");
            if line.as_bytes()[3] != b'-' {
                break;
            }
        }
    }
    for late in [None, Some(1800001), Some(3600001)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let mut transport = TcpTransport::from_stream(server).unwrap();
        let peer = transport.peer_addr().ip();
        fixture_with_peer(
            MAXIMUM,
            8192,
            peer,
            |coordinator, spool, policy, clock, routes| {
                std::thread::scope(|scope| {
                    let client = scope.spawn(move || {
                        client
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        client
                            .set_write_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        let mut reader = BufReader::new(client);
                        reply(&mut reader, "220");
                        for (command, code) in [
                            ("EHLO sender.test\r\n", "250"),
                            ("MAIL FROM:<> BODY=8BITMIME\r\n", "250"),
                            ("RCPT TO:<Alice@example.test>\r\n", "250"),
                            ("RCPT TO:<alias@example.test>\r\n", "250"),
                            ("DATA\r\n", "354"),
                            ("Subject: socket\r\n\r\n..dot\r\n.\r\n", "250"),
                        ] {
                            reader.get_mut().write_all(command.as_bytes()).unwrap();
                            reply(&mut reader, code);
                        }
                        if late == Some(3600001) {
                            reply(&mut reader, "421");
                        } else {
                            reader.get_mut().write_all(b"QUIT\r\n").unwrap();
                            reply(&mut reader, "221");
                        }
                        reader.get_mut().shutdown(Shutdown::Write).unwrap();
                        let mut line = String::new();
                        assert_eq!(reader.read_line(&mut line).unwrap(), 0);
                    });
                    let resources = Limits::default().plan().unwrap();
                    let plan = crate::admission::timers::NetworkLimits::default()
                        .plan(
                            &DiskLimits::default()
                                .plan(
                                    &resources,
                                    WorkLimits::default(),
                                    ViewMode::OnlineBackground,
                                )
                                .unwrap(),
                        )
                        .unwrap();
                    let mut network = Network::new(
                        routes,
                        Settings {
                            hostname: "mx.example.test",
                            message_bytes: MAXIMUM,
                            trace_bytes: smtp_trace_allowance("mx.example.test").unwrap(),
                            recipients: 100,
                            starttls: false,
                        },
                        &plan,
                        clock.as_ref(),
                    )
                    .unwrap();
                    let mut random = Random(30);
                    let mut delivery = None;
                    let mut expected = Vec::new();
                    let stop = Instant::now() + Duration::from_secs(10);
                    loop {
                        assert!(Instant::now() < stop);
                        match network.advance(&mut transport, clock.as_ref()).unwrap() {
                            Progress::Pending => std::thread::sleep(Duration::from_millis(1)),
                            Progress::Advanced => (),
                            Progress::Closed => break,
                            Progress::Work => match network.session().pending() {
                                Pending::BeginData { .. } => {
                                    let job = coordinator
                                        .reserve_delivery(
                                            &td_crypto::Provider,
                                            &mut random,
                                            spool,
                                            policy,
                                            network.session(),
                                            request(),
                                        )
                                        .unwrap();
                                    delivery = Some(job);
                                    network.data_ready(Ok(()), clock.as_ref()).unwrap();
                                }
                                Pending::Data(bytes) => {
                                    delivery.as_mut().unwrap().write(bytes).unwrap();
                                    network.data_written(Ok(()), clock.as_ref()).unwrap();
                                }
                                Pending::Commit => {
                                    let mut job = delivery.take().unwrap();
                                    job.prepare().unwrap();
                                    let result = job.commit().unwrap();
                                    assert!(result.outcome.is_ok());
                                    if let Some(tick) = late {
                                        clock.0.store(tick, Ordering::Relaxed);
                                    }
                                    network.committed(result.outcome, clock.as_ref()).unwrap();
                                    expected.push(Expected {
                                        result,
                                        tail: "Subject: socket\r\n\r\n.dot\r\n".into(),
                                    });
                                }
                                other => panic!("unexpected worker request {other:?}"),
                            },
                        }
                    }
                    client.join().unwrap();
                    assert_eq!(expected.len(), 1);
                    expected
                })
            },
        );
    }
}

struct OwnedPolicy<F: Fn() = fn()> {
    policy: Policy,
    retired: Arc<AtomicU64>,
    retiring: F,
}
impl<F: Fn()> DeliveryAuthorization for OwnedPolicy<F> {
    type Guard<'a>
        = Guard<'a>
    where
        Self: 'a;
    fn authorize(&self, account: AccountId, deadline: Deadline) -> Result<Guard<'_>, ports::Error> {
        self.policy.authorize(account, deadline)
    }
}
impl<F: Fn()> Drop for OwnedPolicy<F> {
    fn drop(&mut self) {
        (self.retiring)();
        self.retired.fetch_add(1, Ordering::SeqCst);
    }
}
fn owned_policy(policy: &Policy, retired: &Arc<AtomicU64>) -> OwnedPolicy {
    OwnedPolicy {
        policy: Policy(policy.0.clone(), policy.1),
        retired: retired.clone(),
        retiring: || {},
    }
}

#[test]
fn delivery_carries_worker_local_authorization_to_later_publication() {
    fixture(|coordinator, spool, policy, _, routes| {
        let coordinator = &*coordinator;
        let retired = Arc::new(AtomicU64::new(0));
        let session = session(routes);
        let result = std::thread::scope(|scope| {
            let job = scope
                .spawn(|| {
                    let authorization = owned_policy(policy, &retired);
                    coordinator
                        .reserve_delivery(
                            &td_crypto::Provider,
                            &mut Random(10),
                            spool,
                            authorization,
                            &session,
                            request(),
                        )
                        .unwrap()
                })
                .join()
                .unwrap();
            assert_eq!(retired.load(Ordering::SeqCst), 0);
            scope
                .spawn(move || {
                    let mut job = job;
                    job.write(b"Subject: worker transfer\r\n").unwrap();
                    job.write(b"\r\n").unwrap();
                    job.write(b"body\r\n").unwrap();
                    job.prepare().unwrap();
                    job.commit().unwrap()
                })
                .join()
                .unwrap()
        });
        assert_eq!(retired.load(Ordering::SeqCst), 1);
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        vec![Expected {
            result,
            tail: "Subject: worker transfer\r\n\r\nbody\r\n".into(),
        }]
    });
}

#[test]
fn owned_delivery_authorization_still_rechecks_revocation_and_releases_on_refusal() {
    for discard in [false, true] {
        fixture(|coordinator, spool, policy, _, routes| {
            let coordinator = &*coordinator;
            let retired = Arc::new(AtomicU64::new(0));
            let session = session(routes);
            let mut job = coordinator
                .reserve_delivery(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    owned_policy(policy, &retired),
                    &session,
                    request(),
                )
                .unwrap();
            job.write(b"\r\n").unwrap();
            job.prepare().unwrap();
            *policy.0.lock().unwrap() = false;
            std::thread::scope(|scope| {
                let retired = &retired;
                scope
                    .spawn(move || {
                        assert!(matches!(
                            job.commit(),
                            Err(DeliveryError::Storage(UploadError::Store(
                                ports::Error::Forbidden
                            )))
                        ));
                        assert_eq!(retired.load(Ordering::SeqCst), 0);
                        if discard {
                            job.discard().unwrap();
                        } else {
                            drop(job);
                        }
                        assert_eq!(retired.load(Ordering::SeqCst), 1);
                    })
                    .join()
                    .unwrap();
            });
            assert_eq!(retired.load(Ordering::SeqCst), 1);
            assert_eq!(spool.status().unwrap().occupied_slots, 0);
            assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 0);
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                4
            );
            assert!(matches!(
                coordinator.reserve_delivery(
                    &td_crypto::Provider,
                    &mut Random(40),
                    spool,
                    owned_policy(policy, &retired),
                    &session,
                    request(),
                ),
                Err(DeliveryError::Storage(UploadError::Store(
                    ports::Error::Forbidden
                )))
            ));
            assert_eq!(retired.load(Ordering::SeqCst), 2);
            assert_eq!(spool.status().unwrap().occupied_slots, 0);
            Vec::new()
        });
    }
}

// Parallel tests may race the one-shot global ticket issue before effects.
fn retry_reservation<T>(
    mut attempt: impl FnMut() -> Result<T, DeliveryError>,
) -> Result<T, DeliveryError> {
    for _ in 0..1000 {
        match attempt() {
            Err(DeliveryError::Storage(UploadError::Ledger(logical::Error::Slot(
                crate::ownership::Error::Contended,
            )))) => std::thread::yield_now(),
            result => return result,
        }
    }
    Err(DeliveryError::Storage(UploadError::Ledger(
        logical::Error::Slot(crate::ownership::Error::Contended),
    )))
}
