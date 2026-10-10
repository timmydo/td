#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{DiskLimits, ViewMode, WorkLimits},
    limits::Limits,
    store_fs::tests::Fixture,
};
use std::sync::{
    atomic::{AtomicI64, AtomicU64, Ordering},
    MutexGuard, TryLockError,
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);
const SEED: BlobId = BlobId::from_bytes([3; 16]);
struct Timer(AtomicU64, Mutex<std::collections::VecDeque<u64>>, AtomicI64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        if let Some(next) = self.1.lock().unwrap().pop_front() {
            self.0.store(next, Ordering::Relaxed);
        }
        Ok(Time {
            utc_ms: self.2.load(Ordering::Relaxed),
            monotonic: Tick(self.0.load(Ordering::Relaxed)),
        })
    }
}
struct PolicyState {
    allowed: bool,
    generation: u64,
    device: DeviceId,
}
struct Policy(Arc<Mutex<PolicyState>>);
struct Guard<'a>(MutexGuard<'a, PolicyState>);
impl UploadGuard for Guard<'_> {
    fn access(&self) -> Access {
        Access {
            account: ACCOUNT,
            principal: Principal::Device(self.0.device),
            config_generation: self.0.generation,
        }
    }
}
impl UploadAuthorization for Policy {
    type Guard<'a> = Guard<'a>;
    fn authorize(
        &self,
        account: AccountId,
        device: DeviceId,
        _deadline: Deadline,
    ) -> Result<Guard<'_>, ports::Error> {
        let guard = self.0.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => ports::Error::Busy,
            TryLockError::Poisoned(_) => ports::Error::Forbidden,
        })?;
        if !guard.allowed || account != ACCOUNT || device != DEVICE {
            return Err(ports::Error::Forbidden);
        }
        Ok(Guard(guard))
    }
}
struct Random(u8);
impl Entropy for Random {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
        self.0 = self.0.checked_add(1).ok_or(td_crypto::Error::Entropy)?;
        output.fill(self.0);
        Ok(())
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn request(maximum: u64) -> UploadRequest {
    UploadRequest {
        account: ACCOUNT,
        device: DEVICE,
        maximum,
        deadline: deadline(),
    }
}
fn row_bytes(row: Row<'_>) -> Vec<u8> {
    let mut bytes = vec![0; 128];
    let n = row.encode(&mut bytes).unwrap();
    bytes.truncate(n);
    bytes
}
fn seed(store: &IndexStore<'_>, expires_at: i64) {
    seed_at(store, ACCOUNT, SEED, expires_at);
}
fn seed_at(store: &IndexStore<'_>, account: AccountId, id: BlobId, expires_at: i64) {
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(b"seed").unwrap();
    let blob = row_bytes(Row::Blob(crate::format::row::BlobRow {
        length: 4,
        digest: hash.finish().unwrap(),
        created_at: 0,
    }));
    let lease = row_bytes(Row::Lease(LeaseRow {
        account,
        device: DEVICE,
        expires_at,
        uses: LeaseUse::Both,
    }));
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account,
                epoch: store.epoch(),
                expected: store
                    .view(account, deadline())
                    .unwrap()
                    .identity()
                    .committed_sequence,
                utc_ms: 0,
                deadline: deadline(),
            },
            &[
                Operation::put(Table::Blobs, id.as_bytes(), &blob).unwrap(),
                Operation::put(Table::Leases, id.as_bytes(), &lease).unwrap(),
            ],
            &mut [BlobSource {
                id,
                source: &mut b"seed".as_slice(),
            }],
        )
        .unwrap();
}
fn fixture(
    run: impl for<'r, 'a, 's> FnOnce(
        &mut StoreCoordinator<'r, 'a, Policy>,
        &'s IngressSpool<'r>,
        &Arc<Timer>,
        &Arc<Mutex<PolicyState>>,
        &std::path::Path,
    ) -> Option<BlobId>,
) {
    fixture_cells(4, run);
}
fn fixture_cells(
    count: usize,
    run: impl for<'r, 'a, 's> FnOnce(
        &mut StoreCoordinator<'r, 'a, Policy>,
        &'s IngressSpool<'r>,
        &Arc<Timer>,
        &Arc<Mutex<PolicyState>>,
        &std::path::Path,
    ) -> Option<BlobId>,
) {
    fixture_config(count, 100000, |_| {}, run);
}
fn fixture_config(
    count: usize,
    expires_at: i64,
    setup: impl FnOnce(&IndexStore<'_>),
    run: impl for<'r, 'a, 's> FnOnce(
        &mut StoreCoordinator<'r, 'a, Policy>,
        &'s IngressSpool<'r>,
        &Arc<Timer>,
        &Arc<Mutex<PolicyState>>,
        &std::path::Path,
    ) -> Option<BlobId>,
) {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool_fixture = Fixture::new();
    let mut spool_root = spool_fixture.locked();
    let clock = Arc::new(Timer(
        AtomicU64::new(1),
        Mutex::new(std::collections::VecDeque::new()),
        AtomicI64::new(1234),
    ));
    let resources = Limits {
        message_bytes: 8,
        header_bytes: 8,
        ..Limits::default()
    }
    .plan()
    .unwrap();
    let plan = DiskLimits {
        body_bytes: 16,
        blob_count: 2,
        ..DiskLimits::default()
    }
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
    seed(&store, expires_at);
    setup(&store);
    let spool = IngressSpool::open(&mut spool_root, &resources, clock.clone(), deadline()).unwrap();
    let mut states = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { LeaseCell::EMPTY }; 4];
    let policy = Arc::new(Mutex::new(PolicyState {
        allowed: true,
        generation: 1,
        device: DEVICE,
    }));
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
        &mut states[..count],
        &mut cells[..count],
        Policy(policy.clone()),
        deadline(),
    );
    if count < 2 {
        assert!(matches!(
            coordinator,
            Err(LedgerInitError::Ledger(logical::Error::Full))
        ));
        return;
    }
    let mut coordinator = coordinator.unwrap();
    assert_recount(&coordinator);
    let published = run(
        &mut coordinator,
        &spool,
        &clock,
        &policy,
        &spool_fixture.path,
    );
    drop(coordinator);
    clock.0.store(1, Ordering::Relaxed);
    let store = IndexStore::open(&mut root, clock, 2, deadline()).unwrap();
    store.validate_integrity(deadline()).unwrap();
    if let Some(id) = published {
        let mut view = store.view(ACCOUNT, deadline()).unwrap();
        let mut value = [0; 64];
        assert_eq!(
            view.get(Key::Lease(id), &mut value).unwrap().unwrap().0,
            Row::Lease(LeaseRow {
                account: ACCOUNT,
                device: DEVICE,
                uses: LeaseUse::Both,
                expires_at: 1234 + 86400000
            })
        );
        let mut input = view.open_blob_input(&td_crypto::Provider, id, 8).unwrap();
        let mut bytes = [0; 8];
        let n = input.read(&mut bytes).unwrap();
        assert_eq!(&bytes[..n], b"body");
        input.finish().unwrap();
    }
}
#[test]
fn upload_reconciles_nonempty_usage_and_reopens_exact_body_and_lease() {
    fixture(|coordinator, spool, _, _, _| {
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        assert_eq!(spool.status().unwrap().occupied_slots, 1);
        upload.write(b"bo").unwrap();
        upload.write(b"dy").unwrap();
        upload.prepare().unwrap();
        let id = upload.id();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert_eq!(result.outcome, Ok(Sequence::from_u64(2)));
        assert!(!result.admission_stopped);
        assert_eq!(result.cleanup_error, None);
        drop(upload);
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
        assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 2);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 8);
        let before = spool.status().unwrap();
        assert!(matches!(
            coordinator.reserve(&td_crypto::Provider, &mut Random(9), spool, request(1)),
            Err(UploadError::Ledger(logical::Error::Quota(Kind::BlobCount)))
        ));
        assert_eq!(spool.status().unwrap(), before);
        Some(id)
    });
}
#[test]
fn revocation_and_expiry_cannot_publish_staged_bytes() {
    fixture(|coordinator, spool, clock, policy, _| {
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        policy.lock().unwrap().allowed = false;
        assert!(matches!(
            upload.commit(),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        policy.lock().unwrap().allowed = true;
        clock.0.store(100, Ordering::Relaxed);
        assert!(matches!(
            upload.commit(),
            Err(UploadError::Store(ports::Error::Deadline))
        ));
        clock.0.store(1, Ordering::Relaxed);
        assert!(matches!(
            upload.commit(),
            Err(UploadError::Store(ports::Error::Deadline))
        ));
        upload.discard().unwrap();
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        assert!(!coordinator.admission_stopped());
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        None
    });
}
#[test]
fn authorization_guard_spans_native_commit_and_is_released_afterward() {
    fixture(|coordinator, spool, _, policy, _| {
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let check = observed.clone();
        let shared = policy.clone();
        lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                    assert!(matches!(shared.try_lock(), Err(TryLockError::WouldBlock)));
                    check.store(true, Ordering::Relaxed);
                }
                Authorization::Allow
            })).unwrap();
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        let id = upload.id();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert!(result.outcome.is_ok());
        assert!(observed.load(Ordering::Relaxed));
        assert!(policy.try_lock().is_ok());
        Some(id)
    });
}
#[test]
fn only_proven_sequence_conflict_permits_one_original_deadline_replan() {
    fixture(|coordinator, spool, _, policy, _| {
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        assert!(upload.replan().is_err());
        // Simulate another admitted mutation at the private core boundary.
        let lease = row_bytes(Row::Lease(LeaseRow {
            account: ACCOUNT,
            device: DEVICE,
            expires_at: 200000,
            uses: LeaseUse::Both,
        }));
        upload
            .coordinator
            .store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: upload.identity.epoch,
                    expected: upload.identity.committed_sequence,
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[Operation::put(Table::Leases, SEED.as_bytes(), &lease).unwrap()],
                &mut [],
            )
            .unwrap();
        assert!(matches!(
            upload.commit().unwrap(),
            UploadAttempt::SequenceConflict
        ));
        assert!(matches!(
            upload.commit(),
            Err(UploadError::Store(ports::Error::Conflict))
        ));
        assert_eq!(
            upload.coordinator.ledger.pending(Kind::BodyBytes).unwrap(),
            4
        );
        assert_eq!(
            upload.coordinator.ledger.pending(Kind::BlobCount).unwrap(),
            1
        );
        assert_eq!(
            upload
                .coordinator
                .ledger
                .pending(Kind::UploadBytes)
                .unwrap(),
            4
        );
        for kind in [Kind::DatabaseBytes, Kind::WalBytes] {
            assert_eq!(upload.coordinator.ledger.pending(kind).unwrap(), 0);
        }
        policy.lock().unwrap().allowed = false;
        assert!(matches!(
            upload.replan(),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        policy.lock().unwrap().allowed = true;
        policy.lock().unwrap().generation += 1;
        assert!(matches!(
            upload.replan(),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        policy.lock().unwrap().generation -= 1;
        upload.replan().unwrap();
        assert_eq!(upload.deadline, deadline());
        assert!(upload.replan().is_err());
        let id = upload.id();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert_eq!(result.outcome, Ok(Sequence::from_u64(3)));
        assert!(!result.admission_stopped);
        Some(id)
    });
}
#[test]
fn forgotten_upload_keeps_its_single_lane_and_spool_reservation() {
    fixture(|coordinator, spool, _, _, _| {
        let upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        std::mem::forget(upload);
        assert!(matches!(
            coordinator.reserve(&td_crypto::Provider, &mut Random(9), spool, request(4)),
            Err(UploadError::Store(ports::Error::Busy))
        ));
        assert_eq!(spool.status().unwrap().occupied_slots, 1);
        assert!(matches!(
            coordinator.checkpoint(deadline()),
            Err(UploadError::Store(ports::Error::Busy))
        ));
        assert_eq!(coordinator.ledger.pending(Kind::BodyBytes).unwrap(), 4);
        None
    });
}
#[test]
fn durable_and_indeterminate_outcomes_survive_accounting_failure() {
    for deny in [false, true] {
        fixture(|coordinator, spool, clock, _, _| {
            let timer = clock.clone();
            lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Transaction { operation }
                        if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                        if deny { return Authorization::Deny; }
                        timer.0.store(100, Ordering::Relaxed);
                    }
                    Authorization::Allow
                })).unwrap();
            let mut upload = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            upload.write(b"body").unwrap();
            upload.prepare().unwrap();
            let id = upload.id();
            let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
                panic!("commit")
            };
            assert!(result.admission_stopped);
            if deny {
                assert!(matches!(result.outcome, Err(CommitError::Indeterminate(_))));
            } else {
                assert_eq!(result.outcome, Ok(Sequence::from_u64(2)));
            }
            assert!(matches!(
                upload.commit(),
                Err(UploadError::Store(ports::Error::WriterStopped))
            ));
            drop(upload);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
            assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 8);
            assert!(coordinator.admission_stopped());
            if !deny {
                assert_eq!(
                    coordinator.used(Kind::DatabaseBytes).unwrap(),
                    MAX_PAGES * PAGE_BYTES
                );
                assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), MAX_WAL_BYTES);
            }
            for kind in [Kind::DatabaseBytes, Kind::WalBytes] {
                assert_eq!(coordinator.ledger.pending(kind).unwrap(), 0);
            }
            (!deny).then_some(id)
        });
    }
}

