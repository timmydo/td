//! Body, account, backup and epoch paths for independent observers.
use crate::store_fs::{
    with_probe_root, BlobSource, CommitError, CommitRequest, IndexStore, LockedRoot,
};
use std::{
    io::{self, Read},
    sync::Arc,
};
use td_mta::{
    format::{
        key::Key,
        operation::Operation,
        row::{BlobKind, BlobRow, MailboxRow, Row},
        ObjectType, Sequence, Table,
    },
    ids::{AccountId, BlobId, MailboxId, StoreEpoch},
    ports::{
        BlobReader, Change, ChangeAction, ChangeCursor, ChangeRecord, ChangeStep, Clock, Crypto,
        Deadline, Digest, Error, ReadView, Tick, Time, ViewIdentity,
    },
    sync::{DataState, DataType},
};

pub const PHASES: &[&str] = &[
    "baseline",
    "opened",
    "writing",
    "committed",
    "verified",
    "reopened",
    "rolled_back",
    "dropped",
];
pub const ACCOUNT_PHASES: &[&str] = &[
    "baseline",
    "opened",
    "writing",
    "committed",
    "verified",
    "account_verified",
    "reopened",
    "rolled_back",
    "dropped",
];
pub const BODY_BYTES: u64 = 32 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const READERS: usize = 8;
const ACCOUNT: AccountId = AccountId::from_bytes([0x35; 16]);
const OTHER: AccountId = AccountId::from_bytes([0x36; 16]);
const BLOB: BlobId = BlobId::from_bytes([0x46; 16]);
struct Fixed;
impl Clock for Fixed {
    fn sample(&self) -> Result<Time, Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(1),
        })
    }
}
struct Generated<'a> {
    byte: u8,
    remaining: u64,
    sampled: bool,
    observe: &'a mut dyn FnMut(),
}
impl Read for Generated<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if !self.sampled && self.remaining <= BODY_BYTES / 2 {
            (self.observe)();
            self.sampled = true;
        }
        let count = output.len().min(CHUNK_BYTES).min(self.remaining as usize);
        output.get_mut(..count).unwrap().fill(self.byte);
        self.remaining -= count as u64;
        Ok(count)
    }
}

