#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{work::Charge as WorkCharge, DiskLimits, ViewMode, WorkLimits},
    compose_text::Address,
    format::row::MailboxRow,
    limits::Limits,
    store_fs::tests::Fixture,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([3; 16]);
const MAILBOX: MailboxId = MailboxId::from_bytes([2; 16]);
const MAXIMUM: u64 = 32768;
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
struct Policy {
    calls: AtomicU64,
    revoke_second: AtomicBool,
    change_generation: AtomicBool,
}
struct Guard(Access);
impl UploadGuard for Guard {
    fn access(&self) -> Access {
        self.0
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
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        if account != ACCOUNT
            || device != DEVICE
            || (call % 2 == 1 && self.revoke_second.load(Ordering::Relaxed))
        {
            return Err(ports::Error::Forbidden);
        }
        Ok(Guard(Access {
            account,
            principal: Principal::Device(device),
            config_generation: 1 + u64::from(
                call % 2 == 1 && self.change_generation.load(Ordering::Relaxed),
            ),
        }))
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
            io_bytes: 1_000_000,
            output_bytes: 1_000_000,
            records: 100_000,
            unlinks: 0,
        },
    )
}
fn request() -> DraftRequest {
    DraftRequest {
        account: ACCOUNT,
        device: DEVICE,
        mailbox: MAILBOX,
        expected: None,
        maximum: MAXIMUM,
        deadline: deadline(),
    }
}
fn message(id: &str) -> Message<'_> {
    Message {
        from: Address {
            name: "Sender",
            email: "sender@example.test",
        },
        to: &[],
        cc: &[],
        bcc: &[],
        reply_to: &[],
        subject: "Hello",
        message_id: id,
        in_reply_to: &[],
        references: &[],
        sent_at_ms: 0,
        text: "hello\nworld",
    }
}
fn fixture(
    run: impl FnOnce(
        &StoreCoordinator<'_, '_, Policy>,
        &IngressSpool<'_>,
        &Arc<Timer>,
    ) -> Vec<DraftCompletion>,
) {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool_fixture = Fixture::new();
    let mut spool_root = spool_fixture.locked();
    let clock = Arc::new(Timer(AtomicU64::new(1)));
    let resources = Limits {
        message_bytes: MAXIMUM as usize,
        header_bytes: 8192,
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
                utc_ms: 0,
                deadline: deadline(),
            },
            &[Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &row[..n]).unwrap()],
            &mut [],
        )
        .unwrap();
    let spool = IngressSpool::open(&mut spool_root, &resources, clock.clone(), deadline()).unwrap();
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
            revoke_second: AtomicBool::new(false),
            change_generation: AtomicBool::new(false),
        },
        deadline(),
    )
    .unwrap();
    let expected = run(&coordinator, &spool, &clock);
    assert_eq!(spool.status().unwrap().occupied_slots, 0);
    assert_eq!(
        coordinator.used(Kind::BlobCount).unwrap(),
        expected.len() as u64
    );
    assert_eq!(
        coordinator.used(Kind::BodyBytes).unwrap(),
        expected.iter().map(|item| item.size).sum::<u64>()
    );
    drop(coordinator);
    clock.0.store(1, Ordering::Relaxed);
    let store = IndexStore::open(&mut root, clock, 2, deadline()).unwrap();
    store.validate_integrity(deadline()).unwrap();
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    let mut value = [0; 65536];
    let mut key = [0; 16];
    let mut after = None::<[u8; 16]>;
    let mut count = 0;
    while let Some(record) = view
        .next(
            Table::Emails,
            after.as_ref().map(|id| id.as_slice()),
            &mut key,
            &mut value,
        )
        .unwrap()
    {
        let Key::Email(id) = record.key else {
            panic!("email")
        };
        after = Some(*id.as_bytes());
        count += 1;
    }
    assert_eq!(count, expected.len());
    for item in expected {
        assert!(item.outcome.is_ok());
        assert!(!item.admission_stopped);
        assert!(item.cleanup_error.is_none());
        let Some((Row::Email(row), _)) = view.get(Key::Email(item.email), &mut value).unwrap()
        else {
            panic!("email")
        };
        assert_eq!(row.origin, EmailOrigin::Jmap);
        assert_eq!(
            (row.blob, row.thread, row.received_at),
            (item.blob, item.thread, 0)
        );
        assert!(view
            .get(Key::Membership(item.email, MAILBOX), &mut value)
            .unwrap()
            .is_some());
        for flag in ["$draft", "$seen"] {
            assert!(view
                .get(Key::Keyword(item.email, flag), &mut value)
                .unwrap()
                .is_some());
        }
        let mut input = view
            .open_blob_input(&td_crypto::Provider, item.blob, MAXIMUM)
            .unwrap();
        let mut body = vec![0; item.size as usize];
        let mut used = 0;
        while used < body.len() {
            let n = input.read(&mut body[used..]).unwrap();
            assert!(n > 0);
            used += n;
        }
        assert_eq!(input.read(&mut [0; 1]).unwrap(), 0);
        input.finish().unwrap();
        assert!(body.starts_with(b"From:"));
        assert!(body.ends_with(b"aGVsbG8NCndvcmxk\r\n"));
    }
}
#[test]
fn draft_body_metadata_flags_and_thread_survive_reopen() {
    fixture(|coordinator, spool, _| {
        let bcc = [Address {
            name: "Private",
            email: "hidden@example.test",
        }];
        let mut first = message("first@example.test");
        first.bcc = &bcc;
        let mut random = Random(10);
        let one = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &first,
                &mut meter(),
            )
            .unwrap();
        let mut second = message("second@example.test");
        second.references = &["first@example.test"];
        let two = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &second,
                &mut meter(),
            )
            .unwrap();
        for (item, has_bcc) in [(&one, true), (&two, false)] {
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut input = view
                .open_blob_input(&td_crypto::Provider, item.blob, MAXIMUM)
                .unwrap();
            let mut bytes = vec![0; item.size as usize];
            assert_eq!(input.read(&mut bytes).unwrap(), bytes.len());
            input.finish().unwrap();
            assert_eq!(
                bytes
                    .windows(b"hidden@example.test".len())
                    .any(|v| v == b"hidden@example.test"),
                has_bcc
            );
        }
        assert_eq!(one.thread, two.thread);
        assert_ne!(one.email, two.email);
        assert_eq!(coordinator.authorization.calls.load(Ordering::Relaxed), 4);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.thread_anchor("second@example.test", &mut [0; 65536])
                .unwrap(),
            Some((two.email, two.thread))
        );
        vec![one, two]
    });
}
#[test]
fn invalid_fields_quota_meter_and_revocation_publish_nothing_and_release_reservations() {
    fixture(|coordinator, spool, _| {
        let mut random = Random(10);
        let mut invalid = message("valid@example.test");
        invalid.subject = "bad\r\nBcc: x";
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &invalid,
                &mut meter()
            ),
            Err(UploadError::Store(ports::Error::Invalid))
        ));
        let small = DraftRequest {
            maximum: 1,
            ..request()
        };
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                small,
                &message("valid@example.test"),
                &mut meter()
            ),
            Err(UploadError::Store(ports::Error::Quota))
        ));
        let mut empty = Meter::new(deadline(), WorkCharge::default());
        assert!(coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &message("valid@example.test"),
                &mut empty
            )
            .is_err());
        coordinator.authorization.calls.store(0, Ordering::Relaxed);
        coordinator
            .authorization
            .revoke_second
            .store(true, Ordering::Relaxed);
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &message("valid@example.test"),
                &mut meter()
            ),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
        coordinator
            .authorization
            .revoke_second
            .store(false, Ordering::Relaxed);
        let good = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &message("good@example.test"),
                &mut meter(),
            )
            .unwrap();
        vec![good]
    });
}
#[test]
fn wrong_device_missing_mailbox_stale_state_and_deadline_refuse() {
    fixture(|coordinator, spool, clock| {
        for (req, error) in [
            (
                DraftRequest {
                    device: DeviceId::from_bytes([9; 16]),
                    ..request()
                },
                ports::Error::Forbidden,
            ),
            (
                DraftRequest {
                    mailbox: MailboxId::from_bytes([9; 16]),
                    ..request()
                },
                ports::Error::NotFound,
            ),
            (
                DraftRequest {
                    expected: Some((coordinator.store.epoch(), Sequence::default())),
                    ..request()
                },
                ports::Error::Conflict,
            ),
            (
                DraftRequest {
                    maximum: MAXIMUM + 1,
                    ..request()
                },
                ports::Error::Capacity,
            ),
        ] {
            assert!(
                matches!(coordinator.create_text_draft(&td_crypto::Provider, &mut Random(10), spool, req, &message("valid@example.test"), &mut meter()), Err(UploadError::Store(actual)) if actual == error)
            );
        }
        clock.0.store(100, Ordering::Relaxed);
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                request(),
                &message("valid@example.test"),
                &mut meter()
            ),
            Err(UploadError::Store(ports::Error::Deadline))
        ));
        vec![]
    });
}
#[test]
fn collisions_and_native_transaction_refusal_leave_no_partial_draft() {
    fixture(|coordinator, spool, _| {
        let first = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                request(),
                &message("first@example.test"),
                &mut meter(),
            )
            .unwrap();
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                request(),
                &message("second@example.test"),
                &mut meter()
            ),
            Err(UploadError::Store(ports::Error::Conflict))
        ));
        {
            let writer = lock(&coordinator.store.writer).unwrap();
            lock(&writer.native.connection)
                .unwrap()
                .authorizer(Some(|ctx: AuthContext<'_>| {
                    if matches!(
                        ctx.action,
                        AuthAction::Insert {
                            table_name: "emails"
                        }
                    ) {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                }))
                .unwrap();
        }
        let refused = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut Random(30),
                spool,
                request(),
                &message("third@example.test"),
                &mut meter(),
            )
            .unwrap();
        assert!(matches!(refused.outcome, Err(CommitError::Rejected(_))));
        assert!(!refused.admission_stopped);
        assert!(refused.cleanup_error.is_none());
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        {
            let writer = lock(&coordinator.store.writer).unwrap();
            lock(&writer.native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
        }
        let next = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut Random(50),
                spool,
                request(),
                &message("after@example.test"),
                &mut meter(),
            )
            .unwrap();
        assert!(next.outcome.is_ok());
        vec![first, next]
    });
}

