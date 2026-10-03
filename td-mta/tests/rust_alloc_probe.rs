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

// Compile filesystem and checker sources with their cfg(test) fixtures.
// Filesystem fixtures need no production exception for the mapped test identity.
use td_mta::{
    bounded, change_cursor, config, format, frame_changes, ids, limits, merge, overlay, ownership,
    ports, store_paths,
};
#[path = "../src/admission.rs"]
#[allow(unused)] // Keep the private append guard in this measured source compilation.
mod admission;
#[path = "../src/row_references.rs"]
#[allow(unused)]
mod measured_row_references;
use td_mta::{mailbox_parents, mailbox_sweep, recipient_sweep, reference_sweep, row_references};
#[path = "../src/mailbox_sweep.rs"]
#[allow(unused)]
mod measured_mailbox_sweep;
#[path = "../src/recipient_sweep.rs"]
#[allow(unused)]
mod measured_recipient_sweep;
#[path = "../src/reference_sweep.rs"]
#[allow(unused)]
mod measured_reference_sweep;
#[path = "../src/store_fs.rs"]
#[allow(unused)] // Second compilation; the library build remains the lint authority.
pub mod measured_store_fs;

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
        measured_row_references::tests::probe();
        measured_reference_sweep::tests::probe();
        measured_recipient_sweep::tests::probe();
        measured_mailbox_sweep::tests::probe();
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
    let bound_view = td_mta::ports::ViewIdentity {
        account: header.account,
        epoch: header.epoch,
        generation: header.generation,
        checkpoint: header.through,
        segment: header.active_segment,
        committed_offset: 228,
        committed_sequence: header.through.successor().unwrap(),
        history_floor: Sequence::default(),
    };
    let route = td_mta::store_fs::ChangeRoute::new(selection, bound_view).unwrap();
    assert_eq!(
        route.source(bound_view, header.through),
        Ok(td_mta::store_fs::ChangeSource::History { index: 0 })
    );
    assert_eq!(
        route.source(bound_view, bound_view.committed_sequence),
        Ok(td_mta::store_fs::ChangeSource::Active)
    );
    assert_eq!(
        route.source(bound_view, Sequence::default()),
        Err(td_mta::ports::Error::HistoryLost)
    );
    let mut changed = bound_view;
    changed.generation += 1;
    assert_eq!(
        route.source(changed, header.through),
        Err(td_mta::ports::Error::Conflict)
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

fn store_temporary_files() {
    let mut samples = [COUNTERS.snapshot(); 2];
    let mut slots = samples.iter_mut();
    measured_store_fs::probe_temporary_io(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    assert_eq!(
        samples.first(),
        samples.get(1),
        "std temporary I/O allocated"
    );
}

fn store_verify_account() {
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    measured_store_fs::probe_verify_account(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "stopped account verification allocated");
    }
}

fn store_reserved_append() {
    let mut samples = [COUNTERS.snapshot(); 32];
    let mut slots = samples.iter_mut();
    measured_store_fs::probe_reserved_append(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "reservation-bound journal append allocated");
    }
}

fn store_journal_publication() {
    let mut samples = [COUNTERS.snapshot(); 32];
    let mut slots = samples.iter_mut();
    measured_store_fs::probe_journal_publication(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "scoped journal publication allocated");
    }
}

fn store_pinned_reads() {
    let mut samples = [COUNTERS.snapshot(); 32];
    let mut slots = samples.iter_mut();
    measured_store_fs::probe_pinned_reads(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "pinned read scope allocated");
    }
}

fn store_read_pool() {
    let mut samples = [COUNTERS.snapshot(); 32];
    let mut slots = samples.iter_mut();
    measured_store_fs::probe_read_pool(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "pooled read scope allocated");
    }
}

fn header_raw() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_raw::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    fn budget(records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100,
                records,
                ..Charge::default()
            },
        )
    }
    let before = COUNTERS.snapshot();
    let mut cursor = Cursor::new(b" a\r\n\t\0\xff\xef\xbf\xbe");
    let saved = cursor;
    let expected = [' ', 'a', '\r', '\n', '\t', '�', '�'];
    let mut meter = budget(100);
    for _ in 0..2 {
        let mut written = 0;
        let mut done = false;
        for _ in 0..30 {
            match cursor.poll(Tick(1), &mut meter).unwrap() {
                Status::Scalar(value) => {
                    assert_eq!(Some(&value), expected.get(written));
                    written += 1;
                }
                Status::Yield => {}
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        assert_eq!(written, expected.len());
        assert!(cursor.is_encoding_problem());
        assert_eq!(
            cursor.poll(Tick(100), &mut meter).unwrap(),
            Status::Complete
        );
        cursor = saved;
    }
    let mut cursor = Cursor::new(b"a");
    assert!(matches!(
        cursor.poll(Tick(1), &mut budget(0)),
        Err(Error::Work(_))
    ));
    let mut fresh = budget(10);
    let remaining = fresh.remaining();
    assert!(matches!(
        cursor.poll(Tick(1), &mut fresh),
        Err(Error::Work(_))
    ));
    assert_eq!(fresh.remaining(), remaining);
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "Raw header projection allocated"
    );
}

fn mime_unfold() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_unfold::{Decoder, Status},
        ports::{Deadline, Tick},
    };
    fn budget(io: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                output_bytes: 100,
                ..Charge::default()
            },
        )
    }
    let before = COUNTERS.snapshot();
    let mut decoder = Decoder::default();
    let mut meter = budget(100);
    let input = b" a\r\n\tb\rx\r\ny";
    let expected = b" a\tb\rx\r\ny";
    let mut source = 0;
    let mut written = 0;
    let mut done = false;
    for _ in 0..100 {
        let end = (source + 1).min(input.len());
        let mut output = [0; 1];
        let step = decoder
            .poll(
                input.get(source..end).unwrap(),
                &mut output,
                end == input.len(),
                Tick(1),
                &mut meter,
            )
            .unwrap();
        source += step.consumed;
        if step.written == 1 {
            assert_eq!(output.first(), expected.get(written));
        }
        written += step.written;
        if step.status == Status::Complete {
            done = true;
            break;
        }
    }
    assert!(done);
    assert_eq!(written, expected.len());
    assert_eq!(
        decoder
            .poll(b"ignored", &mut [], true, Tick(100), &mut meter)
            .unwrap()
            .status,
        Status::Complete
    );
    let mut decoder = Decoder::default();
    assert_eq!(
        decoder.poll(b"a", &mut [0; 1], true, Tick(1), &mut budget(0)),
        Err(Stop::IoBytes)
    );
    let mut fresh = budget(100);
    let remaining = fresh.remaining();
    assert_eq!(
        decoder.poll(b"a", &mut [0; 1], true, Tick(1), &mut fresh),
        Err(Stop::IoBytes)
    );
    assert_eq!(fresh.remaining(), remaining);
    assert_eq!(COUNTERS.snapshot(), before, "header unfolding allocated");
}

fn mime_checkpoints() {
    use td_mta::{
        admission::work::{Charge, Meter},
        mime_base64::Status,
        mime_input::{Checkpoints, Error, Reader, CHECKPOINTS},
        ports::{BlobReader, Clock, Deadline, Error as PolicyError, Tick, Time},
        wire::TransferEncoding,
    };
    struct Source;
    impl BlobReader for Source {
        fn len(&self) -> u64 {
            12
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            let input = b"xxTWFuTWFuyy"
                .get(offset as usize..)
                .ok_or(PolicyError::Invalid)?;
            let count = input.len().min(output.len());
            output
                .get_mut(..count)
                .unwrap()
                .copy_from_slice(input.get(..count).unwrap());
            Ok(count)
        }
    }
    struct GoodClock;
    impl Clock for GoodClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    fn budget() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1000,
                output_bytes: 1000,
                records: 100,
                ..Charge::default()
            },
        )
    }
    fn drain(reader: &mut Reader<'_, '_>, meter: &mut Meter, expected: &[u8]) {
        let mut position = 0;
        for _ in 0..20 {
            let mut output = [0; 1];
            let step = reader.poll(&GoodClock, meter, &mut output).unwrap();
            if step.written == 1 {
                assert_eq!(output.first(), expected.get(position));
            }
            position += step.written;
            if step.status == Status::Complete {
                assert_eq!(position, expected.len());
                return;
            }
        }
        panic!("checkpoint replay did not complete");
    }
    let before = COUNTERS.snapshot();
    let mut source = Source;
    let mut backing = [0; 8];
    let mut slots = Checkpoints::default();
    let mut reader = Reader::with_checkpoints(
        &mut source,
        2,
        8,
        TransferEncoding::Base64,
        &mut backing,
        &mut slots,
    )
    .unwrap();
    let mut meter = budget();
    reader.save_checkpoint(0, &GoodClock, &mut meter).unwrap();
    reader.poll(&GoodClock, &mut meter, &mut []).unwrap();
    let mut first = [0; 1];
    assert_eq!(
        reader
            .poll(&GoodClock, &mut meter, &mut first)
            .unwrap()
            .written,
        1
    );
    assert_eq!(first, *b"M");
    reader.save_checkpoint(1, &GoodClock, &mut meter).unwrap();
    reader
        .restore_checkpoint(1, &GoodClock, &mut meter)
        .unwrap();
    drain(&mut reader, &mut meter, b"anMan");
    reader.save_checkpoint(2, &GoodClock, &mut meter).unwrap();
    reader
        .restore_checkpoint(1, &GoodClock, &mut meter)
        .unwrap();
    drain(&mut reader, &mut meter, b"anMan");
    reader
        .restore_checkpoint(0, &GoodClock, &mut meter)
        .unwrap();
    drain(&mut reader, &mut meter, b"ManMan");
    reader
        .restore_checkpoint(2, &GoodClock, &mut meter)
        .unwrap();
    drain(&mut reader, &mut meter, b"");
    assert_eq!(
        reader.restore_checkpoint(CHECKPOINTS, &GoodClock, &mut meter),
        Err(Error::InvalidCheckpoint)
    );
    let mut fresh = budget();
    let remaining = fresh.remaining();
    assert_eq!(
        reader.restore_checkpoint(0, &GoodClock, &mut fresh),
        Err(Error::InvalidCheckpoint)
    );
    assert_eq!(fresh.remaining(), remaining);
    assert_eq!(COUNTERS.snapshot(), before, "transfer checkpoint allocated");
}

