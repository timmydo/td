//! Explicit, disk-heavy WAL recovery qualification; ordinary suites ignore it.
#![allow(clippy::unwrap_used, clippy::panic)]

use super::super::tests::Fixture;
use super::*;
use crate::{
    format::row::{BlobKind, BlobRow},
    ports::{BlobReader, Crypto, Digest, ReadView, Tick, Time},
    store_fs::with_probe_root_path,
};
use std::{
    io::{self, Read},
    os::unix::process::ExitStatusExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const ACCOUNT: AccountId = AccountId::from_bytes([0xb1; 16]);
const BLOB: BlobId = BlobId::from_bytes([0xb2; 16]);
const EPOCH: StoreEpoch = StoreEpoch::from_bytes([0xb3; 16]);
const BODY_BYTES: u64 = crate::limits::MAX_MESSAGE_BYTES as u64;
static BODY_CHUNK: [u8; crate::limits::SQLITE_BODY_CHUNK_BYTES] =
    [0x5a; crate::limits::SQLITE_BODY_CHUNK_BYTES];
static ALTERED_CHUNK: [u8; crate::limits::SQLITE_BODY_CHUNK_BYTES] =
    [0x5b; crate::limits::SQLITE_BODY_CHUNK_BYTES];
const CHILD_CASE: &str = "store_fs::index::wal_qualification::maximum_wal_child";
const ROOT_ENV: &str = "TD_MTA_MAXIMUM_WAL_ROOT";
const READY: &str = "maximum-wal-ready";
const FRAME_BYTES: u64 = PAGE_BYTES + 24;
const TARGET_FRAMES: u64 = (MAX_WAL_BYTES - 32) / FRAME_BYTES;
const MAX_ROUNDS: usize = 1200;
const GENERATION_LIMIT: Duration = Duration::from_secs(20 * 60);
const RECOVERY_LIMIT: Duration = Duration::from_secs(8 * 60);
const READ_LIMIT: Duration = Duration::from_secs(2 * 60);
const VERIFY_LIMIT: Duration = Duration::from_secs(5 * 60);

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
fn wal_frames(path: &Path) -> u64 {
    let bytes = fs::metadata(path).unwrap().len();
    assert!((32..=MAX_WAL_BYTES).contains(&bytes));
    assert_eq!((bytes - 32) % FRAME_BYTES, 0);
    (bytes - 32) / FRAME_BYTES
}
fn proc_kib(path: &str, key: &str) -> u64 {
    let mut file = fs::File::open(path).unwrap();
    let mut bytes = [0; 8192];
    let mut count = 0;
    loop {
        let read = file.read(bytes.get_mut(count..).unwrap()).unwrap();
        if read == 0 {
            break;
        }
        count += read;
        assert!(count < bytes.len(), "procfs probe exceeds fixed buffer");
    }
    assert!(count > 0);
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
    value
}
fn memory(phase: &str) {
    let rss = proc_kib("/proc/self/smaps_rollup", "Rss:");
    let hwm = proc_kib("/proc/self/status", "VmHWM:");
    eprintln!("maximum-wal memory {phase} rss-kib={rss} vmhwm-kib={hwm}");
}
fn put_body(store: &IndexStore<'_>) {
    let mut digest = td_crypto::Provider.sha256().unwrap();
    for _ in 0..BODY_BYTES / BODY_CHUNK.len() as u64 {
        digest.update(BODY_CHUNK.as_slice()).unwrap();
    }
    let row = Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: BODY_BYTES,
        digest: digest.finish().unwrap(),
        created_at: 0,
    });
    let mut value = [0; 128];
    let length = row.encode(&mut value).unwrap();
    let operation =
        Operation::put(Table::Blobs, BLOB.as_bytes(), value.get(..length).unwrap()).unwrap();
    let mut source = io::repeat(0x5a).take(BODY_BYTES);
    let sequence = store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                epoch: store.epoch(),
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: deadline(),
            },
            &[operation],
            &mut [BlobSource {
                id: BLOB,
                source: &mut source,
            }],
        )
        .unwrap();
    assert_eq!(sequence, Sequence::from_u64(1));
    assert_eq!(source.limit(), 0);
}
fn append_body(writer: &mut Writer, rows: i64, body: &[u8]) {
    writer.native.begin_work(deadline()).unwrap();
    writer
        .native
        .run(|db| {
            db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
            let mut statement = db
                .prepare(
                    "UPDATE blob_chunks SET body=?1 WHERE account=?2 AND blob=?3 AND ordinal=?4",
                )
                .map_err(sql)?;
            for ordinal in 0..rows {
                assert_eq!(
                    statement
                        .execute(params![
                            body,
                            ACCOUNT.as_bytes().as_slice(),
                            BLOB.as_bytes().as_slice(),
                            ordinal
                        ])
                        .map_err(sql)?,
                    1
                );
            }
            Ok(())
        })
        .unwrap();
    finish_commit(writer).unwrap();
}
fn append_metadata(writer: &mut Writer) {
    writer.native.begin_work(deadline()).unwrap();
    writer
        .native
        .run(|db| {
            db.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
            assert_eq!(
                db.execute(
                    "UPDATE blobs SET created_at=created_at+1 WHERE account=?1 AND id=?2",
                    params![ACCOUNT.as_bytes().as_slice(), BLOB.as_bytes().as_slice()],
                )
                .map_err(sql)?,
                1
            );
            Ok(())
        })
        .unwrap();
    finish_commit(writer).unwrap();
}
fn produce(root_path: &Path) {
    let started = Instant::now();
    with_probe_root_path(root_path, |root| {
        let store = IndexStore::create(root, EPOCH, Arc::new(Fixed), 8, deadline()).unwrap();
        store.create_account(ACCOUNT, deadline()).unwrap();
        put_body(&store);
        store.checkpoint(deadline()).unwrap();
        let mut snapshot = store.view(ACCOUNT, deadline()).unwrap();
        assert_eq!(
            snapshot.identity().committed_sequence,
            Sequence::from_u64(1)
        );
        memory("producer-start");
        let wal = db_path(store.root(), RootEntry::Wal).unwrap();
        let mut frames = 0;
        let mut metadata_delta = None;
        let mut metadata_commits = 0i64;
        for round in 0..MAX_ROUNDS {
            assert!(
                started.elapsed() < GENERATION_LIMIT,
                "WAL generation timeout"
            );
            let remaining = TARGET_FRAMES - frames;
            if remaining == 0 {
                break;
            }
            if metadata_delta.is_some_and(|delta| remaining < delta) {
                break;
            }
            let before = frames;
            let rows = if remaining > 50_000 {
                512
            } else if remaining > 4096 {
                16
            } else if remaining > 128 {
                1
            } else {
                0
            };
            if rows > 0 {
                let mut writer = lock(&store.writer).unwrap();
                append_body(&mut writer, rows, ALTERED_CHUNK.as_slice());
                let altered = wal_frames(&wal);
                assert!(altered > before && altered < TARGET_FRAMES);
                append_body(&mut writer, rows, BODY_CHUNK.as_slice());
                frames = wal_frames(&wal);
                assert!(frames > altered && frames <= TARGET_FRAMES);
            } else {
                append_metadata(&mut lock(&store.writer).unwrap());
                metadata_commits += 1;
                frames = wal_frames(&wal);
                let delta = frames.checked_sub(before).unwrap();
                assert!(delta > 0 && frames <= TARGET_FRAMES);
                match metadata_delta {
                    Some(expected) => assert_eq!(delta, expected),
                    None => metadata_delta = Some(delta),
                }
            }
            if round % 32 == 0 || rows == 0 && (remaining <= 16 || round % 16 == 0) {
                let bytes = fs::metadata(&wal).unwrap().len();
                let database = db_path(store.root(), RootEntry::Database).unwrap();
                let shm = db_path(store.root(), RootEntry::SharedMemory).unwrap();
                assert!(fs::metadata(database).unwrap().len() <= MAX_PAGES * PAGE_BYTES);
                assert!(fs::metadata(shm).unwrap().len() <= MAX_SHM_BYTES);
                eprintln!(
                    "maximum-wal producer round={round} frames={frames} bytes={bytes} elapsed={:?}",
                    started.elapsed()
                );
                memory("producer-progress");
            }
        }
        assert!(
            TARGET_FRAMES - frames <= 2,
            "WAL producer stopped too early"
        );
        assert!(metadata_commits > 0);
        assert_eq!(
            snapshot.identity().committed_sequence,
            Sequence::from_u64(1)
        );
        assert!(snapshot
            .get(Key::Blob(BLOB), &mut [0; 128])
            .unwrap()
            .is_some());
        let bytes = fs::metadata(&wal).unwrap().len();
        let database = db_path(store.root(), RootEntry::Database).unwrap();
        let shm = db_path(store.root(), RootEntry::SharedMemory).unwrap();
        assert!(fs::metadata(database).unwrap().len() <= MAX_PAGES * PAGE_BYTES);
        assert!(fs::metadata(shm).unwrap().len() <= MAX_SHM_BYTES);
        eprintln!(
            "maximum-wal producer-ready frames={frames} bytes={bytes} gap-frames={} elapsed={:?}",
            TARGET_FRAMES - frames,
            started.elapsed()
        );
        memory("producer-ready");
        let partial = root_path.join("maximum-wal-ready-partial");
        fs::write(&partial, format!("{frames} {bytes} {metadata_commits}\n")).unwrap();
        fs::File::open(&partial).unwrap().sync_all().unwrap();
        fs::rename(partial, root_path.join(READY)).unwrap();
        fs::File::open(root_path).unwrap().sync_all().unwrap();
        let mut byte = [0];
        io::stdin().read_exact(&mut byte).unwrap();
        panic!("maximum-WAL child unexpectedly resumed");
    });
}

