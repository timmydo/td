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
struct Timer(
    AtomicU64,
    Mutex<std::collections::VecDeque<u64>>,
    AtomicI64,
    Mutex<Option<Arc<Mutex<PolicyState>>>>,
);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        if let Some(policy) = self.3.lock().unwrap().as_ref() {
            assert!(
                matches!(policy.try_lock(), Err(TryLockError::WouldBlock)),
                "authorization released before commit completed"
            );
        }
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
    read_mismatch: bool,
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
    seed_dated(store, account, id, expires_at, 0);
}
fn seed_dated(
    store: &IndexStore<'_>,
    account: AccountId,
    id: BlobId,
    expires_at: i64,
    created_at: i64,
) {
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(b"seed").unwrap();
    let blob = row_bytes(Row::Blob(crate::format::row::BlobRow {
        length: 4,
        digest: hash.finish().unwrap(),
        created_at,
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
    fixture_plan(count, expires_at, 2, setup, run);
}
fn fixture_plan(
    count: usize,
    expires_at: i64,
    blob_count: u64,
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
        Mutex::new(None),
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
        blob_count,
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
        read_mismatch: false,
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
            upload
                .coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::BodyBytes)
                .unwrap(),
            4
        );
        assert_eq!(
            upload
                .coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::BlobCount)
                .unwrap(),
            1
        );
        assert_eq!(
            upload
                .coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::UploadBytes)
                .unwrap(),
            4
        );
        for kind in [Kind::DatabaseBytes, Kind::WalBytes] {
            assert_eq!(
                upload
                    .coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(kind)
                    .unwrap(),
                0
            );
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
fn forgotten_upload_keeps_its_quota_without_blocking_other_jobs_or_maintenance() {
    fixture_plan(
        4,
        100000,
        4,
        |_| {},
        |coordinator, spool, _, _, _| {
            let upload = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            std::mem::forget(upload);
            let other = coordinator
                .reserve(&td_crypto::Provider, &mut Random(9), spool, request(4))
                .unwrap();
            assert_eq!(spool.status().unwrap().occupied_slots, 2);
            other.discard().unwrap();
            assert_eq!(checkpoint(coordinator, deadline()).unwrap().outcome, Ok(()));
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                4
            );
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                3
            );
            None
        },
    );
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
                assert_eq!(
                    coordinator
                        .state
                        .lock()
                        .unwrap()
                        .ledger
                        .pending(kind)
                        .unwrap(),
                    0
                );
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
        assert_eq!(
            coordinator.state.lock().unwrap().ledger.available_cells(),
            4
        );
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
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::DatabaseBytes)
                .unwrap(),
            0
        );
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::WalBytes)
                .unwrap(),
            0
        );
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
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(kind)
                    .unwrap(),
                0
            );
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
        let result = checkpoint(coordinator, deadline()).unwrap();
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
        assert_eq!(checkpoint(coordinator, deadline()).unwrap().outcome, Ok(()));
        assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), 0);
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
        assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 8);
        for kind in [Kind::DatabaseBytes, Kind::WalBytes] {
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(kind)
                    .unwrap(),
                0
            );
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
                    checkpoint(coordinator, later),
                    Err(UploadError::Store(ports::Error::WriterStopped))
                ));
                assert!(coordinator.admission_stopped());
                return None;
            }
            assert_eq!(result.outcome, Ok(Sequence::from_u64(2)));
            assert!(matches!(
                checkpoint(coordinator, deadline()),
                Err(UploadError::Store(ports::Error::Deadline))
            ));
            assert!(coordinator.admission_stopped());
            let early = retry_checkpoint(|| {
                // Admission contention must not consume the native-timeout script.
                *clock.1.lock().unwrap() = [200, 200, 200, 300].into();
                coordinator.checkpoint(later)
            })
            .unwrap();
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
            let refused = checkpoint(coordinator, later).unwrap();
            assert!(refused.outcome.is_err());
            assert!(matches!(refused.files, CommitFileUsage::Measured(_)));
            assert!(refused.admission_stopped);
            assert!(coordinator.admission_stopped());
            lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            let maintenance = checkpoint(coordinator, later).unwrap();
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
        let result = checkpoint(coordinator, deadline()).unwrap();
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
        let result = checkpoint(coordinator, Deadline::after(Tick(100), 100).unwrap()).unwrap();
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
        assert_eq!(checkpoint(coordinator, later).unwrap().outcome, Ok(()));
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
        let result = checkpoint(coordinator, deadline()).unwrap();
        assert_eq!(result.outcome, Err(ports::Error::WriterStopped));
        assert_eq!(result.files, CommitFileUsage::Unchanged);
        assert!(result.admission_stopped);
        assert!(matches!(
            checkpoint(coordinator, deadline()),
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
            |coordinator, spool, _, _, _| {
                let pending = coordinator
                    .reserve(&td_crypto::Provider, &mut Random(30), spool, request(4))
                    .unwrap();
                assert!(matches!(
                    coordinator.reserve_reclaiming(
                        &td_crypto::Provider,
                        &mut Random(40),
                        spool,
                        request(4)
                    ),
                    Err(UploadError::Ledger(logical::Error::Quota(Kind::BlobCount)))
                ));
                drop(pending);
                let mut read = blob_request();
                read.device = DeviceId::from_bytes([99; 16]);
                coordinator
                    .with_authorized_blob(&td_crypto::Provider, read, |body| {
                        let mut bytes = [0; 4];
                        assert_eq!(body.read_at(0, &mut bytes)?, 4);
                        assert_eq!(&bytes, b"seed");
                        Ok(())
                    })
                    .unwrap();
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
                        checkpoint(coordinator, later),
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
                    let maintenance = checkpoint(coordinator, later).unwrap();
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
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::DatabaseBytes)
                    .unwrap(),
                0
            );
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::WalBytes)
                    .unwrap(),
                0
            );
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

#[test]
fn simultaneous_uploads_keep_independent_quota_and_leave_a_publication_cell() {
    fixture_plan(
        4,
        100000,
        4,
        |_| {},
        |coordinator, spool, _, _, _| {
            let first = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            let mut second = coordinator
                .reserve(&td_crypto::Provider, &mut Random(9), spool, request(4))
                .unwrap();
            let third = coordinator
                .reserve(&td_crypto::Provider, &mut Random(10), spool, request(4))
                .unwrap();
            assert!(matches!(
                coordinator.reserve(&td_crypto::Provider, &mut Random(11), spool, request(0)),
                Err(UploadError::Ledger(logical::Error::Full))
            ));
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                1
            );
            assert_eq!(checkpoint(coordinator, deadline()).unwrap().outcome, Ok(()));
            second.write(b"body").unwrap();
            second.prepare().unwrap();
            let id = second.id();
            let UploadAttempt::Complete(receipt) = second.commit().unwrap() else {
                panic!("unexpected conflict")
            };
            assert!(receipt.outcome.is_ok());
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                8
            );
            drop(first);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                4
            );
            drop(second);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                4
            );
            third.discard().unwrap();
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                4
            );
            assert_recount(coordinator);
            Some(id)
        },
    );
}

#[test]
fn overlapping_uploads_retry_against_the_current_sequence_under_one_lock() {
    fixture_plan(
        4,
        100000,
        4,
        |_| {},
        |coordinator, spool, _, _, _| {
            let mut first = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            let mut second = coordinator
                .reserve(&td_crypto::Provider, &mut Random(9), spool, request(4))
                .unwrap();
            {
                let _guard = coordinator.state.lock().unwrap();
                assert!(matches!(
                    first.replan(),
                    Err(UploadError::Store(ports::Error::Invalid))
                ));
            }
            first.write(b"body").unwrap();
            first.prepare().unwrap();
            second.write(b"body").unwrap();
            second.prepare().unwrap();
            assert!(matches!(
                second.commit().unwrap(),
                UploadAttempt::Complete(UploadCompletion { outcome: Ok(_), .. })
            ));
            assert!(matches!(
                first.commit().unwrap(),
                UploadAttempt::SequenceConflict
            ));
            {
                let _guard = coordinator.state.lock().unwrap();
                assert!(matches!(
                    first.commit(),
                    Err(UploadError::Store(ports::Error::Conflict))
                ));
                assert!(matches!(
                    first.replan(),
                    Err(UploadError::Store(ports::Error::Busy))
                ));
            }
            first.replan().unwrap();
            let guard = coordinator.state.lock().unwrap();
            assert!(matches!(
                first.replan(),
                Err(UploadError::Store(ports::Error::Invalid))
            ));
            let (mut first, deferred) = std::thread::scope(|scope| {
                scope
                    .spawn(move || {
                        let result = first.commit();
                        (first, result)
                    })
                    .join()
                    .unwrap()
            });
            assert!(matches!(
                deferred,
                Err(UploadError::Store(ports::Error::Busy))
            ));
            drop(guard);
            let mut third = coordinator
                .reserve(&td_crypto::Provider, &mut Random(10), spool, request(4))
                .unwrap();
            third.write(b"body").unwrap();
            third.prepare().unwrap();
            assert!(matches!(
                third.commit().unwrap(),
                UploadAttempt::Complete(UploadCompletion { outcome: Ok(_), .. })
            ));
            assert!(matches!(
                first.commit().unwrap(),
                UploadAttempt::Complete(UploadCompletion { outcome: Ok(_), .. })
            ));
            drop(first);
            drop(second);
            drop(third);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn uncertain_publication_stops_other_admitted_jobs() {
    fixture_plan(
        4,
        100000,
        4,
        |_| {},
        |coordinator, spool, _, _, _| {
            let mut first = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            let mut second = coordinator
                .reserve(&td_crypto::Provider, &mut Random(9), spool, request(4))
                .unwrap();
            first.write(b"body").unwrap();
            first.prepare().unwrap();
            second.write(b"body").unwrap();
            second.prepare().unwrap();
            lock(&lock(&coordinator.store.writer).unwrap().native.connection).unwrap()
            .authorizer(Some(|context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback)) {
                    Authorization::Deny
                } else { Authorization::Allow }
            })).unwrap();
            let UploadAttempt::Complete(result) = first.commit().unwrap() else {
                panic!("conflict")
            };
            assert!(matches!(result.outcome, Err(CommitError::Indeterminate(_))));
            assert!(matches!(
                second.commit(),
                Err(UploadError::Store(ports::Error::WriterStopped))
            ));
            drop(first);
            drop(second);
            assert_eq!(
                coordinator.state.lock().unwrap().ledger.available_cells(),
                4
            );
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
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
            None
        },
    );
}

#[test]
fn poisoned_coordination_refuses_admission_and_cleanup_does_not_panic() {
    fixture_plan(
        4,
        100000,
        4,
        |_| {},
        |coordinator, spool, _, _, _| {
            let coordinator = &*coordinator;
            let upload = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            std::thread::scope(|scope| {
                assert!(scope
                    .spawn(|| {
                        let _guard = coordinator.state.lock().unwrap();
                        panic!("simulated worker failure");
                    })
                    .join()
                    .is_err());
            });
            assert!(coordinator.admission_stopped());
            assert_eq!(
                coordinator.used(Kind::BodyBytes),
                Err(logical::Error::Poisoned)
            );
            assert!(matches!(
                coordinator.reserve(&td_crypto::Provider, &mut Random(9), spool, request(4)),
                Err(UploadError::Store(ports::Error::WriterStopped))
            ));
            assert!(matches!(
                checkpoint(coordinator, deadline()),
                Err(UploadError::Store(ports::Error::WriterStopped))
            ));
            drop(upload);
            assert_eq!(spool.status().unwrap().occupied_slots, 0);
            None
        },
    );
}