#[test]
fn references_precede_reply_and_own_id_and_only_last_32_resolve() {
    fixture(|coordinator, spool, _| {
        let mut random = Random(10);
        let mut results = Vec::new();
        for id in ["one@example.test", "two@example.test"] {
            results.push(
                coordinator
                    .create_text_draft(
                        &td_crypto::Provider,
                        &mut random,
                        spool,
                        request(),
                        &message(id),
                        &mut meter(),
                    )
                    .unwrap(),
            );
        }
        let mut reply = message("one@example.test");
        reply.references = &["one@example.test", "two@example.test"];
        reply.in_reply_to = &["one@example.test"];
        let third = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &reply,
                &mut meter(),
            )
            .unwrap();
        assert_eq!(third.thread, results[1].thread);
        results.push(third);
        let own = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &message("one@example.test"),
                &mut meter(),
            )
            .unwrap();
        assert_eq!(own.thread, results[0].thread);
        results.push(own);
        let mut ids = ["absent@example.test"; 33];
        ids[0] = "one@example.test";
        let mut no_match = message("last@example.test");
        no_match.references = &ids;
        let last = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &no_match,
                &mut meter(),
            )
            .unwrap();
        assert_ne!(last.thread, results[0].thread);
        assert_ne!(last.thread, results[1].thread);
        results.push(last);
        results
    });
}
struct Rename<'s, 'r> {
    store: &'s IndexStore<'r>,
    before: ViewIdentity,
}
impl Entropy for Rename<'_, '_> {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
        bytes.fill(40);
        let mut row = [0; 128];
        let n = Row::Mailbox(MailboxRow {
            name: "Renamed",
            parent: None,
            role: None,
            sort_order: 0,
            subscribed: true,
        })
        .encode(&mut row)
        .unwrap();
        self.store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: self.before.epoch,
                    expected: self.before.committed_sequence,
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &row[..n]).unwrap()],
                &mut [],
            )
            .unwrap();
        Ok(())
    }
}
#[test]
fn state_precondition_is_rechecked_after_private_staging() {
    fixture(|coordinator, spool, _| {
        let before = coordinator
            .store
            .view(ACCOUNT, deadline())
            .unwrap()
            .identity();
        let req = DraftRequest {
            expected: Some((before.epoch, before.committed_sequence)),
            ..request()
        };
        let mut entropy = Rename {
            store: &coordinator.store,
            before,
        };
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut entropy,
                spool,
                req,
                &message("draft@example.test"),
                &mut meter()
            ),
            Err(UploadError::Store(ports::Error::Conflict))
        ));
        vec![]
    });
}

