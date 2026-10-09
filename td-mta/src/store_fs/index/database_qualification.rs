//! Explicit, disk-heavy maximum-database maintenance qualification.
#![allow(clippy::unwrap_used, clippy::panic)]

use super::super::tests::Fixture;
use super::*;
use crate::{
    format::row::{BlobKind, BlobRow},
    ports::{Crypto, Tick, Time},
    store_fs::{AccountCheckLimits, BodyCheckLimits, MAX_FILE_STEP_BYTES},
};
use std::{
    io::{self, Read},
    time::{Duration, Instant},
};

const ACCOUNT: AccountId = AccountId::from_bytes([0xc1; 16]);
const EPOCH: StoreEpoch = StoreEpoch::from_bytes([0xc2; 16]);
const DATABASE_BYTES: u64 = MAX_PAGES * PAGE_BYTES;
const LIMIT: Duration = Duration::from_secs(30 * 60);
const MAX_ATTEMPTS: u64 = 512;
static BODY: [u8; MAX_FILE_STEP_BYTES] = [0x5a; MAX_FILE_STEP_BYTES];
static ALTERED: [u8; MAX_FILE_STEP_BYTES] = [0xa5; MAX_FILE_STEP_BYTES];

struct Fixed;
impl Clock for Fixed {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(1),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn bounded(started: Instant) {
    assert!(
        started.elapsed() < LIMIT,
        "maximum database fixture timeout"
    );
}
fn pages(store: &IndexStore<'_>) -> u64 {
    let writer = lock(&store.writer).unwrap();
    writer.native.begin_work(deadline()).unwrap();
    let count: i64 = writer
        .native
        .run(|db| {
            db.pragma_query_value(None, "page_count", |r| r.get(0))
                .map_err(sql)
        })
        .unwrap();
    u64::try_from(count).unwrap()
}
fn memory(phase: &str) {
    for (path, key) in [
        ("/proc/self/smaps_rollup", "Rss:"),
        ("/proc/self/status", "VmHWM:"),
    ] {
        let mut bytes = [0; 8192];
        let mut file = fs::File::open(path).unwrap();
        let mut count = 0;
        loop {
            let read = file.read(bytes.get_mut(count..).unwrap()).unwrap();
            if read == 0 {
                break;
            }
            count += read;
            assert!(count < bytes.len(), "procfs probe exceeds fixed buffer");
        }
        let text = std::str::from_utf8(bytes.get(..count).unwrap()).unwrap();
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .unwrap();
        let mut fields = line.split_ascii_whitespace();
        let value = fields.next().unwrap().parse::<u64>().unwrap();
        assert!(value > 0);
        assert_eq!(fields.next(), Some("kB"));
        assert_eq!(fields.next(), None);
        eprintln!("maximum-database memory {phase} {key} {value} KiB");
    }
}
fn row(length: u64) -> BlobRow {
    let mut digest = td_crypto::Provider.sha256().unwrap();
    let mut remaining = length;
    while remaining > 0 {
        let count = remaining.min(BODY.len() as u64) as usize;
        digest.update(BODY.get(..count).unwrap()).unwrap();
        remaining -= count as u64;
    }
    BlobRow {
        kind: BlobKind::Message,
        length,
        digest: digest.finish().unwrap(),
        created_at: 0,
    }
}
fn fill(store: &IndexStore<'_>, started: Instant) -> Vec<(BlobId, BlobRow)> {
    let mut bodies = Vec::new();
    let mut length = MAX_BODY_BYTES;
    let mut rejected = 0;
    for attempt in 0..MAX_ATTEMPTS {
        bounded(started);
        let mut bytes = [0xc3; 16];
        bytes
            .get_mut(8..)
            .unwrap()
            .copy_from_slice(&attempt.to_be_bytes());
        let id = BlobId::from_bytes(bytes);
        let body = row(length);
        let mut value = [0; 128];
        let count = Row::Blob(body).encode(&mut value).unwrap();
        let operation =
            Operation::put(Table::Blobs, id.as_bytes(), value.get(..count).unwrap()).unwrap();
        // Bounded checkpoints during admission keep the normal WAL reserve usable.
        store.checkpoint(deadline()).unwrap();
        let mut source = io::repeat(0x5a).take(length);
        let result = store.commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                expected: Sequence::from_u64(bodies.len() as u64),
                utc_ms: 0,
                deadline: deadline(),
            },
            &[operation],
            &mut [BlobSource {
                id,
                source: &mut source,
            }],
        );
        match result {
            Ok(sequence) => {
                assert_eq!(source.limit(), 0);
                bodies.push((id, body));
                assert_eq!(sequence, Sequence::from_u64(bodies.len() as u64));
            }
            Err(CommitError::Rejected(ports::Error::Capacity)) => {
                rejected += 1;
                let writer = lock(&store.writer).unwrap();
                assert!(!writer.stopped);
                assert!(lock(&writer.native.connection).unwrap().is_autocommit());
                drop(writer);
                if length <= 1024 {
                    break;
                }
                length /= 2;
            }
            other => panic!("maximum database admission: {other:?}"),
        }
        if attempt % 16 == 0 {
            eprintln!(
                "maximum-database fill attempt={attempt} blobs={} pages={} elapsed={:?}",
                bodies.len(),
                pages(store),
                started.elapsed()
            );
        }
    }
    assert!(rejected > 0 && bodies.len() >= 200 && bodies.len() < MAX_ATTEMPTS as usize);
    assert!(
        MAX_PAGES - pages(store) <= 128,
        "public fill stopped too early"
    );
    eprintln!(
        "maximum-database public-fill blobs={} rejected={rejected} pages={}",
        bodies.len(),
        pages(store)
    );
    bodies
}
fn fill_free_pages(store: &IndexStore<'_>, started: Instant) {
    if pages(store) == MAX_PAGES {
        return;
    }
    // Test-only padding reaches the physical cap; DROP retains free pages.
    let writer = lock(&store.writer).unwrap();
    writer.native.begin_work(deadline()).unwrap();
    writer
        .native
        .run(|db| {
            assert_eq!(
                db.pragma_query_value(None, "auto_vacuum", |r| r.get::<_, i64>(0))
                    .map_err(sql)?,
                0
            );
            db.execute_batch("CREATE TABLE qualification_padding(value BLOB) STRICT")
                .map_err(sql)?;
            for size in [3000, 1] {
                for _ in 0..8192 {
                    bounded(started);
                    let count: i64 = db
                        .pragma_query_value(None, "page_count", |r| r.get(0))
                        .map_err(sql)?;
                    if count == MAX_PAGES as i64 {
                        break;
                    }
                    match db.execute(
                        "INSERT INTO qualification_padding VALUES(zeroblob(?1))",
                        [size],
                    ) {
                        Ok(count) => assert_eq!(count, 1),
                        Err(error) => {
                            let error = sql(error);
                            if error == ports::Error::Capacity {
                                break;
                            }
                            return Err(error);
                        }
                    }
                }
            }
            let count: i64 = db
                .pragma_query_value(None, "page_count", |r| r.get(0))
                .map_err(sql)?;
            assert_eq!(count, MAX_PAGES as i64);
            db.execute_batch("DROP TABLE qualification_padding")
                .map_err(sql)?;
            Ok(())
        })
        .unwrap();
}
fn dirty_bodies(store: &IndexStore<'_>, bodies: &[(BlobId, BlobRow)], started: Instant) {
    for (index, (id, body)) in bodies.iter().enumerate() {
        bounded(started);
        let mut writer = lock(&store.writer).unwrap();
        writer.native.begin_work(deadline()).unwrap();
        writer.native.run(|db| {
            db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
            let mut statement = db.prepare("UPDATE blob_chunks SET body=?1 WHERE account=?2 AND blob=?3 AND ordinal=?4").map_err(sql)?;
            // Restore each chunk before moving on, retaining the original digest.
            let mut remaining = body.length;
            let mut ordinal = 0;
            while remaining > 0 {
                let count = remaining.min(BODY.len() as u64) as usize;
                for pattern in [ALTERED.as_slice(), BODY.as_slice()] {
                    assert_eq!(statement.execute(params![pattern.get(..count).unwrap(), ACCOUNT.as_bytes().as_slice(), id.as_bytes().as_slice(), ordinal]).map_err(sql)?, 1);
                }
                remaining -= count as u64;
                ordinal += 1;
            }
            Ok(())
        }).unwrap();
        finish_commit(&mut writer).unwrap();
        drop(writer);
        let wal = db_path(store.root(), RootEntry::Wal).unwrap();
        assert!(fs::metadata(wal).unwrap().len() <= MAX_WAL_BYTES);
        assert!(
            fs::metadata(db_path(store.root(), RootEntry::SharedMemory).unwrap())
                .unwrap()
                .len()
                <= MAX_SHM_BYTES
        );
        if index % 16 == 0 {
            eprintln!(
                "maximum-database dirty blobs={} elapsed={:?}",
                index + 1,
                started.elapsed()
            );
        }
    }
}
fn verify(store: &IndexStore<'_>, bodies: &[(BlobId, BlobRow)], started: Instant) {
    bounded(started);
    let phase = Instant::now();
    store.validate_integrity(deadline()).unwrap();
    bounded(started);
    eprintln!("maximum-database integrity elapsed={:?}", phase.elapsed());
    let writer = lock(&store.writer).unwrap();
    writer.native.begin_work(deadline()).unwrap();
    writer
        .native
        .run(|db| {
            let count: i64 = db
                .query_row("SELECT count(*) FROM blob_ids", [], |r| r.get(0))
                .map_err(sql)?;
            assert_eq!(count, bodies.len() as i64);
            Ok(())
        })
        .unwrap();
    drop(writer);
    let phase = Instant::now();
    let mut view = store.maintenance_view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().epoch, EPOCH);
    assert_eq!(
        view.identity().committed_sequence,
        Sequence::from_u64(bodies.len() as u64)
    );
    assert_eq!(view.identity().history_floor, Sequence::default());
    for (index, (id, body)) in bodies.iter().enumerate() {
        assert_eq!(
            view.get(Key::Blob(*id), &mut [0; 128]).unwrap(),
            Some((Row::Blob(*body), Sequence::from_u64(index as u64 + 1)))
        );
    }
    let bytes = bodies.iter().map(|(_, row)| row.length).sum::<u64>();
    let mut scratch = [0; MAX_FILE_STEP_BYTES];
    let report = view
        .verify_account(
            &td_crypto::Provider,
            0,
            AccountCheckLimits {
                metadata: crate::metadata_sweep::Limits {
                    rows: bodies.len() as u64,
                    parent_reads: 0,
                },
                bodies: BodyCheckLimits {
                    blobs: bodies.len() as u64,
                    bytes,
                },
            },
            &mut scratch,
        )
        .unwrap();
    assert_eq!(report.identity(), view.identity());
    assert_eq!(report.metadata().references().rows(), bodies.len() as u64);
    assert_eq!(
        report.metadata().references().table_rows(Table::Blobs),
        Some(bodies.len() as u64)
    );
    assert_eq!(
        (report.bodies().blobs(), report.bodies().bytes()),
        (bodies.len() as u64, bytes)
    );
    bounded(started);
    eprintln!(
        "maximum-database account blobs={} bytes={bytes} elapsed={:?}",
        bodies.len(),
        phase.elapsed()
    );
}