#[test]
fn expired_upload_cleanup_runs_beside_reserved_ingress_without_releasing_its_quota() {
    fixture_plan(
        4,
        1234,
        4,
        |_| {},
        |coordinator, spool, _, _, _| {
            let mut upload = coordinator
                .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                .unwrap();
            let cleanup = coordinator
                .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                .unwrap()
                .unwrap();
            assert!(matches!(
                cleanup.outcome,
                Ok(UploadSweepDisposition::Retired {
                    body_removed: true,
                    ..
                })
            ));
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                4
            );
            upload.write(b"body").unwrap();
            upload.prepare().unwrap();
            assert!(matches!(
                upload.commit().unwrap(),
                UploadAttempt::SequenceConflict
            ));
            upload.replan().unwrap();
            let id = upload.id();
            assert!(matches!(
                upload.commit().unwrap(),
                UploadAttempt::Complete(UploadCompletion { outcome: Ok(_), .. })
            ));
            drop(upload);
            assert_recount(coordinator);
            Some(id)
        },
    );
}

pub(super) struct CheckingCrypto<F>(pub(super) F);
impl<F: Fn() -> Result<(), td_crypto::Error> + Send + Sync> Crypto for CheckingCrypto<F> {
    type Sha256 = <td_crypto::Provider as Crypto>::Sha256;
    type SigningKey = <td_crypto::Provider as Crypto>::SigningKey;
    fn sha256(&self) -> Result<Self::Sha256, td_crypto::Error> {
        (self.0)()?;
        td_crypto::Provider.sha256()
    }
    fn equal_digest(&self, a: &[u8; 32], b: &[u8; 32]) -> bool {
        td_crypto::Provider.equal_digest(a, b)
    }
    fn generate_p256(&self, output: &mut [u8]) -> Result<usize, td_crypto::Error> {
        td_crypto::Provider.generate_p256(output)
    }
    fn load_p256(&self, input: &[u8]) -> Result<Self::SigningKey, td_crypto::Error> {
        td_crypto::Provider.load_p256(input)
    }
    fn p256_public(
        &self,
        key: &Self::SigningKey,
        output: &mut [u8; 65],
    ) -> Result<(), td_crypto::Error> {
        td_crypto::Provider.p256_public(key, output)
    }
    fn sign_es256(
        &self,
        key: &Self::SigningKey,
        input: &[u8],
        output: &mut [u8; 64],
    ) -> Result<(), td_crypto::Error> {
        td_crypto::Provider.sign_es256(key, input, output)
    }
}

#[test]
fn spool_creation_releases_coordination_and_failure_cancels_only_its_reservation() {
    for fail in [false, true] {
        fixture_plan(
            4,
            100000,
            4,
            |_| {},
            |coordinator, spool, _, _, _| {
                let other = coordinator
                    .reserve(&td_crypto::Provider, &mut Random(8), spool, request(4))
                    .unwrap();
                let crypto = CheckingCrypto(|| {
                    let state = coordinator.try_state().unwrap();
                    assert_eq!(state.ledger.pending(Kind::BodyBytes).unwrap(), 8);
                    if fail {
                        Err(td_crypto::Error::Crypto)
                    } else {
                        Ok(())
                    }
                });
                let result = coordinator.reserve(&crypto, &mut Random(9), spool, request(4));
                if fail {
                    assert!(matches!(
                        result,
                        Err(UploadError::Store(ports::Error::Crypto))
                    ));
                } else {
                    result.unwrap().discard().unwrap();
                }
                assert_eq!(
                    coordinator
                        .state
                        .lock()
                        .unwrap()
                        .ledger
                        .pending(Kind::BodyBytes)
                        .unwrap(),
                    4
                );
                assert_eq!(spool.status().unwrap().occupied_slots, 1);
                drop(other);
                assert_recount(coordinator);
                None
            },
        );
    }
}

// A racing process-local ticket issue refuses before checkpoint effects.
// Keep the test's original deadline and preserve every other result.
fn checkpoint<P>(
    coordinator: &StoreCoordinator<'_, '_, P>,
    deadline: Deadline,
) -> Result<UploadMaintenance, UploadError> {
    retry_checkpoint(|| coordinator.checkpoint(deadline))
}
fn retry_checkpoint(
    mut attempt: impl FnMut() -> Result<UploadMaintenance, UploadError>,
) -> Result<UploadMaintenance, UploadError> {
    for _ in 0..1000 {
        match attempt() {
            Err(UploadError::Ledger(logical::Error::Slot(crate::ownership::Error::Contended))) => {
                std::thread::yield_now()
            }
            result => return result,
        }
    }
    Err(UploadError::Ledger(logical::Error::Slot(
        crate::ownership::Error::Contended,
    )))
}

#[test]
fn pressure_reclaims_by_date_before_id_and_publishes_after_reclamation() {
    const YOUNGER: BlobId = BlobId::from_bytes([1; 16]);
    fixture_config(
        4,
        100000,
        |store| seed_dated(store, ACCOUNT, YOUNGER, 100000, 1),
        |coordinator, spool, _, _, _| {
            let mut upload = coordinator
                .reserve_reclaiming(&td_crypto::Provider, &mut Random(10), spool, request(4))
                .unwrap();
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut row = [0; 128];
            assert!(view.get(Key::Blob(SEED), &mut row).unwrap().is_none());
            assert!(view.get(Key::Lease(SEED), &mut row).unwrap().is_none());
            assert!(view.get(Key::Blob(YOUNGER), &mut row).unwrap().is_some());
            drop(view);
            upload.write(b"body").unwrap();
            upload.prepare().unwrap();
            let id = upload.id();
            let UploadAttempt::Complete(result) = upload.commit().unwrap() else {
                panic!("commit");
            };
            assert_eq!(result.outcome, Ok(Sequence::from_u64(4)));
            drop(upload);
            assert_recount(coordinator);
            Some(id)
        },
    );
}

