#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::{
    format::{operation::Operation, row::Row, Sequence, Table},
    ids::StoreEpoch,
    limits::Limits,
    ports::{ReadView, Time},
    store_fs::{tests::Fixture, BlobSource, CommitRequest, IndexStore},
};
use std::os::unix::fs::symlink;
const ACCOUNT: AccountId = AccountId::from_bytes([1; 16]);
const BLOB: BlobId = BlobId::from_bytes([2; 16]);
struct Timer(AtomicU64);
impl Clock for Timer {
    fn sample(&self) -> Result<Time, ports::Error> {
        Ok(Time {
            utc_ms: 0,
            monotonic: Tick(self.0.load(Ordering::SeqCst)),
        })
    }
}
fn clock() -> Arc<Timer> {
    Arc::new(Timer(AtomicU64::new(1)))
}
fn deadline() -> Deadline {
    Deadline::after(Tick(0), 100).unwrap()
}
fn resources(message_bytes: usize) -> ResourcePlan {
    Limits {
        message_bytes,
        header_bytes: message_bytes.min(256 * 1024),
        ..Limits::default()
    }
    .plan()
    .unwrap()
}
fn temporary(path: &std::path::Path, size: u64) {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    file.set_len(size).unwrap();
}
#[test]
fn capacity_derives_from_ingress_and_allocates_no_message_buffers() {
    let capacity = SpoolCapacity::from_resources(&Limits::default().plan().unwrap()).unwrap();
    assert_eq!(
        capacity,
        SpoolCapacity {
            slots: 16,
            bytes_each: 32 << 20,
            total_bytes: 512 << 20
        }
    );
    assert!(std::mem::size_of::<IngressSpool<'_>>() <= 128);
    assert!(std::mem::size_of::<SpoolWriter<'_, '_, td_crypto::Sha256>>() <= 512);
    assert!(std::mem::size_of::<SpoolInput<'_, '_>>() <= 384);
    for slot in 0..64 {
        let name = Name::ingress_slot(slot).unwrap();
        assert_eq!(parse_slot(name.as_path().unwrap().as_os_str()), Some(slot));
    }
    assert!(Name::ingress_slot(64).is_err());
    for name in ["slot-64", "slot-0", "slot-000", "slot-+1", "../slot-00"] {
        assert!(parse_slot(std::ffi::OsStr::new(name)).is_none());
    }
}
#[test]
fn quota_is_reserved_before_creation_and_held_by_finished_input() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool = IngressSpool::open(&mut root, &resources(8), clock(), deadline()).unwrap();
    let mut owners = Vec::new();
    for _ in 0..spool.capacity().slots {
        owners.push(
            spool
                .begin(
                    &td_crypto::Provider,
                    ACCOUNT,
                    BLOB,
                    BlobKind::Message,
                    deadline(),
                )
                .unwrap()
                .finish()
                .unwrap(),
        );
    }
    assert_eq!(spool.status().unwrap().reserved_bytes, 128);
    assert!(matches!(
        spool.begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Upload,
            deadline()
        ),
        Err(ports::Error::Quota)
    ));
    owners.pop().unwrap().discard().unwrap();
    let replacement = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Upload,
            deadline(),
        )
        .unwrap();
    assert_eq!(spool.status().unwrap().occupied_slots, 16);
    drop(replacement);
    drop(owners);
    assert_eq!(spool.status().unwrap().reserved_bytes, 0);
    assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 1);
}
#[test]
fn finished_bytes_digest_and_passive_metadata_commit_to_sqlite() {
    let fixture = Fixture::new();
    let database = Fixture::new();
    let mut root = fixture.locked();
    let mut database_root = database.locked();
    let timer = clock();
    let spool = IngressSpool::open(&mut root, &resources(16), timer.clone(), deadline()).unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Upload,
            deadline(),
        )
        .unwrap();
    writer.write(b"hello ").unwrap();
    writer.write(b"world").unwrap();
    let mut input = writer.finish().unwrap();
    assert_eq!(
        (input.account(), input.id(), input.kind(), input.len()),
        (ACCOUNT, BLOB, BlobKind::Upload, 11)
    );
    let mut expected = td_crypto::Provider.sha256().unwrap();
    expected.update(b"hello world").unwrap();
    assert_eq!(*input.digest(), expected.finish().unwrap());
    let mut received = [0; 20];
    assert_eq!(input.read(&mut received).unwrap(), 11);
    assert_eq!(received.get(..11).unwrap(), b"hello world");
    assert_eq!(input.read(&mut received).unwrap(), 0);
    input.rewind().unwrap();
    let store = IndexStore::create(
        &mut database_root,
        StoreEpoch::from_bytes([3; 16]),
        timer,
        1,
        deadline(),
    )
    .unwrap();
    store.create_account(ACCOUNT, deadline()).unwrap();
    let mut row = [0; 128];
    let bytes = Row::Blob(input.row(42)).encode(&mut row).unwrap();
    let op = Operation::put(Table::Blobs, BLOB.as_bytes(), row.get(..bytes).unwrap()).unwrap();
    store
        .commit(
            &td_crypto::Provider,
            CommitRequest {
                account: ACCOUNT,
                expected: Sequence::default(),
                utc_ms: 0,
                deadline: deadline(),
            },
            &[op],
            &mut [BlobSource {
                id: BLOB,
                source: &mut input,
            }],
        )
        .unwrap();
    let mut view = store.view(ACCOUNT, deadline()).unwrap();
    assert_eq!(view.identity().committed_sequence, Sequence::from_u64(1));
    input.discard().unwrap();
    assert_eq!(spool.status().unwrap().occupied_slots, 0);
    // Discarding provisional bytes after COMMIT cannot remove authoritative bytes.
    let mut pinned = view
        .open_blob_input(&td_crypto::Provider, BLOB, MAX_MESSAGE_BYTES as u64)
        .unwrap();
    while pinned.position() != pinned.len() {
        pinned.read(&mut [0; 65536]).unwrap();
    }
    let mut pinned = pinned.finish().unwrap();
    use crate::ports::BlobReader;
    assert_eq!(pinned.read_at(0, &mut received).unwrap(), 11);
    assert_eq!(received.get(..11).unwrap(), b"hello world");
}
#[test]
fn maximum_message_streams_in_fixed_chunks_and_larger_turn_is_refused() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool = IngressSpool::open(
        &mut root,
        &Limits::default().plan().unwrap(),
        clock(),
        deadline(),
    )
    .unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    let chunk = [0xa5; SQLITE_BODY_CHUNK_BYTES];
    for _ in 0..MAX_MESSAGE_BYTES / chunk.len() {
        writer.write(&chunk).unwrap();
    }
    let mut input = writer.finish().unwrap();
    let mut output = [0; SQLITE_BODY_CHUNK_BYTES * 2];
    let mut total = 0;
    loop {
        let count = input.read(&mut output).unwrap();
        if count == 0 {
            break;
        }
        assert_eq!(count, SQLITE_BODY_CHUNK_BYTES);
        assert!(output
            .get(..count)
            .unwrap()
            .iter()
            .all(|byte| *byte == 0xa5));
        total += count;
    }
    assert_eq!(total, MAX_MESSAGE_BYTES);
    input.discard().unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    assert_eq!(writer.write(&output), Err(ports::Error::Capacity));
    assert_eq!(fs::metadata(fixture.path.join("slot-00")).unwrap().len(), 0);
    writer.discard().unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    writer.length = u64::MAX;
    assert_eq!(writer.write(b"a"), Err(ports::Error::Capacity));
    writer.discard().unwrap();
}
#[test]
fn startup_cleans_all_old_slots_after_configuration_reduction() {
    let fixture = Fixture::new();
    for slot in [0, 15, 63] {
        temporary(&fixture.path.join(format!("slot-{slot:02}")), 32 << 20);
    }
    let mut root = fixture.locked();
    let limits = Limits {
        smtp_sessions: 1,
        smtp_per_peer: 1,
        https_connections: 1,
        event_streams: 0,
        message_bytes: 1,
        header_bytes: 1,
        ..Limits::default()
    }
    .plan()
    .unwrap();
    let spool = IngressSpool::open(&mut root, &limits, clock(), deadline()).unwrap();
    assert_eq!(spool.capacity().total_bytes, 2);
    assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 1);
}
#[test]
fn crash_cleanup_accepts_only_nonexecuting_owner_permission_subsets() {
    let fixture = Fixture::new();
    for (slot, mode) in [0o000, 0o200, 0o400, 0o600].into_iter().enumerate() {
        let path = fixture.path.join(format!("slot-{slot:02}"));
        temporary(&path, 1);
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(check_cleanup_file(&metadata, metadata.uid().wrapping_add(1)).is_err());
        assert_eq!(check_file(&metadata, metadata.uid()).is_ok(), mode == 0o600);
    }
    let mut root = fixture.locked();
    let spool = IngressSpool::open(&mut root, &resources(8), clock(), deadline()).unwrap();
    assert_eq!(spool.status().unwrap().reserved_bytes, 0);
    assert_eq!(fs::read_dir(&fixture.path).unwrap().count(), 1);
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    writer
        .owner
        .file()
        .unwrap()
        .set_permissions(fs::Permissions::from_mode(0o000))
        .unwrap();
    writer.discard().unwrap();
    assert_eq!(spool.status().unwrap().reserved_bytes, 0);
}
#[test]
fn invalid_startup_artifact_refuses_before_any_valid_file_is_removed() {
    for mode in 0..7 {
        let fixture = Fixture::new();
        temporary(&fixture.path.join("slot-00"), 1);
        let bad = fixture.path.join("slot-63");
        match mode {
            0 => {
                fs::write(fixture.path.join("metadata.sqlite3"), b"authoritative").unwrap();
            }
            1 => symlink("slot-00", &bad).unwrap(),
            2 => fs::hard_link(fixture.path.join("slot-00"), &bad).unwrap(),
            3 => temporary(&bad, MAX_MESSAGE_BYTES as u64 + 1),
            4 => {
                temporary(&bad, 0);
                fs::set_permissions(&bad, fs::Permissions::from_mode(0o644)).unwrap();
            }
            5 => {
                temporary(&bad, 0);
                fs::set_permissions(&bad, fs::Permissions::from_mode(0o700)).unwrap();
            }
            _ => fs::create_dir(&bad).unwrap(),
        }
        let mut root = fixture.locked();
        assert!(IngressSpool::open(&mut root, &resources(8), clock(), deadline()).is_err());
        assert!(fixture.path.join("slot-00").exists());
    }
}
#[test]
fn partial_io_and_length_refusal_are_sticky_and_retain_quota_until_cleanup() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool = IngressSpool::open(&mut root, &resources(4), clock(), deadline()).unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    let error = writer
        .write_with(b"abcd", |file, bytes| {
            file.write_all(bytes.get(..2).ok_or(io::ErrorKind::InvalidInput)?)?;
            Err(io::ErrorKind::WriteZero.into())
        })
        .unwrap_err();
    assert_eq!(fs::metadata(fixture.path.join("slot-00")).unwrap().len(), 2);
    assert_eq!(spool.status().unwrap().reserved_bytes, 4);
    assert_eq!(writer.write(b"a"), Err(error));
    assert!(matches!(writer.finish(), Err(e) if e == error));
    assert_eq!(spool.status().unwrap().reserved_bytes, 0);
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    writer.write(b"abcd").unwrap();
    assert_eq!(writer.write(b"e"), Err(ports::Error::Capacity));
    assert_eq!(fs::metadata(fixture.path.join("slot-00")).unwrap().len(), 4);
    assert_eq!(writer.write(b""), Err(ports::Error::Capacity));
    writer.discard().unwrap();
}
#[test]
fn cleanup_failure_closes_descriptor_and_retires_full_charge_until_restart() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    {
        let spool = IngressSpool::open(&mut root, &resources(8), clock(), deadline()).unwrap();
        let mut input = spool
            .begin(
                &td_crypto::Provider,
                ACCOUNT,
                BLOB,
                BlobKind::Message,
                deadline(),
            )
            .unwrap()
            .finish()
            .unwrap();
        let name = fixture.path.join("slot-00");
        // Simulate an unexpected link, then repair it only after the owner retires.
        fs::hard_link(&name, fixture.path.join("foreign")).unwrap();
        assert!(input.owner.cleanup().is_err());
        assert!(input.owner.file.is_none());
        assert_eq!(
            spool.status().unwrap(),
            SpoolStatus {
                occupied_slots: 1,
                retired_slots: 1,
                reserved_bytes: 8
            }
        );
        drop(input);
        fs::remove_file(fixture.path.join("foreign")).unwrap();
    }
    let spool = IngressSpool::open(&mut root, &resources(8), clock(), deadline()).unwrap();
    assert_eq!(spool.status().unwrap().reserved_bytes, 0);
    assert!(!fixture.path.join("slot-00").exists());
}
#[test]
fn deadline_finish_length_and_source_read_errors_cannot_be_retried() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let timer = clock();
    let spool = IngressSpool::open(&mut root, &resources(8), timer.clone(), deadline()).unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    writer.write(b"abc").unwrap();
    writer.owner.file().unwrap().set_len(4).unwrap();
    assert!(matches!(writer.finish(), Err(ports::Error::Corrupt)));
    let mut input = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap()
        .finish()
        .unwrap();
    input.owner.file = Some(File::open(&fixture.path).unwrap());
    let first = input.read(&mut [0; 1]).unwrap_err().kind();
    assert_eq!(input.read(&mut [0; 1]).unwrap_err().kind(), first);
    assert!(input.rewind().is_err());
    input.discard().unwrap();
    let mut writer = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap();
    timer.0.store(100, Ordering::SeqCst);
    assert_eq!(writer.write(b"a"), Err(ports::Error::Deadline));
    timer.0.store(1, Ordering::SeqCst);
    assert!(matches!(writer.finish(), Err(ports::Error::Deadline)));
}