fn body_value() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        body_value::{Error, Plain, Status},
        ports::{Deadline, Tick},
    };
    fn budget(records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                records,
                output_bytes: 1000,
                ..Charge::default()
            },
        )
    }
    let before = COUNTERS.snapshot();
    for cap in [0, 3] {
        let input = ['a', '\r', '\n', 'é', '\u{ffff}'];
        let full = ['a', '\n', 'é', '\u{fffd}'];
        let short = ['a', '\n'];
        let expected = if cap == 0 {
            full.as_slice()
        } else {
            short.as_slice()
        };
        let mut value = Plain::new(cap);
        let mut work = budget(1000);
        let mut pos = 0;
        let mut written = 0;
        let mut done = false;
        for _ in 0..30 {
            let step = value
                .poll(
                    input.get(pos).copied(),
                    pos == input.len(),
                    Tick(1),
                    &mut work,
                )
                .unwrap();
            if step.consumed {
                pos += 1;
            }
            match step.status {
                Status::Scalar(c) => {
                    assert_eq!(Some(&c), expected.get(written));
                    written += 1;
                }
                Status::Yield | Status::NeedInput => {}
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        assert_eq!(pos, input.len());
        assert_eq!(written, expected.len());
        assert!(value.is_encoding_problem());
        assert_eq!(value.is_truncated(), cap != 0);
        let remaining = work.remaining();
        assert_eq!(
            value
                .poll(Some('x'), true, Tick(100), &mut work)
                .unwrap()
                .status,
            Status::Complete
        );
        assert_eq!(work.remaining(), remaining);
    }
    let mut value = Plain::new(0);
    let mut work = budget(100);
    value.poll(Some('\r'), false, Tick(1), &mut work).unwrap();
    let saved = value;
    for _ in 0..2 {
        assert_eq!(
            value
                .poll(Some('a'), true, Tick(1), &mut work)
                .unwrap()
                .status,
            Status::Scalar('\r')
        );
        value = saved;
    }
    let mut value = Plain::new(0);
    assert_eq!(
        value.poll(Some('a'), true, Tick(1), &mut budget(0)),
        Err(Error::Work(Stop::Records))
    );
    let mut fresh = budget(100);
    let remaining = fresh.remaining();
    assert_eq!(
        value.poll(None, true, Tick(1), &mut fresh),
        Err(Error::Work(Stop::Records))
    );
    assert_eq!(fresh.remaining(), remaining);
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "plain body value filtering allocated"
    );
}

fn mime_text() {
    use td_mta::{
        admission::work::{Charge, Meter},
        body_charset::Plan,
        mime_input::Checkpoints,
        mime_text::{Input, Reader, Status},
        ports::{BlobReader, Clock, Deadline, Error, Tick, Time},
        wire::TransferEncoding,
    };
    struct Source {
        reads: usize,
        fail: usize,
    }
    impl BlobReader for Source {
        fn len(&self) -> u64 {
            9
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, Error> {
            self.reads += 1;
            if self.reads == self.fail {
                return Err(Error::Busy);
            }
            let bytes = b"xxYQ==!yy".get(offset as usize..).ok_or(Error::Invalid)?;
            let n = bytes.len().min(output.len());
            output
                .get_mut(..n)
                .unwrap()
                .copy_from_slice(bytes.get(..n).unwrap());
            Ok(n)
        }
    }
    struct GoodClock;
    impl Clock for GoodClock {
        fn sample(&self) -> Result<Time, Error> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    fn budget(records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10000,
                records,
                output_bytes: 10000,
                ..Charge::default()
            },
        )
    }
    let before = COUNTERS.snapshot();
    for scenario in 0..3 {
        let mut source = Source {
            reads: 0,
            fail: if scenario == 1 { 4 } else { usize::MAX },
        };
        let mut bytes = [0; 2];
        let mut checkpoints = Checkpoints::default();
        let mut reader = Reader::new(
            Input {
                source: &mut source,
                offset: 2,
                length: 5,
                encoding: TransferEncoding::Base64,
                charset: Plan::Prescan,
            },
            &mut bytes,
            &mut checkpoints,
        )
        .unwrap();
        let mut work = budget(if scenario == 2 { 0 } else { 10000 });
        let mut written = 0;
        let mut done = false;
        for _ in 0..100 {
            let step = reader.poll(&GoodClock, &mut work);
            if reader.selection().is_some() {
                assert!(!reader.selection().unwrap().is_encoding_problem);
                assert!(reader.is_encoding_problem());
            }
            match step {
                Ok(Status::Scalar(c)) => {
                    assert_eq!(c, 'a');
                    written += 1;
                }
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => {
                    assert_eq!(scenario, 0);
                    assert_eq!(written, 1);
                    assert!(reader.is_encoding_problem());
                    let remaining = work.remaining();
                    assert_eq!(
                        reader.poll(&GoodClock, &mut work).unwrap(),
                        Status::Complete
                    );
                    assert_eq!(work.remaining(), remaining);
                    done = true;
                    break;
                }
                Err(error) => {
                    assert_ne!(scenario, 0);
                    assert_eq!(written, 0);
                    let mut fresh = budget(10000);
                    let remaining = fresh.remaining();
                    assert_eq!(reader.poll(&GoodClock, &mut fresh), Err(error));
                    assert_eq!(fresh.remaining(), remaining);
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "transfer/charset replay allocated"
    );
}

fn body_charset() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        body_charset::{Error, Plan, Prescan, Selection, Status},
        mime_charset::Charset,
        ports::{Deadline, Tick},
    };
    fn budget(records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100,
                records,
                ..Charge::default()
            },
        )
    }
    let before = COUNTERS.snapshot();
    assert_eq!(Plan::from_label(None), Plan::Prescan);
    assert_eq!(
        Plan::from_label(Some(b"unknown")),
        Plan::Selected(Selection {
            charset: Charset::Utf8,
            is_encoding_problem: true
        })
    );
    for (source, charset, problem) in [
        (b"ascii".as_slice(), Charset::Ascii, false),
        (b"caf\xc3\xa9", Charset::Utf8, true),
        (b"caf\xc3\xa9\xff", Charset::Ascii, true),
    ] {
        let mut scan = Prescan::default();
        let mut work = budget(100);
        let prefix = source.len() - 1;
        for byte in source.get(..prefix).unwrap() {
            let step = scan
                .poll(std::slice::from_ref(byte), false, Tick(1), &mut work)
                .unwrap();
            assert_eq!(step.consumed, 1);
        }
        let saved = scan;
        for _ in 0..2 {
            let mut pos = prefix;
            let mut done = false;
            for _ in 0..100 {
                let end = (pos + 1).min(source.len());
                let step = scan
                    .poll(
                        source.get(pos..end).unwrap(),
                        end == source.len(),
                        Tick(1),
                        &mut work,
                    )
                    .unwrap();
                pos += step.consumed;
                if let Status::Complete(selection) = step.status {
                    assert_eq!(
                        selection,
                        Selection {
                            charset,
                            is_encoding_problem: problem
                        }
                    );
                    assert_eq!(pos, source.len());
                    let remaining = work.remaining();
                    assert_eq!(
                        scan.poll(b"ignored", true, Tick(100), &mut work)
                            .unwrap()
                            .status,
                        step.status
                    );
                    assert_eq!(work.remaining(), remaining);
                    done = true;
                    break;
                }
            }
            assert!(done);
            scan = saved;
        }
    }
    let mut scan = Prescan::default();
    assert_eq!(
        scan.poll(b"a", true, Tick(1), &mut budget(0)),
        Err(Error::Work(Stop::Records))
    );
    let mut fresh = budget(100);
    let remaining = fresh.remaining();
    assert_eq!(
        scan.poll(b"", true, Tick(1), &mut fresh),
        Err(Error::Work(Stop::Records))
    );
    assert_eq!(fresh.remaining(), remaining);
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "body charset prescan allocated"
    );
}