#[test]
fn pressure_reclaims_multiple_oldest_ties_only_until_quota_fits() {
    fixture_plan(
        4,
        100000,
        5,
        |store| {
            for byte in [1, 2, 4] {
                seed_at(store, ACCOUNT, BlobId::from_bytes([byte; 16]), 100000);
            }
        },
        |coordinator, spool, _, _, _| {
            let upload = coordinator
                .reserve_reclaiming(&td_crypto::Provider, &mut Random(10), spool, request(8))
                .unwrap();
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut row = [0; 128];
            for byte in [1, 2, 3, 4] {
                assert_eq!(
                    view.get(Key::Blob(BlobId::from_bytes([byte; 16])), &mut row)
                        .unwrap()
                        .is_some(),
                    byte >= 3
                );
            }
            drop(view);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
            assert_eq!(
                coordinator
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .pending(Kind::BodyBytes)
                    .unwrap(),
                8
            );
            drop(upload);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn pressure_preserves_active_ingress_and_refuses_when_only_pending_work_remains() {
    fixture(|coordinator, spool, _, _, _| {
        let pending = coordinator
            .reserve(&td_crypto::Provider, &mut Random(10), spool, request(8))
            .unwrap();
        let next = coordinator
            .reserve_reclaiming(&td_crypto::Provider, &mut Random(20), spool, request(8))
            .unwrap();
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
        assert_eq!(
            coordinator
                .state
                .lock()
                .unwrap()
                .ledger
                .pending(Kind::BodyBytes)
                .unwrap(),
            16
        );
        assert!(matches!(
            coordinator.reserve_reclaiming(
                &td_crypto::Provider,
                &mut Random(30),
                spool,
                request(1)
            ),
            Err(UploadError::Ledger(logical::Error::Quota(_)))
        ));
        drop(next);
        drop(pending);
        assert_recount(coordinator);
        None
    });
}

#[test]
fn pressure_refusals_do_not_delete_rows_or_bypass_authorization() {
    fixture_config(
        4,
        100000,
        |store| seed_at(store, ACCOUNT, BlobId::from_bytes([4; 16]), 100000),
        |coordinator, spool, clock, policy, _| {
            policy.lock().unwrap().allowed = false;
            assert!(matches!(
                coordinator.reserve_reclaiming(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    request(4)
                ),
                Err(UploadError::Store(ports::Error::Forbidden))
            ));
            policy.lock().unwrap().allowed = true;
            clock.0.store(100, Ordering::Relaxed);
            assert!(matches!(
                coordinator.reserve_reclaiming(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    request(4)
                ),
                Err(UploadError::Store(ports::Error::Deadline))
            ));
            clock.0.store(1, Ordering::Relaxed);
            set_writer_authorizer(coordinator, |context| {
                if matches!(context.action, AuthAction::Delete { table_name, .. } if table_name == "blobs")
                {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            });
            assert!(matches!(
                coordinator.reserve_reclaiming(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    request(4)
                ),
                Err(UploadError::Store(_))
            ));
            lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            assert!(!coordinator.admission_stopped());
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
            assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 2);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn pressure_is_account_scoped_even_when_foreign_upload_is_older() {
    const OTHER: AccountId = AccountId::from_bytes([99; 16]);
    const FOREIGN: BlobId = BlobId::from_bytes([1; 16]);
    fixture_config(
        4,
        100000,
        |store| {
            store.create_account(OTHER, deadline()).unwrap();
            seed_dated(store, OTHER, FOREIGN, 100000, -1);
        },
        |coordinator, spool, _, _, _| {
            let upload = coordinator
                .reserve_reclaiming(&td_crypto::Provider, &mut Random(10), spool, request(4))
                .unwrap();
            let mut view = coordinator.store.view(OTHER, deadline()).unwrap();
            let mut row = [0; 128];
            assert!(view.get(Key::Blob(FOREIGN), &mut row).unwrap().is_some());
            assert!(view.get(Key::Lease(FOREIGN), &mut row).unwrap().is_some());
            drop(view);
            let mut local = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            assert!(local.get(Key::Blob(SEED), &mut row).unwrap().is_none());
            drop(local);
            drop(upload);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn pressure_holds_authorization_through_commit_and_stops_on_uncertain_cleanup() {
    for deny in [false, true] {
        fixture_config(
            4,
            100000,
            |store| seed_at(store, ACCOUNT, BlobId::from_bytes([4; 16]), 100000),
            |coordinator, spool, clock, policy, _| {
                let timer = clock.clone();
                let policy = policy.clone();
                let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let observed = held.clone();
                set_writer_authorizer(coordinator, move |context| {
                    if matches!(context.action, AuthAction::Transaction { operation }
                            if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback))
                    {
                        observed.store(policy.try_lock().is_err(), Ordering::Relaxed);
                        timer.0.store(100, Ordering::Relaxed);
                        if deny {
                            return Authorization::Deny;
                        }
                    }
                    Authorization::Allow
                });
                assert!(matches!(
                    coordinator.reserve_reclaiming(
                        &td_crypto::Provider,
                        &mut Random(10),
                        spool,
                        request(4)
                    ),
                    Err(UploadError::Store(ports::Error::WriterStopped))
                ));
                assert!(held.load(Ordering::Relaxed));
                assert!(coordinator.admission_stopped());
                assert_eq!(
                    coordinator.used(Kind::BodyBytes).unwrap(),
                    if deny { 8 } else { 4 }
                );
                assert_eq!(
                    coordinator.used(Kind::UploadBytes).unwrap(),
                    if deny { 8 } else { 4 }
                );
                lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                    .unwrap()
                    .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                None
            },
        );
    }
}

#[test]
fn pressure_refuses_uncoverable_shortfall_without_deleting_any_upload() {
    const OTHER: AccountId = AccountId::from_bytes([99; 16]);
    fixture_plan(
        4,
        100000,
        3,
        |store| {
            store.create_account(OTHER, deadline()).unwrap();
            seed_at(store, OTHER, BlobId::from_bytes([1; 16]), 100000);
        },
        |coordinator, spool, _, _, _| {
            let pending = coordinator
                .reserve(&td_crypto::Provider, &mut Random(10), spool, request(8))
                .unwrap();
            assert!(matches!(
                coordinator.reserve_reclaiming(
                    &td_crypto::Provider,
                    &mut Random(20),
                    spool,
                    request(8)
                ),
                Err(UploadError::Ledger(logical::Error::Quota(Kind::BodyBytes)))
            ));
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut row = [0; 128];
            assert!(view.get(Key::Blob(SEED), &mut row).unwrap().is_some());
            assert!(view.get(Key::Lease(SEED), &mut row).unwrap().is_some());
            drop(view);
            drop(pending);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn pressure_rechecks_revocation_and_clock_between_failed_reserve_and_deletion() {
    struct Change<'a> {
        clock: &'a Arc<Timer>,
        policy: &'a Arc<Mutex<PolicyState>>,
        regression: bool,
    }
    impl Entropy for Change<'_> {
        fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
            bytes.fill(19);
            if self.regression {
                self.clock.1.lock().unwrap().extend([10, 5]);
            } else {
                self.policy.lock().unwrap().allowed = false;
            }
            Ok(())
        }
    }
    for regression in [false, true] {
        fixture_config(
            4,
            100000,
            |store| seed_at(store, ACCOUNT, BlobId::from_bytes([4; 16]), 100000),
            |coordinator, spool, clock, policy, _| {
                let mut entropy = Change {
                    clock,
                    policy,
                    regression,
                };
                let expected = if regression {
                    ports::Error::Invalid
                } else {
                    ports::Error::Forbidden
                };
                assert!(
                    matches!(coordinator.reserve_reclaiming(&td_crypto::Provider, &mut entropy, spool, request(4)),
                    Err(UploadError::Store(error)) if error == expected)
                );
                policy.lock().unwrap().allowed = true;
                clock.0.store(20, Ordering::Relaxed);
                assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
                assert_recount(coordinator);
                None
            },
        );
    }
}

#[test]
fn pressure_ownership_probes_use_existing_blob_indexes() {
    fixture(|coordinator, _, _, _, _| {
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        view.read_snapshot(|native| {
            native.run(|db| {
                for query in [
                    super::cleanup::RECLAIMABLE_TOTAL,
                    super::cleanup::OLDEST_RECLAIMABLE,
                ] {
                    let mut statement = db
                        .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                        .map_err(sql)?;
                    let details = statement
                        .query_map(params![ACCOUNT.as_bytes().as_slice()], |row| {
                            row.get::<_, String>(3)
                        })
                        .map_err(sql)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(sql)?
                        .join("\n");
                    assert!(details.contains("emails_blob"), "{details}");
                    assert!(details.contains("submissions_blob"), "{details}");
                }
                Ok(())
            })
        })
        .unwrap();
        None
    });
}

fn set_writer_authorizer<P>(
    coordinator: &StoreCoordinator<'_, '_, P>,
    hook: impl for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static,
) {
    let writer = lock(&coordinator.store.writer).unwrap();
    lock(&writer.native.connection)
        .unwrap()
        .authorizer(Some(hook))
        .unwrap();
}

#[test]
fn pressure_stops_at_victim_budget_and_keeps_prior_durable_eviction() {
    fixture_plan(
        4,
        100000,
        5,
        |store| {
            for byte in [1, 2, 4] {
                seed_at(store, ACCOUNT, BlobId::from_bytes([byte; 16]), 100000);
            }
        },
        |coordinator, spool, _, _, _| {
            assert!(matches!(
                coordinator.reserve_with_reclaim_limit(
                    &td_crypto::Provider,
                    &mut Random(10),
                    spool,
                    request(8),
                    1
                ),
                Err(UploadError::Store(ports::Error::Busy))
            ));
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut row = [0; 128];
            assert!(view
                .get(Key::Blob(BlobId::from_bytes([1; 16])), &mut row)
                .unwrap()
                .is_none());
            assert!(view
                .get(Key::Blob(BlobId::from_bytes([2; 16])), &mut row)
                .unwrap()
                .is_some());
            drop(view);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 12);
            assert_recount(coordinator);
            None
        },
    );
}
#[test]
fn pressure_does_not_reclaim_for_a_spool_capacity_refusal() {
    fixture(|coordinator, spool, _, _, _| {
        assert!(matches!(
            coordinator.reserve_reclaiming(
                &td_crypto::Provider,
                &mut Random(10),
                spool,
                request(9)
            ),
            Err(UploadError::Store(ports::Error::Capacity))
        ));
        assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
        assert_recount(coordinator);
        None
    });
}

#[test]
fn chunked_https_quota_refusal_does_not_speculatively_evict() {
    use crate::https_upload::Backend;
    use crate::ports::{FlushProgress, IoProgress, Transport};
    struct NoIo;
    impl Transport for NoIo {
        fn read(&mut self, _: &mut [u8]) -> Result<IoProgress, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn write(&mut self, _: &[u8]) -> Result<IoProgress, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn flush(&mut self) -> Result<FlushProgress, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn close(&mut self) -> Result<FlushProgress, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn abort(&mut self) {}
    }
    fixture_config(
        4,
        100000,
        |store| seed_at(store, ACCOUNT, BlobId::from_bytes([4; 16]), 100000),
        |coordinator, spool, clock, _, _| {
            let resources = Limits::default().plan().unwrap();
            let admission = DiskLimits::default()
                .plan(
                    &resources,
                    WorkLimits::default(),
                    ViewMode::OnlineBackground,
                )
                .unwrap();
            let timeouts = crate::admission::timers::NetworkLimits::default()
                .plan(&admission)
                .unwrap();
            let service = crate::https_upload::Service {
                resources: &resources,
                coordinator,
                spool,
                account: ACCOUNT,
                maximum: 8,
                clock: clock.as_ref(),
                timeouts: &timeouts,
            };
            let result = service.receive(
                crate::https_upload::Request {
                    account: ACCOUNT,
                    device: DEVICE,
                    credentials: &crate::http_auth::parse_basic("u", Some("Basic dTpw")).unwrap(),
                    framing: crate::http_request::Framing::Chunked,
                    content_type: "text/plain",
                    prefix: &[],
                    deadline: deadline(),
                    body_deadline: deadline(),
                    expect_continue: true,
                },
                &mut NoIo,
                &crate::smtp_receiving::Control::default(),
            );
            assert!(matches!(result, Err(ports::Error::Quota)));
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 8);
            assert_recount(coordinator);
            None
        },
    );
}

struct ReadGuard<'a>(Guard<'a>, DeviceId);
impl UploadGuard for ReadGuard<'_> {
    fn access(&self) -> Access {
        Access {
            principal: Principal::Device(self.1),
            ..self.0.access()
        }
    }
}
impl BlobAuthorization for Policy {
    type Guard<'a> = ReadGuard<'a>;
    fn authorize_blob(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<ReadGuard<'_>, ports::Error> {
        if device != DEVICE && device != DeviceId::from_bytes([99; 16]) {
            return Err(ports::Error::Forbidden);
        }
        self.authorize(account, DEVICE, deadline).map(|guard| {
            let principal = if guard.0.read_mismatch {
                DeviceId::from_bytes([98; 16])
            } else {
                device
            };
            ReadGuard(guard, principal)
        })
    }
}
fn blob_request() -> BlobReadRequest {
    BlobReadRequest {
        account: ACCOUNT,
        device: DEVICE,
        id: SEED,
        maximum: 8,
        deadline: deadline(),
    }
}
#[test]
fn authorized_blob_verifies_before_callback_and_releases_snapshot() {
    fixture(|coordinator, _, _, policy, _| {
        let verified = std::sync::atomic::AtomicBool::new(false);
        let crypto = CheckingCrypto(|| {
            assert!(policy.try_lock().is_ok());
            verified.store(true, Ordering::Relaxed);
            Ok(())
        });
        let bytes = coordinator
            .with_authorized_blob(&crypto, blob_request(), |body| {
                assert!(verified.load(Ordering::Relaxed));
                assert!(policy.try_lock().is_ok());
                assert_eq!(body.len(), 4);
                let mut bytes = [0; 4];
                assert_eq!(body.read_at(0, &mut bytes)?, 4);
                Ok(bytes)
            })
            .unwrap();
        assert_eq!(&bytes, b"seed");
        assert_read_pool_free(coordinator);
        None
    });
}
#[test]
fn authorized_blob_refuses_missing_expired_foreign_and_oversized_input() {
    fixture(|coordinator, _, clock, policy, _| {
        let no_output = |_: &mut dyn ports::BlobReader| -> Result<(), ports::Error> {
            panic!("unauthorized output")
        };
        let mut request = blob_request();
        request.id = BlobId::from_bytes([99; 16]);
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, request, no_output),
            Err(ports::Error::NotFound)
        );
        assert_read_pool_free(coordinator);
        request = blob_request();
        request.device = DeviceId::from_bytes([99; 16]);
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, request, no_output),
            Err(ports::Error::NotFound)
        );
        assert_read_pool_free(coordinator);
        request = blob_request();
        request.maximum = 3;
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, request, no_output),
            Err(ports::Error::Capacity)
        );
        assert_read_pool_free(coordinator);
        request = blob_request();
        request.account = AccountId::from_bytes([99; 16]);
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, request, no_output),
            Err(ports::Error::Forbidden)
        );
        policy.lock().unwrap().allowed = false;
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, blob_request(), no_output),
            Err(ports::Error::Forbidden)
        );
        policy.lock().unwrap().allowed = true;
        clock.2.store(100000, Ordering::Relaxed);
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, blob_request(), no_output),
            Err(ports::Error::NotFound)
        );
        assert_read_pool_free(coordinator);
        None
    });
}
#[test]
fn authorized_blob_rechecks_policy_and_expiry_after_verification() {
    for expiry in [false, true] {
        fixture(|coordinator, _, clock, policy, _| {
            let crypto = CheckingCrypto(|| {
                if expiry {
                    clock.2.store(100000, Ordering::Relaxed);
                } else {
                    policy.lock().unwrap().allowed = false;
                }
                Ok(())
            });
            let result = coordinator.with_authorized_blob(
                &crypto,
                blob_request(),
                |_| -> Result<(), ports::Error> { panic!("stale output") },
            );
            assert_eq!(
                result,
                Err(if expiry {
                    ports::Error::NotFound
                } else {
                    ports::Error::Forbidden
                })
            );
            None
        });
    }
}
#[test]
fn authorized_blob_admitted_callback_keeps_original_deadline_after_revocation() {
    fixture(|coordinator, _, clock, policy, _| {
        let result =
            coordinator.with_authorized_blob(&td_crypto::Provider, blob_request(), |body| {
                policy.lock().unwrap().allowed = false;
                let mut bytes = [0; 4];
                assert_eq!(body.read_at(0, &mut bytes)?, 4);
                assert_eq!(&bytes, b"seed");
                clock.0.store(100, Ordering::Relaxed);
                assert_eq!(body.read_at(0, &mut bytes), Err(ports::Error::Deadline));
                Ok(())
            });
        assert_eq!(result, Err(ports::Error::Deadline));
        None
    });
}

#[test]
fn authorized_blob_corruption_never_reaches_callback() {
    fixture_config(
        4,
        100000,
        |store| {
            let writer = lock(&store.writer).unwrap();
            lock(&writer.native.connection)
                .unwrap()
                .execute(
                    "UPDATE blobs SET digest=zeroblob(32) WHERE account=?1 AND id=?2",
                    params![ACCOUNT.as_bytes().as_slice(), SEED.as_bytes().as_slice()],
                )
                .unwrap();
        },
        |coordinator, _, _, _, _| {
            assert_eq!(
                coordinator.with_authorized_blob(
                    &td_crypto::Provider,
                    blob_request(),
                    |_| -> Result<(), ports::Error> { panic!("unverified bytes") }
                ),
                Err(ports::Error::Corrupt)
            );
            assert_read_pool_free(coordinator);
            None
        },
    );
}

