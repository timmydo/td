//! Dedicated process: no libtest worker or concurrent harness allocations.
#![cfg(test)]
#![deny(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "support/allocation_counter.rs"]
mod allocation_counter;
#[path = "support/allocation_registry.rs"]
mod allocation_registry;
#[path = "support/allocation_shim.rs"]
mod allocation_shim;

#[path = "support/tls_allocation_scenario.rs"]
mod tls_allocation_scenario;

use allocation_shim::TD_MTA_ALLOCATION_COUNTERS as COUNTERS;
use std::hint::black_box;
use td_crypto::Digest;

fn forwarding() {
    let before = COUNTERS.snapshot();
    let mut bytes = Vec::<u8>::with_capacity(black_box(16));
    bytes.resize(16, 0x5a);
    black_box(&mut bytes);
    bytes.reserve_exact(48);
    bytes.truncate(8);
    bytes.shrink_to_fit();
    assert_eq!(bytes.as_slice(), &[0x5a; 8]);
    // Valid u8 layout, deliberately beyond the runner's address/data ceiling.
    assert!(bytes.try_reserve_exact(isize::MAX as usize - 8).is_err());
    assert_eq!(bytes.as_slice(), &[0x5a; 8]);
    drop(bytes);
    let zeroes = vec![0u8; black_box(37)];
    assert!(black_box(&zeroes).iter().all(|&b| b == 0));
    drop(zeroes);
    #[repr(align(4096))]
    struct Aligned([u8; 4096]);
    let aligned_before = COUNTERS.snapshot();
    let aligned = Box::new(Aligned([7; 4096]));
    assert_eq!(black_box(&*aligned) as *const Aligned as usize % 4096, 0);
    assert_eq!(black_box(&aligned.0).first(), Some(&7));
    let aligned_live = COUNTERS.snapshot();
    assert_eq!(aligned_live.alloc, aligned_before.alloc + 1);
    assert_eq!(
        aligned_live.live,
        aligned_before.live + std::mem::size_of::<Aligned>()
    );
    drop(aligned);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(after.live, before.live);
    assert!(after.alloc > before.alloc);
    assert!(after.zeroed > before.zeroed);
    assert!(after.realloc >= before.realloc + 3);
    assert!(after.failed > before.failed);
    assert!(after.free > before.free);
    assert!(after.peak >= before.live + 4096);
}

fn hot_paths() {
    let mut scratch = [0; 512];
    let data = [0x5a; 4096];
    let registry = allocation_registry::Registry::<4>::new();
    let before = COUNTERS.snapshot();
    for _ in 0..64 {
        registry.insert(64, 0).unwrap();
        registry.insert(128, 17).unwrap();
        assert_eq!(registry.remove(64), Ok(0));
        assert_eq!(registry.remove(128), Ok(17));
        assert_eq!(registry.snapshot().bytes, 0);
        assert!(!registry.snapshot().invalid);
        let mut digest = td_crypto::Sha256::try_new().unwrap();
        digest.update(black_box(&data)).unwrap();
        black_box(digest.finish().unwrap());
        store_containers();
        store_table_records();
        store_manifests();
        store_frame_parts();
        store_complete_frames();
        store_paths();
        let mut line = td_mta::smtp_wire::LineReader::new(&mut scratch).unwrap();
        assert!(!line.feed(black_box(b"EHLO example")).unwrap().complete);
        assert!(line.feed(black_box(b".test\r\n")).unwrap().complete);
        assert_eq!(line.line(), Some(b"EHLO example.test".as_slice()));
        line.advance().unwrap();
        assert!(line.feed(black_box(b"bad\n")).is_err());
        assert!(line.feed(black_box(b"NOOP\r\n")).is_err());
        assert!(td_mta::smtp_wire::LineReader::new(&mut []).is_err());
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid);
    assert_eq!(
        after, before,
        "Rust allocator activity in measured hot paths"
    );
}

