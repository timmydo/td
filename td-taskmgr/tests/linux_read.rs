#![allow(clippy::unwrap_used, clippy::panic)]
use std::io::{self, Read};
use td_taskmgr::linux_read::{auxv, runtime_units, Reader, Units};
#[test]
fn reader_refuses_one_past_bound_without_reallocating_and_reuses_storage() {
    let mut reader = Reader::new(
        &td_taskmgr::budget::Budget::new(td_taskmgr::budget::LIMIT).unwrap(),
        4096,
    )
    .unwrap();
    let capacity = reader.storage_bytes();
    assert_eq!(reader.read(&b"four"[..], 4).unwrap(), b"four");
    assert_eq!(reader.storage_bytes(), capacity);
    assert_eq!(
        reader.read(&b"fives"[..], 4).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(reader.storage_bytes(), capacity);
    assert_eq!(reader.read(&b""[..], 4).unwrap(), b"");
    assert!(reader.read(&b"x"[..], 4097).is_err());
    let all = vec![b'x'; 4096];
    assert_eq!(reader.read(all.as_slice(), 4096).unwrap().len(), 4096);
    assert_eq!(reader.storage_bytes(), capacity);
    struct Interrupted(bool);
    impl Read for Interrupted {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            if !self.0 {
                self.0 = true;
                return Err(io::ErrorKind::Interrupted.into());
            }
            if let Some(first) = b.first_mut() {
                *first = b'x';
            }
            Ok(1)
        }
    }
    assert_eq!(
        reader.read(Interrupted(false), 2).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}
fn vector(pairs: &[(usize, usize)]) -> Vec<u8> {
    pairs
        .iter()
        .flat_map(|(kind, value)| kind.to_ne_bytes().into_iter().chain(value.to_ne_bytes()))
        .collect()
}
#[test]
fn auxiliary_vector_supplies_runtime_values_and_refuses_missing_or_duplicate_fields() {
    let bytes = vector(&[(6, 65536), (17, 250), (99, 1), (0, 0)]);
    assert_eq!(
        auxv(&bytes).unwrap(),
        Units {
            page_size: 65536,
            ticks_per_second: 250
        }
    );
    for pairs in [
        &[(6, 4096), (0, 0)][..],
        &[(6, 4096), (17, 0), (0, 0)],
        &[(6, 4096), (6, 4096), (17, 100), (0, 0)],
        &[(6, 4096), (17, 100)],
    ] {
        assert!(auxv(&vector(pairs)).is_err());
    }
    assert!(auxv(b"truncated").is_err());
}
#[test]
fn actual_procfs_reads_use_retained_directory_and_runtime_units() {
    let mut reader = Reader::new(
        &td_taskmgr::budget::Budget::new(td_taskmgr::budget::LIMIT).unwrap(),
        65536,
    )
    .unwrap();
    let units = runtime_units(&mut reader).unwrap();
    assert!(units.page_size > 0);
    assert!(units.ticks_per_second > 0);
    let directory = std::fs::File::open("/proc/self").unwrap();
    let bytes = reader.process_file(&directory, "stat", 65536).unwrap();
    let process = td_taskmgr::parsers::process(bytes).unwrap();
    assert_eq!(process.pid, std::process::id());
    let bytes = reader.process_file(&directory, "status", 65536).unwrap();
    assert!(td_taskmgr::parsers::real_uid(bytes).unwrap().is_some());
    assert!(reader
        .process_file(&directory, "../environ", 65536)
        .is_err());
}

#[test]
fn repeated_interruption_is_bounded() {
    struct Interrupted;
    impl Read for Interrupted {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::Interrupted.into())
        }
    }
    let mut reader = Reader::new(
        &td_taskmgr::budget::Budget::new(td_taskmgr::budget::LIMIT).unwrap(),
        32,
    )
    .unwrap();
    assert_eq!(
        reader.read(Interrupted, 32).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
}