impl EmailUpdateAuthorization for Policy {
    type Guard<'a> = Guard;
    fn authorize_email_update(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Guard, ports::Error> {
        self.authorize_draft(account, device, deadline)
    }
}
struct Script(std::vec::IntoIter<u8>);
impl Entropy for Script {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
        bytes.fill(self.0.next().unwrap());
        Ok(())
    }
}
fn destroy(coordinator: &StoreCoordinator<'_, '_, Policy>, email: EmailId) {
    let complete = coordinator
        .destroy_emails(
            &td_crypto::Provider,
            EmailDestroyRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                emails: &[email],
                deadline: deadline(),
            },
            &mut meter(),
        )
        .unwrap();
    assert!(complete.outcome.is_ok());
}
#[test]
fn live_and_retained_email_and_thread_collisions_refuse_after_staging() {
    for removed in [false, true] {
        fixture(|coordinator, spool, _| {
            let first = coordinator
                .create_text_draft(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    request(),
                    &message("first@example.test"),
                    &mut meter(),
                )
                .unwrap();
            if removed {
                destroy(coordinator, first.email);
            }
            for values in [vec![40, 12, 41], vec![40, 41, 13]] {
                assert!(matches!(
                    coordinator.create_text_draft(
                        &td_crypto::Provider,
                        &mut Script(values.into_iter()),
                        spool,
                        request(),
                        &message("new@example.test"),
                        &mut meter()
                    ),
                    Err(UploadError::Store(ports::Error::Conflict))
                ));
                assert_eq!(spool.status().unwrap().occupied_slots, 0);
            }
            if removed {
                vec![]
            } else {
                vec![first]
            }
        });
    }
}
#[test]
fn native_body_read_exhaustion_and_generation_change_release_private_staging() {
    fixture(|coordinator, spool, clock| {
        let text = message("budget@example.test");
        let mut composition = meter();
        let size =
            compose_text::compose(&text, MAXIMUM, clock.as_ref(), &mut composition, |_| Ok(()))
                .unwrap();
        let used = meter().remaining().io_bytes - composition.remaining().io_bytes;
        let mut budget = Meter::new(
            deadline(),
            WorkCharge {
                io_bytes: used + size - 1,
                ..meter().remaining()
            },
        );
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                request(),
                &text,
                &mut budget
            ),
            Err(UploadError::Store(ports::Error::Capacity))
        ));
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
        coordinator.authorization.calls.store(0, Ordering::Relaxed);
        coordinator
            .authorization
            .change_generation
            .store(true, Ordering::Relaxed);
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut Random(30),
                spool,
                request(),
                &text,
                &mut meter()
            ),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        coordinator
            .authorization
            .change_generation
            .store(false, Ordering::Relaxed);
        let good = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut Random(50),
                spool,
                request(),
                &text,
                &mut meter(),
            )
            .unwrap();
        vec![good]
    });
}
#[test]
fn history_walk_pays_each_row_and_a_shorter_phase_keeps_the_original_meter() {
    fixture(|coordinator, spool, clock| {
        let phase = Deadline::after(Tick(0), 50).unwrap();
        let mut original = meter();
        let first = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                DraftRequest {
                    deadline: phase,
                    ..request()
                },
                &message("phase@example.test"),
                &mut original,
            )
            .unwrap();
        assert_eq!(original.deadline(), deadline().into());
        assert!(original.remaining().records < meter().remaining().records);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        let mut budget = Meter::new(
            deadline(),
            WorkCharge {
                records: 1,
                ..WorkCharge::default()
            },
        );
        assert_eq!(
            coordinator.used_identifier(
                &mut view,
                (ObjectType::Email, &[80; 16]),
                deadline(),
                &mut Tick(1),
                &mut budget
            ),
            Err(ports::Error::Capacity)
        );
        let bounded = ComposeClock {
            clock: clock.as_ref(),
            last: AtomicU64::new(1),
            deadline: phase,
        };
        clock.0.store(50, Ordering::Relaxed);
        assert_eq!(bounded.sample(), Err(ports::Error::Deadline));
        clock.0.store(1, Ordering::Relaxed);
        vec![first]
    });
}
#[test]
fn both_reply_lists_include_the_32nd_and_exclude_the_33rd_candidate() {
    fixture(|coordinator, spool, _| {
        let mut random = Random(10);
        let base = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &message("base@example.test"),
                &mut meter(),
            )
            .unwrap();
        let thread = base.thread;
        let mut results = vec![base];
        for reply in [false, true] {
            for length in [32, 33] {
                let mut ids = vec!["absent@example.test"; length];
                ids[0] = "base@example.test";
                let id = format!("{reply}-{length}@example.test");
                let mut text = message(&id);
                if reply {
                    text.in_reply_to = &ids;
                } else {
                    text.references = &ids;
                }
                let result = coordinator
                    .create_text_draft(
                        &td_crypto::Provider,
                        &mut random,
                        spool,
                        request(),
                        &text,
                        &mut meter(),
                    )
                    .unwrap();
                assert_eq!(result.thread == thread, length == 32);
                results.push(result);
            }
        }
        results
    });
}