fn mime_charset() {
    use td_mta::{
        admission::work::{Charge, Meter},
        mime_charset::{Charset, Decoder, Error, Status},
        ports::{Deadline, Tick},
    };
    let before = COUNTERS.snapshot();
    for (charset, bytes, expected) in [
        (
            Charset::Utf8,
            b"\xf0\x90\x80a".as_slice(),
            ['�', 'a'].as_slice(),
        ),
        (Charset::Ascii, b"\xffa".as_slice(), ['�', 'a'].as_slice()),
        (
            Charset::Latin1,
            b"\x80\xff".as_slice(),
            ['\u{80}', 'ÿ'].as_slice(),
        ),
        (
            Charset::Windows1252,
            b"\x80\x81".as_slice(),
            ['€', '�'].as_slice(),
        ),
    ] {
        let mut decoder = Decoder::new(charset);
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100,
                records: 100,
                ..Charge::default()
            },
        );
        let mut source = 0;
        let mut output = ['\0'; 2];
        let mut used = 0;
        let mut done = false;
        for _ in 0..20 {
            let end = (source + 1).min(bytes.len());
            let step = decoder
                .poll(
                    bytes.get(source..end).unwrap(),
                    end == bytes.len(),
                    Tick(1),
                    &mut meter,
                )
                .unwrap();
            source += step.consumed;
            match step.status {
                Status::Scalar(value) => {
                    *output.get_mut(used).unwrap() = value;
                    used += 1;
                }
                Status::Complete => {
                    done = true;
                    break;
                }
                Status::NeedInput => {}
            }
        }
        assert!(done);
        assert_eq!(output, expected);
        assert_eq!(used, 2);
        assert_eq!(decoder.is_encoding_problem(), charset != Charset::Latin1);
        assert_eq!(
            decoder
                .poll(b"ignored", true, Tick(100), &mut meter)
                .unwrap()
                .status,
            Status::Complete
        );
        let mut refused = Decoder::new(charset);
        let mut meter = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        assert!(matches!(
            refused.poll(b"a", true, Tick(1), &mut meter),
            Err(Error::Work(_))
        ));
        assert!(matches!(
            refused.poll(b"", true, Tick(100), &mut meter),
            Err(Error::Work(_))
        ));
        let mut fresh = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10,
                records: 10,
                ..Charge::default()
            },
        );
        let remaining = fresh.remaining();
        assert!(matches!(
            refused.poll(b"a", true, Tick(1), &mut fresh),
            Err(Error::Work(_))
        ));
        assert_eq!(fresh.remaining(), remaining);
    }
    assert_eq!(COUNTERS.snapshot(), before, "charset decoder allocated");
}

fn mime_headers() {
    use td_mta::{
        admission::work::{Charge, Meter},
        mime_headers::{Scanner, Status},
        ports::{Deadline, Tick},
    };
    let before = COUNTERS.snapshot();
    let bytes = b"X: a\r\n\tmore\n\nbody";
    let mut scanner = Scanner::new(0, 12);
    let mut meter = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100,
            records: 1,
            ..Charge::default()
        },
    );
    let mut offset = 0;
    let mut fields = 0;
    let mut done = false;
    for _ in 0..100 {
        let end = (offset + 1).min(bytes.len());
        let step = scanner
            .poll(
                bytes.get(offset..end).unwrap(),
                end == bytes.len(),
                Tick(1),
                &mut meter,
            )
            .unwrap();
        offset += step.consumed;
        match step.status {
            Status::Field(field) => {
                fields += 1;
                assert_eq!((field.value_start, field.value_end), (2, 11));
            }
            Status::Complete(end) => {
                assert_eq!((end.body_start, end.header_bytes), (13, 12));
                done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(done);
    assert_eq!(fields, 1);
    let mut refused = Scanner::new(0, 0);
    assert_eq!(
        refused.poll(b"X:", true, Tick(1), &mut meter),
        Err(td_mta::mime_headers::Error::HeaderLimit)
    );
    assert_eq!(
        refused.poll(b"", true, Tick(100), &mut meter),
        Err(td_mta::mime_headers::Error::HeaderLimit)
    );
    assert_eq!(COUNTERS.snapshot(), before, "header scanner allocated");
}

fn mime_input() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_base64::Status,
        mime_input::{Error, Reader},
        ports::{BlobReader, Clock, Deadline, Error as PolicyError, Tick, Time},
        wire::TransferEncoding,
    };
    struct Source;
    impl BlobReader for Source {
        fn len(&self) -> u64 {
            10
        }
        fn read_at(&mut self, offset: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            let input = b"xxT!Q==Zyy"
                .get(offset as usize..)
                .ok_or(PolicyError::Invalid)?;
            let count = input.len().min(output.len());
            output
                .get_mut(..count)
                .unwrap()
                .copy_from_slice(input.get(..count).unwrap());
            Ok(count)
        }
    }
    struct TimeSource;
    impl Clock for TimeSource {
        fn sample(&self) -> Result<Time, PolicyError> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(2),
            })
        }
    }
    let before = COUNTERS.snapshot();
    for encoding in [TransferEncoding::Identity, TransferEncoding::Base64] {
        let mut source = Source;
        let mut buffer = [0; 2];
        let mut reader = Reader::new(&mut source, 2, 6, encoding, &mut buffer).unwrap();
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 32,
                output_bytes: 8,
                ..Charge::default()
            },
        );
        let expected = if encoding == TransferEncoding::Identity {
            b"T!Q==Z".as_slice()
        } else {
            b"M".as_slice()
        };
        let mut at = 0;
        let mut complete = false;
        for _ in 0..20 {
            let mut output = [0; 1];
            let step = reader
                .poll(black_box(&TimeSource), &mut meter, &mut output)
                .unwrap();
            if step.written != 0 {
                assert_eq!(output.first(), expected.get(at));
                at += step.written;
            }
            if step.status == Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert_eq!(at, expected.len());
        assert_eq!(reader.position(), expected.len() as u64);
        assert_eq!(
            reader.is_encoding_problem(),
            encoding == TransferEncoding::Base64
        );
        assert_eq!(
            meter.charge(Tick(100), Charge::default()),
            Err(Stop::Deadline)
        );
        assert_eq!(
            reader
                .poll(&TimeSource, &mut meter, &mut [])
                .unwrap()
                .status,
            Status::Complete
        );
    }
    let mut source = Source;
    let mut buffer = [0; 2];
    let mut reader = Reader::new(&mut source, 2, 6, TransferEncoding::Base64, &mut buffer).unwrap();
    let mut meter = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    assert_eq!(
        reader.poll(&TimeSource, &mut meter, &mut []),
        Err(Error::Work(Stop::IoBytes))
    );
    assert_eq!(
        reader.poll(&TimeSource, &mut meter, &mut []),
        Err(Error::Work(Stop::IoBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME source input allocated");
}

fn mime_qp_input() {
    use td_mta::{
        admission::work::{Charge, Meter},
        body_charset::Plan,
        mime_base64::Status,
        mime_input::{Checkpoints, Error, Reader},
        mime_text::{Input, Reader as TextReader, Status as Text},
        ports::{BlobReader, Clock, Deadline, Error as PolicyError, Tick, Time},
        wire::TransferEncoding,
    };
    struct Source {
        calls: usize,
        fail: bool,
    }
    impl BlobReader for Source {
        fn len(&self) -> u64 {
            9
        }
        fn read_at(&mut self, at: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            self.calls += 1;
            if self.fail && self.calls == 3 {
                return Err(PolicyError::Busy);
            }
            assert!((2..7).contains(&at));
            assert!(output.len() as u64 <= 7 - at);
            let input = b"xxa \tb=yy".get(at as usize..).unwrap();
            let count = output.len().min(input.len());
            output
                .get_mut(..count)
                .unwrap()
                .copy_from_slice(input.get(..count).unwrap());
            Ok(count)
        }
    }
    struct GoodClock;
    impl Clock for GoodClock {
        fn sample(&self) -> Result<Time, PolicyError> {
            Ok(Time {
                utc_ms: 0,
                monotonic: Tick(1),
            })
        }
    }
    fn budget() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10000,
                records: 10000,
                output_bytes: 10000,
                ..Charge::default()
            },
        )
    }
    let before = COUNTERS.snapshot();
    for fail in [false, true] {
        let mut source = Source { calls: 0, fail };
        let mut bytes = [0; 2];
        let mut slots = Checkpoints::default();
        let mut reader = Reader::with_checkpoints(
            &mut source,
            2,
            5,
            TransferEncoding::QuotedPrintable,
            &mut bytes,
            &mut slots,
        )
        .unwrap();
        let mut work = budget();
        reader.save_checkpoint(0, &GoodClock, &mut work).unwrap();
        for replay in 0..2 {
            if replay != 0 {
                reader.restore_checkpoint(0, &GoodClock, &mut work).unwrap();
            }
            let mut written = 0;
            let mut done = false;
            for _ in 0..100 {
                let mut byte = [0; 1];
                match reader.poll(&GoodClock, &mut work, &mut byte) {
                    Ok(step) => {
                        if step.written != 0 {
                            assert_eq!(byte.first(), b"a \tb".get(written));
                            written += 1;
                        }
                        if step.status == Status::Complete {
                            done = true;
                            break;
                        }
                    }
                    Err(error) => {
                        assert!(fail);
                        assert_eq!(error, Error::Policy(PolicyError::Busy));
                        let mut fresh = budget();
                        let remaining = fresh.remaining();
                        assert_eq!(reader.poll(&GoodClock, &mut fresh, &mut byte), Err(error));
                        assert_eq!(fresh.remaining(), remaining);
                        break;
                    }
                }
            }
            if fail {
                assert_eq!(reader.failure(), Some(Error::Policy(PolicyError::Busy)));
                break;
            }
            assert!(done);
            assert_eq!(written, 4);
            assert!(reader.is_encoding_problem());
        }
        assert_eq!(source.calls, if fail { 3 } else { 8 });
    }
    let mut source = Source {
        calls: 0,
        fail: false,
    };
    let mut bytes = [0; 2];
    let mut slots = Checkpoints::default();
    let mut text = TextReader::new(
        Input {
            source: &mut source,
            offset: 2,
            length: 5,
            encoding: TransferEncoding::QuotedPrintable,
            charset: Plan::Prescan,
        },
        &mut bytes,
        &mut slots,
    )
    .unwrap();
    let mut work = budget();
    let mut written = 0;
    let mut done = false;
    for _ in 0..200 {
        match text.poll(&GoodClock, &mut work).unwrap() {
            Text::Scalar(c) => {
                assert_eq!(u32::from(c), u32::from(*b"a \tb".get(written).unwrap()));
                written += 1;
            }
            Text::Yield => {}
            Text::Complete => {
                done = true;
                break;
            }
        }
    }
    assert!(done);
    assert_eq!(written, 4);
    assert!(text.is_encoding_problem());
    assert!(!text.selection().unwrap().is_encoding_problem);
    assert_eq!(source.calls, 8);
    struct Resident {
        reads: usize,
    }
    impl BlobReader for Resident {
        fn len(&self) -> u64 {
            305
        }
        fn read_at(&mut self, at: u64, output: &mut [u8]) -> Result<usize, PolicyError> {
            assert!(at >= 2 && at + output.len() as u64 <= 303);
            self.reads += 1;
            for (i, byte) in output.iter_mut().enumerate() {
                *byte = if at + i as u64 == 302 { b'x' } else { b' ' };
            }
            Ok(output.len())
        }
    }
    let mut resident = Resident { reads: 0 };
    let mut buffer = [0; 512];
    let mut reader = Reader::new(
        &mut resident,
        2,
        301,
        TransferEncoding::QuotedPrintable,
        &mut buffer,
    )
    .unwrap();
    let mut work = budget();
    let mut at = 0;
    let mut done = false;
    for _ in 0..1000 {
        let mut output = [0; 1];
        let step = reader.poll(&GoodClock, &mut work, &mut output).unwrap();
        if step.written != 0 {
            assert_eq!(output, [if at == 300 { b'x' } else { b' ' }]);
            at += 1;
        }
        if step.status == Status::Complete {
            done = true;
            break;
        }
    }
    assert!(done);
    assert_eq!(at, 301);
    assert_eq!(resident.reads, 1);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "owned QP source/text allocated");
}