#[test]
fn expiry_at_native_handoff_is_proven_unchanged_and_keeps_admission_usable() {
    fixture(|coordinator, spool, clock, _, _| {
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        *clock.1.lock().unwrap() = [1, 1, 100].into();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert_eq!(
            result.outcome,
            Err(CommitError::Rejected(ports::Error::Deadline))
        );
        assert!(!result.admission_stopped);
        drop(upload);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        let later = Deadline::after(Tick(0), 200).unwrap();
        coordinator
            .reserve(
                &td_crypto::Provider,
                &mut Random(9),
                spool,
                UploadRequest {
                    deadline: later,
                    ..request(4)
                },
            )
            .unwrap()
            .discard()
            .unwrap();
        None
    });
}

#[test]
fn exhausted_spool_and_oversized_write_do_not_escape_the_reservation() {
    fixture(|coordinator, spool, _, _, _| {
        let mut held = Vec::new();
        for _ in 0..spool.capacity().slots {
            held.push(
                spool
                    .begin(&td_crypto::Provider, ACCOUNT, SEED, deadline())
                    .unwrap(),
            );
        }
        assert!(matches!(
            coordinator.reserve(&td_crypto::Provider, &mut Random(8), spool, request(4)),
            Err(UploadError::Store(ports::Error::Quota))
        ));
        assert_eq!(coordinator.ledger.pending(Kind::BodyBytes).unwrap(), 0);
        assert_eq!(coordinator.ledger.available_cells(), 4);
        drop(held);
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(9), spool, request(4))
            .unwrap();
        assert!(matches!(
            upload.write(b"too long"),
            Err(UploadError::Store(ports::Error::Quota))
        ));
        assert_eq!(upload.length, 0);
        upload.write(b"body").unwrap();
        upload.discard().unwrap();
        assert_eq!(spool.status().unwrap().occupied_slots, 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        None
    });
}
#[test]
fn id_collision_is_final_rejection_with_measured_physical_usage() {
    fixture(|coordinator, spool, _, _, _| {
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(2), spool, request(4))
            .unwrap();
        assert_eq!(upload.id(), SEED);
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("collision must not replan")
        };
        assert_eq!(
            result.outcome,
            Err(CommitError::Rejected(ports::Error::Conflict))
        );
        assert!(!result.admission_stopped);
        assert!(upload.replan().is_err());
        drop(upload);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
        let files = coordinator
            .store
            .usage_fence(deadline())
            .unwrap()
            .file_usage();
        assert_eq!(
            coordinator.used(Kind::DatabaseBytes).unwrap(),
            files.database_bytes
        );
        assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), files.wal_bytes);
        assert_eq!(coordinator.ledger.pending(Kind::DatabaseBytes).unwrap(), 0);
        assert_eq!(coordinator.ledger.pending(Kind::WalBytes).unwrap(), 0);
        None
    });
}
#[test]
fn cleanup_failure_after_success_retains_receipt_and_retired_slot() {
    fixture(|coordinator, spool, _, _, spool_path| {
        let original = spool_path.join("slot-00");
        let renamed = spool_path.join("removed-slot");
        lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                    fs::rename(&original, &renamed).unwrap();
                }
                Authorization::Allow
            })).unwrap();
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        let id = upload.id();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert_eq!(result.outcome, Ok(Sequence::from_u64(2)));
        assert!(result.cleanup_error.is_some());
        drop(upload);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
        assert_eq!(spool.status().unwrap().retired_slots, 1);
        Some(id)
    });
}
#[test]
fn stale_epoch_and_changed_policy_generation_cannot_be_rebound() {
    fixture(|coordinator, spool, _, policy, _| {
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        let identity = upload.identity;
        upload.identity.epoch = StoreEpoch::from_bytes([99; 16]);
        assert!(matches!(
            upload.commit(),
            Err(UploadError::Store(ports::Error::Invalid))
        ));
        upload.identity = identity;
        policy.lock().unwrap().generation += 1;
        assert!(matches!(
            upload.commit(),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        upload.discard().unwrap();
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        assert!(!coordinator.admission_stopped());
        None
    });
}

#[test]
fn reserve_refuses_policy_context_and_size_before_spooling() {
    fixture(|coordinator, spool, _, policy, _| {
        let before = spool.status().unwrap();
        policy.lock().unwrap().allowed = false;
        assert!(matches!(
            coordinator.reserve(&td_crypto::Provider, &mut Random(8), spool, request(4)),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        policy.lock().unwrap().allowed = true;
        policy.lock().unwrap().device = DeviceId::from_bytes([99; 16]);
        assert!(matches!(
            coordinator.reserve(&td_crypto::Provider, &mut Random(8), spool, request(4)),
            Err(UploadError::Store(ports::Error::Forbidden))
        ));
        policy.lock().unwrap().device = DEVICE;
        assert!(matches!(
            coordinator.reserve(
                &td_crypto::Provider,
                &mut Random(8),
                spool,
                request(spool.capacity().bytes_each + 1)
            ),
            Err(UploadError::Store(ports::Error::Capacity))
        ));
        assert_eq!(spool.status().unwrap(), before);
        for kind in [Kind::BodyBytes, Kind::BlobCount, Kind::UploadBytes] {
            assert_eq!(coordinator.ledger.pending(kind).unwrap(), 0);
        }
        assert!(!coordinator.admission_stopped());
        None
    });
}

#[test]
fn coordinator_refuses_missing_physical_effect_cell_at_initialization() {
    for count in [0, 1] {
        fixture_cells(count, |_, _, _, _, _| {
            panic!("undersized coordinator accepted")
        });
    }
}

#[test]
fn maintenance_checkpoints_and_reconciles_files_without_changing_logical_usage() {
    fixture(|coordinator, spool, _, _, _| {
        assert!(coordinator.used(Kind::WalBytes).unwrap() > 0);
        let before_database = coordinator.used(Kind::DatabaseBytes).unwrap();
        let result = coordinator.checkpoint(deadline()).unwrap();
        assert_eq!(result.outcome, Ok(()));
        assert!(!result.admission_stopped);
        let CommitFileUsage::Measured(files) = result.files else {
            panic!("missing files")
        };
        assert!(files.database_bytes >= before_database);
        assert_eq!(files.wal_bytes, 0);
        assert_eq!(
            coordinator.used(Kind::DatabaseBytes).unwrap(),
            files.database_bytes
        );
        assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        let id = upload.id();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert_eq!(result.outcome, Ok(Sequence::from_u64(2)));
        drop(upload);
        assert!(coordinator.used(Kind::WalBytes).unwrap() > 0);
        assert_eq!(coordinator.checkpoint(deadline()).unwrap().outcome, Ok(()));
        assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 8);
        for kind in [Kind::DatabaseBytes, Kind::WalBytes] {
            assert_eq!(coordinator.ledger.pending(kind).unwrap(), 0);
        }
        Some(id)
    });
}
#[test]
fn separate_maintenance_recovers_known_commit_but_refuses_indeterminate_commit() {
    for deny in [false, true] {
        fixture(|coordinator, spool, clock, _, _| {
            let timer = clock.clone();
            lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Transaction { operation }
                        if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                        timer.0.store(100, Ordering::Relaxed);
                        if deny { return Authorization::Deny; }
                    }
                    Authorization::Allow
                })).unwrap();
            let mut upload = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            upload.write(b"body").unwrap();
            upload.prepare().unwrap();
            let id = upload.id();
            let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
                panic!("commit")
            };
            assert!(result.admission_stopped);
            assert_eq!(upload.deadline, deadline());
            drop(upload);
            assert_eq!(spool.status().unwrap().occupied_slots, 0);
            clock.0.store(200, Ordering::Relaxed);
            let later = Deadline::after(Tick(200), 100).unwrap();
            if deny {
                assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), MAX_WAL_BYTES);
                assert!(matches!(result.outcome, Err(CommitError::Indeterminate(_))));
                assert!(matches!(
                    coordinator.checkpoint(later),
                    Err(UploadError::Store(ports::Error::WriterStopped))
                ));
                assert!(coordinator.admission_stopped());
                return None;
            }
            assert_eq!(result.outcome, Ok(Sequence::from_u64(2)));
            assert!(matches!(
                coordinator.checkpoint(deadline()),
                Err(UploadError::Store(ports::Error::Deadline))
            ));
            assert!(coordinator.admission_stopped());
            *clock.1.lock().unwrap() = [200, 200, 200, 300].into();
            let early = coordinator.checkpoint(later).unwrap();
            assert_eq!(early.outcome, Err(ports::Error::Deadline));
            assert_eq!(early.files, CommitFileUsage::Unchanged);
            assert!(early.admission_stopped);
            let later = Deadline::after(Tick(300), 100).unwrap();
            lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
                .authorizer(Some(|context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Pragma { pragma_name, .. } if pragma_name == "wal_checkpoint") {
                        Authorization::Deny
                    } else {
                        Authorization::Allow
                    }
                })).unwrap();
            let refused = coordinator.checkpoint(later).unwrap();
            assert!(refused.outcome.is_err());
            assert!(matches!(refused.files, CommitFileUsage::Measured(_)));
            assert!(refused.admission_stopped);
            assert!(coordinator.admission_stopped());
            lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            let maintenance = coordinator.checkpoint(later).unwrap();
            assert_eq!(maintenance.outcome, Ok(()));
            assert!(!maintenance.admission_stopped);
            let CommitFileUsage::Measured(files) = maintenance.files else {
                panic!("missing files")
            };
            assert_eq!(
                coordinator.used(Kind::DatabaseBytes).unwrap(),
                files.database_bytes
            );
            assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), 0);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
            assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 8);
            assert!(matches!(
                coordinator.reserve(
                    &td_crypto::Provider,
                    &mut Random(9),
                    spool,
                    UploadRequest {
                        deadline: later,
                        ..request(1)
                    }
                ),
                Err(UploadError::Ledger(logical::Error::Quota(Kind::BlobCount)))
            ));
            Some(id)
        });
    }
}
#[test]
fn maintenance_timeout_stays_conservative_until_a_separate_successful_measurement() {
    fixture(|coordinator, _, clock, _, _| {
        let timer = clock.clone();
        lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Pragma { pragma_name, .. } if pragma_name == "wal_checkpoint") {
                    timer.0.store(100, Ordering::Relaxed);
                }
                Authorization::Allow
            })).unwrap();
        let result = coordinator.checkpoint(deadline()).unwrap();
        assert_eq!(result.outcome, Err(ports::Error::Deadline));
        assert_eq!(
            result.files,
            CommitFileUsage::Unavailable(ports::Error::Deadline)
        );
        assert!(result.admission_stopped);
        assert_eq!(
            coordinator.used(Kind::DatabaseBytes).unwrap(),
            MAX_PAGES * PAGE_BYTES
        );
        assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), MAX_WAL_BYTES);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        lock(&lock(&coordinator.store.writer).unwrap().native.connection)
            .unwrap()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        let result = coordinator
            .checkpoint(Deadline::after(Tick(100), 100).unwrap())
            .unwrap();
        assert_eq!(result.outcome, Ok(()));
        assert!(!result.admission_stopped);
        assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        None
    });
}