fn assert_read_pool_free<P>(coordinator: &StoreCoordinator<'_, '_, P>) {
    let views: Vec<_> = (0..2)
        .map(|_| coordinator.store.view(ACCOUNT, deadline()).unwrap())
        .collect();
    assert_eq!(views.len(), 2);
    assert!(matches!(
        coordinator.store.view(ACCOUNT, deadline()),
        Err(ports::Error::Busy)
    ));
}
#[test]
fn authorized_blob_refuses_mismatched_policy_and_hash_deadline_without_leaking_views() {
    fixture(|coordinator, _, clock, policy, _| {
        policy.lock().unwrap().read_mismatch = true;
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, blob_request(), |_| Ok(())),
            Err(ports::Error::Forbidden)
        );
        policy.lock().unwrap().read_mismatch = false;
        let mut unknown = blob_request();
        unknown.device = DeviceId::from_bytes([97; 16]);
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, unknown, |_| Ok(())),
            Err(ports::Error::Forbidden)
        );
        let crypto = CheckingCrypto(|| {
            clock.0.store(100, Ordering::Relaxed);
            Ok(())
        });
        assert_eq!(
            coordinator.with_authorized_blob(
                &crypto,
                blob_request(),
                |_| -> Result<(), ports::Error> { panic!("expired hashing") }
            ),
            Err(ports::Error::Deadline)
        );
        clock.0.store(1, Ordering::Relaxed);
        assert_read_pool_free(coordinator);
        None
    });
}
#[test]
fn authorized_blob_snapshot_survives_retirement_and_releases_on_callback_failure() {
    fixture(|coordinator, _, clock, _, _| {
        let result = coordinator.with_authorized_blob(
            &td_crypto::Provider,
            blob_request(),
            |body| -> Result<(), ports::Error> {
                clock.2.store(100000, Ordering::Relaxed);
                let retired = coordinator
                    .expire_next_upload(&td_crypto::Provider, ACCOUNT, None, deadline())
                    .unwrap()
                    .unwrap();
                assert!(matches!(
                    retired.outcome,
                    Ok(UploadSweepDisposition::Retired {
                        body_removed: true,
                        ..
                    })
                ));
                let mut bytes = [0; 4];
                assert_eq!(body.read_at(0, &mut bytes)?, 4);
                assert_eq!(&bytes, b"seed");
                Err(ports::Error::Crypto)
            },
        );
        assert_eq!(result, Err(ports::Error::Crypto));
        assert_read_pool_free(coordinator);
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, blob_request(), |_| Ok(())),
            Err(ports::Error::NotFound)
        );
        assert_read_pool_free(coordinator);
        None
    });
}
#[test]
fn authorized_empty_blob_checks_callback_deadline_and_releases_after_panic() {
    fixture(|coordinator, spool, clock, _, _| {
        let mut upload = coordinator
            .reserve(&td_crypto::Provider, &mut Random(8), spool, request(0))
            .unwrap();
        upload.prepare().unwrap();
        let id = upload.id();
        assert!(matches!(
            upload.commit().unwrap(),
            UploadAttempt::Complete(_)
        ));
        drop(upload);
        let mut request = blob_request();
        request.id = id;
        coordinator
            .with_authorized_blob(&td_crypto::Provider, request, |body| {
                assert!(body.is_empty());
                Ok(())
            })
            .unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            coordinator.with_authorized_blob(
                &td_crypto::Provider,
                request,
                |_| -> Result<(), ports::Error> { panic!("callback panic") },
            )
        }));
        assert!(panic.is_err());
        assert_read_pool_free(coordinator);
        assert_eq!(
            coordinator.with_authorized_blob(&td_crypto::Provider, request, |_| {
                clock.0.store(100, Ordering::Relaxed);
                Ok(())
            }),
            Err(ports::Error::Deadline)
        );
        clock.0.store(1, Ordering::Relaxed);
        assert_read_pool_free(coordinator);
        assert_eq!(
            coordinator.with_authorized_blob(
                &td_crypto::Provider,
                request,
                |_| -> Result<(), ports::Error> {
                    clock.0.store(100, Ordering::Relaxed);
                    Err(ports::Error::Crypto)
                }
            ),
            Err(ports::Error::Crypto)
        );
        clock.0.store(1, Ordering::Relaxed);
        assert_read_pool_free(coordinator);
        None
    });
}

impl EmailUpdateAuthorization for Policy {
    type Guard<'a> = Guard<'a>;
    fn authorize_email_update(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Guard<'_>, ports::Error> {
        self.authorize(account, device, deadline)
    }
}
const FLAG_EMAIL: crate::ids::EmailId = crate::ids::EmailId::from_bytes([40; 16]);
const FLAG_MAILBOX: MailboxId = MailboxId::from_bytes([41; 16]);
fn seed_flag_email(store: &IndexStore<'_>) {
    use crate::format::row::{EmailOrigin, EmailRow, MailboxRow};
    let thread = crate::ids::ThreadId::from_bytes([42; 16]);
    let email = row_bytes(Row::Email(EmailRow {
        blob: SEED,
        thread,
        received_at: 0,
        origin: EmailOrigin::Jmap,
    }));
    let mailbox = row_bytes(Row::Mailbox(MailboxRow {
        name: "Inbox",
        parent: None,
        role: Some("inbox"),
        sort_order: 0,
        subscribed: true,
    }));
    let mut membership = [0; 32];
    Key::Membership(FLAG_EMAIL, FLAG_MAILBOX)
        .encode(&mut membership)
        .unwrap();
    let expected = store
        .view(ACCOUNT, deadline())
        .unwrap()
        .identity()
        .committed_sequence;
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected,
                utc_ms: 0,
                deadline: deadline(),
            },
            &[
                Operation::put(Table::Threads, thread.as_bytes(), &[]).unwrap(),
                Operation::put(Table::Emails, FLAG_EMAIL.as_bytes(), &email).unwrap(),
                Operation::put(Table::Mailboxes, FLAG_MAILBOX.as_bytes(), &mailbox).unwrap(),
                Operation::put(Table::Memberships, &membership, &[]).unwrap(),
            ],
            &mut [],
        )
        .unwrap();
}
fn flag_meter(records: u64) -> crate::admission::work::Meter {
    crate::admission::work::Meter::new(
        deadline(),
        crate::admission::work::Charge {
            records,
            ..Default::default()
        },
    )
}
#[test]
fn flag_batch_records_email_and_mailbox_history_without_rewriting_body() {
    fixture_config(4, 100000, seed_flag_email, |coordinator, _, _, _, _| {
        let updates = [
            EmailUpdate {
                mailbox: None,
                email: FLAG_EMAIL,
                seen: Some(true),
                flagged: Some(true),
            },
            EmailUpdate {
                mailbox: None,
                email: crate::ids::EmailId::from_bytes([99; 16]),
                seen: Some(true),
                flagged: None,
            },
        ];
        let request = EmailUpdateRequest {
            account: ACCOUNT,
            device: DEVICE,
            expected: None,
            updates: &updates,
            deadline: deadline(),
        };
        let before = coordinator
            .store
            .view(ACCOUNT, deadline())
            .unwrap()
            .identity();
        let complete = coordinator
            .update_emails(&td_crypto::Provider, request, &mut flag_meter(10000))
            .unwrap();
        let sequence = complete.outcome.unwrap();
        assert!(sequence > before.committed_sequence);
        assert_eq!(complete.before, before);
        assert_eq!(
            complete.results,
            [EmailUpdateResult::Updated, EmailUpdateResult::NotFound]
        );
        assert!(!complete.admission_stopped);
        assert_recount(coordinator);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        let mut scratch = [0; 128];
        for name in ["$seen", "$flagged"] {
            assert!(view
                .get(Key::Keyword(FLAG_EMAIL, name), &mut scratch)
                .unwrap()
                .is_some());
        }
        let cursor = ChangeCursor {
            sequence: before.committed_sequence,
            operation: u32::MAX,
        };
        for (kind, id) in [
            (ObjectType::Email, *FLAG_EMAIL.as_bytes()),
            (ObjectType::Mailbox, *FLAG_MAILBOX.as_bytes()),
        ] {
            let ChangeStep::Record(record) = view.next_change(cursor, kind).unwrap() else {
                panic!("missing history")
            };
            assert_eq!(record.change.id, id);
            assert_eq!(record.change.action, ChangeAction::Updated);
            assert_eq!(record.cursor.sequence, sequence);
        }
        let mut input = view.open_blob_input(&td_crypto::Provider, SEED, 8).unwrap();
        let mut bytes = [0; 4];
        assert_eq!(input.read(&mut bytes).unwrap(), 4);
        input.finish().unwrap();
        assert_eq!(&bytes, b"seed");
        drop(view);
        let noop = coordinator
            .update_emails(&td_crypto::Provider, request, &mut flag_meter(10000))
            .unwrap();
        assert_eq!(noop.outcome, Ok(sequence));
        let updates = [EmailUpdate {
            mailbox: None,
            email: FLAG_EMAIL,
            seen: Some(false),
            flagged: Some(false),
        }];
        let clear = coordinator
            .update_emails(
                &td_crypto::Provider,
                EmailUpdateRequest {
                    updates: &updates,
                    ..request
                },
                &mut flag_meter(10000),
            )
            .unwrap();
        assert!(clear.outcome.unwrap() > sequence);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        for name in ["$seen", "$flagged"] {
            assert!(view
                .get(Key::Keyword(FLAG_EMAIL, name), &mut scratch)
                .unwrap()
                .is_none());
        }
        None
    });
}
#[test]
fn flag_refusals_have_no_partial_writes_or_history() {
    fixture_config(
        4,
        100000,
        seed_flag_email,
        |coordinator, _, clock, policy, _| {
            let updates = [EmailUpdate {
                mailbox: None,
                email: FLAG_EMAIL,
                seen: Some(true),
                flagged: Some(true),
            }];
            let request = EmailUpdateRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                updates: &updates,
                deadline: deadline(),
            };
            let before = coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity();
            assert!(matches!(
                coordinator.update_emails(&td_crypto::Provider, request, &mut flag_meter(3)),
                Err(UploadError::Store(ports::Error::Capacity))
            ));
            assert!(matches!(
                coordinator.update_emails(
                    &td_crypto::Provider,
                    EmailUpdateRequest {
                        expected: Some((
                            StoreEpoch::from_bytes([8; 16]),
                            before.committed_sequence
                        )),
                        ..request
                    },
                    &mut flag_meter(10000)
                ),
                Err(UploadError::Store(ports::Error::Conflict))
            ));
            policy.lock().unwrap().allowed = false;
            assert!(matches!(
                coordinator.update_emails(&td_crypto::Provider, request, &mut flag_meter(10000)),
                Err(UploadError::Store(ports::Error::Forbidden))
            ));
            policy.lock().unwrap().allowed = true;
            policy.lock().unwrap().device = DeviceId::from_bytes([90; 16]);
            assert!(matches!(
                coordinator.update_emails(&td_crypto::Provider, request, &mut flag_meter(10000)),
                Err(UploadError::Store(ports::Error::Forbidden))
            ));
            policy.lock().unwrap().device = DEVICE;
            clock.0.store(100, Ordering::Relaxed);
            assert!(matches!(
                coordinator.update_emails(&td_crypto::Provider, request, &mut flag_meter(10000)),
                Err(UploadError::Store(ports::Error::Deadline))
            ));
            clock.0.store(1, Ordering::Relaxed);
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(view.identity(), before);
            let mut scratch = [0; 128];
            for name in ["$seen", "$flagged"] {
                assert!(view
                    .get(Key::Keyword(FLAG_EMAIL, name), &mut scratch)
                    .unwrap()
                    .is_none());
            }
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn committed_move_and_flags_reopen_with_the_same_history_endpoint() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let clock = Arc::new(Timer(
        AtomicU64::new(1),
        Mutex::new(Default::default()),
        AtomicI64::new(0),
        Mutex::new(None),
    ));
    let resources = Limits::default().plan().unwrap();
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
    seed(&store, 100000);
    seed_move_email(&store);
    let policy = Policy(Arc::new(Mutex::new(PolicyState {
        allowed: true,
        generation: 1,
        device: DEVICE,
        read_mismatch: false,
    })));
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
        policy,
        deadline(),
    )
    .unwrap();
    let updates = [EmailUpdate {
        mailbox: Some(MailboxId::from_bytes([50; 16])),
        email: FLAG_EMAIL,
        seen: Some(true),
        flagged: Some(true),
    }];
    let complete = coordinator
        .update_emails(
            &td_crypto::Provider,
            EmailUpdateRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                updates: &updates,
                deadline: deadline(),
            },
            &mut flag_meter(10000),
        )
        .unwrap();
    let sequence = complete.outcome.unwrap();
    drop(coordinator);
    let store = IndexStore::open(&mut root, clock, 2, deadline()).unwrap();
    store.validate_integrity(deadline()).unwrap();
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().committed_sequence, sequence);
    assert_eq!(
        view.email_mailboxes(FLAG_EMAIL, None, 16).unwrap().values,
        [MailboxId::from_bytes([50; 16])]
    );
    let mut scratch = [0; 128];
    for keyword in ["$seen", "$flagged"] {
        assert_eq!(
            view.get(Key::Keyword(FLAG_EMAIL, keyword), &mut scratch)
                .unwrap()
                .unwrap()
                .1,
            sequence
        );
    }
    let ChangeStep::Record(change) = view
        .next_change(
            ChangeCursor {
                sequence: complete.before.committed_sequence,
                operation: u32::MAX,
            },
            ObjectType::Email,
        )
        .unwrap()
    else {
        panic!("durable history")
    };
    assert_eq!(change.cursor.sequence, sequence);
    assert_eq!(change.change.id, *FLAG_EMAIL.as_bytes());
}

