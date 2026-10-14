#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{DiskLimits, ViewMode, WorkLimits},
    compose_text::{Address, Message},
    format::row::MailboxRow,
    limits::Limits,
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([3; 16]);
const MAILBOX: MailboxId = MailboxId::from_bytes([2; 16]);
const IDENTITY: IdentityId = IdentityId::from_bytes([4; 16]);
const MAXIMUM: u64 = 65536;
struct Timer {
    tick: AtomicU64,
    utc: AtomicI64,
}
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: self.utc.load(Ordering::Relaxed),
            monotonic: Tick(self.tick.load(Ordering::Relaxed)),
        })
    }
}
struct Policy {
    calls: AtomicU64,
    failure: AtomicU64,
    clock: Arc<Timer>,
}
struct Guard {
    generation: u64,
    email: &'static str,
}
impl UploadGuard for Guard {
    fn access(&self) -> Access {
        Access {
            account: ACCOUNT,
            principal: Principal::Device(DEVICE),
            config_generation: self.generation,
        }
    }
}
impl SubmissionGuard for Guard {
    fn identity(&self) -> IdentityId {
        IDENTITY
    }
    fn email(&self) -> &str {
        self.email
    }
}
impl DraftAuthorization for Policy {
    type Guard<'a> = Guard;
    fn authorize_draft(
        &self,
        account: AccountId,
        device: DeviceId,
        _: Deadline,
    ) -> Result<Guard, ports::Error> {
        assert_eq!((account, device), (ACCOUNT, DEVICE));
        Ok(Guard {
            generation: 1,
            email: "sender@example.test",
        })
    }
}
impl SubmissionAuthorization for Policy {
    type Guard<'a> = Guard;
    fn authorize_submission(
        &self,
        account: AccountId,
        device: DeviceId,
        identity: IdentityId,
        _: Deadline,
    ) -> Result<Guard, ports::Error> {
        if (account, device, identity) != (ACCOUNT, DEVICE, IDENTITY) {
            return Err(ports::Error::Forbidden);
        }
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        let failure = self.failure.load(Ordering::Relaxed);
        if call == 1 && failure == 1 {
            return Err(ports::Error::Forbidden);
        }
        if call == 1 && failure == 4 {
            self.clock.tick.store(100, Ordering::Relaxed);
        }
        Ok(Guard {
            generation: if call == 1 && failure == 2 { 2 } else { 1 },
            email: if call == 1 && failure == 3 {
                "other@example.test"
            } else {
                "sender@example.test"
            },
        })
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
fn meter() -> Meter {
    Meter::new(
        deadline(),
        WorkCharge {
            io_bytes: 10_000_000,
            output_bytes: 10_000_000,
            records: 1_000_000,
            unlinks: 0,
        },
    )
}
fn request(email: EmailId) -> SubmissionRequest {
    SubmissionRequest {
        account: ACCOUNT,
        device: DEVICE,
        email,
        identity: IDENTITY,
        expected: None,
        maximum: MAXIMUM,
        header_bytes: MAXIMUM,
        recipients: 1000,
        deadline: deadline(),
    }
}
fn draft(
    coordinator: &StoreCoordinator<'_, '_, Policy>,
    spool: &IngressSpool<'_>,
    random: &mut Random,
    to: &[Address<'_>],
    bcc: &[Address<'_>],
) -> DraftCompletion {
    coordinator
        .create_text_draft(
            &td_crypto::Provider,
            random,
            spool,
            DraftRequest {
                account: ACCOUNT,
                device: DEVICE,
                mailbox: MAILBOX,
                expected: None,
                maximum: MAXIMUM,
                deadline: deadline(),
            },
            &Message {
                from: Address {
                    name: "Sender",
                    email: "sender@example.test",
                },
                to,
                cc: &[],
                bcc,
                reply_to: &[],
                subject: "Send",
                message_id: "send@example.test",
                in_reply_to: &[],
                references: &[],
                sent_at_ms: 0,
                text: "hello\nworld",
            },
            &mut meter(),
        )
        .unwrap()
}
fn bytes(view: &mut IndexReadView<'_, '_>, id: BlobId) -> Vec<u8> {
    let mut input = view
        .open_blob_input(&td_crypto::Provider, id, MAXIMUM)
        .unwrap();
    let mut bytes = vec![0; input.len() as usize];
    let mut used = 0;
    while used < bytes.len() {
        let n = input.read(&mut bytes[used..]).unwrap();
        assert_ne!(n, 0);
        used += n;
    }
    input.finish().unwrap();
    bytes
}
fn fixture(
    run: impl FnOnce(
        &StoreCoordinator<'_, '_, Policy>,
        &IngressSpool<'_>,
        &Arc<Timer>,
    ) -> Vec<SubmissionCompletion>,
) {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let staging = Fixture::new();
    let mut staging_root = staging.locked();
    let clock = Arc::new(Timer {
        tick: AtomicU64::new(1),
        utc: AtomicI64::new(1000),
    });
    let resources = Limits {
        message_bytes: MAXIMUM as usize,
        header_bytes: MAXIMUM as usize,
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
        name: "Drafts",
        parent: None,
        role: None,
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
                utc_ms: 1000,
                deadline: deadline(),
            },
            &[Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &row[..n]).unwrap()],
            &mut [],
        )
        .unwrap();
    let spool =
        IngressSpool::open(&mut staging_root, &resources, clock.clone(), deadline()).unwrap();
    let mut states = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { LeaseCell::EMPTY }; 4];
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
        Policy {
            calls: AtomicU64::new(0),
            failure: AtomicU64::new(0),
            clock: clock.clone(),
        },
        deadline(),
    )
    .unwrap();
    let expected = run(&coordinator, &spool, &clock);
    clock.tick.store(1, Ordering::Relaxed);
    clock.utc.store(1000, Ordering::Relaxed);
    assert_eq!(spool.status().unwrap().occupied_slots, 0);
    assert_eq!(
        coordinator.used(Kind::QueueSubmissions).unwrap(),
        expected.len() as u64
    );
    assert_eq!(
        coordinator.used(Kind::QueueBytes).unwrap(),
        expected.iter().map(|c| c.size).sum::<u64>()
    );
    let usage = coordinator
        .store
        .view(ACCOUNT, deadline())
        .unwrap()
        .logical_usage()
        .unwrap();
    assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), usage.body_bytes);
    assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), usage.blob_count);
    let stopped = coordinator.admission_stopped();
    drop(coordinator);
    let store = IndexStore::open(&mut root, clock, 2, deadline()).unwrap();
    store.validate_integrity(deadline()).unwrap();
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    let mut value = [0; 65536];
    let mut key = [0; 16];
    let mut after = None::<[u8; 16]>;
    let mut count = 0;
    while let Some(record) = view
        .next(
            Table::Submissions,
            after.as_ref().map(|v| v.as_slice()),
            &mut key,
            &mut value,
        )
        .unwrap()
    {
        let Key::Submission(id) = record.key else {
            panic!("submission")
        };
        after = Some(*id.as_bytes());
        count += 1;
    }
    assert_eq!(count, expected.len());
    for item in expected {
        assert!(item.outcome.is_ok());
        assert_eq!(item.admission_stopped, stopped);
        assert!(item.cleanup_error.is_none());
        let Some((Row::Submission(row), _)) = view
            .get(Key::Submission(item.submission), &mut value)
            .unwrap()
        else {
            panic!("submission")
        };
        assert_eq!(row.transmitted_blob, item.blob);
        assert_eq!(row.recipient_count, item.recipients);
        assert_eq!(row.identity, IDENTITY);
        assert_eq!(row.reverse_path, "sender@example.test");
        assert_eq!(row.expires_at, row.send_at + 432_000_000);
        assert_eq!(row.notification, NotificationState::None);
        let email = row.email;
        let original_blob = match view.get(Key::Email(email), &mut value).unwrap().unwrap().0 {
            Row::Email(row) => row.blob,
            _ => panic!("email"),
        };
        assert!(view
            .get(Key::Keyword(email, "$draft"), &mut value)
            .unwrap()
            .is_some());
        assert!(view
            .get(Key::Membership(email, MAILBOX), &mut value)
            .unwrap()
            .is_some());
        let original = bytes(&mut view, original_blob);
        let outgoing = bytes(&mut view, item.blob);
        assert!(original.windows(4).any(|s| s == b"Bcc:"));
        assert!(!outgoing.windows(4).any(|s| s == b"Bcc:"));
        assert_eq!(outgoing.len() as u64, item.size);
        let original_start = original.windows(4).position(|s| s == b"\r\n\r\n").unwrap() + 4;
        let outgoing_start = outgoing.windows(4).position(|s| s == b"\r\n\r\n").unwrap() + 4;
        assert_eq!(&original[original_start..], &outgoing[outgoing_start..]);
        for ordinal in 0..item.recipients {
            let Some((Row::Recipient(row), _)) = view
                .get(Key::Recipient(item.submission, ordinal), &mut value)
                .unwrap()
            else {
                panic!("recipient")
            };
            assert_eq!(row.state, RecipientState::Queued);
            assert_eq!(row.phase, AttemptPhase::None);
            assert_eq!(row.attempt_count, 0);
            assert!(!row.uncertain);
            assert_eq!(row.next_attempt_at, Some(1000));
        }
    }
}
#[test]
fn frozen_body_envelope_and_queue_accounting_survive_reopen() {
    fixture(|coordinator, spool, _| {
        let mut random = Random(10);
        let item = draft(
            coordinator,
            spool,
            &mut random,
            &[Address {
                name: "Recipient",
                email: "dest@example.test",
            }],
            &[
                Address {
                    name: "Duplicate",
                    email: "dest@EXAMPLE.test",
                },
                Address {
                    name: "Private",
                    email: "hidden@example.test",
                },
            ],
        );
        let result = coordinator
            .create_submission(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(item.email),
                &mut meter(),
            )
            .unwrap();
        assert_eq!(result.recipients, 2);
        assert_eq!(result.before.committed_sequence, item.outcome.unwrap());
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        let mut value = [0; 65536];
        for (ordinal, address) in [(0, "dest@example.test"), (1, "hidden@example.test")] {
            assert!(
                matches!(view.get(Key::Recipient(result.submission,ordinal),&mut value).unwrap(),Some((Row::Recipient(row),_)) if row.address==address)
            );
        }
        vec![result]
    });
}
#[test]
fn reauthorization_state_budget_and_timestamp_failures_leave_no_queue_or_leases() {
    fixture(|coordinator, spool, clock| {
        let mut random = Random(10);
        let item = draft(
            coordinator,
            spool,
            &mut random,
            &[],
            &[Address {
                name: "Private",
                email: "hidden@example.test",
            }],
        );
        for failure in 1..=4 {
            coordinator.authorization.calls.store(0, Ordering::Relaxed);
            coordinator
                .authorization
                .failure
                .store(failure, Ordering::Relaxed);
            assert!(coordinator
                .create_submission(
                    &td_crypto::Provider,
                    &mut random,
                    spool,
                    request(item.email),
                    &mut meter()
                )
                .is_err());
            clock.tick.store(1, Ordering::Relaxed);
            assert_eq!(coordinator.used(Kind::QueueSubmissions).unwrap(), 0);
            assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 1);
            assert_eq!(spool.status().unwrap().occupied_slots, 0);
        }
        coordinator
            .authorization
            .failure
            .store(0, Ordering::Relaxed);
        for kind in 0..5 {
            let mut req = request(item.email);
            let mut budget = meter();
            match kind {
                0 => req.expected = Some((StoreEpoch::from_bytes([7; 16]), Sequence::default())),
                1 => budget = Meter::new(deadline(), WorkCharge::default()),
                2 => req.maximum = 1,
                3 => req.email = EmailId::from_bytes([99; 16]),
                _ => clock.utc.store(i64::MAX, Ordering::Relaxed),
            }
            assert!(coordinator
                .create_submission(&td_crypto::Provider, &mut random, spool, req, &mut budget)
                .is_err());
            clock.utc.store(1000, Ordering::Relaxed);
            assert_eq!(coordinator.used(Kind::QueueBytes).unwrap(), 0);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), item.size);
        }
        vec![coordinator
            .create_submission(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(item.email),
                &mut meter(),
            )
            .unwrap()]
    });
}
#[test]
fn maximum_recipient_transaction_and_id_collisions_are_bounded() {
    fixture(|coordinator, spool, _| {
        let addresses: Vec<_> = (0..1000).map(|n| format!("a{n}@example.test")).collect();
        let to: Vec<_> = addresses
            .iter()
            .map(|email| Address { name: "", email })
            .collect();
        let mut random = Random(10);
        let item = draft(
            coordinator,
            spool,
            &mut random,
            &to,
            &[Address {
                name: "",
                email: "a0@example.test",
            }],
        );
        assert!(matches!(
            coordinator.create_submission(
                &td_crypto::Provider,
                &mut random,
                spool,
                SubmissionRequest {
                    recipients: 999,
                    ..request(item.email)
                },
                &mut meter()
            ),
            Err(SubmissionError::Envelope(
                submission_envelope::Error::TooManyRecipients
            ))
        ));
        let mut limited = Meter::new(
            deadline(),
            WorkCharge {
                records: 100_000,
                ..meter().remaining()
            },
        );
        assert!(matches!(
            coordinator.create_submission(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(item.email),
                &mut limited
            ),
            Err(SubmissionError::Envelope(
                submission_envelope::Error::Policy(ports::Error::Capacity)
            ))
        ));
        assert_eq!(coordinator.used(Kind::QueueSubmissions).unwrap(), 0);
        let mut random = Random(40);
        let result = coordinator
            .create_submission(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(item.email),
                &mut meter(),
            )
            .unwrap();
        assert_eq!(result.recipients, 1000);
        assert!(coordinator
            .create_submission(
                &td_crypto::Provider,
                &mut Random(40),
                spool,
                request(item.email),
                &mut meter()
            )
            .is_err());
        struct Collision(std::array::IntoIter<u8, 2>);
        impl Entropy for Collision {
            fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
                bytes.fill(self.0.next().ok_or(td_crypto::Error::Entropy)?);
                Ok(())
            }
        }
        assert!(matches!(
            coordinator.create_submission(
                &td_crypto::Provider,
                &mut Collision([80, 42].into_iter()),
                spool,
                request(item.email),
                &mut meter()
            ),
            Err(SubmissionError::Storage(UploadError::Store(
                ports::Error::Conflict
            )))
        ));
        assert_eq!(coordinator.used(Kind::QueueSubmissions).unwrap(), 1);
        vec![result]
    });
}

#[test]
fn durable_queue_result_survives_accounting_deadline_after_commit() {
    fixture(|coordinator, spool, clock| {
        let mut random = Random(10);
        let item = draft(
            coordinator,
            spool,
            &mut random,
            &[],
            &[Address {
                name: "Private",
                email: "hidden@example.test",
            }],
        );
        let timer = clock.clone();
        lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                    timer.tick.store(100, Ordering::Relaxed);
                }
                Authorization::Allow
            })).unwrap();
        let result = coordinator
            .create_submission(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(item.email),
                &mut meter(),
            )
            .unwrap();
        assert!(result.outcome.is_ok());
        assert!(result.admission_stopped);
        assert!(coordinator.admission_stopped());
        clock.tick.store(1, Ordering::Relaxed);
        assert!(matches!(
            coordinator.create_submission(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(item.email),
                &mut meter()
            ),
            Err(SubmissionError::Storage(UploadError::Store(
                ports::Error::WriterStopped
            )))
        ));
        vec![result]
    });
}