fn encoded_word_candidates() {
    use td_mta::{
        admission::work::{Charge, Meter},
        encoded_word::{Context, Encoding, Word},
        mime_charset::Charset,
        ports::{Deadline, Tick},
    };
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10000,
            records: 10000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    let word = Word::recognize(
        black_box(b"=?UTF-8*en?Q?hello_world?="),
        Context::Phrase,
        Tick(1),
        &mut work,
    )
    .unwrap()
    .unwrap();
    assert_eq!(word.payload(), b"hello_world");
    assert_eq!(word.language(), Some(b"en".as_slice()));
    assert_eq!(word.charset(), Charset::Utf8);
    assert_eq!(word.encoding(), Encoding::Q);
    assert_eq!(
        Word::recognize(
            black_box(b"=?utf-8?Q?(a)?="),
            Context::Comment,
            Tick(1),
            &mut work
        )
        .unwrap(),
        None
    );
    assert!(Word::recognize(
        black_box(b"=?utf-8?B?--?="),
        Context::Text,
        Tick(1),
        &mut work
    )
    .unwrap()
    .is_some());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "encoded-word recognition allocated");
}

fn encoded_word_decoding() {
    use td_mta::{
        admission::work::{Charge, Meter},
        encoded_word::{
            decode::{Cursor, Status},
            Context, Word,
        },
        ports::{Deadline, Tick},
    };
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10000,
            records: 10000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (input, expected, problem) in [
        (b"=?utf-8?B?4oKsZm8=?=".as_slice(), "€fo", false),
        (b"=?utf-8?Q?=E1=QZ=80=00a?=", "��QZ�a", true),
        (b"=?utf-8?B?SGVs----bG8=?=", "Hel�lo", true),
        (b"=?utf-8?Q?=EF=B7=90?=", "�", true),
    ] {
        let word = Word::recognize(black_box(input), Context::Text, Tick(1), &mut work)
            .unwrap()
            .unwrap();
        let mut cursor = Cursor::new(word);
        let mut expected = expected.chars();
        let mut done = false;
        for _ in 0..100 {
            let checkpoint = cursor;
            let first = cursor.poll(Tick(1), &mut work).unwrap();
            cursor = checkpoint;
            assert_eq!(cursor.poll(Tick(1), &mut work).unwrap(), first);
            match first {
                Status::Scalar(value) => assert_eq!(Some(value), expected.next()),
                Status::Yield => {}
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        assert_eq!(expected.next(), None);
        assert_eq!(cursor.is_encoding_problem(), problem);
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "encoded-word decoding allocated");
}

fn header_text() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_text::{Cursor, Status},
        ports::{Deadline, Tick},
    };
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10000,
            records: 10000,
            ..Charge::default()
        },
    );
    let long = "=?utf-8?Q?aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?=";
    assert_eq!(long.len(), 76);
    let before = COUNTERS.snapshot();
    for (input, expected, problem) in [
        (
            b"  =?utf-8?Q?hello?=\r\n\t=?utf-8?B?4oKs?=  x".as_slice(),
            "hello€  x",
            false,
        ),
        (
            b"=?utf-8?Q?=E1=QZ=80?= \r\n\t=?unknown?Q?a?=",
            "��QZ� \t=?unknown?Q?a?=",
            true,
        ),
        (b" \ta\0\xe1\x80b\xef\xbf\xbf", "\ta�b�", true),
        (b"a =?unknown?Q?x?=", "a =?unknown?Q?x?=", false),
        (long.as_bytes(), long, false),
    ] {
        let mut cursor = Cursor::new(black_box(input));
        let mut expected = expected.chars();
        let mut done = false;
        for _ in 0..1000 {
            let checkpoint = cursor;
            let status = cursor.poll(Tick(1), &mut work).unwrap();
            cursor = checkpoint;
            assert_eq!(cursor.poll(Tick(1), &mut work).unwrap(), status);
            match status {
                Status::Scalar(value) => assert_eq!(Some(value), expected.next()),
                Status::Yield => {}
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        assert_eq!(expected.next(), None);
        assert_eq!(cursor.is_encoding_problem(), problem);
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "unstructured header decoding allocated");
}

fn header_date_projection() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_date::{
            project::{render, Error, Outcome},
            Date, Offset,
        },
        ports::{Deadline, Tick},
    };
    let date = Date {
        year: 2000,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        offset: Offset::Known(5999),
    };
    let mut output = [0; 25];
    let before = COUNTERS.snapshot();
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records: 1000,
            output_bytes: 1000,
            ..Charge::default()
        },
    );
    assert_eq!(
        render(black_box(date), &mut output, Tick(1), &mut work),
        Ok(Outcome::Date("1999-12-27T20:01:00Z"))
    );
    assert_eq!(
        render(
            Date {
                offset: Offset::Unknown,
                ..date
            },
            &mut output,
            Tick(1),
            &mut work
        ),
        Ok(Outcome::Date("2000-01-01T00:00:00-00:00"))
    );
    assert_eq!(
        render(
            Date {
                year: u16::MAX,
                ..date
            },
            &mut output,
            Tick(1),
            &mut work
        ),
        Ok(Outcome::OutOfRange)
    );
    assert_eq!(
        render(Date { second: 60, ..date }, &mut output, Tick(1), &mut work),
        Ok(Outcome::LeapSecondUnverified)
    );
    assert_eq!(
        render(date, &mut [], Tick(1), &mut work),
        Err(Error::Capacity)
    );
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records: 8,
            ..Charge::default()
        },
    );
    assert_eq!(
        render(date, &mut output, Tick(1), &mut limited),
        Err(Error::Work(Stop::OutputBytes))
    );
    assert_eq!(
        render(date, &mut output, Tick(100), &mut work),
        Err(Error::Work(Stop::Deadline))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "header date projection allocated");
}