#[test]
fn flags_page_memberships_and_keep_star_history_email_only() {
    fixture_config(4, 100000, seed_move_email, |coordinator, _, _, _, _| {
        let before = coordinator
            .store
            .view(ACCOUNT, deadline())
            .unwrap()
            .identity();
        let updates = [EmailUpdate {
            mailbox: None,
            email: FLAG_EMAIL,
            seen: None,
            flagged: Some(true),
        }];
        let request = EmailUpdateRequest {
            account: ACCOUNT,
            device: DEVICE,
            expected: Some((before.epoch, before.committed_sequence)),
            updates: &updates,
            deadline: deadline(),
        };
        let star = coordinator
            .update_emails(&td_crypto::Provider, request, &mut flag_meter(4))
            .unwrap();
        let star_sequence = star.outcome.unwrap();
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        let cursor = ChangeCursor {
            sequence: before.committed_sequence,
            operation: u32::MAX,
        };
        assert!(matches!(
            view.next_change(cursor, ObjectType::Mailbox).unwrap(),
            ChangeStep::Advanced { .. } | ChangeStep::Complete
        ));
        drop(view);
        assert!(matches!(
            coordinator.update_emails(&td_crypto::Provider, request, &mut flag_meter(10000)),
            Err(UploadError::Store(ports::Error::Conflict))
        ));
        let seen = [EmailUpdate {
            seen: Some(true),
            flagged: None,
            ..updates[0]
        }];
        let request = EmailUpdateRequest {
            expected: None,
            updates: &seen,
            ..request
        };
        // Planning completes, then the final operation charge refuses before COMMIT.
        assert!(matches!(
            coordinator.update_emails(&td_crypto::Provider, request, &mut flag_meter(36)),
            Err(UploadError::Store(ports::Error::Capacity))
        ));
        assert_eq!(
            coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity()
                .committed_sequence,
            star_sequence
        );
        let done = coordinator
            .update_emails(&td_crypto::Provider, request, &mut flag_meter(10000))
            .unwrap();
        let sequence = done.outcome.unwrap();
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        let mut cursor = ChangeCursor {
            sequence: star_sequence,
            operation: u32::MAX,
        };
        let mut count = 0;
        while let ChangeStep::Record(record) =
            view.next_change(cursor, ObjectType::Mailbox).unwrap()
        {
            assert_eq!(record.cursor.sequence, sequence);
            count += 1;
            cursor = record.cursor;
        }
        assert_eq!(count, 21);
        drop(view);
        for (batch, expected) in [
            (vec![updates[0]; 2], ports::Error::Invalid),
            (vec![updates[0]; 257], ports::Error::Capacity),
        ] {
            assert!(
                matches!(coordinator.update_emails(&td_crypto::Provider, EmailUpdateRequest {updates: &batch, ..request}, &mut flag_meter(10000)), Err(UploadError::Store(error)) if error == expected)
            );
        }
        let wider = Deadline::after(Tick(0), 101).unwrap();
        assert!(matches!(
            coordinator.update_emails(
                &td_crypto::Provider,
                EmailUpdateRequest {
                    deadline: wider,
                    ..request
                },
                &mut flag_meter(10000)
            ),
            Err(UploadError::Store(ports::Error::Invalid))
        ));
        assert_recount(coordinator);
        None
    });
}

fn seed_move_email(store: &IndexStore<'_>) {
    seed_flag_email(store);
    let mailbox = row_bytes(Row::Mailbox(crate::format::row::MailboxRow {
        name: "Other",
        parent: None,
        role: None,
        sort_order: 0,
        subscribed: true,
    }));
    for byte in 50..70 {
        let id = MailboxId::from_bytes([byte; 16]);
        let mut key = [0; 32];
        Key::Membership(FLAG_EMAIL, id).encode(&mut key).unwrap();
        let before = store.view(ACCOUNT, deadline()).unwrap().identity();
        store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    account: ACCOUNT,
                    epoch: before.epoch,
                    expected: before.committed_sequence,
                    utc_ms: 0,
                    deadline: deadline(),
                },
                &[
                    Operation::put(Table::Mailboxes, id.as_bytes(), &mailbox).unwrap(),
                    Operation::put(Table::Memberships, &key, &[]).unwrap(),
                ],
                &mut [],
            )
            .unwrap();
    }
}

#[test]
fn moves_replace_paged_memberships_and_preserve_failed_objects() {
    fixture_config(4, 100000, seed_move_email, |coordinator, _, _, _, _| {
        let target = MailboxId::from_bytes([50; 16]);
        let updates = [EmailUpdate {
            mailbox: Some(target),
            email: FLAG_EMAIL,
            seen: None,
            flagged: None,
        }];
        let request = EmailUpdateRequest {
            account: ACCOUNT,
            device: DEVICE,
            expected: None,
            updates: &updates,
            deadline: deadline(),
        };
        let before = coordinator
            .store
            .view(ACCOUNT, deadline())
            .unwrap()
            .identity();
        assert!(matches!(
            coordinator.update_emails(&td_crypto::Provider, request, &mut flag_meter(36)),
            Err(UploadError::Store(ports::Error::Capacity))
        ));
        assert_eq!(
            coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity(),
            before
        );
        let invalid = [
            EmailUpdate {
                mailbox: Some(MailboxId::from_bytes([99; 16])),
                seen: Some(true),
                ..updates[0]
            },
            EmailUpdate {
                email: crate::ids::EmailId::from_bytes([99; 16]),
                ..updates[0]
            },
        ];
        let refusal = coordinator
            .update_emails(
                &td_crypto::Provider,
                EmailUpdateRequest {
                    updates: &invalid,
                    ..request
                },
                &mut flag_meter(10000),
            )
            .unwrap();
        assert_eq!(
            refusal.results,
            [
                EmailUpdateResult::InvalidMailbox,
                EmailUpdateResult::NotFound
            ]
        );
        assert_eq!(refusal.outcome, Ok(before.committed_sequence));
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        assert!(view
            .get(Key::Keyword(FLAG_EMAIL, "$seen"), &mut [0; 128])
            .unwrap()
            .is_none());
        drop(view);
        let done = coordinator
            .update_emails(&td_crypto::Provider, request, &mut flag_meter(10000))
            .unwrap();
        let sequence = done.outcome.unwrap();
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.email_mailboxes(FLAG_EMAIL, None, 16).unwrap().values,
            [target]
        );
        let mut cursor = ChangeCursor {
            sequence: before.committed_sequence,
            operation: u32::MAX,
        };
        let mut affected = Vec::new();
        while let ChangeStep::Record(record) =
            view.next_change(cursor, ObjectType::Mailbox).unwrap()
        {
            assert_eq!(record.cursor.sequence, sequence);
            affected.push(record.change.id);
            cursor = record.cursor;
        }
        assert_eq!(affected.len(), 20);
        assert!(!affected.contains(target.as_bytes())); // Its membership/counts did not change.
        drop(view);
        assert_eq!(
            coordinator
                .update_emails(&td_crypto::Provider, request, &mut flag_meter(10000))
                .unwrap()
                .outcome,
            Ok(sequence)
        );
        let back = [EmailUpdate {
            mailbox: Some(FLAG_MAILBOX),
            seen: Some(true),
            ..updates[0]
        }];
        let done = coordinator
            .update_emails(
                &td_crypto::Provider,
                EmailUpdateRequest {
                    updates: &back,
                    ..request
                },
                &mut flag_meter(10000),
            )
            .unwrap();
        assert!(done.outcome.unwrap() > sequence);
        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            view.email_mailboxes(FLAG_EMAIL, None, 16).unwrap().values,
            [FLAG_MAILBOX]
        );
        assert!(view
            .get(Key::Keyword(FLAG_EMAIL, "$seen"), &mut [0; 128])
            .unwrap()
            .is_some());
        drop(view);
        assert_recount(coordinator);
        None
    });
}

#[test]
fn mixed_moves_commit_only_valid_objects_and_track_retained_unread_counts() {
    let other = crate::ids::EmailId::from_bytes([80; 16]);
    let target = MailboxId::from_bytes([81; 16]);
    fixture_config(
        4,
        100000,
        |store| {
            seed_move_email(store);
            let email = row_bytes(Row::Email(crate::format::row::EmailRow {
                blob: SEED,
                thread: crate::ids::ThreadId::from_bytes([42; 16]),
                received_at: 0,
                origin: crate::format::row::EmailOrigin::Jmap,
            }));
            let mailbox = row_bytes(Row::Mailbox(crate::format::row::MailboxRow {
                name: "Archive",
                parent: None,
                role: None,
                sort_order: 0,
                subscribed: true,
            }));
            let mut key = [0; 32];
            Key::Membership(other, FLAG_MAILBOX)
                .encode(&mut key)
                .unwrap();
            let before = store.view(ACCOUNT, deadline()).unwrap().identity();
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: before.epoch,
                        expected: before.committed_sequence,
                        utc_ms: 0,
                        deadline: deadline(),
                    },
                    &[
                        Operation::put(Table::Emails, other.as_bytes(), &email).unwrap(),
                        Operation::put(Table::Mailboxes, target.as_bytes(), &mailbox).unwrap(),
                        Operation::put(Table::Memberships, &key, &[]).unwrap(),
                    ],
                    &mut [],
                )
                .unwrap();
        },
        |coordinator, _, _, _, _| {
            let updates = [
                EmailUpdate {
                    email: other,
                    mailbox: Some(MailboxId::from_bytes([99; 16])),
                    seen: Some(true),
                    flagged: Some(true),
                },
                EmailUpdate {
                    email: FLAG_EMAIL,
                    mailbox: Some(target),
                    seen: Some(true),
                    flagged: Some(true),
                },
            ];
            let request = EmailUpdateRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                updates: &updates,
                deadline: deadline(),
            };
            let done = coordinator
                .update_emails(&td_crypto::Provider, request, &mut flag_meter(10000))
                .unwrap();
            let sequence = done.outcome.unwrap();
            assert_eq!(
                done.results,
                [
                    EmailUpdateResult::InvalidMailbox,
                    EmailUpdateResult::Updated
                ]
            );
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(
                view.email_mailboxes(FLAG_EMAIL, None, 16).unwrap().values,
                [target]
            );
            assert_eq!(
                view.email_mailboxes(other, None, 16).unwrap().values,
                [FLAG_MAILBOX]
            );
            for keyword in ["$seen", "$flagged"] {
                assert!(view
                    .get(Key::Keyword(other, keyword), &mut [0; 128])
                    .unwrap()
                    .is_none());
                assert!(view
                    .get(Key::Keyword(FLAG_EMAIL, keyword), &mut [0; 128])
                    .unwrap()
                    .is_some());
            }
            for (kind, expected) in [(ObjectType::Email, 1), (ObjectType::Mailbox, 22)] {
                let mut cursor = ChangeCursor {
                    sequence: done.before.committed_sequence,
                    operation: u32::MAX,
                };
                let mut count = 0;
                while let ChangeStep::Record(record) = view.next_change(cursor, kind).unwrap() {
                    if kind == ObjectType::Email {
                        assert_eq!(record.change.id, *FLAG_EMAIL.as_bytes());
                    }
                    assert_eq!(record.cursor.sequence, sequence);
                    count += 1;
                    cursor = record.cursor;
                }
                assert_eq!(count, expected);
            }
            drop(view);
            let updates = [EmailUpdate {
                seen: Some(false),
                ..updates[1]
            }];
            let done = coordinator
                .update_emails(
                    &td_crypto::Provider,
                    EmailUpdateRequest {
                        updates: &updates,
                        ..request
                    },
                    &mut flag_meter(10000),
                )
                .unwrap();
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let ChangeStep::Record(record) = view
                .next_change(
                    ChangeCursor {
                        sequence,
                        operation: u32::MAX,
                    },
                    ObjectType::Mailbox,
                )
                .unwrap()
            else {
                panic!("history");
            };
            assert_eq!(record.change.id, *target.as_bytes());
            assert_eq!(record.cursor.sequence, done.outcome.unwrap());
            assert!(matches!(
                view.next_change(record.cursor, ObjectType::Mailbox)
                    .unwrap(),
                ChangeStep::Complete | ChangeStep::Advanced { .. }
            ));
            drop(view);
            assert_recount(coordinator);
            None
        },
    );
}