#[test]
#[ignore = "explicit qualification fills an 8 GiB database and writes a multi-gigabyte WAL"]
fn maximum_database_checkpoint_and_account_maintenance() {
    let started = Instant::now();
    let fixture = Fixture::new();
    eprintln!("maximum-database root={}", fixture.path.display());
    memory("baseline");
    let mut root = fixture.locked();
    let store = IndexStore::create(&mut root, EPOCH, Arc::new(Fixed), 8, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let bodies = fill(&store, started);
    fill_free_pages(&store, started);
    store.checkpoint(deadline()).unwrap();
    assert_eq!(pages(&store), MAX_PAGES);
    let database = db_path(store.root(), RootEntry::Database).unwrap();
    assert_eq!(fs::metadata(&database).unwrap().len(), DATABASE_BYTES);
    memory("filled");
    dirty_bodies(&store, &bodies, started);
    let wal = db_path(store.root(), RootEntry::Wal).unwrap();
    let wal_bytes = fs::metadata(&wal).unwrap().len();
    assert!((DATABASE_BYTES..=MAX_WAL_BYTES).contains(&wal_bytes));
    assert_eq!((wal_bytes - 32) % (PAGE_BYTES + 24), 0);
    let readers = lock(&store.readers).unwrap();
    assert_eq!(readers.len(), 8);
    assert!(readers
        .iter()
        .all(|slot| matches!(slot, ReaderSlot::Available(_))));
    drop(readers);
    memory("dirty");
    let phase = Instant::now();
    store.checkpoint(deadline()).unwrap();
    bounded(started);
    eprintln!("maximum-database checkpoint wal-bytes={wal_bytes} database-bytes={DATABASE_BYTES} elapsed={:?}", phase.elapsed());
    assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
    assert_eq!(fs::metadata(&database).unwrap().len(), DATABASE_BYTES);
    memory("checkpointed");
    verify(&store, &bodies, started);
    memory("verified");
    drop(store);
    let store = IndexStore::open(&mut root, Arc::new(Fixed), 8, deadline()).unwrap();
    assert_eq!(pages(&store), MAX_PAGES);
    assert_eq!(fs::metadata(&database).unwrap().len(), DATABASE_BYTES);
    verify(&store, &bodies, started);
    memory("reopened-verified");
    eprintln!("maximum-database qualified elapsed={:?}", started.elapsed());
}

#[test]
#[ignore = "explicit qualification copies an 8 GiB database and verifies both roots"]
fn maximum_database_backup_preserves_complete_account() {
    let started = Instant::now();
    let source = Fixture::new();
    let destination = Fixture::new();
    eprintln!(
        "maximum-backup source={} destination={}",
        source.path.display(),
        destination.path.display()
    );
    memory("backup-baseline");
    let mut root = source.locked();
    let mut target = destination.locked();
    let store = IndexStore::create(&mut root, EPOCH, Arc::new(Fixed), 8, deadline()).unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let bodies = fill(&store, started);
    fill_free_pages(&store, started);
    assert_eq!(pages(&store), MAX_PAGES);
    memory("backup-filled");
    let mut scratch = [0; MAX_FILE_STEP_BYTES];
    let phase = Instant::now();
    let receipt = store.backup(&mut target, deadline(), &mut scratch).unwrap();
    bounded(started);
    assert_eq!(
        receipt,
        BackupReceipt {
            epoch: EPOCH,
            bytes: DATABASE_BYTES
        }
    );
    assert_eq!(
        fs::metadata(db_path(&root, RootEntry::Database).unwrap())
            .unwrap()
            .len(),
        DATABASE_BYTES
    );
    assert_eq!(
        fs::metadata(db_path(&target, RootEntry::Database).unwrap())
            .unwrap()
            .len(),
        DATABASE_BYTES
    );
    assert!(
        fs::symlink_metadata(db_path(&target, RootEntry::BackupPartial).unwrap())
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    );
    eprintln!(
        "maximum-backup copy bytes={} elapsed={:?}",
        receipt.bytes,
        phase.elapsed()
    );
    memory("backup-copied");
    // Verify sequentially under both root locks, with nine native owners at a time.
    {
        let store = IndexStore::open(&mut root, Arc::new(Fixed), 8, deadline()).unwrap();
        assert_eq!(pages(&store), MAX_PAGES);
        verify(&store, &bodies, started);
    }
    memory("backup-source-verified");
    {
        let store = IndexStore::open(&mut target, Arc::new(Fixed), 8, deadline()).unwrap();
        assert_eq!(pages(&store), MAX_PAGES);
        verify(&store, &bodies, started);
    }
    memory("backup-destination-verified");
    eprintln!(
        "maximum-backup qualified blobs={} bytes={} elapsed={:?}",
        bodies.len(),
        receipt.bytes,
        started.elapsed()
    );
}