fn header_dates() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_date::{Cursor, Error, Offset, Status},
        ports::{Deadline, Tick},
    };
    let long = format!("({})21 Nov 1997 09:55:06 CST", "🐈".repeat(10_000));
    let year = format!("21 Nov {}1997 09:55:06 CST", "0".repeat(10_000));
    let over = format!(
        "{}{}21 Nov 1997 09:55:06 CST",
        "(".repeat(33),
        ")".repeat(33)
    );
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 200_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for source in [
        b"Fri, 21 Nov 1997 09:55:06 -0600".as_slice(),
        long.as_bytes(),
        year.as_bytes(),
    ] {
        let mut cursor = Cursor::new(black_box(source));
        let mut complete = false;
        for _ in 0..10_000 {
            if let Status::Complete(value) = cursor.poll(Tick(1), &mut work).unwrap() {
                let value = value.unwrap();
                assert_eq!(value.year, 1997);
                assert_eq!(value.offset, Offset::Known(-360));
                complete = true;
                break;
            }
        }
        assert!(complete);
    }
    for source in [
        b"31 Feb 2000 00:00 +0000".as_slice(),
        b"1 Jan 2000 00:00 +0000 (bad",
    ] {
        let mut cursor = Cursor::new(source);
        let mut complete = false;
        for _ in 0..100 {
            if let Status::Complete(value) = cursor.poll(Tick(1), &mut work).unwrap() {
                assert!(value.is_none());
                complete = true;
                break;
            }
        }
        assert!(complete);
    }
    let mut cursor = Cursor::new(over.as_bytes());
    assert_eq!(cursor.poll(Tick(1), &mut work), Ok(Status::Yield));
    assert_eq!(cursor.poll(Tick(1), &mut work), Err(Error::NestingLimit));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "header date parsing allocated");
}