impl MailboxAuthorization for Policy {
    type Guard<'a> = Guard<'a>;
    fn authorize_mailboxes(
        &self,
        account: AccountId,
        device: DeviceId,
        deadline: Deadline,
    ) -> Result<Guard<'_>, ports::Error> {
        self.authorize(account, device, deadline)
    }
}
fn mailbox_meter() -> crate::admission::work::Meter {
    crate::admission::work::Meter::new(
        deadline(),
        crate::admission::work::Charge {
            records: 1_000_000,
            io_bytes: 1_000_000,
            ..Default::default()
        },
    )
}
#[test]
fn root_folder_creation_has_partial_results_unique_names_and_history() {
    fixture_config(
        4,
        100000,
        seed_flag_email,
        |coordinator, _, clock, policy, _| {
            let names = [
                "archive",
                "trash",
                "archive",
                "Inbox",
                "",
                "bad\nname",
                "Cafe\u{301}",
                "Café",
                "\u{ffff}",
            ];
            let request = MailboxCreateRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                names: &names,
                deadline: deadline(),
            };
            struct Armed<'a> {
                random: Random,
                clock: &'a Timer,
                policy: &'a Arc<Mutex<PolicyState>>,
            }
            impl Entropy for Armed<'_> {
                fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
                    *self.clock.3.lock().unwrap() = Some(self.policy.clone());
                    self.random.fill(bytes)
                }
            }
            let complete = coordinator
                .create_mailboxes(
                    &td_crypto::Provider,
                    &mut Armed {
                        random: Random(90),
                        clock,
                        policy,
                    },
                    request,
                    &mut mailbox_meter(),
                )
                .unwrap();
            *clock.3.lock().unwrap() = None;
            assert!(!complete.admission_stopped);
            let files = coordinator
                .store
                .usage_fence(deadline())
                .unwrap()
                .file_usage();
            for (kind, bytes) in [
                (Kind::DatabaseBytes, files.database_bytes),
                (Kind::WalBytes, files.wal_bytes),
            ] {
                assert_eq!(coordinator.used(kind).unwrap(), bytes);
                assert_eq!(
                    coordinator
                        .state
                        .lock()
                        .unwrap()
                        .ledger
                        .pending(kind)
                        .unwrap(),
                    0
                );
            }
            let sequence = complete.outcome.unwrap();
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            for (index, name) in names.iter().enumerate() {
                if [0, 1, 7].contains(&index) {
                    let MailboxCreateResult::Created(id) = complete.results[index] else {
                        panic!("creation");
                    };
                    let mut scratch = [0; 128];
                    let (Row::Mailbox(row), changed) =
                        view.get(Key::Mailbox(id), &mut scratch).unwrap().unwrap()
                    else {
                        panic!("mailbox");
                    };
                    assert_eq!(row.name, *name);
                    assert_eq!(row.parent, None);
                    assert_eq!(row.role, None);
                    assert!(row.subscribed);
                    assert_eq!(changed, sequence);
                } else if [2, 3].contains(&index) {
                    assert_eq!(complete.results[index], MailboxCreateResult::AlreadyExists);
                } else {
                    assert_eq!(complete.results[index], MailboxCreateResult::InvalidName);
                }
            }
            let mut cursor = ChangeCursor {
                sequence: complete.before.committed_sequence,
                operation: u32::MAX,
            };
            let mut count = 0;
            while let ChangeStep::Record(record) =
                view.next_change(cursor, ObjectType::Mailbox).unwrap()
            {
                assert_eq!(record.change.action, ChangeAction::Created);
                assert_eq!(record.cursor.sequence, sequence);
                count += 1;
                cursor = record.cursor;
            }
            assert_eq!(count, 3);
            drop(view);
            struct NoEntropy;
            impl Entropy for NoEntropy {
                fn fill(&mut self, _: &mut [u8]) -> Result<(), td_crypto::Error> {
                    Err(td_crypto::Error::Entropy)
                }
            }
            let retry = coordinator
                .create_mailboxes(
                    &td_crypto::Provider,
                    &mut NoEntropy,
                    request,
                    &mut mailbox_meter(),
                )
                .unwrap();
            assert_eq!(retry.outcome, Ok(sequence));
            assert!(!retry
                .results
                .iter()
                .any(|result| matches!(result, MailboxCreateResult::Created(_))));
            assert_recount(coordinator);
            None
        },
    );
}
#[test]
fn folder_creation_checks_authorization_state_and_original_work_before_effects() {
    fixture_config(
        4,
        100000,
        seed_flag_email,
        |coordinator, _, _, policy, _| {
            let before = coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity();
            let names = ["archive"];
            let request = MailboxCreateRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                names: &names,
                deadline: deadline(),
            };
            assert!(matches!(
                coordinator.create_mailboxes(
                    &td_crypto::Provider,
                    &mut Random(90),
                    request,
                    &mut flag_meter(0)
                ),
                Err(UploadError::Store(ports::Error::Capacity))
            ));
            assert!(matches!(
                coordinator.create_mailboxes(
                    &td_crypto::Provider,
                    &mut Random(90),
                    MailboxCreateRequest {
                        expected: Some((before.epoch, Sequence::default())),
                        ..request
                    },
                    &mut mailbox_meter()
                ),
                Err(UploadError::Store(ports::Error::Conflict))
            ));
            assert!(matches!(
                coordinator.create_mailboxes(
                    &td_crypto::Provider,
                    &mut Random(90),
                    MailboxCreateRequest {
                        deadline: Deadline::after(Tick(0), 101).unwrap(),
                        ..request
                    },
                    &mut mailbox_meter()
                ),
                Err(UploadError::Store(ports::Error::Invalid))
            ));
            policy.lock().unwrap().allowed = false;
            assert!(matches!(
                coordinator.create_mailboxes(
                    &td_crypto::Provider,
                    &mut Random(90),
                    request,
                    &mut mailbox_meter()
                ),
                Err(UploadError::Store(ports::Error::Forbidden))
            ));
            assert_eq!(
                coordinator
                    .store
                    .view(ACCOUNT, deadline())
                    .unwrap()
                    .identity(),
                before
            );
            None
        },
    );
}

#[test]
fn folder_creation_bounds_names_collision_attempts_and_entropy_deadline() {
    fixture_config(
        4,
        100000,
        seed_flag_email,
        |coordinator, _, clock, policy, _| {
            struct Collision<'a> {
                clock: &'a Timer,
                policy: &'a Mutex<PolicyState>,
                late: bool,
                calls: usize,
            }
            impl Entropy for Collision<'_> {
                fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
                    assert!(matches!(
                        self.policy.try_lock(),
                        Err(TryLockError::WouldBlock)
                    ));
                    output.copy_from_slice(FLAG_MAILBOX.as_bytes());
                    self.calls += 1;
                    if self.late {
                        self.clock.0.store(100, Ordering::Relaxed);
                    }
                    Ok(())
                }
            }
            let before = coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity();
            for late in [false, true] {
                let mut entropy = Collision {
                    clock,
                    policy,
                    late,
                    calls: 0,
                };
                let result = coordinator.create_mailboxes(
                    &td_crypto::Provider,
                    &mut entropy,
                    MailboxCreateRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        names: &["archive"],
                        deadline: deadline(),
                    },
                    &mut mailbox_meter(),
                );
                assert!(
                    matches!(result, Err(UploadError::Store(error)) if error == if late { ports::Error::Deadline } else { ports::Error::Capacity })
                );
                assert_eq!(entropy.calls, if late { 1 } else { 4 });
                clock.0.store(1, Ordering::Relaxed);
                assert_eq!(
                    coordinator
                        .store
                        .view(ACCOUNT, deadline())
                        .unwrap()
                        .identity(),
                    before
                );
            }
            let too_long = "x".repeat(format::row::MAX_MAILBOX_NAME + 1);
            let names = [too_long.as_str()];
            let request = MailboxCreateRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                names: &names,
                deadline: deadline(),
            };
            let result = coordinator
                .create_mailboxes(
                    &td_crypto::Provider,
                    &mut Random(90),
                    request,
                    &mut mailbox_meter(),
                )
                .unwrap();
            assert_eq!(result.results, [MailboxCreateResult::InvalidName]);
            assert_eq!(result.outcome, Ok(before.committed_sequence));
            let names = vec!["archive"; MAX_MAILBOX_CREATES + 1];
            assert!(matches!(
                coordinator.create_mailboxes(
                    &td_crypto::Provider,
                    &mut Random(90),
                    MailboxCreateRequest {
                        names: &names,
                        ..request
                    },
                    &mut mailbox_meter()
                ),
                Err(UploadError::Store(ports::Error::Capacity))
            ));
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn root_creation_ignores_child_names_and_retries_in_batch_id_collisions() {
    fixture_config(
        4,
        100000,
        |store| {
            seed_flag_email(store);
            let encoded = row_bytes(Row::Mailbox(crate::format::row::MailboxRow {
                name: "archive",
                parent: Some(FLAG_MAILBOX),
                role: None,
                sort_order: 0,
                subscribed: true,
            }));
            let before = store.view(ACCOUNT, deadline()).unwrap().identity();
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: before.epoch,
                        expected: before.committed_sequence,
                        utc_ms: 0,
                        deadline: deadline(),
                    },
                    &[Operation::put(
                        Table::Mailboxes,
                        MailboxId::from_bytes([60; 16]).as_bytes(),
                        &encoded,
                    )
                    .unwrap()],
                    &mut [],
                )
                .unwrap();
        },
        |coordinator, _, _, _, _| {
            struct Repeated(std::collections::VecDeque<u8>);
            impl Entropy for Repeated {
                fn fill(&mut self, bytes: &mut [u8]) -> Result<(), td_crypto::Error> {
                    bytes.fill(self.0.pop_front().unwrap());
                    Ok(())
                }
            }
            let mut entropy = Repeated([91, 91, 92].into());
            let complete = coordinator
                .create_mailboxes(
                    &td_crypto::Provider,
                    &mut entropy,
                    MailboxCreateRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        names: &["archive", "trash"],
                        deadline: deadline(),
                    },
                    &mut mailbox_meter(),
                )
                .unwrap();
            assert!(complete.outcome.is_ok());
            assert_eq!(
                complete.results,
                [
                    MailboxCreateResult::Created(MailboxId::from_bytes([91; 16])),
                    MailboxCreateResult::Created(MailboxId::from_bytes([92; 16]))
                ]
            );
            assert!(entropy.0.is_empty());
            None
        },
    );
}