fn store_containers() {
    use td_mta::{
        format::{
            container::{Current, Error as ContainerError, JournalHeader, StoreIdentity},
            Error as FormatError, Sequence,
        },
        ids::{AccountId, InstanceId, StoreEpoch},
    };
    let crypto = td_crypto::Provider;
    let mut bytes = [0; 120];
    let identity = StoreIdentity {
        instance: InstanceId::from_bytes([0x11; 16]),
        epoch: StoreEpoch::from_bytes([0x22; 16]),
    };
    let n = identity.encode(&crypto, black_box(&mut bytes)).unwrap();
    assert_eq!(
        StoreIdentity::decode(&crypto, bytes.get(..n).unwrap()).unwrap(),
        identity
    );
    let current = Current {
        account: AccountId::from_bytes([0x33; 16]),
        epoch: identity.epoch,
        generation: 1,
        manifest_digest: [0x44; 32],
    };
    let n = current.encode(&crypto, black_box(&mut bytes)).unwrap();
    assert_eq!(
        Current::decode(&crypto, bytes.get(..n).unwrap()).unwrap(),
        current
    );
    assert_eq!(
        current.encode(&crypto, bytes.get_mut(..n - 1).unwrap()),
        Err(ContainerError::Format(FormatError::OutputFull))
    );
    let journal = JournalHeader {
        account: current.account,
        epoch: current.epoch,
        segment: 1,
        base: Sequence::default(),
    };
    let n = journal.encode(&crypto, black_box(&mut bytes)).unwrap();
    assert_eq!(
        JournalHeader::decode(&crypto, bytes.get(..n).unwrap()).unwrap(),
        journal
    );
    assert_eq!(
        JournalHeader::decode(&crypto, bytes.get(..n - 1).unwrap()),
        Err(ContainerError::Format(FormatError::Truncated))
    );
    *bytes.get_mut(n - 1).unwrap() ^= 1;
    assert_eq!(
        JournalHeader::decode(&crypto, bytes.get(..n).unwrap()),
        Err(ContainerError::Checksum)
    );
}

fn store_table_records() {
    use td_mta::{
        format::{
            container::Error,
            row::{BlobKind, BlobRow, Row},
            table::{record_extent, Record, TableHeader},
            table_stream::Verifier,
            Error as FormatError, Sequence, Table,
        },
        ids::{AccountId, StoreEpoch},
    };
    let crypto = td_crypto::Provider;
    let through = Sequence::from_u64(1);
    let header = TableHeader {
        table: Table::Blobs,
        account: AccountId::from_bytes([0x33; 16]),
        epoch: StoreEpoch::from_bytes([0x22; 16]),
        generation: 2,
        through,
        record_count: 1,
        payload_bytes: 113,
    };
    let mut header_bytes = [0; 112];
    header.encode(&crypto, &mut header_bytes).unwrap();
    let mut stream = Verifier::new(&crypto, &header_bytes).unwrap();
    let mut bytes = [0; 120];
    let n = header.encode(&crypto, black_box(&mut bytes)).unwrap();
    assert_eq!(
        TableHeader::decode(&crypto, bytes.get(..n).unwrap()),
        Ok(header)
    );
    let mut value = [0; 49];
    let row = Row::Blob(BlobRow {
        kind: BlobKind::Message,
        length: 3,
        digest: [0; 32],
        created_at: 0,
    });
    assert_eq!(row.encode(&mut value), Ok(value.len()));
    let key = [0x44; 16];
    let record = Record::new(Table::Blobs, through, &key, &value).unwrap();
    let n = record
        .encode(&crypto, through, black_box(&mut bytes))
        .unwrap();
    assert_eq!(
        Record::decode(&crypto, Table::Blobs, through, bytes.get(..n).unwrap()).unwrap(),
        record
    );
    assert_eq!(record_extent(bytes.get(..16).unwrap()), Ok(n));
    assert_eq!(stream.push(bytes.get(..n).unwrap()), Ok(record));
    assert_eq!(stream.finish().unwrap().header(), header);
    let mut failed = Verifier::new(&crypto, &header_bytes).unwrap();
    assert_eq!(
        failed.push(bytes.get(..n - 1).unwrap()),
        Err(Error::Format(FormatError::Truncated))
    );
    assert_eq!(
        failed.push(bytes.get(..n).unwrap()),
        Err(Error::Format(FormatError::Truncated))
    );
    assert_eq!(failed.finish(), Err(Error::Format(FormatError::Truncated)));
    assert_eq!(
        record.encode(&crypto, through, bytes.get_mut(..n - 1).unwrap()),
        Err(Error::Format(FormatError::OutputFull))
    );
    assert_eq!(
        Record::decode(&crypto, Table::Blobs, through, bytes.get(..n - 1).unwrap()),
        Err(Error::Format(FormatError::Truncated))
    );
    *bytes.get_mut(n - 1).unwrap() ^= 1;
    assert_eq!(
        Record::decode(&crypto, Table::Blobs, through, bytes.get(..n).unwrap()),
        Err(Error::Checksum)
    );
}