fn header_comments() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_cfws::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    let long = format!("({}) token", "🐈".repeat(10_000));
    let nested = format!("{}{}", "(".repeat(32), ")".repeat(32));
    let over = format!("{}{}", "(".repeat(33), ")".repeat(33));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 100_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, count, consumed) in [
        (b" (one)\r\n\t(two(nested)) <id>".as_slice(), 2, 23),
        (long.as_bytes(), 1, 40003),
        (nested.as_bytes(), 1, 64),
        (b"not-cfws", 0, 0),
    ] {
        let mut cursor = Cursor::new(black_box(source), 0);
        let mut comments = 0;
        let mut complete = false;
        for _ in 0..10_000 {
            match cursor.poll(Tick(1), &mut work).unwrap() {
                Status::Comment(comment) => {
                    assert!(comment.end <= source.len());
                    comments += 1;
                }
                Status::Yield => {}
                Status::Complete(end) => {
                    assert_eq!(end.position, consumed);
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        assert_eq!(comments, count);
    }
    for (source, expected) in [
        (b"(bad".as_slice(), Error::Malformed),
        (over.as_bytes(), Error::NestingLimit),
    ] {
        let mut cursor = Cursor::new(source, 0);
        let mut refused = false;
        for _ in 0..100 {
            if let Err(error) = cursor.poll(Tick(1), &mut work) {
                assert_eq!(error, expected);
                refused = true;
                break;
            }
        }
        assert!(refused);
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "header comment scanning allocated");
}

fn header_selection() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property,
        header_select::{Cursor, SourceEnd, Status},
        ports::{Deadline, Tick},
    };
    let name = "x".repeat(4096);
    let long_key = format!("header:{name}:all");
    let long_source = format!("{name}: one\n{}: two\n\n", name.to_uppercase());
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 100_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (key, source, expected) in [
        ("subject", b"Subject: a\nSUBJECT: b\n\n".as_slice(), 1),
        ("header:Subject:all", b"Subject: a\nSUBJECT: b\n\n", 2),
        ("header:Missing:all", b"Subject: a\n\n", 0),
        (long_key.as_str(), long_source.as_bytes(), 2),
    ] {
        let mut selector = header_property::Cursor::new(key, header_property::Context::Email);
        let mut selected = None;
        for _ in 0..1000 {
            if let header_property::Status::Complete(value) =
                selector.poll(Tick(1), &mut work).unwrap()
            {
                selected = value;
                break;
            }
        }
        let mut cursor = Cursor::new(
            black_box(source),
            0,
            source.len() as u64,
            selected.unwrap(),
            SourceEnd::Prefix,
        );
        let mut complete = false;
        let mut count = 0;
        for _ in 0..10_000 {
            match cursor.poll(Tick(1), &mut work).unwrap() {
                Status::Match(field) => {
                    assert!(field.value_end <= source.len() as u64);
                    count += 1;
                }
                Status::Yield => {}
                Status::Complete(_) => {
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        assert_eq!(count, expected);
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "header occurrence traversal allocated");
}

fn header_properties() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property::{Context, Cursor, Error, Form, Occurrence, Status},
        ports::{Deadline, Tick},
    };
    let long = format!("header:{}:asDate:all", "x".repeat(100_000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (key, form, occurrence) in [
        ("subject", Form::Text, Occurrence::Last),
        (
            "header:Resent-Reply-To:asGroupedAddresses:all",
            Form::GroupedAddresses,
            Occurrence::All,
        ),
        (long.as_str(), Form::Date, Occurrence::All),
    ] {
        let mut cursor = Cursor::new(black_box(key), Context::Email);
        let mut done = false;
        for _ in 0..10_000 {
            match cursor.poll(Tick(1), &mut work).unwrap() {
                Status::Yield => {}
                Status::Complete(value) => {
                    let value = value.unwrap();
                    assert_eq!(value.requested(), key);
                    assert_eq!(value.form(), form);
                    assert_eq!(value.occurrence(), occurrence);
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
    }
    for (key, expected) in [
        ("header:X:all:asText", Error::InvalidProperty),
        ("header:dAtE:asText", Error::ForbiddenForm),
    ] {
        let mut cursor = Cursor::new(key, Context::Email);
        let mut refused = false;
        for _ in 0..100 {
            if let Err(error) = cursor.poll(Tick(1), &mut work) {
                assert_eq!(error, expected);
                refused = true;
                break;
            }
        }
        assert!(refused);
    }
    let mut body = Cursor::new("from", Context::BodyPart);
    assert_eq!(body.poll(Tick(1), &mut work), Ok(Status::Complete(None)));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "header property selection allocated");
}

fn header_nfc() {
    use td_mta::{
        admission::work::{Charge, Meter},
        nfc::{Cursor, HeaderBudget, Scratch, Status},
        ports::{Deadline, Tick},
    };
    let mut bytes = [0; 8192];
    let starter = b"=?utf-8?Q?a?=";
    bytes
        .get_mut(..starter.len())
        .unwrap()
        .copy_from_slice(starter);
    let mut used = starter.len();
    for _ in 0..300 {
        let word = b" =?utf-8?Q?=CC=80?=";
        let end = used + word.len();
        bytes.get_mut(used..end).unwrap().copy_from_slice(word);
        used = end;
    }
    let source = bytes.get(..used).unwrap();
    let mut scratch = Scratch::new();
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 16 * 1024 * 1024,
            records: 2_000_000,
            output_bytes: 1024 * 1024,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    for (input, long) in [
        (b"=?utf-8?Q?e?=\r\n =?utf-8?Q?=CC=81?=".as_slice(), false),
        (source, true),
    ] {
        let mut cursor = Cursor::from_unstructured_header(
            black_box(input),
            &mut scratch,
            &mut work,
            &mut budget,
        );
        let mut count = 0;
        let mut done = false;
        for _ in 0..1_000_000 {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Scalar(value) => {
                    let expected = if !long {
                        'é'
                    } else if count == 0 {
                        'à'
                    } else {
                        '\u{300}'
                    };
                    assert_eq!(value, expected);
                    cursor
                        .charge_output(Tick(1), value.len_utf8() as u64)
                        .unwrap();
                    count += 1;
                }
                Status::Yield => {}
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        assert_eq!(count, if long { 300 } else { 1 });
        assert!(!cursor.is_encoding_problem());
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "decoded-header NFC allocated");
}

fn unicode_nfc() {
    use td_mta::{
        admission::work::{Charge, Meter},
        nfc::{Cursor, HeaderBudget, Scratch, Status},
        ports::{Deadline, Tick},
    };
    let input = format!("a{}z", "\u{315}\u{300}".repeat(300));
    let expected = format!("à{}{}z", "\u{300}".repeat(299), "\u{315}".repeat(300));
    let mut scratch = Scratch::new();
    let before = COUNTERS.snapshot();
    for (input, expected) in [("e\u{301}", "é"), (input.as_str(), expected.as_str())] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: td_mta::admission::WorkLimits::default().foreground_io_bytes,
                records: td_mta::admission::WorkLimits::default().foreground_records,
                output_bytes: expected.len() as u64,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(black_box(input), &mut scratch, &mut work, &mut budget);
        let mut expected = expected.chars();
        let mut complete = false;
        for _ in 0..10000 {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Scalar(value) => {
                    cursor
                        .charge_output(Tick(1), value.len_utf8() as u64)
                        .unwrap();
                    assert_eq!(Some(value), expected.next());
                }
                Status::Yield => {}
                Status::Complete => {
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        cursor.charge_output(Tick(1), 0).unwrap();
        assert_eq!(expected.next(), None);
        assert_eq!(work.remaining().output_bytes, 0);
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "NFC fast/replay paths allocated");
}

fn unicode_lookups() {
    use td_mta::unicode;
    let before = COUNTERS.snapshot();
    for (value, expected, class, lower) in [
        ('é', "e\u{301}", 0, 'é'),
        ('\u{ac01}', "\u{1100}\u{1161}\u{11a8}", 0, '\u{ac01}'),
        ('\u{1fa}', "A\u{30a}\u{301}", 0, '\u{1fb}'),
        ('\u{0301}', "\u{0301}", 230, '\u{0301}'),
        ('\u{10400}', "\u{10400}", 0, '\u{10428}'),
        ('\u{0378}', "\u{0378}", 0, '\u{0378}'),
    ] {
        let original = unicode::decompose(black_box(value)).unwrap();
        let copied = black_box(original);
        assert!(original.iter().eq(expected.chars()));
        assert!(copied.iter().eq(expected.chars()));
        assert_eq!(unicode::combining_class(black_box(value)).unwrap(), class);
        assert_eq!(unicode::simple_lowercase(black_box(value)).unwrap(), lower);
    }
    for (left, right, expected) in [
        ('e', '\u{301}', Some('é')),
        ('\u{1100}', '\u{1161}', Some('\u{ac00}')),
        ('\u{ac00}', '\u{11a8}', Some('\u{ac01}')),
        ('\u{915}', '\u{93c}', None),
    ] {
        assert_eq!(
            unicode::compose(black_box(left), black_box(right)).unwrap(),
            expected
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Unicode lookups allocated");
}

fn mime_qp() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_qp::{Decoder, Status},
        ports::{Deadline, Tick},
    };
    let mut long = [b' '; 513];
    *long.last_mut().unwrap() = b'x';
    let before = COUNTERS.snapshot();
    for (input, expected, problem) in [
        (
            b"a=20\r\nb \t\r\nc=0A=QZ=".as_slice(),
            b"a \r\nb\r\nc\n=QZ".as_slice(),
            true,
        ),
        (b"ab= \t\r\ncd".as_slice(), b"abcd".as_slice(), false),
        (b"=4".as_slice(), b"=4".as_slice(), true),
        (b"".as_slice(), b"".as_slice(), false),
        (long.as_slice(), long.as_slice(), false),
    ] {
        let mut decoder = Decoder::new(input.len() as u64);
        let mut resident = Decoder::new(3);
        let mut resident_work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 5,
                output_bytes: 3,
                ..Charge::default()
            },
        );
        let mut resident_output = [0; 3];
        assert_eq!(
            resident
                .poll(b"a b", &mut resident_output, Tick(1), &mut resident_work)
                .unwrap()
                .status,
            Status::Complete
        );
        assert_eq!(resident_output, *b"a b");
        assert_eq!(resident_work.remaining(), Charge::default());
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10000,
                output_bytes: 10000,
                ..Charge::default()
            },
        );
        let mut written = 0;
        let mut done = false;
        for _ in 0..10000 {
            let at = decoder.position() as usize;
            let end = (at + 2).min(input.len());
            let mut byte = [0; 1];
            let step = decoder
                .poll(
                    black_box(input.get(at..end).unwrap()),
                    &mut byte,
                    Tick(1),
                    &mut work,
                )
                .unwrap();
            if step.written != 0 {
                assert_eq!(byte.first(), expected.get(written));
                written += step.written;
            }
            if step.status == Status::Complete {
                done = true;
                break;
            }
        }
        assert!(done);
        assert_eq!(written, expected.len());
        assert_eq!(decoder.is_encoding_problem(), problem);
    }
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 20,
            output_bytes: 20,
            ..Charge::default()
        },
    );
    let mut decoder = Decoder::new(4);
    assert_eq!(
        decoder
            .poll(b"= ", &mut [], Tick(1), &mut work)
            .unwrap()
            .status,
        Status::NeedInput
    );
    assert_eq!(
        decoder
            .poll(b"\tx", &mut [], Tick(1), &mut work)
            .unwrap()
            .status,
        Status::Reposition
    );
    let saved = decoder;
    for mut replay in [decoder, saved] {
        let mut output = [0; 4];
        assert_eq!(
            replay
                .poll(b" \tx", &mut output, Tick(1), &mut work)
                .unwrap()
                .status,
            Status::Complete
        );
        assert_eq!(output, *b"= \tx");
    }
    assert_eq!(work.remaining().io_bytes, 10);
    assert_eq!(work.remaining().output_bytes, 12);
    let mut stopped = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    let mut decoder = Decoder::new(1);
    assert_eq!(
        decoder.poll(b"x", &mut [], Tick(1), &mut stopped),
        Err(Stop::IoBytes)
    );
    let remaining = work.remaining();
    assert_eq!(
        decoder.poll(b"x", &mut [], Tick(1), &mut work),
        Err(Stop::IoBytes)
    );
    assert_eq!(work.remaining(), remaining);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME quoted-printable allocated");
}

fn mime_base64() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_base64::{Decoder, Status},
        ports::{Deadline, Tick},
    };
    let before = COUNTERS.snapshot();
    for (input, expected, problem) in [
        (b"TWFu".as_slice(), b"Man".as_slice(), false),
        (b"T!Q==Z".as_slice(), b"M".as_slice(), true),
        (b"".as_slice(), b"".as_slice(), false),
    ] {
        let mut decoder = Decoder::default();
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 6,
                output_bytes: 3,
                ..Charge::default()
            },
        );
        let mut input_at = 0;
        let mut output_at = 0;
        loop {
            let mut byte = [0; 1];
            let step = decoder
                .poll(
                    black_box(input.get(input_at..).unwrap()),
                    &mut byte,
                    true,
                    Tick(1),
                    &mut meter,
                )
                .unwrap();
            input_at += step.consumed;
            if step.written != 0 {
                assert_eq!(byte.first(), expected.get(output_at));
                output_at += step.written;
            }
            if step.status == Status::Complete {
                break;
            }
        }
        assert_eq!(input_at, input.len());
        assert_eq!(output_at, expected.len());
        assert_eq!(decoder.is_encoding_problem(), problem);
    }
    let mut decoder = Decoder::default();
    let mut meter = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    assert_eq!(
        decoder.poll(b"T", &mut [], true, Tick(1), &mut meter),
        Err(Stop::IoBytes)
    );
    assert_eq!(
        decoder.poll(b"", &mut [], true, Tick(1), &mut meter),
        Err(Stop::IoBytes)
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME base64 allocated");
}

fn store_pinned_blobs() {
    let mut samples = [COUNTERS.snapshot(); 40];
    let mut slots = samples.iter_mut();
    measured_store_fs::probe_pinned_blobs(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "pinned body read allocated");
    }
}