#[test]
fn root_folder_exact_batch_and_utf8_byte_limits_are_accepted() {
    struct Counter(u128);
    impl Entropy for Counter {
        fn fill(&mut self, output: &mut [u8]) -> Result<(), td_crypto::Error> {
            self.0 += 1;
            output.copy_from_slice(&self.0.to_be_bytes());
            Ok(())
        }
    }
    fixture_config(
        4,
        100000,
        |_| {},
        |coordinator, _, _, _, _| {
            let mut owned: Vec<_> = (0..MAX_MAILBOX_CREATES)
                .map(|n| format!("folder{n}"))
                .collect();
            owned[0] = format!("{}é", "a".repeat(1022));
            assert_eq!(owned[0].len(), format::row::MAX_MAILBOX_NAME);
            let names: Vec<_> = owned.iter().map(String::as_str).collect();
            let complete = coordinator
                .create_mailboxes(
                    &td_crypto::Provider,
                    &mut Counter(1000),
                    MailboxCreateRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        names: &names,
                        deadline: deadline(),
                    },
                    &mut mailbox_meter(),
                )
                .unwrap();
            assert!(complete.outcome.is_ok());
            assert_eq!(complete.results.len(), MAX_MAILBOX_CREATES);
            assert!(complete
                .results
                .iter()
                .all(|result| matches!(result, MailboxCreateResult::Created(_))));
            assert_recount(coordinator);
            None
        },
    );
}

fn seed_destroy_email(store: &IndexStore<'_>, retained_lease: bool, shared: bool) {
    seed_flag_email(store);
    let before = store.view(ACCOUNT, deadline()).unwrap().identity();
    let mut anchor = [0; 128];
    let anchor_len = Key::ThreadAnchor("message@example.test", FLAG_EMAIL)
        .encode(&mut anchor)
        .unwrap();
    let mut keyword = [0; 128];
    let keyword_len = Key::Keyword(FLAG_EMAIL, "$seen")
        .encode(&mut keyword)
        .unwrap();
    let other = crate::ids::EmailId::from_bytes([43; 16]);
    let other_value = row_bytes(Row::Email(crate::format::row::EmailRow {
        blob: SEED,
        thread: crate::ids::ThreadId::from_bytes([42; 16]),
        received_at: 1,
        origin: crate::format::row::EmailOrigin::Jmap,
    }));
    let mut operations = vec![
        Operation::put(Table::ThreadAnchors, &anchor[..anchor_len], &[]).unwrap(),
        Operation::put(Table::Keywords, &keyword[..keyword_len], &[]).unwrap(),
    ];
    if !retained_lease {
        operations.push(Operation::delete(Table::Leases, SEED.as_bytes()).unwrap());
    }
    if shared {
        operations.push(Operation::put(Table::Emails, other.as_bytes(), &other_value).unwrap());
    }
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: before.epoch,
                expected: before.committed_sequence,
                utc_ms: 0,
                deadline: deadline(),
            },
            &operations,
            &mut [],
        )
        .unwrap();
}
#[test]
fn deletion_retires_last_owner_and_preserves_shared_bodies_and_leases() {
    for retained_lease in [false, true] {
        for shared in [false, true] {
            fixture_config(
                4,
                100000,
                |store| seed_destroy_email(store, retained_lease, shared),
                |coordinator, _, _, _, _| {
                    let missing = crate::ids::EmailId::from_bytes([99; 16]);
                    let emails = [FLAG_EMAIL, missing];
                    let request = EmailDestroyRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        emails: &emails,
                        deadline: deadline(),
                    };
                    let before = coordinator
                        .store
                        .view(ACCOUNT, deadline())
                        .unwrap()
                        .identity();
                    let complete = coordinator
                        .destroy_emails(&td_crypto::Provider, request, &mut flag_meter(100000))
                        .unwrap();
                    let sequence = complete.outcome.unwrap();
                    assert!(sequence > before.committed_sequence);
                    assert_eq!(
                        complete.results,
                        [EmailDestroyResult::Destroyed, EmailDestroyResult::NotFound]
                    );
                    assert!(!complete.admission_stopped);
                    let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
                    let mut scratch = [0; 128];
                    for key in [
                        Key::Email(FLAG_EMAIL),
                        Key::Membership(FLAG_EMAIL, FLAG_MAILBOX),
                        Key::Keyword(FLAG_EMAIL, "$seen"),
                        Key::ThreadAnchor("message@example.test", FLAG_EMAIL),
                    ] {
                        assert!(view.get(key, &mut scratch).unwrap().is_none());
                    }
                    assert!(view
                        .get(Key::Mailbox(FLAG_MAILBOX), &mut scratch)
                        .unwrap()
                        .is_some());
                    assert_eq!(
                        view.get(
                            Key::Thread(crate::ids::ThreadId::from_bytes([42; 16])),
                            &mut scratch
                        )
                        .unwrap()
                        .is_some(),
                        shared
                    );
                    assert_eq!(
                        view.get(Key::Blob(SEED), &mut scratch).unwrap().is_some(),
                        shared || retained_lease
                    );
                    assert_eq!(
                        view.get(Key::Lease(SEED), &mut scratch).unwrap().is_some(),
                        retained_lease
                    );
                    drop(view);
                    assert_recount(coordinator);
                    let retry = coordinator
                        .destroy_emails(&td_crypto::Provider, request, &mut flag_meter(100000))
                        .unwrap();
                    assert_eq!(retry.outcome, Ok(sequence));
                    assert_eq!(retry.results, [EmailDestroyResult::NotFound; 2]);
                    if shared {
                        let other = [crate::ids::EmailId::from_bytes([43; 16])];
                        let result = coordinator
                            .destroy_emails(
                                &td_crypto::Provider,
                                EmailDestroyRequest {
                                    emails: &other,
                                    ..request
                                },
                                &mut flag_meter(100000),
                            )
                            .unwrap();
                        assert!(result.outcome.is_ok());
                        let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
                        assert_eq!(
                            view.get(Key::Blob(SEED), &mut scratch).unwrap().is_some(),
                            retained_lease
                        );
                        assert!(view
                            .get(
                                Key::Thread(crate::ids::ThreadId::from_bytes([42; 16])),
                                &mut scratch
                            )
                            .unwrap()
                            .is_none());
                        drop(view);
                        assert_recount(coordinator);
                    }
                    None
                },
            );
        }
    }
}
#[test]
fn deletion_refusals_preserve_rows_and_history() {
    fixture_config(
        4,
        100000,
        |store| seed_destroy_email(store, false, false),
        |coordinator, _, clock, policy, _| {
            let emails = [FLAG_EMAIL];
            let request = EmailDestroyRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                emails: &emails,
                deadline: deadline(),
            };
            let before = coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity();
            assert!(matches!(
                coordinator.destroy_emails(&td_crypto::Provider, request, &mut flag_meter(2)),
                Err(UploadError::Store(ports::Error::Capacity))
            ));
            assert!(matches!(
                coordinator.destroy_emails(
                    &td_crypto::Provider,
                    EmailDestroyRequest {
                        expected: Some((
                            StoreEpoch::from_bytes([99; 16]),
                            before.committed_sequence
                        )),
                        ..request
                    },
                    &mut flag_meter(100000)
                ),
                Err(UploadError::Store(ports::Error::Conflict))
            ));
            policy.lock().unwrap().allowed = false;
            assert!(matches!(
                coordinator.destroy_emails(&td_crypto::Provider, request, &mut flag_meter(100000)),
                Err(UploadError::Store(ports::Error::Forbidden))
            ));
            policy.lock().unwrap().allowed = true;
            let duplicates = [FLAG_EMAIL; 2];
            assert!(matches!(
                coordinator.destroy_emails(
                    &td_crypto::Provider,
                    EmailDestroyRequest {
                        emails: &duplicates,
                        ..request
                    },
                    &mut flag_meter(100000)
                ),
                Err(UploadError::Store(ports::Error::Invalid))
            ));
            clock.0.store(100, Ordering::Relaxed);
            assert!(matches!(
                coordinator.destroy_emails(&td_crypto::Provider, request, &mut flag_meter(100000)),
                Err(UploadError::Store(ports::Error::Deadline))
            ));
            clock.0.store(1, Ordering::Relaxed);
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(view.identity(), before);
            assert!(view
                .get(Key::Email(FLAG_EMAIL), &mut [0; 128])
                .unwrap()
                .is_some());
            drop(view);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn deletion_guard_known_commit_and_uncertain_outcomes_keep_correct_quota() {
    for deny in [false, true] {
        fixture_config(
            4,
            100000,
            |store| seed_destroy_email(store, false, false),
            |coordinator, _, clock, policy, _| {
                let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let check = observed.clone();
                let shared = policy.clone();
                let timer = clock.clone();
                set_writer_authorizer(coordinator, move |context| {
                    if matches!(context.action, AuthAction::Transaction { operation }
                    if !matches!(operation, TransactionOperation::Begin | TransactionOperation::Rollback))
                    {
                        assert!(matches!(shared.try_lock(), Err(TryLockError::WouldBlock)));
                        check.store(true, Ordering::Relaxed);
                        if deny {
                            return Authorization::Deny;
                        }
                        timer.0.store(100, Ordering::Relaxed);
                    }
                    Authorization::Allow
                });
                let request = EmailDestroyRequest {
                    account: ACCOUNT,
                    device: DEVICE,
                    expected: None,
                    emails: &[FLAG_EMAIL],
                    deadline: deadline(),
                };
                let complete = coordinator
                    .destroy_emails(&td_crypto::Provider, request, &mut flag_meter(100000))
                    .unwrap();
                assert!(observed.load(Ordering::Relaxed));
                assert!(policy.try_lock().is_ok());
                assert!(complete.admission_stopped);
                assert_eq!(complete.results, [EmailDestroyResult::Destroyed]);
                if deny {
                    assert!(matches!(
                        complete.outcome,
                        Err(CommitError::Indeterminate(_))
                    ));
                    assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
                    assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 1);
                } else {
                    assert!(complete.outcome.is_ok());
                    assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
                    assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 0);
                }
                clock.0.store(1, Ordering::Relaxed);
                assert!(matches!(
                    coordinator.destroy_emails(
                        &td_crypto::Provider,
                        request,
                        &mut flag_meter(100000)
                    ),
                    Err(UploadError::Store(ports::Error::WriterStopped))
                ));
                lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                    .unwrap()
                    .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                    .unwrap();
                None
            },
        );
    }
}

#[test]
fn deletion_batch_retires_shared_thread_once_and_records_all_object_changes() {
    fixture_config(
        4,
        100000,
        |store| seed_destroy_email(store, false, true),
        |coordinator, _, _, _, _| {
            let other = crate::ids::EmailId::from_bytes([43; 16]);
            // Deliberately descending: ownership uses a separately sorted selection.
            let emails = [other, FLAG_EMAIL];
            let request = EmailDestroyRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                emails: &emails,
                deadline: deadline(),
            };
            let complete = coordinator
                .destroy_emails(&td_crypto::Provider, request, &mut flag_meter(100000))
                .unwrap();
            let sequence = complete.outcome.unwrap();
            assert_eq!(complete.results, [EmailDestroyResult::Destroyed; 2]);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 0);
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            for (kind, expected, action) in [
                (ObjectType::Email, 2, ChangeAction::Destroyed),
                (ObjectType::Thread, 1, ChangeAction::Destroyed),
                (ObjectType::Mailbox, 1, ChangeAction::Updated),
            ] {
                let mut cursor = ChangeCursor {
                    sequence: complete.before.committed_sequence,
                    operation: u32::MAX,
                };
                let mut count = 0;
                while let ChangeStep::Record(record) = view.next_change(cursor, kind).unwrap() {
                    assert_eq!(record.change.action, action);
                    assert_eq!(record.cursor.sequence, sequence);
                    cursor = record.cursor;
                    count += 1;
                }
                assert_eq!(count, expected);
            }
            drop(view);
            assert_recount(coordinator);
            let before = coordinator
                .store
                .usage_fence(deadline())
                .unwrap()
                .file_usage();
            assert_eq!(
                coordinator.used(Kind::DatabaseBytes).unwrap(),
                before.database_bytes
            );
            assert_eq!(coordinator.used(Kind::WalBytes).unwrap(), before.wal_bytes);
            None
        },
    );
}