#[test]
#[ignore = "child endpoint invoked only by the maximum-WAL parent"]
fn maximum_wal_child() {
    let Some(path) = std::env::var_os(ROOT_ENV) else {
        eprintln!("maximum-WAL child runs only under its parent fixture");
        return;
    };
    produce(Path::new(&path));
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn kill_ready(root: &Path) -> (u64, u64, i64) {
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            CHILD_CASE,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT_ENV, root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let until = Instant::now() + GENERATION_LIMIT + Duration::from_secs(60);
    let marker = loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "WAL child exited early"
        );
        match fs::read(root.join(READY)) {
            Ok(marker) => break marker,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => panic!("WAL marker read: {error}"),
        }
        assert!(Instant::now() < until, "WAL child timed out");
        thread::sleep(Duration::from_millis(100));
    };
    let text = std::str::from_utf8(&marker).unwrap();
    let mut fields = text.split_ascii_whitespace();
    let frames = fields.next().unwrap().parse().unwrap();
    let bytes = fields.next().unwrap().parse().unwrap();
    let metadata_commits = fields.next().unwrap().parse().unwrap();
    assert_eq!(fields.next(), None);
    child.0.kill().unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(9));
    (frames, bytes, metadata_commits)
}
fn verify_body(store: &IndexStore<'_>, metadata_commits: i64) {
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().epoch, EPOCH);
    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
    let mut value = [0; 128];
    let (Row::Blob(row), changed) = view.get(Key::Blob(BLOB), &mut value).unwrap().unwrap() else {
        panic!("expected committed blob row")
    };
    assert_eq!(changed, Sequence::from_u64(1));
    assert_eq!(row.created_at, metadata_commits);
    let mut input = view
        .open_blob_input(&td_crypto::Provider, BLOB, BODY_BYTES)
        .unwrap();
    let mut buffer = [0; crate::limits::SQLITE_BODY_CHUNK_BYTES];
    while input.position() != BODY_BYTES {
        let count = input.read(&mut buffer).unwrap();
        assert!(
            count > 0
                && buffer
                    .get(..count)
                    .unwrap()
                    .iter()
                    .all(|byte| *byte == 0x5a)
        );
    }
    let pin = input.finish().unwrap();
    assert_eq!(pin.len(), BODY_BYTES);
}