struct Receiving;
impl DeliveryGuard for Receiving {
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
            tls: crate::format::row::ReceiptTls::Plain,
            gateway: None,
        }
    }
}
impl DeliveryAuthorization for Receiving {
    type Guard<'a> = Receiving;
    fn authorize(&self, account: AccountId, _: Deadline) -> Result<Receiving, ports::Error> {
        assert_eq!(account, ACCOUNT);
        Ok(Receiving)
    }
}
fn receive<'r>(
    coordinator: &StoreCoordinator<'r, '_, Policy>,
    spool: &IngressSpool<'r>,
    random: &mut Random,
    source: &[u8],
) -> DeliveryCompletion {
    use crate::{
        config::{
            routing::{AliasSlot, Builder, DomainSlot},
            syntax::Location,
        },
        smtp_session::{Pending, Session, Settings},
    };
    let mut text = [0; 1024];
    let mut domains = [DomainSlot::EMPTY; 1];
    let mut aliases = [AliasSlot::EMPTY; 1];
    let at = Location {
        line: std::num::NonZeroU32::new(1).unwrap(),
        column: 1,
    };
    let mut builder = Builder::new(&mut text, &mut domains, &mut aliases).unwrap();
    builder.account(ACCOUNT, at).unwrap();
    builder.domain("example.test", at).unwrap();
    builder.alias("user@example.test", ACCOUNT, at).unwrap();
    let routes = builder.finish().unwrap();
    let mut session = Session::new(
        &routes,
        Settings {
            hostname: "mx.example.test",
            message_bytes: MAXIMUM as usize,
            trace_bytes: smtp_trace_allowance("mx.example.test").unwrap(),
            recipients: 100,
            starttls: false,
        },
    )
    .unwrap();
    session.reply_sent().unwrap();
    for line in [
        b"EHLO sender.test\r\n".as_slice(),
        b"MAIL FROM:<>\r\n",
        b"RCPT TO:<user@example.test>\r\n",
    ] {
        assert_eq!(session.feed(line).unwrap(), line.len());
        assert!(
            matches!(session.pending(), Pending::Reply { bytes, .. } if bytes.starts_with(b"250"))
        );
        session.reply_sent().unwrap();
    }
    assert_eq!(session.feed(b"DATA\r\n").unwrap(), 6);
    let mut delivery = coordinator
        .reserve_delivery(
            &td_crypto::Provider,
            random,
            spool,
            Receiving,
            &session,
            DeliveryRequest {
                header_bytes: 8192,
                deadline: deadline(),
            },
        )
        .unwrap();
    session.data_ready(Ok(())).unwrap();
    session.reply_sent().unwrap();
    for byte in source {
        assert_eq!(session.feed(&[*byte]).unwrap(), 1);
        if let Pending::Data(line) = session.pending() {
            delivery.write(line).unwrap();
            session.data_written(Ok(())).unwrap();
        }
    }
    assert_eq!(session.feed(b".\r\n").unwrap(), 3);
    delivery.prepare(deadline()).unwrap();
    let complete = delivery.commit().unwrap();
    assert!(complete.outcome.is_ok());
    complete
}
#[test]
fn drafts_and_received_messages_share_exact_parser_anchors_in_both_directions() {
    fixture(|coordinator, spool, _| {
        let before = coordinator
            .store
            .view(ACCOUNT, deadline())
            .unwrap()
            .identity();
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
        coordinator
            .store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: before.epoch,
                    expected: before.committed_sequence,
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::put(Table::Mailboxes, MAILBOX.as_bytes(), &row[..n]).unwrap()],
                &mut [],
            )
            .unwrap();
        let mut random = Random(10);
        let received = receive(
            coordinator,
            spool,
            &mut random,
            b"Message-ID: (comment) <shared@[(a),;\"@]>\r\n\r\nbody\r\n",
        );
        let mut text = message("draft@[IPv6:2001:db8::1]");
        text.references = &["shared@[(a),;\"@]"];
        let draft = coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut random,
                spool,
                request(),
                &text,
                &mut meter(),
            )
            .unwrap();
        assert_eq!(draft.thread, received.thread);
        destroy(coordinator, received.email);
        let reply = receive(coordinator, spool, &mut random, b"Message-ID: <reply@example.test>\r\nReferences: <draft@[IPv6:2001:db8::1]>\r\n\r\nbody\r\n");
        assert_eq!(reply.thread, draft.thread);
        destroy(coordinator, reply.email);
        vec![draft]
    });
}

#[test]
fn exhausted_staging_is_busy_and_creation_recovers_after_slots_are_released() {
    fixture(|coordinator, spool, _| {
        let mut held = Vec::new();
        for slot in 0..spool.capacity().slots {
            held.push(
                spool
                    .begin(
                        &td_crypto::Provider,
                        ACCOUNT,
                        BlobId::from_bytes([slot; 16]),
                        deadline(),
                    )
                    .unwrap(),
            );
        }
        assert!(matches!(
            coordinator.create_text_draft(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                request(),
                &message("busy@example.test"),
                &mut meter(),
            ),
            Err(UploadError::Store(ports::Error::Busy))
        ));
        assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
        assert_eq!(
            coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity()
                .committed_sequence,
            Sequence::from_u64(1),
        );
        drop(held);
        vec![coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut Random(20),
                spool,
                request(),
                &message("after-busy@example.test"),
                &mut meter(),
            )
            .unwrap()]
    });
}