#[test]
fn deletion_request_bounds_refuse_before_effects() {
    fixture_config(
        4,
        100000,
        |store| seed_destroy_email(store, false, false),
        |coordinator, _, _, _, _| {
            let emails = [FLAG_EMAIL; MAX_EMAIL_DESTROYS + 1];
            let request = EmailDestroyRequest {
                account: ACCOUNT,
                device: DEVICE,
                expected: None,
                emails: &emails,
                deadline: deadline(),
            };
            assert!(matches!(
                coordinator.destroy_emails(&td_crypto::Provider, request, &mut flag_meter(100000)),
                Err(UploadError::Store(ports::Error::Capacity))
            ));
            let short = crate::admission::work::Meter::new(
                Deadline::after(Tick(0), 50).unwrap(),
                crate::admission::work::Charge {
                    records: 100000,
                    ..Default::default()
                },
            );
            assert!(matches!(
                coordinator.destroy_emails(
                    &td_crypto::Provider,
                    EmailDestroyRequest {
                        emails: &[FLAG_EMAIL],
                        ..request
                    },
                    &mut { short }
                ),
                Err(UploadError::Store(ports::Error::Invalid))
            ));
            None
        },
    );
}

#[test]
fn deletion_preserves_submission_body_and_historical_message_identifiers() {
    use crate::{
        format::row::{FailureReason, NotificationState, RecipientState, SubmissionRow},
        ids::{IdentityId, SubmissionId, ThreadId},
    };
    let submission = SubmissionId::from_bytes([61; 16]);
    fixture_config(
        4,
        100000,
        |store| {
            seed_destroy_email(store, false, false);
            let mut recipient = super::super::recipient_tests::queued();
            recipient.state = RecipientState::Canceled;
            recipient.next_attempt_at = None;
            recipient.reason = FailureReason::Canceled;
            let value = row_bytes(Row::Submission(SubmissionRow {
                email: FLAG_EMAIL,
                thread: ThreadId::from_bytes([42; 16]),
                identity: IdentityId::from_bytes([62; 16]),
                transmitted_blob: SEED,
                reverse_path: "",
                send_at: 0,
                expires_at: 432000000,
                recipient_count: 1,
                completed_at: Some(1),
                notification: NotificationState::None,
                notification_email: None,
            }));
            let recipient_value = row_bytes(Row::Recipient(recipient));
            let mut recipient_key = [0; 20];
            Key::Recipient(submission, 0)
                .encode(&mut recipient_key)
                .unwrap();
            let before = store.view(ACCOUNT, deadline()).unwrap().identity();
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: before.epoch,
                        expected: before.committed_sequence,
                        utc_ms: 1,
                        deadline: deadline(),
                    },
                    &[
                        Operation::put(Table::Submissions, submission.as_bytes(), &value).unwrap(),
                        Operation::put(Table::Recipients, &recipient_key, &recipient_value)
                            .unwrap(),
                    ],
                    &mut [],
                )
                .unwrap();
        },
        |coordinator, _, _, _, _| {
            let complete = coordinator
                .destroy_emails(
                    &td_crypto::Provider,
                    EmailDestroyRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        emails: &[FLAG_EMAIL],
                        deadline: deadline(),
                    },
                    &mut flag_meter(100000),
                )
                .unwrap();
            assert!(complete.outcome.is_ok());
            assert!(!complete.admission_stopped);
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            let mut bytes = [0; 128];
            assert!(view
                .get(Key::Email(FLAG_EMAIL), &mut bytes)
                .unwrap()
                .is_none());
            assert!(view
                .get(Key::Thread(ThreadId::from_bytes([42; 16])), &mut bytes)
                .unwrap()
                .is_none());
            assert!(view.get(Key::Blob(SEED), &mut bytes).unwrap().is_some());
            let (Row::Submission(row), _) = view
                .get(Key::Submission(submission), &mut bytes)
                .unwrap()
                .unwrap()
            else {
                panic!("submission");
            };
            assert_eq!(row.email, FLAG_EMAIL);
            assert_eq!(row.thread, ThreadId::from_bytes([42; 16]));
            assert_eq!(row.transmitted_blob, SEED);
            drop(view);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn deletion_cascades_smtp_receipts_and_pages_memberships_and_keywords() {
    use crate::format::row::{EmailOrigin, EmailRow, ReceiptRecipients, ReceiptTls, SmtpReceipt};
    fixture_config(
        4,
        100000,
        |store| {
            seed_destroy_email(store, false, false);
            let mut recipients = [0; 128];
            let recipients = ReceiptRecipients::encode(
                &["one@example.test", "two@example.test"],
                &mut recipients,
            )
            .unwrap();
            let row = Row::Email(EmailRow {
                blob: SEED,
                thread: crate::ids::ThreadId::from_bytes([42; 16]),
                received_at: 1,
                origin: EmailOrigin::Smtp(SmtpReceipt {
                    peer: "192.0.2.1".parse().unwrap(),
                    gateway: None,
                    tls: ReceiptTls::Tls13,
                    ehlo: "sender.test",
                    reverse_path: "",
                    recipients,
                }),
            });
            let mut value = [0; 512];
            let length = row.encode(&mut value).unwrap();
            let keys: Vec<_> = (0..40)
                .map(|n| {
                    let mut key = [0; 128];
                    let length = Key::Keyword(FLAG_EMAIL, &format!("custom{n:03}"))
                        .encode(&mut key)
                        .unwrap();
                    key[..length].to_vec()
                })
                .collect();
            let boxes: Vec<_> = (70..87)
                .map(|byte| MailboxId::from_bytes([byte; 16]))
                .collect();
            let values: Vec<_> = boxes
                .iter()
                .enumerate()
                .map(|(n, _)| {
                    row_bytes(Row::Mailbox(crate::format::row::MailboxRow {
                        name: &format!("box{n}"),
                        parent: None,
                        role: None,
                        sort_order: 0,
                        subscribed: true,
                    }))
                })
                .collect();
            let memberships: Vec<_> = boxes
                .iter()
                .map(|mailbox| {
                    let mut key = [0; 32];
                    Key::Membership(FLAG_EMAIL, *mailbox)
                        .encode(&mut key)
                        .unwrap();
                    key
                })
                .collect();
            let mut operations =
                vec![
                    Operation::put(Table::Emails, FLAG_EMAIL.as_bytes(), &value[..length]).unwrap(),
                ];
            for key in &keys {
                operations.push(Operation::put(Table::Keywords, key, &[]).unwrap());
            }
            for ((mailbox, value), membership) in boxes.iter().zip(&values).zip(&memberships) {
                operations
                    .push(Operation::put(Table::Mailboxes, mailbox.as_bytes(), value).unwrap());
                operations.push(Operation::put(Table::Memberships, membership, &[]).unwrap());
            }
            let before = store.view(ACCOUNT, deadline()).unwrap().identity();
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: ACCOUNT,
                        epoch: before.epoch,
                        expected: before.committed_sequence,
                        utc_ms: 1,
                        deadline: deadline(),
                    },
                    &operations,
                    &mut [],
                )
                .unwrap();
        },
        |coordinator, _, _, _, _| {
            let receipt_count = || {
                let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
                view.read_snapshot(|native| {
                    native.run(|db| {
                        db.query_row("SELECT count(*) FROM smtp_receipt_recipients", [], |row| {
                            row.get::<_, i64>(0)
                        })
                        .map_err(sql)
                    })
                })
                .unwrap()
            };
            assert_eq!(receipt_count(), 2);
            let result = coordinator
                .destroy_emails(
                    &td_crypto::Provider,
                    EmailDestroyRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        emails: &[FLAG_EMAIL],
                        deadline: deadline(),
                    },
                    &mut flag_meter(100000),
                )
                .unwrap();
            assert!(result.outcome.is_ok());
            assert_eq!(receipt_count(), 0);
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            assert!(view
                .email_keywords(FLAG_EMAIL, None, 16)
                .unwrap()
                .values
                .is_empty());
            assert!(view
                .email_mailboxes(FLAG_EMAIL, None, 16)
                .unwrap()
                .values
                .is_empty());
            drop(view);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn rejected_deletion_preserves_message_sequence_and_body_charges() {
    fixture_config(
        4,
        100000,
        |store| seed_destroy_email(store, false, false),
        |coordinator, _, _, _, _| {
            let before = coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity();
            set_writer_authorizer(coordinator, |context| {
                if matches!(context.action, AuthAction::Delete { table_name, .. } if table_name == "emails")
                {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            });
            let result = coordinator
                .destroy_emails(
                    &td_crypto::Provider,
                    EmailDestroyRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        emails: &[FLAG_EMAIL],
                        deadline: deadline(),
                    },
                    &mut flag_meter(100000),
                )
                .unwrap();
            assert!(matches!(result.outcome, Err(CommitError::Rejected(_))));
            assert!(!result.admission_stopped);
            lock(&lock(&coordinator.store.writer).unwrap().native.connection)
                .unwrap()
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(view.identity(), before);
            assert!(view
                .get(Key::Email(FLAG_EMAIL), &mut [0; 128])
                .unwrap()
                .is_some());
            drop(view);
            assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
            assert_eq!(coordinator.used(Kind::BlobCount).unwrap(), 1);
            assert_recount(coordinator);
            None
        },
    );
}

#[test]
fn deletion_plan_exhaustion_preserves_all_rows_and_history() {
    fixture_config(
        4,
        100000,
        |store| {
            seed_destroy_email(store, false, false);
            for batch in 0..16 {
                let keys: Vec<_> = (0..256)
                    .map(|n| {
                        let mut bytes = [0; 128];
                        let length =
                            Key::Keyword(FLAG_EMAIL, &format!("custom{:04}", batch * 256 + n))
                                .encode(&mut bytes)
                                .unwrap();
                        bytes[..length].to_vec()
                    })
                    .collect();
                let operations: Vec<_> = keys
                    .iter()
                    .map(|key| Operation::put(Table::Keywords, key, &[]).unwrap())
                    .collect();
                let before = store.view(ACCOUNT, deadline()).unwrap().identity();
                store
                    .commit(
                        &td_crypto::Provider,
                        CommitRequest {
                            account: ACCOUNT,
                            epoch: before.epoch,
                            expected: before.committed_sequence,
                            utc_ms: 1,
                            deadline: deadline(),
                        },
                        &operations,
                        &mut [],
                    )
                    .unwrap();
            }
        },
        |coordinator, _, _, _, _| {
            let before = coordinator
                .store
                .view(ACCOUNT, deadline())
                .unwrap()
                .identity();
            assert!(matches!(
                coordinator.destroy_emails(
                    &td_crypto::Provider,
                    EmailDestroyRequest {
                        account: ACCOUNT,
                        device: DEVICE,
                        expected: None,
                        emails: &[FLAG_EMAIL],
                        deadline: deadline(),
                    },
                    &mut flag_meter(100000)
                ),
                Err(UploadError::Store(ports::Error::Capacity))
            ));
            let mut view = coordinator.store.view(ACCOUNT, deadline()).unwrap();
            assert_eq!(view.identity(), before);
            assert!(view
                .get(Key::Email(FLAG_EMAIL), &mut [0; 128])
                .unwrap()
                .is_some());
            let count = view
                .read_snapshot(|native| {
                    native.run(|db| {
                        db.query_row("SELECT count(*) FROM keywords", [], |row| {
                            row.get::<_, i64>(0)
                        })
                        .map_err(sql)
                    })
                })
                .unwrap();
            assert_eq!(count, 4097);
            drop(view);
            assert_recount(coordinator);
            None
        },
    );
}