#[test]
#[ignore = "explicit qualification writes up to 17.28 GB of WAL and kills its child"]
fn maximum_wal_crash_recovery_and_checkpoint() {
    let started = Instant::now();
    let fixture = Fixture::new();
    eprintln!("maximum-wal root={}", fixture.path.display());
    memory("recovery-baseline");
    let (frames, bytes, metadata_commits) = kill_ready(&fixture.path);
    assert!(TARGET_FRAMES - frames <= 2);
    assert_eq!(bytes, 32 + frames * FRAME_BYTES);
    let mut root = fixture.locked();
    let wal = db_path(&root, RootEntry::Wal).unwrap();
    assert_eq!(fs::metadata(&wal).unwrap().len(), bytes);
    let phase = Instant::now();
    let store = IndexStore::open(&mut root, Arc::new(Fixed), 8, deadline()).unwrap();
    assert!(phase.elapsed() < RECOVERY_LIMIT, "WAL recovery timeout");
    eprintln!("maximum-wal recovery-open elapsed={:?}", phase.elapsed());
    memory("recovery-opened");
    let readers = lock(&store.readers).unwrap();
    assert_eq!(readers.len(), 8);
    assert!(readers
        .iter()
        .all(|slot| matches!(slot, ReaderSlot::Available(_))));
    drop(readers);
    let database = db_path(store.root(), RootEntry::Database).unwrap();
    let shm = db_path(store.root(), RootEntry::SharedMemory).unwrap();
    assert_eq!(fs::metadata(&wal).unwrap().len(), bytes);
    assert!(fs::metadata(&database).unwrap().len() <= MAX_PAGES * PAGE_BYTES);
    assert!(fs::metadata(&shm).unwrap().len() <= MAX_SHM_BYTES);
    let phase = Instant::now();
    verify_body(&store, metadata_commits);
    assert!(phase.elapsed() < READ_LIMIT, "WAL read timeout");
    eprintln!("maximum-wal recovery-read elapsed={:?}", phase.elapsed());
    memory("recovery-read");
    assert_eq!(fs::metadata(&wal).unwrap().len(), bytes);
    let phase = Instant::now();
    store.checkpoint(deadline()).unwrap();
    assert!(phase.elapsed() < RECOVERY_LIMIT, "WAL checkpoint timeout");
    eprintln!(
        "maximum-wal recovery-checkpoint elapsed={:?}",
        phase.elapsed()
    );
    memory("recovery-checkpointed");
    assert_eq!(fs::metadata(&wal).unwrap().len(), 0);
    assert!(fs::metadata(&database).unwrap().len() <= MAX_PAGES * PAGE_BYTES);
    assert!(fs::metadata(&shm).unwrap().len() <= MAX_SHM_BYTES);
    let phase = Instant::now();
    store.validate_integrity(deadline()).unwrap();
    verify_body(&store, metadata_commits);
    assert!(phase.elapsed() < VERIFY_LIMIT, "WAL verification timeout");
    memory("recovery-verified");
    eprintln!(
        "maximum-wal qualified frames={frames} bytes={bytes} ceiling-gap={} elapsed={:?}",
        MAX_WAL_BYTES - bytes,
        started.elapsed()
    );
}