#[test]
fn maintenance_after_timed_out_rollback_preserves_logical_quota_for_a_new_upload() {
    fixture(|coordinator, spool, clock, _, _| {
        let timer = clock.clone();
        lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Insert { table_name } if table_name == "blobs") {
                    timer.0.store(100, Ordering::Relaxed);
                }
                Authorization::Allow
            })).unwrap();
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        let old_id = upload.id();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert_eq!(
            result.outcome,
            Err(CommitError::Rejected(ports::Error::Deadline))
        );
        assert!(result.admission_stopped);
        drop(upload);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
        lock(&lock(&coordinator.store.writer).unwrap().native.connection)
            .unwrap()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        let later = Deadline::after(Tick(100), 100).unwrap();
        assert_eq!(coordinator.checkpoint(later).unwrap().outcome, Ok(()));
        assert!(!coordinator.admission_stopped());
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        assert_eq!(
            coordinator
                .store
                .view(ACCOUNT, later)
                .unwrap()
                .get(Key::Blob(old_id), &mut [0; 64])
                .unwrap(),
            None
        );
        let mut upload = coordinator
            .reserve(
                &td_crypto::Provider,
                &mut Random(9),
                spool,
                UploadRequest {
                    deadline: later,
                    ..request(4)
                },
            )
            .unwrap();
        upload.write(b"body").unwrap();
        upload.prepare().unwrap();
        let id = upload.id();
        let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
            panic!("commit")
        };
        assert_eq!(result.outcome, Ok(Sequence::from_u64(2)));
        assert!(!result.admission_stopped);
        Some(id)
    });
}

