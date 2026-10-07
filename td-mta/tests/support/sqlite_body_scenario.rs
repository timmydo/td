//! Maximum-size body persistence shared by independent allocation/RSS observers.
use crate::store_fs::{with_probe_root, BlobSource, CommitError, CommitRequest, IndexStore};
use std::{
    io::{self, Read},
    sync::Arc,
};
use td_mta::{
    format::{
        key::Key,
        operation::Operation,
        row::{BlobKind, BlobRow, Row},
        Sequence, Table,
    },
    ids::{AccountId, BlobId, StoreEpoch},
    ports::{BlobReader, Clock, Crypto, Deadline, Digest, Error, ReadView, Tick, Time},
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

pub fn run(mut observe: impl FnMut()) {
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
    observe();
    with_probe_root(|root| {
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
            observe: &mut observe,
        };
        let sequence = store
            .commit(
                &td_crypto::Provider,
                request,
                &[op],
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
        observe();
        // An incomplete source must roll back its body row and ID registration.
        let refused = BlobId::from_bytes([0x68; 16]);
        let op =
            Operation::put(Table::Blobs, refused.as_bytes(), bytes.get(..len).unwrap()).unwrap();
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
    });
    observe();
}