fn journal_overlay() {
    use td_mta::{
        format::{
            container::JournalHeader, frame, key::Key, operation::Operation, Sequence, Table,
            FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES, MAX_FRAME_OPERATIONS,
            MAX_JOURNAL_OPERATIONS, OPERATION_HEADER_BYTES,
        },
        ids::{AccountId, StoreEpoch, ThreadId},
        overlay::{Cell, Overlay},
    };
    let mut header = [0; JOURNAL_HEADER_BYTES];
    JournalHeader {
        account: AccountId::from_bytes([3; 16]),
        epoch: StoreEpoch::from_bytes([4; 16]),
        segment: 1,
        base: Sequence::from_u64(0),
    }
    .encode(&td_crypto::Provider, &mut header)
    .unwrap();
    let frame_length = FRAME_HEADER_BYTES
        + FRAME_FOOTER_BYTES
        + MAX_FRAME_OPERATIONS * (OPERATION_HEADER_BYTES + 16);
    let mut bytes = Vec::with_capacity(frame_length * 2);
    for sequence in 1..=2 {
        let mut encoded = vec![0; frame_length];
        let mut offset = FRAME_HEADER_BYTES;
        for ordinal in 0..MAX_FRAME_OPERATIONS {
            let key = [(ordinal % 251) as u8; 16];
            offset += Operation::delete(Table::Threads, &key)
                .unwrap()
                .encode(encoded.get_mut(offset..).unwrap())
                .unwrap();
        }
        frame::seal(
            &td_crypto::Provider,
            Sequence::from_u64(sequence),
            MAX_FRAME_OPERATIONS,
            &mut encoded,
        )
        .unwrap();
        bytes.extend_from_slice(&encoded);
    }
    let mut cells = vec![Cell::EMPTY; MAX_JOURNAL_OPERATIONS];
    let before = COUNTERS.snapshot();
    {
        let overlay = Overlay::decode(
            &td_crypto::Provider,
            black_box(&header),
            black_box(&bytes),
            &mut cells,
        )
        .unwrap();
        assert_eq!(overlay.operation_count(), MAX_JOURNAL_OPERATIONS);
        for id in 0..=250 {
            assert!(overlay
                .get(Key::Thread(ThreadId::from_bytes([id; 16])))
                .unwrap()
                .is_some());
            assert!(
                overlay
                    .next(Table::Threads, Some(&[id; 16]))
                    .unwrap()
                    .is_some()
                    == (id != 250)
            );
        }
        assert!(overlay
            .get(Key::Thread(ThreadId::from_bytes([255; 16])))
            .unwrap()
            .is_none());
        let table = td_mta::format::table::TableHeader {
            table: Table::Threads,
            account: AccountId::from_bytes([3; 16]),
            epoch: StoreEpoch::from_bytes([4; 16]),
            generation: 1,
            through: Sequence::from_u64(0),
            record_count: 0,
            payload_bytes: 0,
        };
        assert_eq!(
            td_mta::merge::Merge::new(table, &overlay)
                .unwrap()
                .finish(|_| Ok::<_, ()>(()))
                .unwrap(),
            0
        );
    }
    assert!(Overlay::decode(
        &td_crypto::Provider,
        &header,
        &bytes,
        cells.get_mut(..MAX_JOURNAL_OPERATIONS - 1).unwrap()
    )
    .is_err());
    *bytes.last_mut().unwrap() ^= 1;
    assert!(Overlay::decode(&td_crypto::Provider, &header, &bytes, &mut cells).is_err());
    assert!(!before.invalid);
    assert_eq!(COUNTERS.snapshot(), before, "journal overlay allocated");
}

fn journal_merge() {
    use td_mta::{
        format::{
            container::JournalHeader,
            frame,
            key::Key,
            operation::Operation,
            table::{Record, TableHeader},
            Sequence, Table, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES,
        },
        ids::{AccountId, StoreEpoch},
        merge::{Error, Merge},
        overlay::{Cell, Overlay},
    };
    let account = AccountId::from_bytes([3; 16]);
    let epoch = StoreEpoch::from_bytes([4; 16]);
    let mut header = [0; JOURNAL_HEADER_BYTES];
    JournalHeader {
        account,
        epoch,
        segment: 2,
        base: Sequence::from_u64(5),
    }
    .encode(&td_crypto::Provider, &mut header)
    .unwrap();
    let operations = [
        Operation::put(Table::Threads, &[1; 16], &[]).unwrap(),
        Operation::delete(Table::Threads, &[2; 16]).unwrap(),
        Operation::put(Table::Threads, &[4; 16], &[]).unwrap(),
    ];
    let mut bytes = vec![
        0;
        FRAME_HEADER_BYTES
            + FRAME_FOOTER_BYTES
            + operations
                .iter()
                .map(|v| v.encoded_len().unwrap())
                .sum::<usize>()
    ];
    let mut offset = FRAME_HEADER_BYTES;
    for operation in operations {
        offset += operation.encode(bytes.get_mut(offset..).unwrap()).unwrap();
    }
    frame::seal(&td_crypto::Provider, Sequence::from_u64(6), 3, &mut bytes).unwrap();
    let mut cells = [Cell::EMPTY; 3];
    let overlay = Overlay::decode(&td_crypto::Provider, &header, &bytes, &mut cells).unwrap();
    let table = TableHeader {
        table: Table::Threads,
        account,
        epoch,
        generation: 1,
        through: Sequence::from_u64(5),
        record_count: 2,
        payload_bytes: 128,
    };
    let first = Record::new(Table::Threads, Sequence::from_u64(3), &[2; 16], &[]).unwrap();
    let second = Record::new(Table::Threads, Sequence::from_u64(3), &[3; 16], &[]).unwrap();
    let expected = [(1, 6), (3, 3), (4, 6)];
    let before = COUNTERS.snapshot();
    let mut outputs = 0;
    let mut sink = |row: td_mta::ports::Record<'_>| {
        let id = match row.key {
            Key::Thread(id) => *id.as_bytes().first().unwrap(),
            _ => panic!("wrong table"),
        };
        assert_eq!(Some(&(id, row.last_change.number())), expected.get(outputs));
        outputs += 1;
        Ok::<_, ()>(())
    };
    let mut merge = Merge::new(table, &overlay).unwrap();
    merge.push(first, &mut sink).unwrap();
    merge.push(second, &mut sink).unwrap();
    assert_eq!(merge.finish(&mut sink).unwrap(), 3);
    assert_eq!(outputs, 3);
    assert!(Merge::new(
        TableHeader {
            account: AccountId::from_bytes([9; 16]),
            ..table
        },
        &overlay
    )
    .is_err());
    let mut merge = Merge::new(table, &overlay).unwrap();
    assert_eq!(merge.push(first, |_| Err(7)), Err(Error::Sink(7)));
    assert_eq!(merge.finish(|_| Ok::<_, i32>(())), Err(Error::Failed));
    let mut merge = Merge::new(table, &overlay).unwrap();
    merge.push(first, |_| Ok::<_, ()>(())).unwrap();
    assert!(matches!(
        merge.push(first, |_| Ok::<_, ()>(())),
        Err(Error::Format(_))
    ));
    assert!(merge.is_failed());
    assert!(matches!(
        Merge::new(table, &overlay)
            .unwrap()
            .finish(|_| Ok::<_, ()>(())),
        Err(Error::Format(_))
    ));
    assert_eq!(COUNTERS.snapshot(), before, "journal merge allocated");
}

fn main() {
    allocation_counter::Counters::verify_model();
    forwarding();
    if std::env::args()
        .nth(1)
        .is_some_and(|arg| arg == "--store-files")
    {
        store_temporary_files();
        store_verify_account();
        store_reserved_append();
        store_journal_publication();
        store_pinned_reads();
        store_read_pool();
        store_pinned_blobs();
        mime_base64();
        mime_qp();
        mime_qp_input();
        unicode_lookups();
        unicode_nfc();
        header_date_projection();
        header_dates();
        header_comments();
        header_selection();
        header_properties();
        header_nfc();
        encoded_word_candidates();
        encoded_word_decoding();
        header_text();
        mime_input();
        mime_headers();
        body_value();
        mime_text();
        body_charset();
        mime_charset();
        mime_checkpoints();
        mime_unfold();
        header_raw();
        println!("std-temporary-allocation-v1: passed");
        return;
    }
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
    store_temporary_files();
    store_verify_account();
    store_reserved_append();
    store_journal_publication();
    store_pinned_reads();
    store_read_pool();
    store_pinned_blobs();
    mime_base64();
    mime_qp();
    mime_qp_input();
    unicode_lookups();
    unicode_nfc();
    header_date_projection();
    header_dates();
    header_comments();
    header_selection();
    header_properties();
    header_nfc();
    encoded_word_candidates();
    encoded_word_decoding();
    header_text();
    mime_input();
    mime_headers();
    body_value();
    mime_text();
    body_charset();
    mime_charset();
    mime_checkpoints();
    mime_unfold();
    header_raw();
    journal_overlay();
    journal_merge();
    mailbox_parent_walks();
    collected_frame_changes();
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
    for fail in [false, true] {
        let mut incremental = td_mta::format::frame_stream::Verifier::new(
            &crypto,
            Sequence::default(),
            bytes.get(..64).unwrap(),
        )
        .unwrap();
        let operation = bytes.get(64..92).unwrap();
        if fail {
            assert!(incremental.push(operation.get(..27).unwrap()).is_err());
            assert!(incremental.push(operation).is_err());
            assert!(incremental.finish(bytes.get(92..).unwrap()).is_err());
        } else {
            assert_eq!(incremental.push(operation).unwrap().ordinal, 0);
            assert_eq!(
                incremental
                    .finish(bytes.get(92..).unwrap())
                    .unwrap()
                    .header(),
                frame.header()
            );
        }
    }
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
    Operation::change(
        td_mta::format::ObjectType::Email,
        td_mta::ports::ChangeAction::Updated,
        &[4; 16],
    )
    .encode(bytes.get_mut(64..92).unwrap())
    .unwrap();
    seal(&crypto, Sequence::from_u64(1), 1, &mut bytes).unwrap();
    let mut cells = [td_mta::frame_changes::Cell::EMPTY; 1];
    let mut changes =
        td_mta::format::journal_stream::changes::Verifier::new(&crypto, &header_bytes).unwrap();
    let mut pending = changes.begin(bytes.get(..64).unwrap(), &mut cells).unwrap();
    pending.push(bytes.get(64..92).unwrap()).unwrap();
    let complete = pending.finish(bytes.get(92..).unwrap()).unwrap();
    assert_eq!(complete.records().next().unwrap().change.id, [4; 16]);
    assert_eq!(changes.finish().unwrap().operations(), 1);
    let mut changes =
        td_mta::format::journal_stream::changes::Verifier::new(&crypto, &header_bytes).unwrap();
    {
        let mut pending = changes.begin(bytes.get(..64).unwrap(), &mut cells).unwrap();
        pending.push(bytes.get(64..92).unwrap()).unwrap();
    }
    assert!(changes.finish().is_err());
}