fn store_frame_parts() {
    use td_mta::{
        format::{
            frame_header::Header,
            operation::{extent, Operation},
            Error, ObjectType, Sequence, Table,
        },
        ports::ChangeAction,
    };
    let crypto = td_crypto::Provider;
    let header = Header {
        frame_bytes: 132,
        operations: 1,
        sequence: Sequence::from_u64(1),
    };
    let mut header_bytes = [0; 64];
    header
        .encode(&crypto, black_box(&mut header_bytes))
        .unwrap();
    assert_eq!(Header::decode(&crypto, &header_bytes), Ok(header));
    let key = [0x44; 16];
    let members = [0x55; 32];
    for operation in [
        Operation::delete(Table::Blobs, &key).unwrap(),
        Operation::put(Table::Memberships, &members, &[]).unwrap(),
        Operation::change(ObjectType::Email, ChangeAction::Updated, &key),
    ] {
        let mut bytes = [0; 64];
        let n = operation.encode(black_box(&mut bytes)).unwrap();
        assert_eq!(extent(bytes.get(..12).unwrap()), Ok(n));
        assert_eq!(Operation::decode(bytes.get(..n).unwrap()), Ok(operation));
        assert_eq!(
            operation.encode(bytes.get_mut(..n - 1).unwrap()),
            Err(Error::OutputFull)
        );
        assert_eq!(
            Operation::decode(bytes.get(..n - 1).unwrap()),
            Err(Error::Truncated)
        );
    }
}

fn store_paths() {
    use td_mta::{
        format::{row::BlobKind, Table},
        ids::{AccountId, BlobId},
        store_paths::{parse_blob_name, AccountEntry, Name, Number, RootEntry},
    };
    let account = AccountId::from_bytes(black_box([0xff; 16]));
    let blob = BlobId::from_bytes(black_box([0xab; 16]));
    let generation = Number::new(black_box(u64::MAX)).unwrap();
    for entry in [RootEntry::Format, RootEntry::Lock, RootEntry::Accounts] {
        black_box(Name::root(black_box(entry)).unwrap());
    }
    for entry in [
        AccountEntry::Root,
        AccountEntry::Current,
        AccountEntry::Table(generation, Table::ThreadAnchors),
        AccountEntry::Manifest(generation),
        AccountEntry::Journal(generation),
        AccountEntry::Shard(BlobKind::Message, 0xab),
        AccountEntry::Blob(BlobKind::Message, blob),
        AccountEntry::Blob(BlobKind::Upload, blob),
    ] {
        let name = Name::account(account, black_box(entry)).unwrap();
        black_box(name.as_bytes().unwrap());
        black_box(name.as_str().unwrap());
        black_box(name.as_path().unwrap());
        black_box(name.as_c_str().unwrap());
    }
    assert_eq!(
        Number::parse(black_box("18446744073709551615")).unwrap(),
        generation
    );
    assert_eq!(
        Number::parse(black_box("00000000000000000042"))
            .unwrap()
            .value(),
        42
    );
    assert!(Number::parse(black_box("18446744073709551616")).is_err());
    assert!(Number::parse(black_box("0000001")).is_err());
    let text = black_box("abababababababababababababababab.eml");
    assert_eq!(
        parse_blob_name(BlobKind::Message, 0xab, text).unwrap(),
        blob
    );
    assert!(parse_blob_name(BlobKind::Message, 0xac, text).is_err());
    assert!(parse_blob_name(BlobKind::Upload, 0xab, text).is_err());
    assert!(parse_blob_name(BlobKind::Message, 0xab, black_box("../bad.eml")).is_err());
}