#[test]
fn a_fresh_seek_error_is_terminal_for_rewind_and_read() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool = IngressSpool::open(&mut root, &resources(8), clock(), deadline()).unwrap();
    let mut input = spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline(),
        )
        .unwrap()
        .finish()
        .unwrap();
    let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(peer);
    let descriptor: std::os::fd::OwnedFd = socket.into();
    input.owner.file = Some(File::from(descriptor));
    assert_eq!(input.failure(), None);
    let error = input.rewind().unwrap_err();
    assert_eq!(input.failure(), Some(error));
    assert!(matches!(error, ports::Error::Io { .. }));
    assert_eq!(input.rewind(), Err(error));
    assert!(input.read(&mut [0; 1]).is_err());
    input.discard().unwrap();
}

#[test]
fn unproven_creation_releases_only_positive_absence_and_never_removes_an_inode() {
    let fixture = Fixture::new();
    let mut root = fixture.locked();
    let spool = IngressSpool::open(&mut root, &resources(8), clock(), deadline()).unwrap();
    let mut missing = Temporary {
        spool: &spool,
        slot: spool.reserve().unwrap(),
        file: None,
        identity: None,
        released: false,
    };
    missing.cleanup().unwrap();
    assert_eq!(spool.status().unwrap().reserved_bytes, 0);
    // A failed create_new must not reclaim an already existing unknown inode.
    temporary(&fixture.path.join("slot-00"), 1);
    assert!(spool
        .begin(
            &td_crypto::Provider,
            ACCOUNT,
            BLOB,
            BlobKind::Message,
            deadline()
        )
        .is_err());
    assert_eq!(spool.status().unwrap().retired_slots, 1);
    assert_eq!(fs::metadata(fixture.path.join("slot-00")).unwrap().len(), 1);
    // A created file whose first metadata lookup failed is closed but retained.
    let mut unknown = Temporary {
        spool: &spool,
        slot: spool.reserve().unwrap(),
        file: Some(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(fixture.path.join("slot-01"))
                .unwrap(),
        ),
        identity: None,
        released: false,
    };
    assert!(unknown.cleanup().is_err());
    assert!(unknown.file.is_none());
    assert_eq!(spool.status().unwrap().retired_slots, 2);
    assert_eq!(spool.status().unwrap().reserved_bytes, 16);
    assert!(fixture.path.join("slot-01").exists());
}