fn mailbox_parent_walks() {
    use td_mta::{
        format::{
            key::Key,
            row::{MailboxRow, Row},
            ObjectType, Sequence, Table,
        },
        ids::{AccountId, MailboxId, StoreEpoch},
        mailbox_parents::{Error, ParentWalk},
        ports::{self, ChangeCursor, ChangeStep, ReadView, Record, ViewIdentity},
    };
    struct View {
        identity: ViewIdentity,
        cycle: bool,
    }
    impl ReadView for View {
        fn identity(&self) -> ViewIdentity {
            self.identity
        }
        fn next_change(
            &mut self,
            _: ChangeCursor,
            _: ObjectType,
        ) -> Result<ChangeStep, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn next<'a>(
            &mut self,
            _: Table,
            _: Option<&[u8]>,
            _: &'a mut [u8],
            _: &'a mut [u8],
        ) -> Result<Option<Record<'a>>, ports::Error> {
            Err(ports::Error::Invalid)
        }
        fn get<'a>(
            &mut self,
            key: Key<'_>,
            value: &'a mut [u8],
        ) -> Result<Option<(Row<'a>, Sequence)>, ports::Error> {
            let Key::Mailbox(id) = key else {
                return Err(ports::Error::Invalid);
            };
            let parent = match id.as_bytes().first().copied() {
                Some(0) => Some(1),
                Some(1) => Some(if self.cycle { 0 } else { 2 }),
                Some(2) => None,
                _ => return Ok(None),
            };
            let row = Row::Mailbox(MailboxRow {
                name: "folder",
                parent: parent.map(|v| MailboxId::from_bytes([v; 16])),
                role: None,
                sort_order: 0,
                subscribed: true,
            });
            let used = row.encode(value).map_err(|_| ports::Error::Capacity)?;
            let row = Row::decode(
                Table::Mailboxes,
                value.get(..used).ok_or(ports::Error::Invalid)?,
            )
            .map_err(|_| ports::Error::Corrupt)?;
            Ok(Some((row, Sequence::from_u64(5))))
        }
    }
    let identity = ViewIdentity {
        account: AccountId::from_bytes([1; 16]),
        epoch: StoreEpoch::from_bytes([2; 16]),
        generation: 1,
        checkpoint: Sequence::from_u64(3),
        segment: 1,
        committed_offset: 256,
        committed_sequence: Sequence::from_u64(5),
        history_floor: Sequence::from_u64(0),
    };
    let mut value = [0; 64];
    let before = COUNTERS.snapshot();
    for (cycle, start, budget, expected) in [
        (false, 0, 10, None),
        (true, 0, 10, Some(Error::Cycle)),
        (false, 0, 1, Some(Error::ReadLimit)),
        (
            false,
            9,
            10,
            Some(Error::Missing(MailboxId::from_bytes([9; 16]))),
        ),
    ] {
        let mut view = View { identity, cycle };
        let mut walk =
            ParentWalk::new(identity, MailboxId::from_bytes([start; 16]), budget).unwrap();
        let result = loop {
            if let Err(error) = walk.advance(&mut view, &mut value) {
                assert!(walk.is_failed());
                assert_eq!(walk.advance(&mut view, &mut value), Err(Error::Failed));
                assert_eq!(walk.finish(), Err(Error::Failed));
                break Err(error);
            }
            if walk.is_complete() {
                break walk.finish();
            }
        };
        assert_eq!(result.err(), expected);
    }
    assert_eq!(COUNTERS.snapshot(), before, "mailbox parent walk allocated");
}

fn collected_frame_changes() {
    use td_mta::{
        format::{frame, operation::Operation, ObjectType, Sequence, MAX_FRAME_OPERATIONS},
        frame_changes::{Cell, Collector},
        ports::ChangeAction,
    };
    let mut cells = vec![Cell::EMPTY; MAX_FRAME_OPERATIONS];
    let mut bytes = vec![0; 104 + 28 * MAX_FRAME_OPERATIONS];
    let operation = Operation::change(ObjectType::Email, ChangeAction::Updated, &[7; 16]);
    let end = bytes.len() - 40;
    for output in bytes.get_mut(64..end).unwrap().as_chunks_mut::<28>().0 {
        operation.encode(output).unwrap();
    }
    frame::seal(
        &td_crypto::Provider,
        Sequence::from_u64(1),
        MAX_FRAME_OPERATIONS,
        &mut bytes,
    )
    .unwrap();
    let mut bad_footer = [0; 40];
    bad_footer.copy_from_slice(bytes.get(end..).unwrap());
    *bad_footer.last_mut().unwrap() ^= 1;
    let before = COUNTERS.snapshot();
    for mode in [0, 1, 2, 0] {
        let slots = if mode == 2 {
            MAX_FRAME_OPERATIONS - 1
        } else {
            MAX_FRAME_OPERATIONS
        };
        let mut collector = Collector::new(
            &td_crypto::Provider,
            Sequence::default(),
            bytes.get(..64).unwrap(),
            cells.get_mut(..slots).unwrap(),
        )
        .unwrap();
        for (index, input) in bytes
            .get(64..end)
            .unwrap()
            .as_chunks::<28>()
            .0
            .iter()
            .enumerate()
        {
            let result = collector.push(black_box(input));
            if mode == 2 && index == MAX_FRAME_OPERATIONS - 1 {
                assert!(result.is_err());
                assert!(collector.push(input).is_err());
            } else {
                assert!(result.is_ok());
            }
        }
        let footer = if mode == 1 {
            &bad_footer
        } else {
            bytes.get(end..).unwrap()
        };
        let result = collector.finish(footer);
        if mode == 0 {
            let complete = result.unwrap();
            assert_eq!(complete.len(), MAX_FRAME_OPERATIONS);
            let view = td_mta::ports::ViewIdentity {
                account: td_mta::ids::AccountId::from_bytes([1; 16]),
                epoch: td_mta::ids::StoreEpoch::from_bytes([2; 16]),
                generation: 1,
                checkpoint: Sequence::default(),
                segment: 1,
                committed_offset: bytes.len() as u64 + 96,
                committed_sequence: Sequence::from_u64(1),
                history_floor: Sequence::default(),
            };
            let start = td_mta::ports::ChangeCursor {
                sequence: Sequence::default(),
                operation: u32::MAX,
            };
            let mut cursor =
                td_mta::change_cursor::Cursor::new(view, start, ObjectType::Email).unwrap();
            for ordinal in 0..MAX_FRAME_OPERATIONS {
                assert!(
                    matches!(cursor.poll(view, cursor.after(), Some(&complete)).unwrap(),
                    td_mta::change_cursor::Step::Change(td_mta::ports::ChangeStep::Record(record)) if record.cursor.operation as usize == ordinal)
                );
            }
            assert!(
                matches!(cursor.poll(view, cursor.after(), Some(&complete)).unwrap(),
                td_mta::change_cursor::Step::Change(td_mta::ports::ChangeStep::Advanced { through }) if through.number() == 1)
            );
            assert_eq!(
                cursor.poll(view, cursor.after(), None).unwrap(),
                td_mta::change_cursor::Step::Change(td_mta::ports::ChangeStep::Complete)
            );
            let mut empty =
                td_mta::change_cursor::Cursor::new(view, start, ObjectType::Thread).unwrap();
            assert!(matches!(
                empty.poll(view, start, Some(&complete)).unwrap(),
                td_mta::change_cursor::Step::Change(td_mta::ports::ChangeStep::Advanced { .. })
            ));
            let mut changed = view;
            changed.generation += 1;
            assert_eq!(
                empty.poll(changed, empty.after(), None),
                Err(td_mta::ports::Error::Conflict)
            );
            assert_eq!(
                empty.poll(view, empty.after(), None),
                Err(td_mta::ports::Error::Conflict)
            );
            for _ in 0..2 {
                for (index, record) in complete.records().enumerate() {
                    assert_eq!(black_box(record).cursor.operation as usize, index);
                    assert_eq!(record.change.id, [7; 16]);
                }
            }
        } else {
            assert!(result.is_err());
        }
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "frame change collection allocated"
    );
}