fn store_manifests() {
    use td_crypto::Crypto;
    use td_mta::{
        format::{
            bindings::Selection,
            container::{Current, Error, JournalHeader, StoreIdentity},
            manifest::{self, Header, HistoryDescriptor, Manifest, TableDescriptor},
            table::TableHeader,
            table_stream::Verifier,
            Error as FormatError, Sequence, Table, MAX_MANIFEST_BYTES,
        },
        ids::{AccountId, InstanceId, StoreEpoch},
    };
    let crypto = td_crypto::Provider;
    let header = Header {
        account: AccountId::from_bytes([0x33; 16]),
        epoch: StoreEpoch::from_bytes([0x22; 16]),
        generation: 1,
        through: Sequence::from_u64(1),
        active_segment: 2,
    };
    let mut table_bytes = [0; 112];
    TableHeader {
        table: Table::Blobs,
        account: header.account,
        epoch: header.epoch,
        generation: header.generation,
        through: header.through,
        record_count: 0,
        payload_bytes: 0,
    }
    .encode(&crypto, &mut table_bytes)
    .unwrap();
    let summary = Verifier::new(&crypto, &table_bytes)
        .unwrap()
        .finish()
        .unwrap();
    let mut tables = std::array::from_fn(|i| TableDescriptor {
        table: Table::from_tag(u16::try_from(i + 1).unwrap()).unwrap(),
        record_count: 0,
        file_bytes: 112,
        digest: [0; 32],
    });
    tables.first_mut().unwrap().digest = summary.digest();
    let mut retained_bytes = [0; 228];
    JournalHeader {
        account: header.account,
        epoch: header.epoch,
        segment: 1,
        base: Sequence::default(),
    }
    .encode(&crypto, retained_bytes.get_mut(..96).unwrap())
    .unwrap();
    td_mta::format::operation::Operation::delete(Table::Blobs, &[0x44; 16])
        .unwrap()
        .encode(retained_bytes.get_mut(160..188).unwrap())
        .unwrap();
    td_mta::format::frame::seal(
        &crypto,
        header.through,
        1,
        black_box(retained_bytes.get_mut(96..).unwrap()),
    )
    .unwrap();
    let mut journal_stream =
        td_mta::format::journal_stream::Verifier::new(&crypto, retained_bytes.get(..96).unwrap())
            .unwrap();
    journal_stream
        .push(retained_bytes.get(96..).unwrap())
        .unwrap();
    let retained_summary = journal_stream.finish().unwrap();
    let history = [HistoryDescriptor {
        segment: 1,
        base: Sequence::default(),
        through: header.through,
        file_bytes: 228,
        digest: retained_summary.digest(),
    }];
    let mut bytes = [0; MAX_MANIFEST_BYTES];
    let n = manifest::encode(&crypto, header, &tables, &history, black_box(&mut bytes)).unwrap();
    let view = Manifest::decode(&crypto, bytes.get(..n).unwrap()).unwrap();
    assert_eq!(view.header(), header);
    assert_eq!(
        view.history(0),
        history.first().copied().ok_or(FormatError::InvalidValue)
    );
    assert_eq!(
        view.table(Table::Blobs),
        tables.first().copied().ok_or(FormatError::InvalidValue)
    );
    assert_eq!(view.history(1), Err(FormatError::InvalidValue));
    let mut format_bytes = [0; 80];
    StoreIdentity {
        instance: InstanceId::from_bytes([0x11; 16]),
        epoch: header.epoch,
    }
    .encode(&crypto, &mut format_bytes)
    .unwrap();
    let mut digest = crypto.sha256().unwrap();
    digest.update(bytes.get(..n).unwrap()).unwrap();
    let mut current_bytes = [0; 120];
    Current {
        account: header.account,
        epoch: header.epoch,
        generation: header.generation,
        manifest_digest: digest.finish().unwrap(),
    }
    .encode(&crypto, &mut current_bytes)
    .unwrap();
    let selection = Selection::decode(
        &crypto,
        header.account,
        &format_bytes,
        &current_bytes,
        bytes.get(..n).unwrap(),
    )
    .unwrap();
    assert_eq!(
        selection.check_table(&crypto, Table::Blobs, summary),
        Ok(())
    );
    assert_eq!(
        selection.check_table(&crypto, Table::Mailboxes, summary),
        Err(Error::Format(FormatError::InvalidValue))
    );
    let mut journal_bytes = [0; 96];
    let journal = JournalHeader {
        account: header.account,
        epoch: header.epoch,
        segment: header.active_segment,
        base: header.through,
    };
    journal.encode(&crypto, &mut journal_bytes).unwrap();
    assert_eq!(
        selection.check_active_header(&crypto, &journal_bytes),
        Ok(journal)
    );
    let active = td_mta::format::journal_stream::Verifier::new(&crypto, &journal_bytes)
        .unwrap()
        .finish()
        .unwrap();
    assert_eq!(
        selection.check_active_prefix(header.through, 96, active),
        Ok(())
    );
    assert_eq!(
        selection.check_active_prefix(header.through, 95, active),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(
        selection.check_history_journal(&crypto, 0, retained_summary),
        Ok(())
    );
    assert_eq!(
        selection.check_history_journal(&crypto, 1, retained_summary),
        Err(Error::Format(FormatError::InvalidValue))
    );
    let retained = JournalHeader {
        segment: 1,
        base: Sequence::default(),
        ..journal
    };
    retained.encode(&crypto, &mut journal_bytes).unwrap();
    assert_eq!(
        selection.check_history_header(&crypto, 0, &journal_bytes),
        Ok(retained)
    );
    assert_eq!(
        selection.check_history_header(&crypto, 1, &journal_bytes),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(
        manifest::encode(
            &crypto,
            header,
            &tables,
            &history,
            bytes.get_mut(..n - 1).unwrap()
        ),
        Err(Error::Format(FormatError::OutputFull))
    );
    assert_eq!(
        Manifest::decode(&crypto, bytes.get(..n - 1).unwrap()),
        Err(Error::Format(FormatError::Truncated))
    );
    *bytes.get_mut(n - 1).unwrap() ^= 1;
    assert_eq!(
        Manifest::decode(&crypto, bytes.get(..n).unwrap()),
        Err(Error::Checksum)
    );
}

fn tls_clients() {
    let mut samples = [COUNTERS.snapshot(); tls_allocation_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_allocation_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(samples.get(3), samples.get(4), "reservation allocated");
    assert_eq!(samples.get(7), samples.get(8), "capacity refusal allocated");
    assert_eq!(
        samples.get(9).unwrap().live,
        samples.get(10).unwrap().live,
        "warm construction retained Rust bytes"
    );
    for (phase, s) in tls_allocation_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-client-allocation-v1: rust passed");
}

#[path = "support/tls_handshake_scenario.rs"]
mod tls_handshake_scenario;

fn tls_handshake() {
    let mut samples = [COUNTERS.snapshot(); tls_handshake_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_handshake_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(
        samples.get(7).unwrap().live,
        samples.get(8).unwrap().live,
        "warm records retained Rust bytes"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .live
            .checked_sub(samples.get(11).unwrap().live),
        Some(4 * td_mta::tls_io::TLS_WIRE_BYTES),
        "returned wire buffers did not release their requested bytes"
    );
    for (phase, s) in tls_handshake_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-handshake {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-handshake-allocation-v2: rust passed");
}

fn tls_large_chain() {
    let mut samples = [COUNTERS.snapshot(); tls_handshake_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_handshake_scenario::run_large(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(
        samples.get(7).unwrap().live,
        samples.get(8).unwrap().live,
        "warm records retained Rust bytes"
    );
    assert_eq!(
        samples
            .get(10)
            .unwrap()
            .live
            .checked_sub(samples.get(11).unwrap().live),
        Some(4 * td_mta::tls_io::TLS_WIRE_BYTES),
        "returned wire buffers did not release their requested bytes"
    );
    for (phase, s) in tls_handshake_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-large-chain {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-large-chain-allocation-v2: rust passed");
}

#[path = "support/tls_fragment_scenario.rs"]
mod tls_fragment_scenario;

fn tls_fragments() {
    let mut samples = [COUNTERS.snapshot(); tls_fragment_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_fragment_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(
        samples.get(9).unwrap().live,
        samples.get(10).unwrap().live,
        "repeated refusals retained Rust bytes"
    );
    for (phase, s) in tls_fragment_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-fragment {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-fragment-allocation-v1: rust passed");
}

fn tls_certificate_list() {
    let mut samples = [COUNTERS.snapshot(); tls_fragment_scenario::CERTIFICATE_LIST_PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_fragment_scenario::run_certificate_list(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert!(
        samples
            .get(5)
            .unwrap()
            .peak
            .checked_sub(samples.get(4).unwrap().peak)
            .unwrap()
            >= 512 * 1024,
        "certificate list did not exercise decoded-entry allocation"
    );
    assert!(
        samples
            .get(9)
            .unwrap()
            .peak
            .checked_sub(samples.get(1).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES + td_mta::limits::TLS_HANDSHAKE_BYTES,
        "certificate-list processing exceeds planned requested-byte allowance"
    );
    assert_eq!(
        samples.get(8).unwrap().live,
        samples.get(9).unwrap().live,
        "repeated refusals retained Rust bytes"
    );
    for (phase, s) in tls_fragment_scenario::CERTIFICATE_LIST_PHASES
        .into_iter()
        .zip(samples)
    {
        println!(
            "tls-rust-certificate-list {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-certificate-list-allocation-v1: rust passed");
}

#[path = "support/entropy_worker_scenario.rs"]
mod entropy_worker_scenario;

fn entropy_workers() {
    let mut samples = [COUNTERS.snapshot(); entropy_worker_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    entropy_worker_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    for sample in &samples {
        assert!(!sample.invalid);
    }
    assert_eq!(samples.get(3), samples.get(4), "warm entropy allocated");
    for (phase, s) in entropy_worker_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-entropy {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-entropy-allocation-v1: rust passed");
}

fn store_directories() {
    use td_mta::{
        store_fs::{Directory, MAX_PATH_BYTES},
        store_paths::{Name, RootEntry},
    };
    let path = std::env::temp_dir().join(format!("td-mta-dir-alloc-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    std::fs::create_dir(path.join("accounts")).unwrap();
    let root = Directory::from_path(path.to_str().unwrap()).unwrap();
    // Exercise std's conversion at the service's full path bound, not just
    // common short deployment paths. Construct fixture paths before measuring.
    let prefix = path.join("x".repeat(160));
    std::fs::create_dir(&prefix).unwrap();
    let tail_length = MAX_PATH_BYTES
        .checked_sub(prefix.as_os_str().len())
        .and_then(|remaining| remaining.checked_sub(1))
        .filter(|length| (1..=255).contains(length))
        .expect("allocation fixture TMPDIR must leave a valid maximum-path component");
    let long = prefix.join("y".repeat(tail_length));
    std::fs::create_dir(&long).unwrap();
    let present = Name::root(RootEntry::Accounts).unwrap();
    let missing = Name::root(RootEntry::Lock).unwrap();
    let absent = long.with_file_name("z".repeat(long.file_name().unwrap().len()));
    let before = COUNTERS.snapshot();
    for _ in 0..64 {
        let directory = root.open(black_box(&present)).unwrap();
        assert!(directory.metadata().unwrap().is_dir());
        drop(directory);
        drop(Directory::from_path(black_box(long.to_str().unwrap())).unwrap());
        assert_eq!(
            Directory::from_path(black_box(absent.to_str().unwrap()))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            root.open(black_box(&missing)).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "std directory lookup allocated"
    );
    drop(root);
    std::fs::remove_dir_all(path).unwrap();
}

fn main() {
    allocation_counter::Counters::verify_model();
    forwarding();
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-clients")
    {
        tls_clients();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-handshake")
    {
        tls_handshake();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--entropy-workers")
    {
        entropy_workers();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-certificate-list")
    {
        tls_certificate_list();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-fragments")
    {
        tls_fragments();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-remote-chain")
    {
        tls_remote_chain();
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-generation-trust")
    {
        tls_generations(tls_generation_scenario::Scenario::Trust);
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-generation-routing")
    {
        tls_generations(tls_generation_scenario::Scenario::Routing);
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-generations")
    {
        tls_generations(tls_generation_scenario::Scenario::Ordinary);
        return;
    }
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--tls-large-chain")
    {
        tls_large_chain();
        return;
    }
    store_directories();
    hot_paths();
    println!("rust-allocation-probe-v1: counter-model forwarding hot-paths passed");
}

#[path = "support/tls_generation_scenario.rs"]
mod tls_generation_scenario;

fn tls_generations(scenario: tls_generation_scenario::Scenario) {
    let label = scenario.label();
    let mut samples = [COUNTERS.snapshot(); tls_generation_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_generation_scenario::run(scenario, || *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|s| !s.invalid));
    let retained = samples
        .get(4)
        .unwrap()
        .live
        .checked_sub(samples.get(3).unwrap().live)
        .unwrap();
    assert!(
        samples
            .get(3)
            .unwrap()
            .live
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_GENERATION_BYTES,
        "first generation exceeds planned requested-byte allowance"
    );
    assert!(
        retained <= td_mta::limits::TLS_GENERATION_BYTES,
        "candidate exceeds planned requested-byte allowance"
    );
    assert!(
        samples
            .get(8)
            .unwrap()
            .peak
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= 2 * td_mta::limits::TLS_GENERATION_BYTES,
        "generation overlap exceeds planned requested-byte allowance"
    );
    for (peak, baseline) in [(3, 2), (4, 3), (8, 3)] {
        assert!(
            samples
                .get(peak)
                .unwrap()
                .peak
                .checked_sub(samples.get(baseline).unwrap().live)
                .unwrap()
                <= td_mta::limits::TLS_GENERATION_BYTES,
            "generation construction exceeds its own planned allowance"
        );
    }
    if scenario == tls_generation_scenario::Scenario::Routing {
        assert!(
            retained <= 1024 * 1024,
            "large routing candidate exceeds retained-byte fixture allowance"
        );
    }
    assert_eq!(
        samples.get(5),
        samples.get(6),
        "generation refusal allocated"
    );
    assert_eq!(
        samples.get(3).unwrap().live,
        samples.get(7).unwrap().live,
        "old generation release retained Rust bytes"
    );
    assert_eq!(
        samples.get(7).unwrap().live,
        samples.get(8).unwrap().live,
        "replacement retained Rust bytes"
    );
    for (phase, s) in tls_generation_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-{label} {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-{label}-allocation-v1: rust passed");
}

#[path = "support/tls_remote_chain_scenario.rs"]
mod tls_remote_chain_scenario;

fn tls_remote_chain() {
    let mut samples = [COUNTERS.snapshot(); tls_remote_chain_scenario::PHASES.len()];
    let mut slots = samples.iter_mut();
    tls_remote_chain_scenario::run(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|s| !s.invalid));
    assert_eq!(
        samples.get(6).unwrap().live,
        samples.get(7).unwrap().live,
        "remote records retained Rust bytes"
    );
    assert_eq!(
        samples
            .get(8)
            .unwrap()
            .live
            .checked_sub(samples.get(9).unwrap().live),
        Some(2 * td_mta::tls_io::TLS_WIRE_BYTES)
    );
    assert!(
        samples
            .get(6)
            .unwrap()
            .live
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES,
        "remote session exceeds planned requested-byte allowance"
    );
    let processing =
        td_mta::limits::TLS_HANDSHAKE_BYTES.max(td_mta::limits::TLS_ESTABLISHED_PROCESSING_BYTES);
    assert!(
        samples
            .get(7)
            .unwrap()
            .peak
            .checked_sub(samples.get(2).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_SESSION_BYTES + processing,
        "remote processing exceeds planned requested-byte allowance"
    );
    assert!(
        samples
            .get(7)
            .unwrap()
            .peak
            .checked_sub(samples.get(5).unwrap().live)
            .unwrap()
            <= td_mta::limits::TLS_ESTABLISHED_PROCESSING_BYTES,
        "established processing exceeds its own planned allowance"
    );
    let scenario = tls_remote_chain_scenario::label();
    for (phase, s) in tls_remote_chain_scenario::PHASES.into_iter().zip(samples) {
        println!(
            "tls-rust-{scenario} {phase} {} {} {} {} {} {} {}",
            s.alloc, s.zeroed, s.realloc, s.free, s.failed, s.live, s.peak
        );
    }
    println!("tls-{scenario}-allocation-v1: rust passed");
}

fn store_complete_frames() {
    use td_mta::{
        format::{
            container::JournalHeader,
            frame::{seal, Frame},
            journal_stream::Verifier,
            operation::Operation,
            Sequence, Table,
        },
        ids::{AccountId, StoreEpoch},
    };
    let crypto = td_crypto::Provider;
    let mut bytes = [0; 132];
    Operation::delete(Table::Blobs, &[0x44; 16])
        .unwrap()
        .encode(bytes.get_mut(64..92).unwrap())
        .unwrap();
    seal(&crypto, Sequence::from_u64(1), 1, black_box(&mut bytes)).unwrap();
    let frame = Frame::decode(&crypto, Sequence::default(), black_box(&bytes)).unwrap();
    let mut count = 0;
    for entry in frame.operations() {
        assert_eq!(entry.unwrap().ordinal, count);
        count += 1;
    }
    assert_eq!(count, 1);
    let header = JournalHeader {
        account: AccountId::from_bytes([0x33; 16]),
        epoch: StoreEpoch::from_bytes([0x22; 16]),
        segment: 1,
        base: Sequence::default(),
    };
    let mut header_bytes = [0; 96];
    header
        .encode(&crypto, black_box(&mut header_bytes))
        .unwrap();
    let mut stream = Verifier::new(&crypto, &header_bytes).unwrap();
    stream.push(&bytes).unwrap();
    assert_eq!(stream.finish().unwrap().operations(), 1);
    let mut stream = Verifier::new(&crypto, &header_bytes).unwrap();
    assert!(stream.push(bytes.get(..131).unwrap()).is_err());
    assert!(stream.push(&bytes).is_err());
    assert!(stream.finish().is_err());
}