#[test]
fn maintenance_cannot_clear_a_stopped_native_writer() {
    fixture(|coordinator, _, _, _, _| {
        lock(&coordinator.store.writer).unwrap().stopped = true;
        let result = coordinator.checkpoint(deadline()).unwrap();
        assert_eq!(result.outcome, Err(ports::Error::WriterStopped));
        assert_eq!(result.files, CommitFileUsage::Unchanged);
        assert!(result.admission_stopped);
        assert!(matches!(
            coordinator.checkpoint(deadline()),
            Err(UploadError::Store(ports::Error::WriterStopped))
        ));
        None
    });
}

#[test]
fn expired_upload_releases_quota_and_preserves_permanent_ids() {
    fixture_config(
        4,
        1234,
        |_| {},
        |coordinator, spool, _, _, _| {
            let receipt = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .unwrap();
            assert_eq!(receipt.id, SEED);
            assert_eq!(
                receipt.outcome,
                Ok(UploadSweepDisposition::Retired {
                    sequence: Sequence::from_u64(2),
                    body_removed: true
                })
            );
            assert!(!receipt.admission_stopped);
            for kind in [Kind::BodyBytes, Kind::BlobCount, Kind::UploadBytes] {
                assert_eq!(coordinator.used(kind).unwrap(), 0);
            }
            assert!(coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .is_none());
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut bytes = [0; 64];
            assert!(view.get(Key::Blob(SEED), &mut bytes).unwrap().is_none());
            assert!(view.get(Key::Lease(SEED), &mut bytes).unwrap().is_none());
            drop(view);
            let mut reused = coordinator
                .reserve(&td_crypto::Provider, &mut Random(2), spool, request(4))
                .unwrap();
            reused.write(b"body").unwrap();
            reused.prepare().unwrap();
            let UploadAttempt::Complete(rejected) = reused.commit().unwrap() else {
                panic!("collision")
            };
            assert_eq!(
                rejected.outcome,
                Err(CommitError::Rejected(ports::Error::Conflict))
            );
            drop(reused);
            let mut upload = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            upload.write(b"body").unwrap();
            upload.prepare().unwrap();
            let id = upload.id();
            let UploadAttempt::Complete(published) = upload.commit().unwrap() else {
                panic!("commit")
            };
            assert_eq!(published.outcome, Ok(Sequence::from_u64(3)));
            drop(upload);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
            let mut second = coordinator
                .reserve(&td_crypto::Provider, &mut Random(10), spool, request(4))
                .unwrap();
            second.write(b"body").unwrap();
            second.prepare().unwrap();
            let UploadAttempt::Complete(second_result) = second.commit().unwrap() else {
                panic!("commit")
            };
            assert_eq!(second_result.outcome, Ok(Sequence::from_u64(4)));
            drop(second);
            assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 2);
            assert!(matches!(
                coordinator.reserve(&td_crypto::Provider, &mut Random(12), spool, request(1)),
                Err(UploadError::Ledger(logical::Error::Quota(Kind::BlobCount)))
            ));
            assert_recount(coordinator);
            Some(id)
        },
    );
}

