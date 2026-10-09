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
        Sequence, Table,
    },
    ids::{AccountId, BlobId, MailboxId, StoreEpoch},
    ports::{
        BlobReader, Clock, Crypto, Deadline, Digest, Error, ReadView, Tick, Time, ViewIdentity,
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
const ACCOUNT: AccountId = AccountId::from_bytes([0x35; 16]);
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
        output.get_mut(..count).unwrap().fill(0x5a);
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
struct CountingEntropy {
    source: td_crypto::SystemEntropy,
    calls: usize,
}
impl td_mta::ports::Entropy for CountingEntropy {
    fn fill(&mut self, output: &mut [u8]) -> Result<(), td_mta::ports::CryptoError> {
        assert_eq!(output.len(), 16);
        self.calls += 1;
        td_mta::ports::Entropy::fill(&mut self.source, output)
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
}
impl Mode {
    pub fn from_argument(argument: &str) -> Option<Self> {
        match argument {
            "--sqlite-body" => Some(Self::Body),
            "--sqlite-account" => Some(Self::Account),
            "--sqlite-backup" => Some(Self::Backup),
            "--sqlite-epoch" => Some(Self::Epoch),
            _ => None,
        }
    }
    pub fn phases(self) -> &'static [&'static str] {
        match self {
            Self::Body => PHASES,
            Self::Account => ACCOUNT_PHASES,
            Self::Backup => BACKUP_PHASES,
            Self::Epoch => EPOCH_PHASES,
        }
    }
    pub fn scenario(self) -> &'static str {
        match self {
            Self::Body => "sqlite-body",
            Self::Account => "sqlite-account",
            Self::Backup => "sqlite-backup",
            Self::Epoch => "sqlite-epoch",
        }
    }
    fn has_backup(self) -> bool {
        matches!(self, Self::Backup | Self::Epoch)
    }
    pub fn has_account(self) -> bool {
        self != Self::Body
    }
}
fn check_account(
    store: &IndexStore<'_>,
    epoch: StoreEpoch,
    sequence: Sequence,
    deadline: Deadline,
    scratch: &mut [u8],
) {
    use crate::store_fs::{AccountCheckLimits, BodyCheckLimits};
    let mut view = store.maintenance_view(ACCOUNT, deadline).unwrap();
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
    assert_eq!(report.identity().account, ACCOUNT);
    assert_eq!(store.epoch(), epoch);
    assert_eq!(report.identity().epoch, epoch);
    assert_eq!(report.identity().history_floor, Sequence::default());
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
    let mut entropy = (mode == Mode::Epoch).then(|| CountingEntropy {
        source: td_crypto::SystemEntropy::try_new().unwrap(),
        calls: 0,
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
    let operations: &[Operation<'_>] = if account_check {
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
        2,
        deadline,
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline).unwrap();
    observe();
    let mut source = Generated {
        remaining: BODY_BYTES,
        sampled: false,
        observe: &mut *observe,
    };
    let sequence = store
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
    assert_eq!(sequence, Sequence::from_u64(1));
    observe();
    {
        let mut view = store.view(ACCOUNT, deadline).unwrap();
        let mut input = view
            .open_blob_input(&td_crypto::Provider, BLOB, BODY_BYTES)
            .unwrap();
        while input.position() != input.len() {
            let n = input.read(&mut scratch).unwrap();
            assert!(n > 0);
            assert!(scratch.get(..n).unwrap().iter().all(|&b| b == 0x5a));
        }
        let mut pin = input.finish().unwrap();
        assert_eq!(pin.len(), BODY_BYTES);
        assert_eq!(pin.read_at(65535, &mut scratch).unwrap(), CHUNK_BYTES);
        assert!(scratch.iter().all(|&b| b == 0x5a));
        assert_eq!(pin.read_at(BODY_BYTES - 1, &mut scratch).unwrap(), 1);
        assert_eq!(scratch.first(), Some(&0x5a));
        observe();
    }
    if account_check {
        check_account(
            &store,
            StoreEpoch::from_bytes([0x57; 16]),
            sequence,
            deadline,
            &mut scratch,
        );
        observe();
    }
    if mode.has_backup() {
        let prior =
            (mode == Mode::Epoch).then(|| store.view(ACCOUNT, deadline).unwrap().identity());
        let destination = destination.unwrap();
        let receipt = store
            .backup(
                destination,
                deadline,
                scratch.as_mut_slice().try_into().unwrap(),
            )
            .unwrap();
        assert_eq!(receipt.epoch, StoreEpoch::from_bytes([0x57; 16]));
        assert!(receipt.bytes >= BODY_BYTES && receipt.bytes <= 8 * 1024 * 1024 * 1024);
        observe();
        let original = IndexStore::open(root, clock.clone(), 2, deadline).unwrap();
        original.validate_integrity(deadline).unwrap();
        check_account(
            &original,
            StoreEpoch::from_bytes([0x57; 16]),
            sequence,
            deadline,
            &mut scratch,
        );
        observe();
        let restored = IndexStore::open(destination, clock.clone(), 2, deadline).unwrap();
        restored.validate_integrity(deadline).unwrap();
        check_account(
            &restored,
            StoreEpoch::from_bytes([0x57; 16]),
            sequence,
            deadline,
            &mut scratch,
        );
        observe();
        if mode == Mode::Epoch {
            let prior = prior.unwrap();
            let old = state(prior);
            assert!(old.is_retained_for(old, prior.history_floor));
            assert_eq!(restored.view(ACCOUNT, deadline).unwrap().identity(), prior);
            let entropy = entropy.unwrap();
            let restored = restored.renew_epoch(entropy, deadline).unwrap();
            assert_eq!(entropy.calls, 1);
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
            restored.validate_integrity(deadline).unwrap();
            check_account(&restored, current.epoch, sequence, deadline, &mut scratch);
            observe();
            restored.checkpoint(deadline).unwrap();
            drop(restored);
            let restored = IndexStore::open(destination, clock, 2, deadline).unwrap();
            assert_eq!(
                restored.view(ACCOUNT, deadline).unwrap().identity(),
                current
            );
            assert!(!old.is_retained_for(state(current), current.history_floor));
            restored.validate_integrity(deadline).unwrap();
            check_account(&restored, current.epoch, sequence, deadline, &mut scratch);
            observe();
            assert_eq!(original.view(ACCOUNT, deadline).unwrap().identity(), prior);
            assert!(old.is_retained_for(state(prior), prior.history_floor));
            original.validate_integrity(deadline).unwrap();
            check_account(&original, prior.epoch, sequence, deadline, &mut scratch);
            observe();
        }
        return;
    }
    store.checkpoint(deadline).unwrap();
    drop(store);
    let reopened = IndexStore::open(root, clock, 2, deadline).unwrap();
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
        check_account(
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
        Sequence::from_u64(2)
    );
    observe();
}