pub const BACKUP_PHASES: &[&str] = &[
    "baseline",
    "opened",
    "writing",
    "committed",
    "verified",
    "account_verified",
    "backed_up",
    "source_verified",
    "restored_verified",
    "dropped",
];
pub const EPOCH_PHASES: &[&str] = &[
    "baseline",
    "opened",
    "writing",
    "committed",
    "verified",
    "account_verified",
    "backed_up",
    "source_verified",
    "restored_verified",
    "epoch_renewed",
    "epoch_reopened",
    "source_preserved",
    "dropped",
];
pub const OVERLAPPING_PHASES: &[&str] = &[
    "baseline",
    "opened",
    "writing",
    "committed",
    "verified",
    "account_verified",
    "backed_up",
    "source_verified",
    "restored_verified",
    "epoch_renewed",
    "epoch_reopened",
    "source_preserved",
    "pools_verified",
    "dropped",
];
pub const PRUNED_OVERLAPPING_PHASES: &[&str] = &[
    "baseline",
    "opened",
    "writing",
    "committed",
    "verified",
    "account_verified",
    "backed_up",
    "source_verified",
    "restored_verified",
    "epoch_renewed",
    "epoch_reopened",
    "source_preserved",
    "pools_pruned",
    "pools_verified",
    "dropped",
];
struct CountingEntropy {
    source: td_crypto::SystemEntropy,
    calls: usize,
    actual: Option<[u8; 16]>,
}
impl td_mta::ports::Entropy for CountingEntropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), td_mta::ports::CryptoError> {
        assert_eq!(output.len(), 16);
        self.calls += 1;
        td_mta::ports::Entropy::fill(&mut self.source, output)?;
        let mut bytes = [0; 16];
        bytes.copy_from_slice(output);
        self.actual = Some(bytes);
        Ok(())
    }
}
fn state(identity: ViewIdentity) -> DataState {
    DataState {
        account: identity.account,
        epoch: identity.epoch,
        kind: DataType::Email,
        sequence: identity.committed_sequence,
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Mode {
    Body,
    Account,
    Backup,
    Epoch,
    MultiAccount,
    OverlappingAccounts,
    PrunedOverlappingAccounts,
}
impl Mode {
    pub fn from_argument(argument: &str) -> Option<Self> {
        match argument {
            "--sqlite-body" => Some(Self::Body),
            "--sqlite-account" => Some(Self::Account),
            "--sqlite-backup" => Some(Self::Backup),
            "--sqlite-epoch" => Some(Self::Epoch),
            "--sqlite-multi-account" => Some(Self::MultiAccount),
            "--sqlite-overlapping-accounts" => Some(Self::OverlappingAccounts),
            "--sqlite-pruned-overlapping-accounts" => Some(Self::PrunedOverlappingAccounts),
            _ => None,
        }
    }
    pub fn phases(self) -> &'static [&'static str] {
        match self {
            Self::Body => PHASES,
            Self::Account => ACCOUNT_PHASES,
            Self::Backup => BACKUP_PHASES,
            Self::Epoch | Self::MultiAccount => EPOCH_PHASES,
            Self::OverlappingAccounts => OVERLAPPING_PHASES,
            Self::PrunedOverlappingAccounts => PRUNED_OVERLAPPING_PHASES,
        }
    }
    pub fn scenario(self) -> &'static str {
        match self {
            Self::Body => "sqlite-body",
            Self::Account => "sqlite-account",
            Self::Backup => "sqlite-backup",
            Self::Epoch => "sqlite-epoch",
            Self::MultiAccount => "sqlite-multi-account",
            Self::OverlappingAccounts => "sqlite-overlapping-accounts",
            Self::PrunedOverlappingAccounts => "sqlite-pruned-overlapping-accounts",
        }
    }
    fn has_backup(self) -> bool {
        matches!(
            self,
            Self::Backup
                | Self::Epoch
                | Self::MultiAccount
                | Self::OverlappingAccounts
                | Self::PrunedOverlappingAccounts
        )
    }
    fn renews_epoch(self) -> bool {
        matches!(
            self,
            Self::Epoch
                | Self::MultiAccount
                | Self::OverlappingAccounts
                | Self::PrunedOverlappingAccounts
        )
    }
    fn has_multiple_accounts(self) -> bool {
        matches!(
            self,
            Self::MultiAccount | Self::OverlappingAccounts | Self::PrunedOverlappingAccounts
        )
    }
    fn has_overlapping_pools(self) -> bool {
        matches!(
            self,
            Self::OverlappingAccounts | Self::PrunedOverlappingAccounts
        )
    }
    fn prunes_history(self) -> bool {
        self == Self::PrunedOverlappingAccounts
    }
    pub fn has_account(self) -> bool {
        self != Self::Body
    }
}
fn expected_floor(mode: Mode, account: AccountId) -> Sequence {
    Sequence::from_u64(if mode.prunes_history() && account == ACCOUNT {
        2
    } else {
        0
    })
}
fn cursor(sequence: u64, operation: u32) -> ChangeCursor {
    ChangeCursor {
        sequence: Sequence::from_u64(sequence),
        operation,
    }
}
fn check_history(mode: Mode, view: &mut crate::store_fs::IndexReadView<'_, '_>) {
    if !mode.prunes_history() {
        return;
    }
    let identity = view.identity();
    if identity.history_floor == Sequence::from_u64(2) {
        assert_eq!(
            view.next_change(cursor(0, u32::MAX), ObjectType::Mailbox),
            Err(Error::HistoryLost)
        );
        assert_eq!(
            view.next_change(cursor(2, u32::MAX), ObjectType::Mailbox)
                .unwrap(),
            ChangeStep::Complete
        );
    } else {
        assert_eq!(identity.history_floor, Sequence::default());
        assert_eq!(identity.committed_sequence, Sequence::from_u64(1));
        assert_eq!(
            view.next_change(cursor(0, u32::MAX), ObjectType::Mailbox)
                .unwrap(),
            ChangeStep::Record(ChangeRecord {
                cursor: cursor(1, 3),
                change: Change {
                    kind: ObjectType::Mailbox,
                    action: ChangeAction::Created,
                    id: *MailboxId::from_bytes([0x70; 16]).as_bytes()
                },
            })
        );
        assert_eq!(
            view.next_change(cursor(1, 3), ObjectType::Mailbox).unwrap(),
            ChangeStep::Complete
        );
    }
}
fn prune_request(deadline: Deadline) -> crate::store_fs::HistoryPruneRequest {
    crate::store_fs::HistoryPruneRequest {
        account: ACCOUNT,
        expected: Sequence::from_u64(2),
        through: Sequence::from_u64(2),
        max_rows: 1,
        deadline,
    }
}
fn check_account(
    mode: Mode,
    store: &IndexStore<'_>,
    account: AccountId,
    epoch: StoreEpoch,
    sequence: Sequence,
    deadline: Deadline,
    scratch: &mut [u8],
) {
    use crate::store_fs::{AccountCheckLimits, BodyCheckLimits};
    let mut view = store.maintenance_view(account, deadline).unwrap();
    let report = view
        .verify_account(
            &td_crypto::Provider,
            0,
            AccountCheckLimits {
                metadata: td_mta::metadata_sweep::Limits {
                    rows: 3,
                    parent_reads: 3,
                },
                bodies: BodyCheckLimits {
                    blobs: 1,
                    bytes: BODY_BYTES,
                },
            },
            scratch.try_into().unwrap(),
        )
        .unwrap();
    assert_eq!(report.identity(), view.identity());
    assert_eq!(report.identity().account, account);
    assert_eq!(store.epoch(), epoch);
    assert_eq!(report.identity().epoch, epoch);
    assert_eq!(
        report.identity().history_floor,
        expected_floor(mode, account)
    );
    check_history(mode, &mut view);
    assert_eq!(report.identity().committed_sequence, sequence);
    assert_eq!(report.metadata().references().rows(), 3);
    assert_eq!(
        report.metadata().references().table_rows(Table::Blobs),
        Some(1)
    );
    assert_eq!(report.metadata().mailboxes().mailboxes(), 2);
    assert_eq!(report.metadata().mailboxes().reads(), 3);
    assert_eq!(
        (report.bodies().blobs(), report.bodies().bytes()),
        (1, BODY_BYTES)
    );
    let parent = MailboxId::from_bytes([0x70; 16]);
    let child = MailboxId::from_bytes([0x71; 16]);
    let mut metadata = [0; 128];
    for (id, name, owning_parent, changed) in [
        (
            parent,
            if account == OTHER {
                "other parent"
            } else {
                "updated parent"
            },
            None,
            sequence,
        ),
        (
            child,
            if account == OTHER {
                "other child"
            } else {
                "child"
            },
            Some(parent),
            Sequence::from_u64(1),
        ),
    ] {
        assert_eq!(
            view.get(Key::Mailbox(id), &mut metadata).unwrap(),
            Some((
                Row::Mailbox(MailboxRow {
                    name,
                    parent: owning_parent,
                    role: None,
                    sort_order: 0,
                    subscribed: true,
                }),
                changed
            ))
        );
    }
    let blob = view.get(Key::Blob(BLOB), &mut metadata).unwrap();
    let mut digest = td_crypto::Provider.sha256().unwrap();
    let mut input = view
        .open_blob_input(&td_crypto::Provider, BLOB, BODY_BYTES)
        .unwrap();
    assert_eq!(input.len(), BODY_BYTES);
    while input.position() != BODY_BYTES {
        assert_eq!(input.read(scratch).unwrap(), CHUNK_BYTES);
        assert!(scratch.iter().all(|&byte| byte == body_byte(account)));
        digest.update(scratch).unwrap();
    }
    drop(input.finish().unwrap());
    assert_eq!(
        blob,
        Some((
            Row::Blob(BlobRow {
                kind: BlobKind::Message,
                length: BODY_BYTES,
                digest: digest.finish().unwrap(),
                created_at: 0,
            }),
            Sequence::from_u64(1)
        ))
    );
}
fn check_accounts(
    mode: Mode,
    store: &IndexStore<'_>,
    epoch: StoreEpoch,
    sequence: Sequence,
    deadline: Deadline,
    scratch: &mut [u8],
) {
    check_account(mode, store, ACCOUNT, epoch, sequence, deadline, scratch);
    if mode.has_multiple_accounts() {
        check_account(
            mode,
            store,
            OTHER,
            epoch,
            Sequence::from_u64(1),
            deadline,
            scratch,
        );
    }
}
fn account_at(mode: Mode, index: usize) -> AccountId {
    if mode.has_multiple_accounts() && index % 2 == 1 {
        OTHER
    } else {
        ACCOUNT
    }
}
fn body_byte(account: AccountId) -> u8 {
    if account == OTHER {
        0xa5
    } else {
        0x5a
    }
}
pub fn run(mode: Mode, mut observe: impl FnMut()) {
    // Initialize native process globals before the retention baseline.
    with_probe_root(|root| {
        let deadline = Deadline::after(Tick(0), 100).unwrap();
        drop(
            IndexStore::create(
                root,
                StoreEpoch::from_bytes([0x24; 16]),
                Arc::new(Fixed),
                1,
                deadline,
            )
            .unwrap(),
        );
    });
    // Keep the actual entropy handle on this observing thread, warm before baseline.
    let mut entropy = mode.renews_epoch().then(|| CountingEntropy {
        source: td_crypto::SystemEntropy::try_new().unwrap(),
        calls: 0,
        actual: None,
    });
    observe();
    with_probe_root(|root| {
        if mode.has_backup() {
            with_probe_root(|destination| {
                run_with_roots(
                    root,
                    Some(destination),
                    mode,
                    entropy.as_mut(),
                    &mut observe,
                )
            });
        } else {
            run_with_roots(root, None, mode, None, &mut observe);
        }
    });
    observe();
}
fn run_with_roots(
    root: &mut LockedRoot,
    destination: Option<&mut LockedRoot>,
    mode: Mode,
    entropy: Option<&mut CountingEntropy>,
    observe: &mut dyn FnMut(),
) {
    let account_check = mode.has_account();
    let deadline = Deadline::after(Tick(0), 100).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(Fixed);
    let mut digest = td_crypto::Provider.sha256().unwrap();
    // Reuse fixed caller storage; neither source nor oracle holds a whole body.
    let mut scratch = vec![0x5a; CHUNK_BYTES];
    for _ in 0..BODY_BYTES / CHUNK_BYTES as u64 {
        digest.update(&scratch).unwrap();
    }
    let row = Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: BODY_BYTES,
        digest: digest.finish().unwrap(),
        created_at: 0,
    });
    let mut bytes = [0; 64];
    let len = row.encode(&mut bytes).unwrap();
    let op = Operation::put(Table::Blobs, BLOB.as_bytes(), bytes.get(..len).unwrap()).unwrap();
    let parent = MailboxId::from_bytes([0x70; 16]);
    let child = MailboxId::from_bytes([0x71; 16]);
    let mut parent_bytes = [0; 128];
    let mut child_bytes = [0; 128];
    let mailbox = |name, parent| {
        Row::Mailbox(MailboxRow {
            name,
            parent,
            role: None,
            sort_order: 0,
            subscribed: true,
        })
    };
    let parent_len = mailbox("parent", None).encode(&mut parent_bytes).unwrap();
    let child_len = mailbox("child", Some(parent))
        .encode(&mut child_bytes)
        .unwrap();
    let body_operations = [op];
    let account_operations = [
        op,
        Operation::put(
            Table::Mailboxes,
            parent.as_bytes(),
            parent_bytes.get(..parent_len).unwrap(),
        )
        .unwrap(),
        Operation::put(
            Table::Mailboxes,
            child.as_bytes(),
            child_bytes.get(..child_len).unwrap(),
        )
        .unwrap(),
    ];
    let pruned_operations = mode.prunes_history().then(|| {
        let mut operations = account_operations.to_vec();
        operations.push(Operation::change(
            ObjectType::Mailbox,
            ChangeAction::Created,
            parent.as_bytes(),
        ));
        operations
    });
    let operations: &[Operation<'_>] = if let Some(operations) = &pruned_operations {
        operations
    } else if account_check {
        &account_operations
    } else {
        &body_operations
    };
    let request = CommitRequest {
        account: ACCOUNT,
        expected: Sequence::default(),
        utc_ms: 0,
        deadline,
    };
    let store = IndexStore::create(
        root,
        StoreEpoch::from_bytes([0x57; 16]),
        clock.clone(),
        READERS,
        deadline,
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline).unwrap();
    if mode.has_multiple_accounts() {
        store.create_account(OTHER, deadline).unwrap();
    }
    observe();
    let mut source = Generated {
        byte: 0x5a,
        remaining: BODY_BYTES,
        sampled: false,
        observe: &mut *observe,
    };
    let initial_sequence = store
        .commit(
            &td_crypto::Provider,
            request,
            operations,
            &mut [BlobSource {
                id: BLOB,
                source: &mut source,
            }],
        )
        .unwrap();
    assert_eq!(source.remaining, 0);
    assert!(source.sampled);
    assert_eq!(initial_sequence, Sequence::from_u64(1));
    let other_row = if mode.has_multiple_accounts() {
        scratch.fill(0xa5);
        let mut digest = td_crypto::Provider.sha256().unwrap();
        for _ in 0..BODY_BYTES / CHUNK_BYTES as u64 {
            digest.update(&scratch).unwrap();
        }
        let other_row = Row::Blob(BlobRow {
            kind: BlobKind::Message,
            length: BODY_BYTES,
            digest: digest.finish().unwrap(),
            created_at: 0,
        });
        let mut other_bytes = [0; 64];
        let other_len = other_row.encode(&mut other_bytes).unwrap();
        let mut parent_bytes = [0; 128];
        let parent_len = mailbox("other parent", None)
            .encode(&mut parent_bytes)
            .unwrap();
        let mut child_bytes = [0; 128];
        let child_len = mailbox("other child", Some(parent))
            .encode(&mut child_bytes)
            .unwrap();
        let mut silent = || {};
        let mut source = Generated {
            byte: 0xa5,
            remaining: BODY_BYTES,
            sampled: false,
            observe: &mut silent,
        };
        let other_operations = [
            Operation::put(
                Table::Blobs,
                BLOB.as_bytes(),
                other_bytes.get(..other_len).unwrap(),
            )
            .unwrap(),
            Operation::put(
                Table::Mailboxes,
                parent.as_bytes(),
                parent_bytes.get(..parent_len).unwrap(),
            )
            .unwrap(),
            Operation::put(
                Table::Mailboxes,
                child.as_bytes(),
                child_bytes.get(..child_len).unwrap(),
            )
            .unwrap(),
        ];
        let pruned_other_operations = mode.prunes_history().then(|| {
            let mut operations = other_operations.to_vec();
            operations.push(Operation::change(
                ObjectType::Mailbox,
                ChangeAction::Created,
                parent.as_bytes(),
            ));
            operations
        });
        assert_eq!(
            store
                .commit(
                    &td_crypto::Provider,
                    CommitRequest {
                        account: OTHER,
                        ..request
                    },
                    pruned_other_operations
                        .as_deref()
                        .unwrap_or(&other_operations),
                    &mut [BlobSource {
                        id: BLOB,
                        source: &mut source
                    }],
                )
                .unwrap(),
            Sequence::from_u64(1)
        );
        assert_eq!(source.remaining, 0);
        assert!(source.sampled);
        Some(other_row)
    } else {
        None
    };
    let sequence = {
        let mut views: [_; READERS] =
            std::array::from_fn(|index| store.view(account_at(mode, index), deadline).unwrap());
        let identity = views.first().unwrap().identity();
        assert_eq!(identity.account, ACCOUNT);
        assert_eq!(identity.epoch, store.epoch());
        assert_eq!(identity.committed_sequence, initial_sequence);
        assert_eq!(identity.history_floor, Sequence::default());
        for (index, view) in views.iter().enumerate() {
            assert_eq!(
                view.identity(),
                ViewIdentity {
                    account: account_at(mode, index),
                    ..identity
                }
            );
        }
        assert!(matches!(store.view(ACCOUNT, deadline), Err(Error::Busy)));
        let mut inputs = views.each_mut().map(|view| {
            view.open_blob_input(&td_crypto::Provider, BLOB, BODY_BYTES)
                .unwrap()
        });
        for (index, input) in inputs.iter_mut().enumerate() {
            assert_eq!(input.read(&mut scratch).unwrap(), CHUNK_BYTES);
            assert!(scratch
                .iter()
                .all(|&b| b == body_byte(account_at(mode, index))));
        }
        let updated_parent = mailbox("updated parent", None);
        let mut updated_bytes = [0; 128];
        let updated_len = updated_parent.encode(&mut updated_bytes).unwrap();
        let updated_operations = [Operation::put(
            Table::Mailboxes,
            parent.as_bytes(),
            updated_bytes.get(..updated_len).unwrap(),
        )
        .unwrap()];
        let pruned_updated_operations = mode.prunes_history().then(|| {
            let mut operations = updated_operations.to_vec();
            operations.push(Operation::change(
                ObjectType::Mailbox,
                ChangeAction::Updated,
                parent.as_bytes(),
            ));
            operations
        });
        let sequence = store
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    expected: initial_sequence,
                    ..request
                },
                pruned_updated_operations
                    .as_deref()
                    .unwrap_or(&updated_operations),
                &mut [],
            )
            .unwrap();
        assert_eq!(sequence, Sequence::from_u64(2));
        let mut fence = Some(store.usage_fence(deadline).unwrap());
        let usage = fence.as_ref().unwrap().usage();
        assert_eq!(
            (
                usage.epoch,
                usage.accounts,
                usage.body_bytes,
                usage.blob_count,
                usage.upload_bytes,
                usage.queue_bytes,
                usage.queue_submissions
            ),
            (
                store.epoch(),
                if mode.has_multiple_accounts() { 2 } else { 1 },
                BODY_BYTES * if mode.has_multiple_accounts() { 2 } else { 1 },
                if mode.has_multiple_accounts() { 2 } else { 1 },
                0,
                0,
                0
            ),
        );
        let files = fence.as_ref().unwrap().file_usage();
        assert!(
            files.database_bytes > 0
                && files.database_bytes <= td_mta::limits::SQLITE_DATABASE_BYTES
        );
        assert!(files.wal_bytes > 0 && files.wal_bytes <= td_mta::limits::SQLITE_WAL_BYTES);
        assert_eq!(
            store.commit(
                &td_crypto::Provider,
                CommitRequest {
                    expected: sequence,
                    ..request
                },
                &[Operation::put(
                    Table::Mailboxes,
                    parent.as_bytes(),
                    updated_bytes.get(..updated_len).unwrap()
                )
                .unwrap()],
                &mut [],
            ),
            Err(CommitError::Rejected(Error::Busy))
        );
        assert_eq!(store.checkpoint(deadline), Err(Error::Busy));
        assert!(matches!(store.usage_fence(deadline), Err(Error::Busy)));
        assert!(matches!(store.view(ACCOUNT, deadline), Err(Error::Busy)));
        observe();
        if mode.prunes_history() {
            drop(fence.take());
            assert_eq!(
                store.prune_history(prune_request(deadline)).unwrap(),
                crate::store_fs::HistoryPruned {
                    identity: ViewIdentity {
                        account: ACCOUNT,
                        epoch: store.epoch(),
                        committed_sequence: sequence,
                        history_floor: Sequence::from_u64(2)
                    },
                    removed: 1,
                    more: true,
                }
            );
            assert!(matches!(store.view(ACCOUNT, deadline), Err(Error::Busy)));
            assert_eq!(store.checkpoint(deadline), Err(Error::Busy));
        }
        while inputs.first().unwrap().position() != BODY_BYTES {
            for (index, input) in inputs.iter_mut().enumerate() {
                let n = input.read(&mut scratch).unwrap();
                assert_eq!(n, CHUNK_BYTES);
                assert!(scratch
                    .get(..n)
                    .unwrap()
                    .iter()
                    .all(|&b| b == body_byte(account_at(mode, index))));
            }
        }
        assert!(inputs
            .iter()
            .all(|input| input.position() == BODY_BYTES && input.len() == BODY_BYTES));
        let mut pins = inputs.map(|input| input.finish().unwrap());
        for (index, pin) in pins.iter_mut().enumerate() {
            let byte = body_byte(account_at(mode, index));
            assert_eq!(pin.len(), BODY_BYTES);
            assert_eq!(pin.read_at(65535, &mut scratch).unwrap(), CHUNK_BYTES);
            assert!(scratch.iter().all(|&b| b == byte));
            assert_eq!(pin.read_at(BODY_BYTES - 1, &mut scratch).unwrap(), 1);
            assert_eq!(scratch.first(), Some(&byte));
        }
        assert!(matches!(store.view(ACCOUNT, deadline), Err(Error::Busy)));
        observe();
        drop(fence);
        drop(pins);
        let mut metadata = [0; 128];
        for (index, view) in views.iter_mut().enumerate() {
            check_history(mode, view);
            let account = account_at(mode, index);
            assert_eq!(
                view.identity(),
                ViewIdentity {
                    account,
                    ..identity
                }
            );
            let expected = account_check.then(|| {
                (
                    mailbox(
                        if account == OTHER {
                            "other parent"
                        } else {
                            "parent"
                        },
                        None,
                    ),
                    initial_sequence,
                )
            });
            if mode.has_multiple_accounts() {
                assert_eq!(
                    view.get(Key::Blob(BLOB), &mut metadata).unwrap(),
                    Some((
                        if account == OTHER {
                            other_row.unwrap()
                        } else {
                            row
                        },
                        initial_sequence
                    ))
                );
            }
            assert_eq!(
                view.get(Key::Mailbox(parent), &mut metadata).unwrap(),
                expected
            );
        }
        sequence
    };
    let mut returned: [_; READERS] =
        std::array::from_fn(|index| store.view(account_at(mode, index), deadline).unwrap());
    for (index, view) in returned.iter().enumerate() {
        let account = account_at(mode, index);
        assert_eq!(
            view.identity(),
            ViewIdentity {
                account,
                epoch: store.epoch(),
                committed_sequence: if account == OTHER {
                    initial_sequence
                } else {
                    sequence
                },
                history_floor: expected_floor(mode, account)
            }
        );
    }
    let mut metadata = [0; 128];
    for (index, view) in returned.iter_mut().enumerate() {
        check_history(mode, view);
        let account = account_at(mode, index);
        assert_eq!(
            view.get(Key::Mailbox(parent), &mut metadata).unwrap(),
            Some((
                mailbox(
                    if account == OTHER {
                        "other parent"
                    } else {
                        "updated parent"
                    },
                    None
                ),
                if account == OTHER {
                    initial_sequence
                } else {
                    sequence
                }
            ))
        );
        if mode.has_multiple_accounts() {
            assert_eq!(
                view.get(Key::Blob(BLOB), &mut metadata).unwrap(),
                Some((
                    if account == OTHER {
                        other_row.unwrap()
                    } else {
                        row
                    },
                    initial_sequence
                ))
            );
        }
    }
    drop(returned);
    if account_check {
        check_accounts(
            mode,
            &store,
            StoreEpoch::from_bytes([0x57; 16]),
            sequence,
            deadline,
            &mut scratch,
        );
        observe();
    }
    if mode.has_backup() {
        let prior = mode
            .renews_epoch()
            .then(|| store.view(ACCOUNT, deadline).unwrap().identity());
        let other_prior = mode
            .has_multiple_accounts()
            .then(|| store.view(OTHER, deadline).unwrap().identity());
        let destination = destination.unwrap();
        let receipt = store
            .backup(
                destination,
                deadline,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        assert_eq!(receipt.epoch, StoreEpoch::from_bytes([0x57; 16]));
        assert!(
            receipt.bytes >= BODY_BYTES * if mode.has_multiple_accounts() { 2 } else { 1 }
                && receipt.bytes <= td_mta::limits::SQLITE_DATABASE_BYTES
        );
        observe();
        let original = IndexStore::open(root, clock.clone(), READERS, deadline).unwrap();
        original.validate_integrity(deadline).unwrap();
        check_accounts(
            mode,
            &original,
            StoreEpoch::from_bytes([0x57; 16]),
            sequence,
            deadline,
            &mut scratch,
        );
        observe();
        let restored = IndexStore::open(destination, clock.clone(), READERS, deadline).unwrap();
        restored.validate_integrity(deadline).unwrap();
        check_accounts(
            mode,
            &restored,
            StoreEpoch::from_bytes([0x57; 16]),
            sequence,
            deadline,
            &mut scratch,
        );
        observe();
        if mode.renews_epoch() {
            let prior = prior.unwrap();
            let old = state(prior);
            assert!(old.is_retained_for(old, prior.history_floor));
            assert_eq!(restored.view(ACCOUNT, deadline).unwrap().identity(), prior);
            let entropy = entropy.unwrap();
            let restored = restored.renew_epoch(entropy, deadline).unwrap();
            assert_eq!(entropy.calls, 1);
            assert_eq!(
                restored.epoch(),
                StoreEpoch::from_bytes(entropy.actual.unwrap())
            );
            let current = restored.view(ACCOUNT, deadline).unwrap().identity();
            assert_ne!(current.epoch, prior.epoch);
            assert_eq!(
                current,
                ViewIdentity {
                    epoch: current.epoch,
                    ..prior
                }
            );
            assert!(!old.is_retained_for(state(current), current.history_floor));
            assert!(state(current).is_retained_for(state(current), current.history_floor));
            if let Some(prior) = other_prior {
                let expected = ViewIdentity {
                    epoch: current.epoch,
                    ..prior
                };
                assert_eq!(restored.view(OTHER, deadline).unwrap().identity(), expected);
                assert!(!state(prior).is_retained_for(state(expected), expected.history_floor));
                assert!(state(expected).is_retained_for(state(expected), expected.history_floor));
            }
            restored.validate_integrity(deadline).unwrap();
            check_accounts(
                mode,
                &restored,
                current.epoch,
                sequence,
                deadline,
                &mut scratch,
            );
            observe();
            restored.checkpoint(deadline).unwrap();
            drop(restored);
            let restored = IndexStore::open(destination, clock, READERS, deadline).unwrap();
            assert_eq!(
                restored.view(ACCOUNT, deadline).unwrap().identity(),
                current
            );
            assert!(!old.is_retained_for(state(current), current.history_floor));
            if let Some(prior) = other_prior {
                let expected = ViewIdentity {
                    epoch: current.epoch,
                    ..prior
                };
                assert_eq!(restored.view(OTHER, deadline).unwrap().identity(), expected);
                assert!(!state(prior).is_retained_for(state(expected), expected.history_floor));
                assert!(state(expected).is_retained_for(state(expected), expected.history_floor));
            }
            restored.validate_integrity(deadline).unwrap();
            check_accounts(
                mode,
                &restored,
                current.epoch,
                sequence,
                deadline,
                &mut scratch,
            );
            observe();
            assert_eq!(original.view(ACCOUNT, deadline).unwrap().identity(), prior);
            assert!(old.is_retained_for(state(prior), prior.history_floor));
            if let Some(prior) = other_prior {
                assert_eq!(original.view(OTHER, deadline).unwrap().identity(), prior);
                assert!(state(prior).is_retained_for(state(prior), prior.history_floor));
            }
            original.validate_integrity(deadline).unwrap();
            check_accounts(
                mode,
                &original,
                prior.epoch,
                sequence,
                deadline,
                &mut scratch,
            );
            observe();
            if mode.has_overlapping_pools() {
                {
                    let mut source_views: [_; READERS] = std::array::from_fn(|index| {
                        original.view(account_at(mode, index), deadline).unwrap()
                    });
                    let mut copied_views: [_; READERS] = std::array::from_fn(|index| {
                        restored.view(account_at(mode, index), deadline).unwrap()
                    });
                    let check =
                        |view: &mut crate::store_fs::IndexReadView<'_, '_>, account, epoch| {
                            let expected_sequence = if account == OTHER {
                                Sequence::from_u64(1)
                            } else {
                                sequence
                            };
                            assert_eq!(
                                view.identity(),
                                ViewIdentity {
                                    account,
                                    epoch,
                                    committed_sequence: expected_sequence,
                                    history_floor: expected_floor(mode, account),
                                }
                            );
                            check_history(mode, view);
                            let mut metadata = [0; 128];
                            assert_eq!(
                                view.get(Key::Blob(BLOB), &mut metadata).unwrap(),
                                Some((
                                    if account == OTHER {
                                        other_row.unwrap()
                                    } else {
                                        row
                                    },
                                    Sequence::from_u64(1)
                                ))
                            );
                            assert_eq!(
                                view.get(Key::Mailbox(parent), &mut metadata).unwrap(),
                                Some((
                                    mailbox(
                                        if account == OTHER {
                                            "other parent"
                                        } else {
                                            "updated parent"
                                        },
                                        None
                                    ),
                                    expected_sequence
                                ))
                            );
                            assert_eq!(
                                view.get(Key::Mailbox(child), &mut metadata).unwrap(),
                                Some((
                                    mailbox(
                                        if account == OTHER {
                                            "other child"
                                        } else {
                                            "child"
                                        },
                                        Some(parent)
                                    ),
                                    Sequence::from_u64(1)
                                ))
                            );
                        };
                    for (views, epoch) in [
                        (&mut source_views, prior.epoch),
                        (&mut copied_views, current.epoch),
                    ] {
                        for (index, view) in views.iter_mut().enumerate() {
                            check(view, account_at(mode, index), epoch);
                        }
                    }
                    let mut source_inputs = source_views.each_mut().map(|view| {
                        view.open_blob_input(&td_crypto::Provider, BLOB, BODY_BYTES)
                            .unwrap()
                    });
                    let mut copied_inputs = copied_views.each_mut().map(|view| {
                        view.open_blob_input(&td_crypto::Provider, BLOB, BODY_BYTES)
                            .unwrap()
                    });
                    for inputs in [&mut source_inputs, &mut copied_inputs] {
                        for (index, input) in inputs.iter_mut().enumerate() {
                            assert_eq!(input.len(), BODY_BYTES);
                            assert_eq!(input.read(&mut scratch).unwrap(), CHUNK_BYTES);
                            assert_eq!(input.position(), CHUNK_BYTES as u64);
                            assert!(scratch
                                .iter()
                                .all(|&byte| byte == body_byte(account_at(mode, index))));
                        }
                    }
                    for store in [&original, &restored] {
                        assert!(matches!(store.view(ACCOUNT, deadline), Err(Error::Busy)));
                        assert_eq!(store.checkpoint(deadline), Err(Error::Busy));
                    }
                    if mode.prunes_history() {
                        for (store, epoch) in [(&restored, current.epoch), (&original, prior.epoch)]
                        {
                            assert_eq!(store.epoch(), epoch);
                            for removed in [1, 0] {
                                assert_eq!(
                                    store.prune_history(prune_request(deadline)).unwrap(),
                                    crate::store_fs::HistoryPruned {
                                        identity: ViewIdentity {
                                            account: ACCOUNT,
                                            epoch,
                                            committed_sequence: sequence,
                                            history_floor: Sequence::from_u64(2)
                                        },
                                        removed,
                                        more: false,
                                    }
                                );
                            }
                            assert!(matches!(store.view(OTHER, deadline), Err(Error::Busy)));
                            assert_eq!(store.checkpoint(deadline), Err(Error::Busy));
                        }
                        observe();
                    }
                    for _ in 1..BODY_BYTES / CHUNK_BYTES as u64 {
                        for inputs in [&mut source_inputs, &mut copied_inputs] {
                            for (index, input) in inputs.iter_mut().enumerate() {
                                assert_eq!(input.read(&mut scratch).unwrap(), CHUNK_BYTES);
                                assert!(scratch
                                    .iter()
                                    .all(|&byte| byte == body_byte(account_at(mode, index))));
                            }
                        }
                    }
                    for inputs in [&source_inputs, &copied_inputs] {
                        assert!(inputs.iter().all(
                            |input| input.position() == BODY_BYTES && input.len() == BODY_BYTES
                        ));
                    }
                    let mut source_pins = source_inputs.map(|input| input.finish().unwrap());
                    let mut copied_pins = copied_inputs.map(|input| input.finish().unwrap());
                    for pins in [&mut source_pins, &mut copied_pins] {
                        for (index, pin) in pins.iter_mut().enumerate() {
                            let byte = body_byte(account_at(mode, index));
                            assert_eq!(pin.len(), BODY_BYTES);
                            assert_eq!(pin.read_at(65535, &mut scratch).unwrap(), CHUNK_BYTES);
                            assert!(scratch.iter().all(|&actual| actual == byte));
                            assert_eq!(pin.read_at(BODY_BYTES - 1, &mut scratch).unwrap(), 1);
                            assert_eq!(scratch.first(), Some(&byte));
                        }
                    }
                    for store in [&original, &restored] {
                        assert!(matches!(store.view(OTHER, deadline), Err(Error::Busy)));
                        assert_eq!(store.checkpoint(deadline), Err(Error::Busy));
                    }
                    observe();
                    drop(copied_pins);
                    drop(source_pins);
                    for (views, epoch) in [
                        (&mut source_views, prior.epoch),
                        (&mut copied_views, current.epoch),
                    ] {
                        for (index, view) in views.iter_mut().enumerate() {
                            check(view, account_at(mode, index), epoch);
                        }
                    }
                    for store in [&original, &restored] {
                        assert!(matches!(store.view(ACCOUNT, deadline), Err(Error::Busy)));
                        assert_eq!(store.checkpoint(deadline), Err(Error::Busy));
                    }
                }
                for (store, epoch) in [(&original, prior.epoch), (&restored, current.epoch)] {
                    store.checkpoint(deadline).unwrap();
                    store.validate_integrity(deadline).unwrap();
                    check_accounts(mode, store, epoch, sequence, deadline, &mut scratch);
                }
            }
        }
        return;
    }
    store.checkpoint(deadline).unwrap();
    drop(store);
    let reopened = IndexStore::open(root, clock, READERS, deadline).unwrap();
    {
        let mut view = reopened.view(ACCOUNT, deadline).unwrap();
        assert_eq!(view.identity().committed_sequence, sequence);
        let mut input = view
            .open_blob_input(&td_crypto::Provider, BLOB, BODY_BYTES)
            .unwrap();
        while input.position() != input.len() {
            assert!(input.read(&mut scratch).unwrap() > 0);
        }
        let mut pin = input.finish().unwrap();
        assert_eq!(pin.len(), BODY_BYTES);
        assert_eq!(pin.read_at(65535, &mut scratch).unwrap(), CHUNK_BYTES);
        assert!(scratch.iter().all(|&b| b == 0x5a));
    }
    if account_check {
        check_accounts(
            mode,
            &reopened,
            StoreEpoch::from_bytes([0x57; 16]),
            sequence,
            deadline,
            &mut scratch,
        );
    }
    observe();
    // An incomplete source must roll back its body row and ID registration.
    let refused = BlobId::from_bytes([0x68; 16]);
    let op = Operation::put(Table::Blobs, refused.as_bytes(), bytes.get(..len).unwrap()).unwrap();
    let mut short = io::Cursor::new(b"short");
    assert!(matches!(
        reopened.commit(
            &td_crypto::Provider,
            CommitRequest {
                expected: sequence,
                ..request
            },
            &[op],
            &mut [BlobSource {
                id: refused,
                source: &mut short
            }]
        ),
        Err(CommitError::Rejected(_))
    ));
    {
        let mut view = reopened.view(ACCOUNT, deadline).unwrap();
        assert_eq!(view.identity().committed_sequence, sequence);
        assert!(view.get(Key::Blob(refused), &mut bytes).unwrap().is_none());
    }
    let too_large = Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: BODY_BYTES + 1,
        digest: [0; 32],
        created_at: 0,
    });
    let mut oversized_bytes = [0; 64];
    let oversized_len = too_large.encode(&mut oversized_bytes).unwrap();
    let op = Operation::put(
        Table::Blobs,
        refused.as_bytes(),
        oversized_bytes.get(..oversized_len).unwrap(),
    )
    .unwrap();
    let mut unread = io::Cursor::new(b"must not be consumed");
    assert!(matches!(
        reopened.commit(
            &td_crypto::Provider,
            CommitRequest {
                expected: sequence,
                ..request
            },
            &[op],
            &mut [BlobSource {
                id: refused,
                source: &mut unread
            }]
        ),
        Err(CommitError::Rejected(Error::Capacity))
    ));
    assert_eq!(unread.position(), 0);
    let mut digest = td_crypto::Provider.sha256().unwrap();
    digest.update(b"retry").unwrap();
    let row = Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: 5,
        digest: digest.finish().unwrap(),
        created_at: 0,
    });
    let length = row.encode(&mut bytes).unwrap();
    let op = Operation::put(
        Table::Blobs,
        refused.as_bytes(),
        bytes.get(..length).unwrap(),
    )
    .unwrap();
    let mut retry = io::Cursor::new(b"retry");
    assert_eq!(
        reopened
            .commit(
                &td_crypto::Provider,
                CommitRequest {
                    expected: sequence,
                    ..request
                },
                &[op],
                &mut [BlobSource {
                    id: refused,
                    source: &mut retry
                }]
            )
            .unwrap(),
        Sequence::from_u64(3)
    );
    observe();
}