#[test]
fn upload_sweep_advances_past_unexpired_lease_and_is_account_scoped() {
    const OTHER: AccountId = AccountId::from_bytes([99; 16]);
    const LATER: BlobId = BlobId::from_bytes([4; 16]);
    const FOREIGN: BlobId = BlobId::from_bytes([1; 16]);
    fixture_config(
        4,
        1235,
        |store| seed_at(store, ACCOUNT, LATER, 1234),
        |coordinator, _, _, _, _| {
            let first = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .unwrap();
            assert_eq!(first.id, SEED);
            assert_eq!(first.outcome, Ok(UploadSweepDisposition::Retained));
            let next = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, Some(first.id), deadline())
                .unwrap()
                .unwrap();
            assert_eq!(next.id, LATER);
            assert_eq!(
                next.outcome,
                Ok(UploadSweepDisposition::Retired {
                    sequence: Sequence::from_u64(3),
                    body_removed: true
                })
            );
            assert!(coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, Some(next.id), deadline())
                .unwrap()
                .is_none());
            assert_recount(coordinator);
            None
        },
    );
    fixture_config(
        4,
        1234,
        |store| {
            store.create_account(OTHER, deadline()).unwrap();
            seed_at(store, OTHER, FOREIGN, 1234);
        },
        |coordinator, _, _, _, _| {
            let local = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .unwrap();
            assert_eq!(local.id, SEED);
            assert_eq!(
                local.outcome,
                Ok(UploadSweepDisposition::Retired {
                    sequence: Sequence::from_u64(2),
                    body_removed: true
                })
            );
            let mut view = coordinator.store.view(OTHER, deadline()).unwrap();
            let mut value = [0; 48];
            assert!(view.get(Key::Lease(FOREIGN), &mut value).unwrap().is_some());
            assert!(view.get(Key::Blob(FOREIGN), &mut value).unwrap().is_some());
            drop(view);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
            assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn expired_lease_preserves_email_and_completed_submission_owners() {
    use crate::{
        format::row::{
            EmailOrigin, EmailRow, FailureReason, NotificationState, RecipientState, SubmissionRow,
        },
        ids::{EmailId, IdentityId, SubmissionId, ThreadId},
    };
    for queue in [false, true] {
        fixture_config(
            4,
            1234,
            |store| {
                let email = EmailId::from_bytes([8; 16]);
                let thread = ThreadId::from_bytes([9; 16]);
                let submission = SubmissionId::from_bytes([10; 16]);
                let mut recipient = super::super::recipient_tests::queued();
                recipient.state = RecipientState::Canceled;
                recipient.next_attempt_at = None;
                recipient.reason = FailureReason::Canceled;
                let rows = if queue {
                    vec![
                        (
                            Key::Submission(submission),
                            Row::Submission(SubmissionRow {
                                email,
                                thread,
                                identity: IdentityId::from_bytes([11; 16]),
                                transmitted_blob: SEED,
                                reverse_path: "",
                                send_at: 0,
                                expires_at: 432000000,
                                recipient_count: 1,
                                completed_at: Some(1),
                                notification: NotificationState::None,
                                notification_email: None,
                            }),
                        ),
                        (Key::Recipient(submission, 0), Row::Recipient(recipient)),
                    ]
                } else {
                    vec![
                        (Key::Thread(thread), Row::Thread),
                        (
                            Key::Email(email),
                            Row::Email(EmailRow {
                                blob: SEED,
                                thread,
                                received_at: 0,
                                origin: EmailOrigin::Jmap,
                            }),
                        ),
                    ]
                };
                let encoded: Vec<_> = rows
                    .iter()
                    .map(|(key, row)| {
                        let mut key_bytes = [0; 32];
                        let len = key.encode(&mut key_bytes).unwrap();
                        (key.table(), key_bytes[..len].to_vec(), row_bytes(*row))
                    })
                    .collect();
                let operations: Vec<_> = encoded
                    .iter()
                    .map(|(table, key, row)| Operation::put(*table, key, row).unwrap())
                    .collect();
                store
                    .commit(
                        &td_crypto::Provider,
                        CommitRequest {
                            account: ACCOUNT,
                            epoch: store.epoch(),
                            expected: Sequence::from_u64(1),
                            utc_ms: 1,
                            deadline: deadline(),
                        },
                        &operations,
                        &mut [],
                    )
                    .unwrap();
            },
            |coordinator, _, _, _, _| {
                let result = coordinator
                    .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    result.outcome,
                    Ok(UploadSweepDisposition::Retired {
                        sequence: Sequence::from_u64(3),
                        body_removed: false
                    })
                );
                assert!(!result.admission_stopped);
                assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
                assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 1);
                assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 0);
                assert_eq!(
                    coordinator.used(Kind::QueueBytes).unwrap(),
                    if queue { 4 } else { 0 }
                );
                assert_recount(coordinator);
                let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
                let mut input = view.open_blob_input(&td_crypto::Provider, SEED, 8).unwrap();
                let mut bytes = [0; 8];
                assert_eq!(input.read(&mut bytes).unwrap(), 4);
                assert_eq!(&bytes[..4], b"seed");
                input.finish().unwrap();
                None
            },
        );
    }
}

