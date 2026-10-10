#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::{DiskLimits, ViewMode, WorkLimits},
    limits::Limits,
    store_fs::tests::Fixture,
};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    MutexGuard, TryLockError,
};
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const DEVICE: DeviceId = DeviceId::from_bytes([2; 16]);
const SEED: BlobId = BlobId::from_bytes([3; 16]);
struct Timer(AtomicU64, Mutex<std::collections::VecDeque<u64>>);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        if let Some(next) = self.1.lock().unwrap().pop_front() {
            self.0.store(next, Ordering::Relaxed);
        }
        Ok(Time {
            utc_ms: 1234,
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
fn seed(store: &IndexStore<'_>) {
    let mut hash = td_crypto::Provider.sha256().unwrap();
    hash.update(b"seed").unwrap();
    let blob = row_bytes(Row::Blob(crate::format::row::BlobRow {
        length: 4,
        digest: hash.finish().unwrap(),
        created_at: 0,
    }));
    let lease = row_bytes(Row::Lease(LeaseRow {
        account: ACCOUNT,
        device: DEVICE,
        expires_at: 100000,
        uses: LeaseUse::Both,
    }));
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::from_u64(0),
                utc_ms: 0,
                deadline: deadline(),
            },
            &[
                Operation::put(Table::Blobs, SEED.as_bytes(), &blob).unwrap(),
                Operation::put(Table::Leases, SEED.as_bytes(), &lease).unwrap(),
            ],
            &mut [BlobSource {
                id: SEED,
                source: &mut b"seed".as_slice(),
            }],
        )
        .unwrap();
}
fn fixture(
    run: impl for<'r, 'a, 's> FnOnce(
        &mut UploadCoordinator<'r, 'a, Policy>,
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
        &mut UploadCoordinator<'r, 'a, Policy>,
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
    seed(&store);
    let spool = IngressSpool::open(&mut spool_root, &resources, clock.clone(), deadline()).unwrap();
    let mut states = [const { SlotState::EMPTY }; 4];
    let mut cells = [const { LeaseCell::EMPTY }; 4];
    let policy = Arc::new(Mutex::new(PolicyState {
        allowed: true,
        generation: 1,
        device: DEVICE,
    }));
    let coordinator = UploadCoordinator::new(
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
    assert_eq!(coordinator.used(Kind::BodyBytes).unwrap(), 4);
    assert_eq!(coordinator.used(Kind::UploadBytes).unwrap(), 4);
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