#[test]
fn upload_retirement_preserves_known_and_indeterminate_outcomes_after_deadline() {
    for deny in [false, true] {
        fixture_config(
            4,
            1234,
            |_| {},
            |coordinator, _, clock, _, _| {
                let timer = clock.clone();
                lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if matches!(context.action, AuthAction::Transaction { operation }
                        if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                        timer.0.store(100, Ordering::Relaxed);
                        if deny { return Authorization::Deny; }
                    }
                    Authorization::Allow
                })).unwrap();
                let result = coordinator
                    .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                    .unwrap()
                    .unwrap();
                assert!(result.admission_stopped);
                assert_eq!(
                    coordinator.used(Kind::BodyBytes).unwrap(),
                    if deny { 4 } else { 0 }
                );
                assert_eq!(
                    coordinator.used(Kind::UploadBytes).unwrap(),
                    if deny { 4 } else { 0 }
                );
                assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), MAX_WAL_BYTES);
                lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                    .unwrap()
                    .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                clock.0.store(200, Ordering::Relaxed);
                let later = Deadline::after(Tick(200), 100).unwrap();
                if deny {
                    assert!(matches!(result.outcome, Err(CommitError::Indeterminate(_))));
                    assert!(matches!(
                        coordinator.checkpoint(later),
                        Err(UploadError::Store(ports::Error::WriterStopped))
                    ));
                } else {
                    assert_eq!(
                        result.outcome,
                        Ok(UploadSweepDisposition::Retired {
                            sequence: Sequence::from_u64(2),
                            body_removed: true
                        })
                    );
                    let maintenance = coordinator.checkpoint(later).unwrap();
                    assert_eq!(maintenance.outcome, Ok(()));
                    assert!(!maintenance.admission_stopped);
                    assert!(coordinator
                        .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, later)
                        .unwrap()
                        .is_none());
                }
                None
            },
        );
    }
}

#[test]
fn rejected_upload_retirement_keeps_rows_and_logical_charges() {
    fixture_config(
        4,
        1234,
        |_| {},
        |coordinator, _, _, _, _| {
            lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
            .authorizer(Some(|context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Delete { table_name, .. } if table_name == "blobs") {
                    Authorization::Deny
                } else { Authorization::Allow }
            })).unwrap();
            let result = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .unwrap();
            assert!(matches!(result.outcome, Err(CommitError::Rejected(_))));
            assert!(!result.admission_stopped);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
            assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 1);
            assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
            assert_eq!(coordinator.ledger.pending(Kind::DatabaseBytes).unwrap(), 0);
            assert_eq!(coordinator.ledger.pending(Kind::WalBytes).unwrap(), 0);
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut bytes = [0; 64];
            assert!(view.get(Key::Lease(SEED), &mut bytes).unwrap().is_some());
            assert!(view.get(Key::Blob(SEED), &mut bytes).unwrap().is_some());
            drop(view);
            lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            let retried = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .unwrap();
            assert_eq!(
                retried.outcome,
                Ok(UploadSweepDisposition::Retired {
                    sequence: Sequence::from_u64(2),
                    body_removed: true
                })
            );
            None
        },
    );
}

fn assert_recount(coordinator: &StoreCoordinator<'_, '_, Policy>) {
    let usage = coordinator.store.usage_fence(deadline()).unwrap().usage();
    for (kind, amount) in [
        (Kind::BodyBytes, usage.body_bytes),
        (Kind::BlobCount, usage.blob_count),
        (Kind::UploadBytes, usage.upload_bytes),
        (Kind::QueueBytes, usage.queue_bytes),
        (Kind::QueueSubmissions, usage.queue_submissions),
    ] {
        assert_eq!(coordinator.used(kind).unwrap(), amount, "{kind:?}");
    }
}

#[test]
fn backwards_utc_after_planning_retains_the_upload() {
    fixture_config(
        4,
        1234,
        |_| {},
        |coordinator, _, clock, _, _| {
            for slot in lock(&coordinator.store.readers).unwrap().iter() {
                let ReaderSlot::Available(native) = slot else {
                    panic!("available reader")
                };
                let timer = clock.clone();
                lock(&native.connection).unwrap().authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Read { table_name, .. } if table_name == "submissions") {
                    timer.2.store(1233, Ordering::Relaxed);
                }
                Authorization::Allow
            })).unwrap();
            }
            let receipt = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .unwrap();
            assert_eq!(clock.2.load(Ordering::Relaxed), 1233);
            assert_eq!(receipt.outcome, Ok(UploadSweepDisposition::Retained));
            assert!(!receipt.admission_stopped);
            assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
            assert_recount(coordinator);
            None
        },
    );
}
