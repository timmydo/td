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
    ports, store_paths, wire,
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
use measured_store_fs as store_fs;

// Expand at this root to retain production child paths and restricted visibility.
include!("../src/mime_probe_modules.rs");

#[path = "../src/body_properties.rs"]
#[allow(unused)] // The production library remains the lint authority.
mod body_properties;
#[path = "../src/body_property.rs"]
#[allow(unused)] // The production library remains the lint authority.
mod body_property;

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

fn mime_filename_retention() {
    use td_mta::{
        admission::work::{Charge, Meter},
        mime_filename::{Cursor, Fields, Origin, Status},
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let long = format!(
        "attachment;filename=\"a{}{}x\"",
        "\u{301}".repeat(257),
        "\u{327}".repeat(257)
    );
    let long_normalized = format!("á{}{}x", "\u{327}".repeat(257), "\u{301}".repeat(256));
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 4800);
    let nested = format!("{}{}text/plain;name=f", "(".repeat(33), ")".repeat(33));
    let exhausting = format!("{}: one\n{}: two\n\n", "x".repeat(4096), "x".repeat(4096));
    let key = format!("header:{}:all", "x".repeat(4096));
    let mut property =
        td_mta::header_property::Cursor::new(&key, td_mta::header_property::Context::Email);
    let mut prep = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 100_000_000,
            ..Charge::default()
        },
    );
    let mut selected = None;
    for _ in 0..10_000 {
        if let td_mta::header_property::Status::Complete(value) =
            property.poll(Tick(1), &mut prep).unwrap()
        {
            selected = value;
            break;
        }
    }
    let selected = selected.unwrap();
    let before = COUNTERS.snapshot();
    for (disposition, content_type, expected, origin, rejected, problem) in [
        (
            Some(long.as_bytes()),
            None,
            Some(long_normalized.as_str()),
            Some(Origin::Disposition),
            false,
            false,
        ),
        (
            Some(b"attachment;filename*=utf-8''%xx".as_slice()),
            Some(b"text/plain;name*=utf-8'en'e%CC%81".as_slice()),
            Some("é"),
            Some(Origin::ContentType),
            true,
            false,
        ),
        (
            Some(b"attachment;filename=\"=?utf-8?Q?e?= =?utf-8?Q?=CC=81?=\"".as_slice()),
            Some(b"text/plain;name=other".as_slice()),
            Some("é"),
            Some(Origin::Disposition),
            false,
            false,
        ),
        (
            Some(b"attachment;filename=\"\"".as_slice()),
            Some(b"text/plain;name=other".as_slice()),
            Some(""),
            Some(Origin::Disposition),
            false,
            false,
        ),
        (
            Some(b"attachment;filename*=utf-8''%FF".as_slice()),
            None,
            Some("�"),
            Some(Origin::Disposition),
            false,
            true,
        ),
        (None, None, None, None, false, false),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 100_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 2048];
        let mut cursor = Cursor::new(
            Fields {
                disposition: black_box(disposition),
                content_type: black_box(content_type),
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        assert!(cursor.value().is_none());
        let mut finished = false;
        for _ in 0..100_000 {
            if let Status::Complete(end) = cursor.poll(Tick(1)).unwrap() {
                assert_eq!(cursor.value(), expected.map(str::as_bytes));
                assert_eq!(end.origin, origin);
                assert_eq!(end.invalid_extended, rejected);
                assert_eq!(end.is_encoding_problem, problem);
                assert_eq!(end.bytes, expected.map_or(0, str::len));
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(end)));
                cursor.check_deadline(Tick(1)).unwrap();
                let error = cursor.check_deadline(Tick(100)).err().unwrap();
                assert!(cursor.value().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                finished = true;
                break;
            }
        }
        assert!(finished);
        assert!(cursor.finish(Tick(1)).is_err());
    }
    // Healthy handoff retains bytes while returning the exact original owners.
    {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let addresses = (&work as *const _, &budget as *const _, &scratch as *const _);
        let mut output = [0; 16];
        let mut cursor = Cursor::new(
            Fields {
                disposition: black_box(Some(b"attachment;filename=a".as_slice())),
                content_type: None,
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        let mut finished = false;
        for _ in 0..100_000 {
            if let Status::Complete(_) = cursor.poll(Tick(1)).unwrap() {
                finished = true;
                break;
            }
        }
        assert!(finished);
        let (retained, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(retained.value(), Some(b"a".as_slice()));
        assert_eq!(
            addresses,
            (work as *const _, budget as *const _, scratch as *const _)
        );
    }
    for capacity in 0..3 {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 3];
        let mut cursor = Cursor::new(
            Fields {
                disposition: Some(b"attachment;filename*=utf-8''e%CC%81x"),
                content_type: Some(b"text/plain;name=f"),
            },
            output.get_mut(..capacity).unwrap(),
            &mut work,
            &mut budget,
            &mut scratch,
        );
        let mut error = None;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete(_)) => panic!("short filename backing completed"),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        let error = error.unwrap();
        assert_eq!(error, td_mta::mime_filename::Error::OutputCapacity);
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
    }
    for charge in [
        Charge {
            io_bytes: 0,
            records: 100_000,
            output_bytes: 100_000,
            ..Charge::default()
        },
        Charge {
            io_bytes: 100_000,
            records: 0,
            output_bytes: 100_000,
            ..Charge::default()
        },
        Charge {
            io_bytes: 100_000,
            records: 100_000,
            output_bytes: 0,
            ..Charge::default()
        },
    ] {
        let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge);
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 16];
        let mut cursor = Cursor::new(
            Fields {
                disposition: Some(b"attachment;filename*=utf-8''e%CC%81x"),
                content_type: Some(b"text/plain;name=f"),
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        let mut error = None;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete(_)) => panic!("refused filename completed"),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        let error = error.unwrap();
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
    }
    for source in [b"text/plain;name=f;broken".as_slice(), nested.as_bytes()] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut output = [0; 16];
        let mut cursor = Cursor::new(
            Fields {
                disposition: None,
                content_type: black_box(Some(source)),
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        let mut error = None;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete(_)) => panic!("malformed filename field completed"),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        let error = error.unwrap();
        assert!(matches!(error, td_mta::mime_filename::Error::Decode(_)));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.poll(Tick(1)), Err(error));
    }
    // Exhaust the same aggregate through another public interpretation owner.
    {
        use td_mta::header_select::{
            Cursor as Select, Error as SelectError, SourceEnd, Status as Selected,
        };
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut exhausted = false;
        for _ in 0..2000 {
            let mut cursor = Select::new(
                black_box(exhausting.as_bytes()),
                0,
                exhausting.len() as u64,
                selected,
                SourceEnd::Prefix,
            );
            let mut complete = false;
            for _ in 0..10_000 {
                match cursor.poll_with_budget(Tick(1), &mut work, &mut budget) {
                    Ok(Selected::Complete(_)) => {
                        complete = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(SelectError::InterpretationLimit) => {
                        exhausted = true;
                        break;
                    }
                    Err(e) => panic!("unexpected aggregate refusal: {e}"),
                }
            }
            assert!(complete || exhausted);
            if exhausted {
                break;
            }
        }
        assert!(exhausted);
        let mut scratch = Scratch::new();
        let mut output = [0; 16];
        let mut cursor = Cursor::new(
            Fields {
                disposition: Some(b"attachment;filename=a"),
                content_type: None,
            },
            &mut output,
            &mut work,
            &mut budget,
            &mut scratch,
        );
        let error =
            td_mta::mime_filename::Error::Admission(td_mta::nfc::Error::InterpretationLimit);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
        assert!(cursor.value().is_none());
        assert!(cursor.finish(Tick(1)).is_err());
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "filename retention allocated");
}

fn mime_label_fields() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        mime_label_fields::{Cursor, Error, Input, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!(
        "Content-ID: <{}@b>\r\nContent-Language: en-GB, FR\r\n\r\n",
        "a".repeat(8192)
    );
    let tags = format!("Content-Language: {}FR\r\n\r\n", "en,".repeat(4096));
    let unicode = format!("Content-ID: ({}) <A@B>\r\n\r\n", "🐈".repeat(1024));
    let deep = format!(
        "Content-ID: <A@B>\r\nContent-Language: {}en{}\r\n\r\n",
        "(".repeat(33),
        ")".repeat(33)
    );
    let exhausting = format!("{}a", " ".repeat(8192));
    let case_0 = b"".as_slice();
    let case_1 = b"Content-ID: <bad>\r\nContent-Language: en,\r\n\r\n".as_slice();
    let case_2 = concat!(
        "Content-ID: <bad>\r\n",
        "Content-ID: <A@B>\r\n",
        "Content-ID: <late@id>\r\n",
        "Content-Language: en,,FR\r\n",
        "Content-Language: en-GB, FR\r\n",
        "\r\n",
        "body",
    )
    .as_bytes();
    let case_3 = b"Content-ID: <A@B>\r\n\r\n".as_slice();
    let case_4 = b"Content-ID: <A@B>\r\n".as_slice();
    let case_5 = b"Content-ID: <A@B>\r\n\r\n".as_slice();
    let case_6 = b"Content-ID: <A@B>\r\n\r\n".as_slice();
    let case_7 = b"Content-ID: <A@B>\r\n\r\n".as_slice();
    let before = COUNTERS.snapshot();
    for (source, ending, limit, records, exhausted, fault) in [
        (case_0, SourceEnd::Eof, 1_000_000, 100_000_000, false, None),
        (case_1, SourceEnd::Eof, 1_000_000, 100_000_000, false, None),
        (case_2, SourceEnd::Eof, 1_000_000, 100_000_000, false, None),
        (
            case_3,
            SourceEnd::Prefix,
            1_000_000,
            100_000_000,
            false,
            None,
        ),
        (
            long.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            100_000_000,
            false,
            None,
        ),
        (
            tags.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            100_000_000,
            false,
            None,
        ),
        (
            unicode.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            100_000_000,
            false,
            None,
        ),
        (
            deep.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            100_000_000,
            false,
            Some(Error::NestingLimit),
        ),
        (
            case_4,
            SourceEnd::Prefix,
            1_000_000,
            100_000_000,
            false,
            Some(Error::Truncated),
        ),
        (
            case_5,
            SourceEnd::Eof,
            0,
            100_000_000,
            false,
            Some(Error::Headers(td_mta::mime_headers::Error::HeaderLimit)),
        ),
        (
            case_6,
            SourceEnd::Eof,
            1_000_000,
            0,
            false,
            Some(Error::Work(Stop::Records)),
        ),
        (
            case_7,
            SourceEnd::Eof,
            1_000_000,
            100_000_000,
            true,
            Some(Error::InterpretationLimit),
        ),
    ] {
        for trial in 0..if fault.is_some() { 1 } else { 2 } {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records,
                    output_bytes: 100_000_000,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            if exhausted {
                let mut refused = false;
                for _ in 0..4000 {
                    let mut probe = td_mta::mime_location_selection::Cursor::new(
                        exhausting.as_bytes(),
                        &mut work,
                        &mut budget,
                    );
                    let mut complete = false;
                    for _ in 0..20_000 {
                        match probe.poll(Tick(1)) {
                            Ok(td_mta::mime_location_selection::Status::Yield) => {}
                            Ok(td_mta::mime_location_selection::Status::Complete(_)) => {
                                complete = true;
                                break;
                            }
                            Err(td_mta::mime_location_selection::Error::InterpretationLimit) => {
                                refused = true;
                                break;
                            }
                            Err(error) => panic!("unexpected aggregate refusal: {error}"),
                        }
                    }
                    assert!(complete || refused);
                    if refused {
                        break;
                    }
                }
                assert!(refused);
                assert_eq!(work.stopped(), None);
            }
            let wp = &work as *const Meter;
            let bp = &budget as *const HeaderBudget;
            let mut cursor = Cursor::new(
                Input {
                    source: black_box(source),
                    base: 0,
                    header_limit: limit,
                    source_end: ending,
                },
                &mut work,
                &mut budget,
            )
            .unwrap();
            let mut complete = false;
            let mut failed = None;
            for _ in 0..200_000 {
                match cursor.poll(Tick(1)) {
                    Ok(Status::Yield) => assert!(cursor.selection().is_none()),
                    Ok(Status::Complete) => {
                        complete = true;
                        break;
                    }
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(failed, fault);
            if let Some(error) = fault {
                assert!(cursor.selection().is_none());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            assert!(complete);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            if trial == 1 {
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
                assert!(cursor.selection().is_none());
                assert_eq!(
                    cursor.finish(Tick(1)).err(),
                    Some(Error::Work(Stop::Deadline))
                );
                continue;
            }
            let (mut work, mut budget, selected) = cursor.finish(Tick(1)).unwrap();
            assert!(std::ptr::eq(work, wp));
            assert!(std::ptr::eq(budget, bp));
            if let Some(field) = selected.content_id {
                let value =
                    td_header::resident::slice(source, 0, field.value_start..field.value_end)
                        .unwrap();
                let mut next = td_mta::mime_content_id::Cursor::new(value, work, budget);
                let mut complete = false;
                for _ in 0..200_000 {
                    match next.poll(Tick(1)).unwrap() {
                        td_mta::mime_content_id::Status::Scalar(c) => {
                            black_box(c);
                        }
                        td_mta::mime_content_id::Status::Complete => {
                            complete = true;
                            break;
                        }
                        _ => {}
                    }
                }
                assert!(complete);
                (work, budget) = next.finish(Tick(1)).unwrap();
            }
            if let Some(field) = selected.content_language {
                let value =
                    td_header::resident::slice(source, 0, field.value_start..field.value_end)
                        .unwrap();
                let mut next = td_mta::mime_language::Cursor::new(value, work, budget);
                let mut complete = false;
                for _ in 0..200_000 {
                    match next.poll(Tick(1)).unwrap() {
                        td_mta::mime_language::Status::Tag(e) => {
                            black_box(value.get(e.start..e.end).unwrap());
                        }
                        td_mta::mime_language::Status::Complete => {
                            complete = true;
                            break;
                        }
                        td_mta::mime_language::Status::Yield => {}
                    }
                }
                assert!(complete);
                (work, budget) = next.finish(Tick(1)).unwrap();
            }
            let mut next = Cursor::new(
                Input {
                    source: b"Content-ID: <x@y>\r\n\r\n",
                    base: 0,
                    header_limit: 100,
                    source_end: SourceEnd::Eof,
                },
                work,
                budget,
            )
            .unwrap();
            let mut complete = false;
            for _ in 0..1000 {
                if next.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            let (work, budget, _) = next.finish(Tick(1)).unwrap();
            assert!(std::ptr::eq(work, wp));
            assert!(std::ptr::eq(budget, bp));
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(
        before, after,
        "resident CID/language selection/projection allocated"
    );
}

fn uri_literal_field_values() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_literal_field::{Cursor, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("(lead) {} (tail)", "../a%2Fb?x/".repeat(4096));
    let deep = format!("{}x{}a", "(".repeat(33), ")".repeat(33));
    let unicode = format!("({})a(b)", "🐈".repeat(1024));
    let before = COUNTERS.snapshot();
    for (source, fault, records) in [
        (b"".as_slice(), None, 100_000_000),
        (b" (only)\t", None, 100_000_000),
        (b"(lead) a\r\n b (tail)", None, 100_000_000),
        (b"a (bad (tail)", None, 100_000_000),
        (b"(x) http://[::1]/a%2Fb (tail)", None, 100_000_000),
        (b"(x) =?ascii?Q?a_b?= (tail)", None, 100_000_000),
        (long.as_bytes(), None, 100_000_000),
        (unicode.as_bytes(), None, 100_000_000),
        (b"(bad", Some(Error::MalformedCfws), 100_000_000),
        (deep.as_bytes(), Some(Error::NestingLimit), 100_000_000),
        (b"(x) a% (tail)", Some(Error::MalformedUri), 100_000_000),
        (b"(x) a\rX (tail)", Some(Error::MalformedFold), 100_000_000),
        (b"(x) a (tail)", Some(Error::Work(Stop::Records)), 0),
    ] {
        let trials = if fault.is_some() { 1 } else { 2 };
        for trial in 0..trials {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records,
                    output_bytes: 100_000_000,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let work_ptr = &work as *const Meter;
            let budget_ptr = &budget as *const HeaderBudget;
            let mut cursor = Cursor::new(black_box(source), &mut work, &mut budget);
            let mut complete = false;
            let mut result = None;
            let mut output = 0;
            for _ in 0..200_000 {
                match cursor.poll(Tick(1)) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Octet { byte, position }) => {
                        assert_eq!(source.get(position), Some(&byte));
                        output += 1;
                        black_box(byte);
                    }
                    Ok(Status::Complete) => {
                        complete = true;
                        break;
                    }
                    Err(error) => {
                        result = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(result, fault);
            if let Some(error) = fault {
                assert!(!cursor.is_complete());
                assert_eq!(output, 0);
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            assert!(complete);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            if trial == 1 {
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
                assert!(!cursor.is_complete());
                assert_eq!(
                    cursor.finish(Tick(1)).err(),
                    Some(Error::Work(Stop::Deadline))
                );
            } else {
                let (work, budget, range) = cursor.finish(Tick(1)).unwrap();
                black_box(source.get(range.start..range.end).unwrap());
                assert!(std::ptr::eq(work, work_ptr));
                assert!(std::ptr::eq(budget, budget_ptr));
                let mut next = Cursor::new(b"(x) a(b) (y)", work, budget);
                let mut complete = false;
                for _ in 0..64 {
                    if let Status::Complete = next.poll(Tick(1)).unwrap() {
                        complete = true;
                        break;
                    }
                }
                assert!(complete);
                let (work, budget, _) = next.finish(Tick(1)).unwrap();
                assert!(std::ptr::eq(work, work_ptr));
                assert!(std::ptr::eq(budget, budget_ptr));
            }
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "literal URI field pipeline allocated");
}

fn uri_selection_values() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_selection::{Cursor, Error, Spelling, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("(lead) a{}(bad{}", " ".repeat(8192), "x".repeat(8192));
    let deep = format!("{}x{}a", "(".repeat(33), ")".repeat(33));
    let unicode = format!("({})a(b)", "🐈".repeat(1024));
    let before = COUNTERS.snapshot();
    for (source, fault, records) in [
        (b"".as_slice(), None, 100_000_000),
        (b" (only)\t", None, 100_000_000),
        (b"(lead) a (b) c (tail)", None, 100_000_000),
        (b"a (bad", None, 100_000_000),
        (b"a (bad (tail)", None, 100_000_000),
        (b"a%", None, 100_000_000),
        (long.as_bytes(), None, 100_000_000),
        (unicode.as_bytes(), None, 100_000_000),
        (b"(bad", Some(Error::Malformed), 100_000_000),
        (deep.as_bytes(), Some(Error::NestingLimit), 100_000_000),
        (b"a (tail)", Some(Error::Work(Stop::Records)), 0),
    ] {
        let trials = if fault.is_some() { 1 } else { 2 };
        for trial in 0..trials {
            let late = trial == 1;
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records,
                    output_bytes: 100_000_000,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let work_ptr = &work as *const Meter;
            let budget_ptr = &budget as *const HeaderBudget;
            let mut cursor = Cursor::new(black_box(source), &mut work, &mut budget);
            let mut spelling = None;
            let mut result = None;
            for _ in 0..200_000 {
                match cursor.poll(Tick(1)) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete(range)) => {
                        spelling = Some(range);
                        break;
                    }
                    Err(error) => {
                        result = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(result, fault);
            if let Some(error) = fault {
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            let range = spelling.unwrap();
            black_box(source.get(range.start..range.end).unwrap());
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(range)));
            if late {
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
                assert!(!cursor.is_complete());
                assert_eq!(
                    cursor.finish(Tick(1)).err(),
                    Some(Error::Work(Stop::Deadline))
                );
            } else {
                let (work, budget, returned) = cursor.finish(Tick(1)).unwrap();
                assert_eq!(returned, range);
                assert!(std::ptr::eq(work, work_ptr));
                assert!(std::ptr::eq(budget, budget_ptr));
                let mut next = Cursor::new(b"a(b)", work, budget);
                let mut complete = false;
                for _ in 0..16 {
                    if let Status::Complete(range) = next.poll(Tick(1)).unwrap() {
                        assert_eq!(range, Spelling { start: 0, end: 4 });
                        complete = true;
                        break;
                    }
                }
                assert!(complete);
                let (work, budget, selected) = next.finish(Tick(1)).unwrap();
                let mut literal = td_mta::mime_location_literal::Cursor::new(
                    b"a(b)".get(selected.start..selected.end).unwrap(),
                    work,
                    budget,
                );
                let mut complete = false;
                for _ in 0..32 {
                    match literal.poll(Tick(1)).unwrap() {
                        td_mta::mime_location_literal::Status::Yield => {}
                        td_mta::mime_location_literal::Status::Octet { byte, position } => {
                            assert_eq!(b"a(b)".get(position), Some(&byte));
                            black_box(byte);
                        }
                        td_mta::mime_location_literal::Status::Complete => {
                            complete = true;
                            break;
                        }
                    }
                }
                assert!(complete);
                let (work, budget) = literal.finish(Tick(1)).unwrap();
                assert!(std::ptr::eq(work, work_ptr));
                assert!(std::ptr::eq(budget, budget_ptr));
            }
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "mail URI spelling selection allocated");
}

fn uri_spelling_values() {
    use td_header::{
        uri::spelling::{Cursor, Error, Status},
        Charge, Work,
    };
    struct Budget {
        calls: usize,
        cut: Option<usize>,
    }
    impl Work for Budget {
        type Error = u8;
        fn charge(&mut self, charge: Charge) -> Result<(), u8> {
            black_box(charge);
            let call = self.calls;
            self.calls += 1;
            if self.cut == Some(call) {
                return Err(77);
            }
            Ok(())
        }
    }
    assert!(std::mem::size_of::<Cursor<'_, td_mta::mime_location_literal::Error>>() <= 160);
    let long = format!("(lead) a{}(bad{}", " ".repeat(8192), "x".repeat(8192));
    let deep = format!("{}x{}a", "(".repeat(33), ")".repeat(33));
    let before = COUNTERS.snapshot();
    for (source, fault, cut) in [
        (b"".as_slice(), None, None),
        (b" (only)\t", None, None),
        (b"(lead) ../a(b) (tail)", None, None),
        (b"a (b) c (tail)", None, None),
        (b"a (bad", None, None),
        (b"a (bad (tail)", None, None),
        (b"a\r\n", None, None),
        (long.as_bytes(), None, None),
        (b"(bad", Some(Error::Malformed), None),
        (deep.as_bytes(), Some(Error::NestingLimit), None),
        (b"a (tail)", Some(Error::Work(77)), Some(0)),
    ] {
        for late in [false, true] {
            let mut work = Budget { calls: 0, cut };
            let mut cursor = Cursor::new(black_box(source));
            let mut result = None;
            let mut spelling = None;
            for _ in 0..200_000 {
                match cursor.poll(&mut work) {
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete(range)) => {
                        spelling = Some(range);
                        break;
                    }
                    Err(error) => {
                        result = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(result, fault);
            if let Some(error) = fault {
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(&mut work), Err(error));
                assert_eq!(cursor.finish(), Err(error));
                break;
            }
            let range = spelling.unwrap();
            black_box(source.get(range.start..range.end).unwrap());
            let calls = work.calls;
            assert_eq!(cursor.poll(&mut work), Ok(Status::Complete(range)));
            assert_eq!(work.calls, calls);
            if late {
                work.cut = Some(work.calls);
                assert_eq!(cursor.check_work(&mut work), Err(Error::Work(77)));
                assert!(!cursor.is_complete());
                assert_eq!(cursor.finish(), Err(Error::Work(77)));
            } else {
                cursor.check_work(&mut work).unwrap();
                assert_eq!(cursor.finish(), Ok(range));
            }
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "URI spelling selector allocated");
}

fn uri_literal_values() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_literal::{Cursor, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = "../a%2Fb?x/".repeat(4096);
    let before = COUNTERS.snapshot();
    for (source, fault, records) in [
        (b"".as_slice(), None, 100_000_000),
        (b"../a%\r\n 2Fb?x#Y", None, 100_000_000),
        (b"http://[::1]/a(b)", None, 100_000_000),
        (b"//[vF.a:!]/x", None, 100_000_000),
        (b"=?ascii?Q?file_name?=", None, 100_000_000),
        (long.as_bytes(), None, 100_000_000),
        (b"a%", Some(Error::MalformedUri), 100_000_000),
        (b"1g:h", Some(Error::MalformedUri), 100_000_000),
        (b"http://[:::]/", Some(Error::MalformedUri), 100_000_000),
        (b"a\r\n", Some(Error::MalformedFold), 100_000_000),
        (b"../a", Some(Error::Work(Stop::Records)), 0),
    ] {
        let trials = if fault.is_some() { 1 } else { 2 };
        for trial in 0..trials {
            let late = trial == 1;
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records,
                    output_bytes: 100_000_000,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut cursor = Cursor::new(source, &mut work, &mut budget);
            let mut result = None;
            let mut complete = false;
            for _ in 0..100_000 {
                match cursor.poll(Tick(1)) {
                    Ok(Status::Octet { byte, position }) => {
                        assert_eq!(source.get(position), Some(&byte));
                        black_box(byte);
                    }
                    Ok(Status::Yield) => {}
                    Ok(Status::Complete) => {
                        complete = true;
                        break;
                    }
                    Err(error) => {
                        result = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(result, fault);
            if let Some(error) = fault {
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            } else {
                assert!(complete);
                assert!(cursor.is_complete());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                if late {
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert!(!cursor.is_complete());
                    assert!(cursor.finish(Tick(1)).is_err());
                } else {
                    let (work, budget) = cursor.finish(Tick(1)).unwrap();
                    black_box((work, budget));
                }
            }
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "literal URI reader allocated");
}

fn uri_word_values() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_word::{Cursor, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let maximum = format!("=?ascii?Q?{}\r\n     {}?=", "a".repeat(31), "a".repeat(32));
    let boundary = format!("=?ascii?Q?{}?=", "a".repeat(64));
    let oversized = format!("=?ascii?Q?{}?=", "a".repeat(100));
    let mut bad_tail = oversized.clone();
    bad_tail.push_str("\r\n");
    let before = COUNTERS.snapshot();
    for (source, recognized, fault) in [
        (b"=?utf-8?Q?e=CC\r\n =81?=".as_slice(), true, None),
        (b"=?ascii*en?B?Zm 9v?=", true, None),
        (b"=?utf-8?Q?=FF=00=EF=B7=90?=", true, None),
        (b"=?ascii?B?Zh==?=", true, None),
        (b"=?unknown?Q?a?=", false, None),
        (maximum.as_bytes(), true, None),
        (boundary.as_bytes(), false, None),
        (oversized.as_bytes(), false, None),
        (bad_tail.as_bytes(), false, Some(Error::MalformedFold)),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let mut result = None;
        let mut complete = false;
        for _ in 0..10_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Scalar(value)) => {
                    black_box(value);
                }
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => {
                    complete = true;
                    break;
                }
                Err(error) => {
                    result = Some(error);
                    break;
                }
            }
        }
        assert_eq!(result, fault);
        if let Some(error) = fault {
            assert_eq!(cursor.end(), None);
            assert_eq!(cursor.poll(Tick(1)), Err(error));
        } else {
            assert!(complete);
            assert_eq!(cursor.end().unwrap().recognized, recognized);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (work, budget, end) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(end.recognized, recognized);
            let mut cursor = Cursor::new(source, work, budget);
            let mut complete = false;
            for _ in 0..10_000 {
                match cursor.poll(Tick(1)).unwrap() {
                    Status::Scalar(value) => {
                        black_box(value);
                    }
                    Status::Yield => {}
                    Status::Complete => {
                        complete = true;
                        break;
                    }
                }
            }
            assert!(complete);
            assert_eq!(cursor.end().unwrap().recognized, recognized);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert!(cursor.finish(Tick(1)).is_err());
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "URI selected word allocated");
}

fn uri_word_runs() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_word::{Cursor, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = "=?utf-8?Q?e=CC=81?= \r\n ".repeat(2048);
    let maximum = format!("=?ascii?Q?{}?= =?ascii?Q?b?=", "a".repeat(63));
    let oversized = format!("=?ascii?Q?a?= =?ascii?Q?{}?=", "a".repeat(64));
    let mut bad_tail = oversized.clone();
    bad_tail.push_str("\r\n");
    let before = COUNTERS.snapshot();
    for (source, recognized, problem, bytes, fault, output_limit, records) in [
        (long.as_bytes(), true, false, 6144, None, 100_000, 1_000_000),
        (
            maximum.as_bytes(),
            true,
            false,
            64,
            None,
            100_000,
            1_000_000,
        ),
        (
            b"=?utf-8?Q?=FF?= =?ascii?Q?ok?=".as_slice(),
            true,
            true,
            5,
            None,
            100_000,
            1_000_000,
        ),
        (b"", false, false, 0, None, 100_000, 1_000_000),
        (
            b"=?ascii?Q?a?= =?unknown?Q?b?=",
            false,
            false,
            0,
            None,
            100_000,
            1_000_000,
        ),
        (
            b"=?ascii?Q?a?= tail",
            false,
            false,
            0,
            None,
            100_000,
            1_000_000,
        ),
        (
            oversized.as_bytes(),
            false,
            false,
            0,
            None,
            100_000,
            1_000_000,
        ),
        (
            bad_tail.as_bytes(),
            false,
            false,
            0,
            Some(Error::MalformedFold),
            100_000,
            1_000_000,
        ),
        (
            b"=?ascii?Q?a?= =?ascii?Q?b?=",
            false,
            false,
            1,
            Some(Error::Work(Stop::OutputBytes)),
            1,
            1_000_000,
        ),
        (
            b"=?ascii?Q?a?=",
            false,
            false,
            0,
            Some(Error::Work(Stop::Records)),
            100_000,
            0,
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records,
                output_bytes: output_limit,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let work_pointer = std::ptr::from_ref(&work);
        let budget_pointer = std::ptr::from_ref(&budget);
        let mut cursor = Cursor::new_run(source, &mut work, &mut budget);
        let mut count = 0;
        let mut result = None;
        let mut complete = false;
        for _ in 0..2_000_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Scalar(value)) => {
                    count += value.len_utf8();
                    black_box(value);
                }
                Ok(Status::Yield) => assert_eq!(cursor.end(), None),
                Ok(Status::Complete) => {
                    complete = true;
                    break;
                }
                Err(error) => {
                    result = Some(error);
                    break;
                }
            }
        }
        assert_eq!(count, bytes);
        assert_eq!(result, fault);
        if let Some(error) = fault {
            assert_eq!(cursor.end(), None);
            assert_eq!(cursor.poll(Tick(100)), Err(error));
            assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
        } else {
            assert!(complete);
            let end = cursor.end().unwrap();
            assert_eq!(end.recognized, recognized);
            assert_eq!(end.encoding_problem, problem);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (work, budget, end) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(std::ptr::from_ref(work), work_pointer);
            assert_eq!(std::ptr::from_ref(budget), budget_pointer);
            assert_eq!(end.recognized, recognized);
            let mut cursor = Cursor::new_run(b"=?ascii?Q?x?=", work, budget);
            let mut complete = false;
            for _ in 0..1000 {
                match cursor.poll(Tick(1)).unwrap() {
                    Status::Scalar(value) => {
                        assert_eq!(value, 'x');
                        black_box(value);
                    }
                    Status::Yield => {}
                    Status::Complete => {
                        complete = true;
                        break;
                    }
                }
            }
            assert!(complete);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.end(), None);
            assert!(cursor.finish(Tick(1)).is_err());
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "URI encoded-word run allocated");
}

fn part_header_location_json() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        mime_label_fields::json as labels,
        mime_location_field::retained,
        mime_location_fields::json as location,
        mime_metadata::Context,
        mime_part_headers::{
            self,
            label_json::{Backing, Cursor, Error, Status},
        },
        nfc::{self, HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let prefix = "Content-Type: image/png\r\nContent-ID: <id@a>\r\nContent-Language: fr\r\n";
    let literal = format!(
        "{prefix}Content-Location: ../{}\r\n\r\nbody",
        "a%2F/".repeat(8192)
    );
    let words = format!(
        "{prefix}Content-Location: {}\r\n\r\nbody",
        "=?utf-8?Q?e=CC=81?= \r\n ".repeat(1024)
    );
    let duplicates = format!(
        "{prefix}{}Content-Location: ../ok\r\n\r\n",
        "Content-Location: a%\r\n".repeat(1024)
    );
    let mut location_backing =
        vec![0; retained::capacity_bound(literal.len().max(words.len())).unwrap()];
    let mut heads = [0; 128];
    let mut charset = [0; 32];
    let mut filename = [0; 128];
    let mut id = [0; 256];
    let mut lang = [0; 256];
    let mut next_heads = [0; 32];
    let mut next_charset = [0; 32];
    let mut next_filename = [0; 32];
    let before = COUNTERS.snapshot();
    let cases = [
        (literal.as_bytes(), location_backing.len(), true, None),
        (words.as_bytes(), location_backing.len(), true, None),
        (duplicates.as_bytes(), location_backing.len(), true, None),
        (
            b"Content-ID: <id@a>\r\nContent-Language: fr\r\nContent-Location: \r\n\r\n".as_slice(),
            location_backing.len(),
            true,
            None,
        ),
        (
            concat!(
                "Content-ID: <id@a>\r\nContent-Language: fr\r\n",
                "Content-Location: =?utf-8?Q?=FF?=\r\n\r\n"
            )
            .as_bytes(),
            location_backing.len(),
            true,
            None,
        ),
        (
            b"Content-ID: <id@a>\r\nContent-Language: fr\r\nContent-Location: a%\r\n\r\n",
            0,
            false,
            None,
        ),
        (
            b"Content-ID: <id@a>\r\nContent-Language: fr\r\nContent-Location: ../ok\r\n\r\n",
            3,
            true,
            Some(Error::Location(location::Error::Retention(
                retained::Error::OutputCapacity,
            ))),
        ),
    ];
    for (source, capacity, wanted, fault) in cases {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 2_000_000,
                output_bytes: 1_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let original = (
            std::ptr::from_ref(&work),
            std::ptr::from_ref(&budget),
            std::ptr::from_ref(&scratch),
        );
        let mut cursor = Cursor::new(
            mime_part_headers::Entity {
                source,
                base: 37,
                source_end: SourceEnd::Eof,
                header_limit: 1_000_000,
                context: Context::Normal,
            },
            Backing {
                headers: mime_part_headers::Backing {
                    heads: &mut heads,
                    charset: &mut charset,
                    filename: &mut filename,
                },
                labels: labels::Backing {
                    content_id: &mut id,
                    content_language: &mut lang,
                },
                content_location: location_backing.get_mut(..capacity).unwrap(),
            },
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        let mut result = None;
        for _ in 0..2_000_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => assert!(cursor.value().is_none()),
                Ok(Status::Complete) => {
                    result = Some(Ok(()));
                    break;
                }
                Err(error) => {
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    result = Some(Err(error));
                    break;
                }
            }
        }
        assert_eq!(result, Some(fault.map_or(Ok(()), Err)));
        if let Some(error) = fault {
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        } else {
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (view, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(view.labels.content_id, Some(b"\"id@a\"".as_slice()));
            assert_eq!(view.labels.content_language, Some(b"[\"fr\"]".as_slice()));
            assert_eq!(view.location.value.is_some(), wanted);
            black_box(view.location.value);
            assert_eq!(
                view.location.selection.end.body_start,
                view.headers.body_start
            );
            assert_eq!(
                view.location.selection.end.header_bytes,
                view.headers.header_bytes
            );
            assert_eq!(
                (
                    std::ptr::from_ref(&*work),
                    std::ptr::from_ref(&*budget),
                    std::ptr::from_ref(&*scratch)
                ),
                original
            );
            let mut next = Cursor::new(
                mime_part_headers::Entity {
                    source: b"\r\n",
                    base: 0,
                    source_end: SourceEnd::Eof,
                    header_limit: 100,
                    context: Context::Normal,
                },
                Backing {
                    headers: mime_part_headers::Backing {
                        heads: &mut next_heads,
                        charset: &mut next_charset,
                        filename: &mut next_filename,
                    },
                    labels: labels::Backing {
                        content_id: &mut [],
                        content_language: &mut [],
                    },
                    content_location: &mut [],
                },
                work,
                budget,
                scratch,
            )
            .unwrap();
            let mut complete = false;
            for _ in 0..10000 {
                if next.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            let deadline = Error::Admission(nfc::Error::Work(Stop::Deadline));
            assert_eq!(next.check_deadline(Tick(100)), Err(deadline));
            assert!(next.value().is_none());
            assert_eq!(next.finish(Tick(1)).err(), Some(deadline));
        }
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "Rust allocation in part location JSON"
    );
}

fn uri_location_source_bound() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        mime_location_field::retained,
        mime_location_fields::{
            json::{Cursor, Error, Status},
            Input,
        },
        nfc::{self, HeaderBudget},
        ports::{Deadline, Tick},
    };
    let literal = format!("Content-Location: ../{}\r\n\r\nbody", "a%2F/".repeat(8192));
    let words = format!(
        "Content-Location: {}\r\n\r\nbody",
        "=?utf-8?Q?e=CC=81?= \r\n ".repeat(1024)
    );
    let duplicates = format!(
        "{}Content-Location: ../ok\r\n\r\n",
        "Content-Location: a%\r\n".repeat(1024)
    );
    let mut backing = vec![0; retained::capacity_bound(literal.len().max(words.len())).unwrap()];
    let before = COUNTERS.snapshot();
    for (source, present, capacity, fault) in [
        (literal.as_bytes(), true, backing.len(), None),
        (words.as_bytes(), true, backing.len(), None),
        (duplicates.as_bytes(), true, backing.len(), None),
        (
            b"Content-Location: \r\nContent-Location: ../later\r\n\r\n".as_slice(),
            true,
            backing.len(),
            None,
        ),
        (
            b"Content-Location: =?utf-8?Q?=FF?=\r\n\r\n",
            true,
            backing.len(),
            None,
        ),
        (b"Content-Location: a%\r\n\r\n", false, 0, None),
        (b"Other: x\r\n\r\n", false, 0, None),
        (
            b"Content-Location: ../ok\r\n\r\n",
            true,
            3,
            Some(Error::Retention(retained::Error::OutputCapacity)),
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 2_000_000,
                output_bytes: 1_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(
            Input {
                source,
                base: 100,
                header_limit: 1_000_000,
                source_end: SourceEnd::Eof,
            },
            backing.get_mut(..capacity).unwrap(),
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut result = None;
        for _ in 0..2_000_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => assert!(cursor.view().is_none()),
                Ok(Status::Complete) => {
                    result = Some(Ok(()));
                    break;
                }
                Err(error) => {
                    assert!(cursor.view().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    result = Some(Err(error));
                    break;
                }
            }
        }
        assert_eq!(result, Some(fault.map_or(Ok(()), Err)));
        if let Some(error) = fault {
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        } else {
            let view = cursor.view().unwrap();
            assert_eq!(view.value.is_some(), present);
            assert_eq!(view.selection.content_location.is_some(), present);
            black_box(view.value);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(retained.value.is_some(), present);
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                pointers
            );
            let mut next = Cursor::new(
                Input {
                    source: b"Other: x\r\n\r\n",
                    base: 0,
                    header_limit: 100,
                    source_end: SourceEnd::Eof,
                },
                &mut [],
                work,
                budget,
            )
            .unwrap();
            let mut complete = false;
            for _ in 0..1000 {
                if next.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            let deadline = Error::Admission(nfc::Error::Work(Stop::Deadline));
            assert_eq!(next.check_deadline(Tick(100)), Err(deadline));
            assert!(next.view().is_none());
            assert_eq!(next.finish(Tick(1)).err(), Some(deadline));
        }
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "Rust allocation in source-bound location JSON"
    );
}

fn uri_location_discovery() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        mime_headers,
        mime_location_field::{self as field, retained},
        mime_location_fields::{Cursor, Error, Input, Status},
        mime_location_literal as literal, mime_location_selection as selection,
        nfc::{self, HeaderBudget},
        ports::{Deadline, Tick},
    };
    let literal_source = format!("Content-Location: ../{}\r\n\r\nbody", "a%2F/".repeat(8192));
    let word_source = format!(
        "Content-Location: {}\r\n\r\nbody",
        "=?utf-8?Q?e=CC=81?= \r\n ".repeat(1024)
    );
    let malformed_duplicates = format!(
        "{}Content-Location: ../ok\r\n\r\n",
        "Content-Location: a%\r\n".repeat(1024)
    );
    let ignored_duplicates = format!(
        "Content-Location: ../ok\r\n{}\r\n",
        "Content-Location: (bad\r\n".repeat(1024)
    );
    let nested_source = format!(
        "Content-Location: {}x{}\r\nContent-Location: ../ok\r\n\r\n",
        "(".repeat(33),
        ")".repeat(33)
    );
    let mut backing =
        vec![0; retained::capacity_bound(literal_source.len().max(word_source.len())).unwrap()];
    let before = COUNTERS.snapshot();
    for (source, source_end, limit, output, records, wanted, fault) in [
        (
            literal_source.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            2_000_000,
            true,
            None,
        ),
        (
            word_source.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            2_000_000,
            true,
            None,
        ),
        (
            malformed_duplicates.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            2_000_000,
            true,
            None,
        ),
        (
            ignored_duplicates.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            2_000_000,
            true,
            None,
        ),
        (
            b"Content-Location: \r\nContent-Location: ../later\r\n\r\n".as_slice(),
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            2_000_000,
            true,
            None,
        ),
        (
            b"Content-Location: =?utf-8?Q?=FF?=\r\n\r\n",
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            2_000_000,
            true,
            None,
        ),
        (
            b"Content-Location: a%\r\n\r\n",
            SourceEnd::Eof,
            1_000_000,
            0,
            2_000_000,
            false,
            None,
        ),
        (
            b"Other: x\r\n\r\n",
            SourceEnd::Eof,
            1_000_000,
            0,
            2_000_000,
            false,
            None,
        ),
        (
            b"Content-Location: ../ok\r\n",
            SourceEnd::Prefix,
            1_000_000,
            1_000_000,
            2_000_000,
            false,
            Some(Error::Truncated),
        ),
        (
            b"Content-Location: ../ok\r\nOther: x\r\n\r\n",
            SourceEnd::Eof,
            27,
            1_000_000,
            2_000_000,
            false,
            Some(Error::Headers(mime_headers::Error::HeaderLimit)),
        ),
        (
            b"Content-Location: ../ok\r\n\r\n",
            SourceEnd::Eof,
            0,
            1_000_000,
            2_000_000,
            false,
            Some(Error::Headers(mime_headers::Error::HeaderLimit)),
        ),
        (
            nested_source.as_bytes(),
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            2_000_000,
            false,
            Some(Error::Location(field::Error::Selection(
                selection::Error::NestingLimit,
            ))),
        ),
        (
            b"Content-Location: ../ok\r\nContent-Location: ../later\r\n\r\n",
            SourceEnd::Eof,
            1_000_000,
            1_000_000,
            0,
            false,
            Some(Error::Headers(mime_headers::Error::Work(Stop::Records))),
        ),
        (
            b"Content-Location: ../ok\r\nContent-Location: ../later\r\n\r\n",
            SourceEnd::Eof,
            1_000_000,
            0,
            2_000_000,
            false,
            Some(Error::Location(field::Error::Literal(
                literal::Error::Work(Stop::OutputBytes),
            ))),
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let input = Input {
            source,
            base: 100,
            header_limit: limit,
            source_end,
        };
        let mut cursor = Cursor::new(input, &mut work, &mut budget).unwrap();
        let mut result = None;
        for _ in 0..2_000_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => assert!(cursor.selection().is_none()),
                Ok(Status::Complete) => {
                    result = Some(Ok(()));
                    break;
                }
                Err(error) => {
                    result = Some(Err(error));
                    break;
                }
            }
        }
        assert_eq!(result, Some(fault.map_or(Ok(()), Err)));
        if let Some(error) = fault {
            assert!(cursor.selection().is_none());
            assert_eq!(cursor.poll(Tick(100)), Err(error));
            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        } else {
            let selected = cursor.selection().unwrap();
            assert_eq!(selected.content_location.is_some(), wanted);
            assert_eq!(selected.location_end.is_some(), wanted);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (work, budget, selected) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                pointers
            );
            let value = selected.content_location.map(|field| {
                td_header::resident::slice(source, 100, field.value_start..field.value_end).unwrap()
            });
            let mut projection = retained::Cursor::new(value, &mut backing, work, budget);
            let mut complete = false;
            for _ in 0..2_000_000 {
                if projection.poll(Tick(1)).unwrap() == retained::Status::Complete {
                    complete = true;
                    break;
                }
                assert!(projection.view().is_none());
            }
            assert!(complete);
            let (retained, work, budget) = projection.finish(Tick(1)).unwrap();
            assert_eq!(retained.value.is_some(), wanted);
            assert_eq!(retained.end, selected.location_end);
            black_box(retained.value);
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                pointers
            );
            let mut next = Cursor::new(
                Input {
                    source: b"Other: x\r\n\r\n",
                    base: 0,
                    header_limit: 100,
                    source_end: SourceEnd::Eof,
                },
                work,
                budget,
            )
            .unwrap();
            let mut complete = false;
            for _ in 0..1000 {
                if next.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            assert_eq!(next.poll(Tick(100)), Ok(Status::Complete));
            let deadline = Error::Admission(nfc::Error::Work(Stop::Deadline));
            assert_eq!(next.check_deadline(Tick(100)), Err(deadline));
            assert!(next.selection().is_none());
            assert_eq!(next.finish(Tick(1)).err(), Some(deadline));
        }
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "Rust allocation in resident location discovery"
    );
}

fn uri_location_retention() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_field::{
            self as field, json,
            retained::{Cursor, Error, Status},
        },
        mime_location_literal as literal, mime_location_selection as selection,
        mime_location_word as word,
        nfc::{self, HeaderBudget},
        ports::{Deadline, Tick},
    };
    let long_literal = format!("../{}", "a%2F/".repeat(8192));
    let long_words = "=?utf-8?Q?e=CC=81?= \r\n ".repeat(1024);
    let mut bad_literal = long_literal.clone();
    bad_literal.push('%');
    let mut backing = vec![
        0xa5;
        td_mta::mime_location_field::retained::capacity_bound(
            bad_literal.len().max(long_words.len())
        )
        .unwrap()
    ];
    let mut next_backing = [0; 64];
    let before = COUNTERS.snapshot();
    for (source, size, wanted, encoded, problem, fault, output, records) in [
        (
            Some(long_literal.as_bytes()),
            long_literal.len() + 2,
            long_literal.len() + 2,
            false,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            Some(long_words.as_bytes()),
            3074,
            3074,
            true,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            Some(b"=?ascii?Q?=00=22=5C=0A?=".as_slice()),
            6,
            6,
            true,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            Some(b"(x) =?utf-8?Q?=FF?= =?ascii?Q?ok?= (tail)"),
            7,
            7,
            true,
            true,
            None,
            100_000,
            2_000_000,
        ),
        (None, 0, 0, false, false, None, 0, 0),
        (Some(b""), 2, 2, false, false, None, 100_000, 2_000_000),
        (
            Some(b"=?unknown?Q?a?="),
            b"=?unknown?Q?a?=".len() + 2,
            b"=?unknown?Q?a?=".len() + 2,
            false,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            Some(bad_literal.as_bytes()),
            bad_literal.len() + 2,
            0,
            false,
            false,
            Some(Error::Projection(json::Error::Source(
                field::Error::Literal(literal::Error::MalformedUri),
            ))),
            100_000,
            2_000_000,
        ),
        (
            Some(b"../a\r\nX"),
            64,
            0,
            false,
            false,
            Some(Error::Projection(json::Error::Source(field::Error::Words(
                word::Error::MalformedFold,
            )))),
            100_000,
            2_000_000,
        ),
        (
            Some(b"(broken"),
            64,
            0,
            false,
            false,
            Some(Error::Projection(json::Error::Source(
                field::Error::Selection(selection::Error::Malformed),
            ))),
            100_000,
            2_000_000,
        ),
        (
            Some(b"../a"),
            0,
            0,
            false,
            false,
            Some(Error::OutputCapacity),
            100_000,
            2_000_000,
        ),
        (
            Some(b"../a"),
            1,
            0,
            false,
            false,
            Some(Error::OutputCapacity),
            100_000,
            2_000_000,
        ),
        (
            Some(b"../a"),
            64,
            0,
            false,
            false,
            Some(Error::Projection(json::Error::Source(
                field::Error::Selection(selection::Error::Work(Stop::Records)),
            ))),
            100_000,
            0,
        ),
        (
            Some(b"../a"),
            64,
            0,
            false,
            false,
            Some(Error::Projection(json::Error::Source(
                field::Error::Selection(selection::Error::Work(Stop::OutputBytes)),
            ))),
            0,
            2_000_000,
        ),
        (
            Some(b""),
            2,
            0,
            false,
            false,
            Some(Error::Projection(json::Error::Source(
                field::Error::Admission(nfc::Error::Work(Stop::OutputBytes)),
            ))),
            1,
            2_000_000,
        ),
        (
            Some(b"=?ascii?Q?=22?="),
            16,
            0,
            true,
            false,
            Some(Error::Projection(json::Error::Source(field::Error::Words(
                word::Error::Work(Stop::OutputBytes),
            )))),
            2,
            2_000_000,
        ),
        (
            Some(b"../a"),
            6,
            0,
            false,
            false,
            Some(Error::Projection(json::Error::Source(
                field::Error::Literal(literal::Error::Work(Stop::OutputBytes)),
            ))),
            2,
            2_000_000,
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(
            source,
            backing.get_mut(..size).unwrap(),
            &mut work,
            &mut budget,
        );
        let mut error = None;
        let mut complete = false;
        for _ in 0..2_000_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => assert!(cursor.view().is_none()),
                Ok(Status::Complete) => {
                    complete = true;
                    break;
                }
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            }
        }
        assert_eq!(error, fault);
        if let Some(error) = fault {
            assert!(cursor.view().is_none());
            assert_eq!(cursor.poll(Tick(100)), Err(error));
            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        } else {
            assert!(complete);
            let view = cursor.view().unwrap();
            assert_eq!(view.value.map_or(0, <[u8]>::len), wanted);
            if let Some(end) = view.end {
                assert_eq!(
                    (end.encoded_words, end.encoding_problem),
                    (encoded, problem)
                );
                assert!(source
                    .unwrap()
                    .get(end.spelling.start..end.spelling.end)
                    .is_some());
            } else {
                assert!(source.is_none());
            }
            assert_eq!(view.value.is_some(), source.is_some());
            black_box(view.value);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (_, work, budget) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                pointers
            );
            let mut next = Cursor::new(
                if source.is_some() {
                    Some(b"../x")
                } else {
                    None
                },
                &mut next_backing,
                work,
                budget,
            );
            let mut complete = false;
            for _ in 0..1000 {
                if next.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
                assert!(next.view().is_none());
            }
            assert!(complete);
            assert_eq!(next.poll(Tick(100)), Ok(Status::Complete));
            let deadline = Error::Admission(nfc::Error::Work(Stop::Deadline));
            assert_eq!(next.check_deadline(Tick(100)), Err(deadline));
            assert!(next.view().is_none());
            assert_eq!(next.finish(Tick(1)).err(), Some(deadline));
        }
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "Rust allocation in retained location JSON"
    );
}

fn uri_location_json() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_field::{
            self as field,
            json::{Cursor, Error, Status},
        },
        mime_location_literal as literal, mime_location_selection as selection,
        mime_location_word as word,
        nfc::{self, HeaderBudget},
        ports::{Deadline, Tick},
    };
    let long_literal = format!("../{}", "a%2F/".repeat(8192));
    let long_words = "=?utf-8?Q?e=CC=81?= \r\n ".repeat(1024);
    let mut bad_literal = long_literal.clone();
    bad_literal.push('%');
    let before = COUNTERS.snapshot();
    for (source, bytes, encoded, problem, fault, output, records) in [
        (
            long_literal.as_bytes(),
            long_literal.len() + 2,
            false,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            long_words.as_bytes(),
            3074,
            true,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            b"=?ascii?Q?=00=22=5C=0A?=".as_slice(),
            6,
            true,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            b"(x) =?utf-8?Q?=FF?= =?ascii?Q?ok?= (tail)",
            7,
            true,
            true,
            None,
            100_000,
            2_000_000,
        ),
        (
            b"=?utf-8?Q?=F0=9F=90=88?=",
            6,
            true,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (b"", 2, false, false, None, 100_000, 2_000_000),
        (
            b"=?unknown?Q?a?=",
            b"=?unknown?Q?a?=".len() + 2,
            false,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            bad_literal.as_bytes(),
            1,
            false,
            false,
            Some(Error::Source(field::Error::Literal(
                literal::Error::MalformedUri,
            ))),
            100_000,
            2_000_000,
        ),
        (
            b"../a\r\nX",
            1,
            false,
            false,
            Some(Error::Source(field::Error::Words(
                word::Error::MalformedFold,
            ))),
            100_000,
            2_000_000,
        ),
        (
            b"(broken",
            1,
            false,
            false,
            Some(Error::Source(field::Error::Selection(
                selection::Error::Malformed,
            ))),
            100_000,
            2_000_000,
        ),
        (
            b"../a",
            1,
            false,
            false,
            Some(Error::Source(field::Error::Selection(
                selection::Error::Work(Stop::Records),
            ))),
            100_000,
            0,
        ),
        (
            b"../a",
            0,
            false,
            false,
            Some(Error::Source(field::Error::Selection(
                selection::Error::Work(Stop::OutputBytes),
            ))),
            0,
            2_000_000,
        ),
        (
            b"",
            1,
            false,
            false,
            Some(Error::Source(field::Error::Admission(nfc::Error::Work(
                Stop::OutputBytes,
            )))),
            1,
            2_000_000,
        ),
        (
            b"=?ascii?Q?=22?=",
            1,
            true,
            false,
            Some(Error::Source(field::Error::Words(word::Error::Work(
                Stop::OutputBytes,
            )))),
            2,
            2_000_000,
        ),
        (
            b"../a",
            1,
            false,
            false,
            Some(Error::Source(field::Error::Literal(literal::Error::Work(
                Stop::OutputBytes,
            )))),
            2,
            2_000_000,
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let mut count = 0;
        let mut error = None;
        let mut complete = false;
        for _ in 0..2_000_000 {
            let mut window = [0; 1];
            match cursor.poll(Tick(1), &mut window) {
                Ok(progress) => {
                    count += progress.written;
                    black_box(&window);
                    if progress.status == Status::Complete {
                        complete = true;
                        break;
                    }
                    assert_eq!(cursor.end(), None);
                    assert_eq!(
                        cursor.poll(Tick(1), &mut []).unwrap().status,
                        Status::NeedOutput
                    );
                }
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            }
        }
        assert_eq!(count, bytes);
        assert_eq!(error, fault);
        if let Some(error) = fault {
            assert_eq!(cursor.end(), None);
            assert_eq!(cursor.poll(Tick(100), &mut [0]), Err(error));
            assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        } else {
            assert!(complete);
            let end = cursor.end().unwrap();
            assert_eq!(
                (end.encoded_words, end.encoding_problem),
                (encoded, problem)
            );
            assert!(source.get(end.spelling.start..end.spelling.end).is_some());
            assert_eq!(
                cursor.poll(Tick(100), &mut []).unwrap().status,
                Status::Complete
            );
            let (work, budget, _) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                pointers
            );
            let mut next = Cursor::new(b"=?ascii?Q?=22=5C?=", work, budget);
            let mut complete = false;
            let mut count = 0;
            for _ in 0..1000 {
                let progress = next.poll(Tick(1), &mut [0]).unwrap();
                count += progress.written;
                if progress.status == Status::Complete {
                    complete = true;
                    break;
                }
                assert_eq!(next.end(), None);
            }
            assert!(complete);
            assert_eq!(count, 6);
            assert_eq!(
                next.poll(Tick(100), &mut []).unwrap().status,
                Status::Complete
            );
            let deadline = Error::Source(field::Error::Admission(nfc::Error::Work(Stop::Deadline)));
            assert_eq!(next.check_deadline(Tick(100)), Err(deadline));
            assert_eq!(next.end(), None);
            assert_eq!(next.finish(Tick(1)).err(), Some(deadline));
        }
    }
    assert_eq!(
        COUNTERS.snapshot(),
        before,
        "Rust allocation in URI location JSON"
    );
}

fn uri_location_fields() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_location_field::{Cursor, Error, Status},
        mime_location_literal as literal, mime_location_selection as selection,
        mime_location_word as word,
        nfc::{self, HeaderBudget},
        ports::{Deadline, Tick},
    };
    let long_literal = format!("../{}", "a%2F/".repeat(8192));
    let long_words = "=?utf-8?Q?e=CC=81?= \r\n ".repeat(1024);
    let mut bad_literal = long_literal.clone();
    bad_literal.push('%');
    let before = COUNTERS.snapshot();
    for (source, bytes, encoded, problem, fault, output, records) in [
        (
            long_literal.as_bytes(),
            long_literal.len(),
            false,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            long_words.as_bytes(),
            3072,
            true,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            b"(x) =?utf-8?Q?=FF?= =?ascii?Q?ok?= (tail)".as_slice(),
            5,
            true,
            true,
            None,
            100_000,
            2_000_000,
        ),
        (b"", 0, false, false, None, 100_000, 2_000_000),
        (
            b"=?ascii?Q?a?= =?unknown?Q?b?=",
            b"=?ascii?Q?a?==?unknown?Q?b?=".len(),
            false,
            false,
            None,
            100_000,
            2_000_000,
        ),
        (
            bad_literal.as_bytes(),
            0,
            false,
            false,
            Some(Error::Literal(literal::Error::MalformedUri)),
            100_000,
            2_000_000,
        ),
        (
            b"=?ascii?Q?a?=\r\n",
            0,
            false,
            false,
            Some(Error::Words(word::Error::MalformedFold)),
            100_000,
            2_000_000,
        ),
        (
            b"(broken",
            0,
            false,
            false,
            Some(Error::Selection(selection::Error::Malformed)),
            100_000,
            2_000_000,
        ),
        (
            b"../a",
            0,
            false,
            false,
            Some(Error::Selection(selection::Error::Work(Stop::Records))),
            100_000,
            0,
        ),
        (
            b"=?ascii?Q?a?= =?ascii?Q?b?=",
            1,
            true,
            false,
            Some(Error::Words(word::Error::Work(Stop::OutputBytes))),
            1,
            2_000_000,
        ),
        (
            b"../a",
            1,
            false,
            false,
            Some(Error::Literal(literal::Error::Work(Stop::OutputBytes))),
            1,
            2_000_000,
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let pointers = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let mut count = 0;
        let mut result = None;
        let mut complete = false;
        for _ in 0..2_000_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Scalar(value)) => {
                    assert_eq!(cursor.end(), None);
                    count += value.len_utf8();
                    black_box(value);
                }
                Ok(Status::Yield) => assert_eq!(cursor.end(), None),
                Ok(Status::Complete) => {
                    complete = true;
                    break;
                }
                Err(error) => {
                    result = Some(error);
                    break;
                }
            }
        }
        assert_eq!(count, bytes);
        assert_eq!(result, fault);
        if let Some(error) = fault {
            assert_eq!(cursor.end(), None);
            assert_eq!(cursor.poll(Tick(100)), Err(error));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
        } else {
            assert!(complete);
            let end = cursor.end().unwrap();
            assert_eq!(
                (end.encoded_words, end.encoding_problem),
                (encoded, problem)
            );
            assert!(source.get(end.spelling.start..end.spelling.end).is_some());
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (work, budget, _) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                pointers
            );
            let mut cursor = Cursor::new(b"=?ascii?Q?x?=", work, budget);
            let mut complete = false;
            for _ in 0..1000 {
                match cursor.poll(Tick(1)).unwrap() {
                    Status::Scalar(value) => {
                        assert_eq!(value, 'x');
                        black_box(value);
                    }
                    Status::Yield => {}
                    Status::Complete => {
                        complete = true;
                        break;
                    }
                }
            }
            assert!(complete);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Admission(nfc::Error::Work(Stop::Deadline)))
            );
            assert_eq!(cursor.end(), None);
            assert!(cursor.finish(Tick(1)).is_err());
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "location field composition allocated");
}

fn uri_unfold_values() {
    use td_header::uri::unfold::{Cursor, Error, Status};
    struct UnfoldWork {
        calls: usize,
        cut: Option<usize>,
    }
    impl td_header::Work for UnfoldWork {
        type Error = u8;
        fn charge(&mut self, charge: td_header::Charge) -> Result<(), u8> {
            assert!(charge.visits <= 1 && charge.records <= 1);
            let call = self.calls;
            self.calls += 1;
            if self.cut == Some(call) {
                Err(77)
            } else {
                Ok(())
            }
        }
    }
    const _: () = assert!(std::mem::size_of::<Cursor<'_, td_mta::header_urls::Error>>() <= 64);
    let long = "a%\r\n 2F\t".repeat(4096);
    let before = COUNTERS.snapshot();
    for (source, expected, cut) in [
        (b"".as_slice(), None, None),
        (b"../a%\r\n 2Fb", None, None),
        (long.as_bytes(), None, None),
        (b"=?utf-8?Q?a_\n b?=", None, None),
        (b"a\r\nX", Some(Error::Malformed), None),
        (b"a\r", Some(Error::Malformed), None),
        (b"a\r\n", Some(Error::Malformed), None),
        (b"a\n", Some(Error::Malformed), None),
        (b"a", Some(Error::Work(77)), Some(0)),
        (b"a", Some(Error::Work(77)), Some(1)),
    ] {
        let mut cursor = Cursor::new(source);
        let mut work = UnfoldWork { calls: 0, cut };
        let mut result = None;
        for _ in 0..=source.len() {
            match cursor.poll(&mut work) {
                Ok(Status::Octet { byte, position }) => {
                    assert_eq!(source.get(position), Some(&byte));
                    black_box(byte);
                }
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => break,
                Err(error) => {
                    result = Some(error);
                    break;
                }
            }
        }
        assert_eq!(result, expected);
        if let Some(error) = expected {
            assert!(!cursor.is_complete());
            let calls = work.calls;
            work.cut = None;
            assert_eq!(cursor.poll(&mut work), Err(error));
            assert_eq!(cursor.check_work(&mut work), Err(error));
            assert_eq!(work.calls, calls);
        } else {
            assert!(cursor.is_complete());
            let calls = work.calls;
            assert_eq!(cursor.poll(&mut work), Ok(Status::Complete));
            assert_eq!(work.calls, calls);
            cursor.check_work(&mut work).unwrap();
            work.cut = Some(work.calls);
            assert_eq!(cursor.check_work(&mut work), Err(Error::Work(77)));
            assert!(!cursor.is_complete());
            assert_eq!(cursor.poll(&mut work), Err(Error::Work(77)));
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "URI wire unfolding allocated");
}

fn uri_reference_values() {
    use td_header::uri::{Error, Validator};
    struct RefWork {
        records: u64,
        refuse: bool,
    }
    impl td_header::Work for RefWork {
        type Error = u8;
        fn charge(&mut self, charge: td_header::Charge) -> Result<(), u8> {
            assert_eq!(charge.visits, 0);
            if self.refuse {
                return Err(77);
            }
            self.records = self.records.checked_sub(charge.records).ok_or(77)?;
            Ok(())
        }
    }
    let long = format!(
        "./{}?{}#{}",
        "a/".repeat(4096),
        "x%20".repeat(4096),
        "s".repeat(4096)
    );
    let before = COUNTERS.snapshot();
    for (source, expected, records) in [
        (b"".as_slice(), None, 64),
        (b"../g?x#s", None, 64),
        (long.as_bytes(), None, 64),
        (b"http://user:pass@host:42/a", None, 64),
        (b"http://[::1]/", None, 64),
        (b"//[::1]/", None, 64),
        (b"//[v1.a:!]/", None, 64),
        (b"1g:h", Some(Error::Malformed), 64),
        (b"g%a", Some(Error::Malformed), 64),
        (b"//[1::2::3]/", Some(Error::Malformed), 64),
        (b"//[::1]/", Some(Error::Work(77)), 0),
    ] {
        let mut work = RefWork {
            records,
            refuse: false,
        };
        let mut validator = Validator::reference();
        let mut result = Ok(());
        for byte in source {
            if let Err(error) = validator.push(*byte, &mut work) {
                result = Err(error);
                break;
            }
        }
        if result.is_ok() {
            result = validator.finish();
        }
        assert_eq!(result.err(), expected);
        if let Some(error) = expected {
            assert!(!validator.is_complete());
            assert_eq!(validator.finish(), Err(error));
            assert_eq!(validator.push(b'x', &mut work), Err(error));
            assert_eq!(validator.check_work(&mut work), Err(error));
        } else {
            assert!(validator.is_complete());
            validator.finish().unwrap();
            let remaining = work.records;
            validator.check_work(&mut work).unwrap();
            assert_eq!(work.records, remaining);
            work.refuse = true;
            assert_eq!(validator.check_work(&mut work), Err(Error::Work(77)));
            assert!(!validator.is_complete());
            assert_eq!(validator.finish(), Err(Error::Work(77)));
            assert_eq!(validator.push(b'x', &mut work), Err(Error::Work(77)));
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "URI-reference spelling allocated");
}

fn content_id_json() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_content_id::{
            self,
            json::{Cursor, Error, Status},
        },
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("<{}@b>", "a".repeat(8192));
    let deep = format!("<a@b> {}x{}", "(".repeat(33), ")".repeat(33));
    let before = COUNTERS.snapshot();
    for (source, fault, records, output_bytes) in [
        (b"(x) <A@b> (y)".as_slice(), None, 100_000_000, 100_000_000),
        (
            "(🐈) <e\u{301}@EXAMPLE>".as_bytes(),
            None,
            100_000_000,
            100_000_000,
        ),
        ("<\u{fdd0}@b>".as_bytes(), None, 100_000_000, 100_000_000),
        (b"<\"a\\b\"@c>", None, 100_000_000, 100_000_000),
        (long.as_bytes(), None, 100_000_000, 100_000_000),
        (
            b"<local>",
            Some(Error::Source(mime_content_id::Error::Malformed)),
            100_000_000,
            100_000_000,
        ),
        (
            deep.as_bytes(),
            Some(Error::Source(mime_content_id::Error::NestingLimit)),
            100_000_000,
            100_000_000,
        ),
        (
            b"<a@b>",
            Some(Error::Source(mime_content_id::Error::Work(Stop::Records))),
            0,
            100_000_000,
        ),
        (
            b"<a@b>",
            Some(Error::Source(mime_content_id::Error::Work(
                Stop::OutputBytes,
            ))),
            100_000_000,
            0,
        ),
    ] {
        for trial in 0..if fault.is_some() { 1 } else { 2 } {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records,
                    output_bytes,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
            let mut cursor = Cursor::new(black_box(source), &mut work, &mut budget);
            assert_eq!(cursor.end(), None);
            let mut complete = false;
            let mut failed = None;
            let mut byte = [0];
            for turn in 0..200_000 {
                if turn % 7 == 0 {
                    if let Err(error) = cursor.poll(Tick(1), &mut []) {
                        failed = Some(error);
                        break;
                    }
                }
                match cursor.poll(Tick(1), &mut byte) {
                    Ok(progress) => {
                        black_box(byte.get(..progress.written).unwrap());
                        if progress.status == Status::Complete {
                            complete = true;
                            break;
                        }
                        assert_eq!(cursor.end(), None);
                    }
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(failed, fault);
            if let Some(error) = fault {
                assert_eq!(cursor.end(), None);
                assert_eq!(cursor.poll(Tick(1), &mut byte), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            assert!(complete);
            assert_eq!(
                cursor.poll(Tick(100), &mut []).unwrap().status,
                Status::Complete
            );
            if trial == 1 {
                let error = Error::Source(mime_content_id::Error::Work(Stop::Deadline));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert_eq!(cursor.end(), None);
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            let (work, budget, end) = cursor.finish(Tick(1)).unwrap();
            black_box(end);
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
            let mut next = mime_content_id::Cursor::new(b"<next@id>", work, budget);
            let mut complete = false;
            for _ in 0..1000 {
                match next.poll(Tick(1)).unwrap() {
                    mime_content_id::Status::Scalar(c) => {
                        black_box(c);
                    }
                    mime_content_id::Status::Complete => {
                        complete = true;
                        break;
                    }
                    _ => {}
                }
            }
            assert!(complete);
            let (work, budget) = next.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Content-ID JSON projection allocated");
}

fn content_language_json() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_language::{
            self,
            json::{Cursor, Error, Status},
        },
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("({})x{}", "🐈".repeat(1024), "-abcdefgh".repeat(1024));
    let many = "EN-us, fr, ".repeat(1024) + "EN-us";
    let deep = format!("en {}x{}", "(".repeat(33), ")".repeat(33));
    let mut baseline_work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 100_000_000,
            output_bytes: 100_000_000,
            ..Charge::default()
        },
    );
    let mut baseline_budget = HeaderBudget::new();
    {
        let mut baseline =
            mime_language::Cursor::new(b"en-US", &mut baseline_work, &mut baseline_budget);
        loop {
            if matches!(
                baseline.poll(Tick(1)).unwrap(),
                mime_language::Status::Tag(_)
            ) {
                break;
            }
        }
    }
    let replay_io_cap = 100_000_000 - baseline_work.remaining().io_bytes + 2;
    let replay_header_cap =
        HeaderBudget::new().steps_remaining() - baseline_budget.steps_remaining() + 2;
    let preload = "x".to_owned() + &"-abcdefgh".repeat(900_000);
    let mut preload_work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 100_000_000,
            ..Charge::default()
        },
    );
    let mut preload_budget = HeaderBudget::new();
    let initial_steps = preload_budget.steps_remaining();
    {
        let mut preload_cursor =
            mime_language::Cursor::new(preload.as_bytes(), &mut preload_work, &mut preload_budget);
        // The leading CFWS poll charges one visit/step pair; each tag byte
        // then charges two steps. Leave two or three replay steps available.
        let polls = (initial_steps - replay_header_cap) / 2;
        for _ in 0..polls {
            preload_cursor.poll(Tick(1)).unwrap();
        }
    }
    assert!((replay_header_cap..=replay_header_cap + 1).contains(&preload_budget.steps_remaining()));
    let mut prepared_budget = Some(preload_budget);
    let before = COUNTERS.snapshot();
    for (source, fault, records, output_bytes, io_bytes, limited_header) in [
        (
            b"(x) EN-us, en-US, x-Ab12 (tail)".as_slice(),
            None,
            100_000_000,
            100_000_000,
            100_000_000,
            false,
        ),
        (
            long.as_bytes(),
            None,
            100_000_000,
            100_000_000,
            100_000_000,
            false,
        ),
        (
            many.as_bytes(),
            None,
            100_000_000,
            100_000_000,
            100_000_000,
            false,
        ),
        (
            b"en,",
            Some(Error::Source(mime_language::Error::Malformed)),
            100_000_000,
            100_000_000,
            100_000_000,
            false,
        ),
        (
            deep.as_bytes(),
            Some(Error::Source(mime_language::Error::NestingLimit)),
            100_000_000,
            100_000_000,
            100_000_000,
            false,
        ),
        (
            b"en",
            Some(Error::Source(mime_language::Error::Work(Stop::Records))),
            0,
            100_000_000,
            100_000_000,
            false,
        ),
        (
            b"en",
            Some(Error::Source(mime_language::Error::Work(Stop::OutputBytes))),
            100_000_000,
            0,
            100_000_000,
            false,
        ),
        (
            b"en-US",
            Some(Error::Source(mime_language::Error::Work(Stop::IoBytes))),
            100_000_000,
            100_000_000,
            replay_io_cap,
            false,
        ),
        (
            b"en-US",
            Some(Error::Source(mime_language::Error::InterpretationLimit)),
            100_000_000,
            100_000_000,
            100_000_000,
            true,
        ),
        (
            b"en-US",
            Some(Error::Source(mime_language::Error::Work(Stop::OutputBytes))),
            100_000_000,
            4,
            100_000_000,
            false,
        ),
    ] {
        for trial in 0..if fault.is_some() { 1 } else { 2 } {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes,
                    records,
                    output_bytes,
                    ..Charge::default()
                },
            );
            let mut budget = if limited_header {
                prepared_budget.take().unwrap()
            } else {
                HeaderBudget::new()
            };
            let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
            let mut cursor = Cursor::new(black_box(source), &mut work, &mut budget);
            assert!(!cursor.is_complete());
            let mut emitted = 0;
            let mut complete = false;
            let mut failed = None;
            let mut byte = [0];
            for turn in 0..200_000 {
                if turn % 7 == 0 {
                    if let Err(error) = cursor.poll(Tick(1), &mut []) {
                        failed = Some(error);
                        break;
                    }
                }
                match cursor.poll(Tick(1), &mut byte) {
                    Ok(progress) => {
                        emitted += progress.written;
                        black_box(byte.get(..progress.written).unwrap());
                        if progress.status == Status::Complete {
                            complete = true;
                            break;
                        }
                        assert!(!cursor.is_complete());
                    }
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(failed, fault);
            if let Some(error) = fault {
                if source == b"en-US" {
                    assert!(emitted >= 4);
                }
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(1), &mut byte), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            assert!(complete);
            assert_eq!(
                cursor.poll(Tick(100), &mut []).unwrap().status,
                Status::Complete
            );
            if trial == 1 {
                let error = Error::Source(mime_language::Error::Work(Stop::Deadline));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert!(!cursor.is_complete());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            let (work, budget) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
            let mut next = mime_language::Cursor::new(b"x-next", work, budget);
            let mut complete = false;
            for _ in 0..1000 {
                match next.poll(Tick(1)).unwrap() {
                    mime_language::Status::Tag(c) => {
                        black_box(c);
                    }
                    mime_language::Status::Complete => {
                        complete = true;
                        break;
                    }
                    _ => {}
                }
            }
            assert!(complete);
            let (work, budget) = next.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Content-Language JSON projection allocated");
}

fn composed_part_label_json() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        mime_label_fields::json as labels,
        mime_metadata::Context,
        mime_part_headers::{
            self,
            label_json::{Backing, Cursor, Error, Status},
        },
        nfc::{self, HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let normal = concat!(
        "Content-ID: bad\r\nContent-ID: <A@B>\r\n",
        "Content-Language: bad_\r\nContent-Language: en, en\r\n",
        "\r\nContent-ID: <body@id>"
    )
    .as_bytes();
    let long = format!(
        concat!(
            "Content-Type: text/plain\r\n",
            "Content-Disposition: inline;filename*=utf-8''e%CC%81\r\n",
            "Content-ID: <{}@B>\r\nContent-Language: x{}\r\n\r\nBODY"
        ),
        "a".repeat(8192),
        "-abcdefgh".repeat(1024)
    );
    let mut heads = [0; 128];
    let mut charset = [0; 32];
    let mut filename = [0; 128];
    let mut id = vec![0; 20_000];
    let mut lang = vec![0; 20_000];
    let before = COUNTERS.snapshot();
    for (source, id_cap, lang_cap, records, output, failed) in [
        (
            b"\r\nContent-ID: <body@id>".as_slice(),
            0,
            0,
            100_000_000,
            100_000_000,
            false,
        ),
        (normal, 5, 11, 100_000_000, 100_000_000, false),
        (
            long.as_bytes(),
            20_000,
            20_000,
            100_000_000,
            100_000_000,
            false,
        ),
        (normal, 0, 11, 100_000_000, 100_000_000, true),
        (normal, 5, 0, 100_000_000, 100_000_000, true),
        (normal, 5, 11, 0, 100_000_000, true),
        (normal, 5, 11, 100_000_000, 25, true),
    ] {
        for trial in 0..if failed { 1 } else { 2 } {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records,
                    output_bytes: output,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let identity = (
                std::ptr::from_ref(&work),
                std::ptr::from_ref(&budget),
                std::ptr::from_ref(&scratch),
            );
            let mut cursor = Cursor::new(
                mime_part_headers::Entity {
                    source: black_box(source),
                    base: 37,
                    source_end: SourceEnd::Eof,
                    header_limit: 1_000_000,
                    context: Context::Normal,
                },
                Backing {
                    headers: mime_part_headers::Backing {
                        heads: &mut heads,
                        charset: &mut charset,
                        filename: &mut filename,
                    },
                    labels: labels::Backing {
                        content_id: id.get_mut(..id_cap).unwrap(),
                        content_language: lang.get_mut(..lang_cap).unwrap(),
                    },
                    content_location: &mut [],
                },
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            let mut completion = false;
            let mut error = None;
            for _ in 0..200_000 {
                match cursor.poll(Tick(1)) {
                    Ok(Status::Complete) => {
                        completion = true;
                        break;
                    }
                    Ok(Status::Yield) => {
                        assert!(cursor.value().is_none());
                        assert!(!cursor.is_complete());
                    }
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            if failed {
                let error = error.unwrap();
                let expected = if records == 0 {
                    Error::Headers(mime_part_headers::Error::Admission(nfc::Error::Work(
                        Stop::Records,
                    )))
                } else if output == 25 {
                    Error::Labels(labels::Error::ContentLanguage(
                        td_mta::mime_language::json::Error::Source(
                            td_mta::mime_language::Error::Work(Stop::OutputBytes),
                        ),
                    ))
                } else {
                    Error::Labels(labels::Error::OutputCapacity)
                };
                assert_eq!(error, expected);
                assert!(!completion);
                assert!(cursor.value().is_none());
                assert!(!cursor.is_complete());
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            assert!(completion);
            assert!(error.is_none());
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            if trial == 1 {
                let error = Error::Admission(nfc::Error::Work(Stop::Deadline));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert!(cursor.value().is_none());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            let (view, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
            black_box(view.headers);
            black_box(view.labels.content_id);
            black_box(view.labels.content_id_end);
            black_box(view.labels.content_language);
            assert_eq!(
                (
                    std::ptr::from_ref(&*work),
                    std::ptr::from_ref(&*budget),
                    std::ptr::from_ref(&*scratch)
                ),
                identity
            );
            let mut next = td_mta::mime_language::Cursor::new(b"fr", work, budget);
            let mut complete = false;
            for _ in 0..1000 {
                if next.poll(Tick(1)).unwrap() == td_mta::mime_language::Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            let (work, budget) = next.finish(Tick(1)).unwrap();
            assert_eq!(
                (
                    std::ptr::from_ref(&*work),
                    std::ptr::from_ref(&*budget),
                    std::ptr::from_ref(&*scratch)
                ),
                identity
            );
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "composed part label JSON allocated");
}

fn retained_label_json() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_content_id,
        mime_label_fields::json::{Backing, Cursor, Error, Status, Values},
        mime_language,
        nfc::{self, HeaderBudget},
        ports::{Deadline, Tick},
    };
    let long_id = format!("<{}@B>", "a".repeat(8192));
    let long_language = "x".to_owned() + &"-abcdefgh".repeat(1024);
    let mut id_output = vec![0; 20_000];
    let mut language_output = vec![0; 20_000];
    let id_bad = Error::ContentId(mime_content_id::json::Error::Source(
        mime_content_id::Error::Malformed,
    ));
    let language_bad = Error::ContentLanguage(mime_language::json::Error::Source(
        mime_language::Error::Malformed,
    ));
    let records_bad = Error::ContentId(mime_content_id::json::Error::Source(
        mime_content_id::Error::Work(Stop::Records),
    ));
    let late_output_bad = Error::ContentLanguage(mime_language::json::Error::Source(
        mime_language::Error::Work(Stop::OutputBytes),
    ));
    let before = COUNTERS.snapshot();
    for (id, language, id_cap, language_cap, records, output_bytes, fault) in [
        (None, None, 0, 0, 100_000_000, 100_000_000, None),
        (
            Some(b"<A@B>".as_slice()),
            Some(b"en, en".as_slice()),
            5,
            11,
            100_000_000,
            100_000_000,
            None,
        ),
        (
            Some(long_id.as_bytes()),
            Some(long_language.as_bytes()),
            20_000,
            20_000,
            100_000_000,
            100_000_000,
            None,
        ),
        (
            Some(b"<a@b><c@d>"),
            None,
            64,
            0,
            100_000_000,
            100_000_000,
            Some(id_bad),
        ),
        (
            Some(b"<A@B>"),
            Some(b"en,"),
            5,
            64,
            100_000_000,
            100_000_000,
            Some(language_bad),
        ),
        (
            Some(b"<A@B>"),
            Some(b"en"),
            0,
            64,
            100_000_000,
            100_000_000,
            Some(Error::OutputCapacity),
        ),
        (
            Some(b"<A@B>"),
            Some(b"en"),
            5,
            0,
            100_000_000,
            100_000_000,
            Some(Error::OutputCapacity),
        ),
        (
            Some(b"<A@B>"),
            Some(b"en"),
            5,
            64,
            0,
            100_000_000,
            Some(records_bad),
        ),
        (
            Some(b"<A@B>"),
            Some(b"en"),
            5,
            64,
            100_000_000,
            14,
            Some(late_output_bad),
        ),
    ] {
        for trial in 0..if fault.is_some() { 1 } else { 2 } {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records,
                    output_bytes,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
            let mut cursor = Cursor::new(
                Values {
                    content_id: black_box(id),
                    content_language: black_box(language),
                },
                Backing {
                    content_id: id_output.get_mut(..id_cap).unwrap(),
                    content_language: language_output.get_mut(..language_cap).unwrap(),
                },
                &mut work,
                &mut budget,
            );
            let mut complete = false;
            let mut failed = None;
            for _ in 0..200_000 {
                match cursor.poll(Tick(1)) {
                    Ok(Status::Complete) => {
                        complete = true;
                        break;
                    }
                    Ok(Status::Yield) => {
                        assert!(!cursor.is_complete());
                    }
                    Err(error) => {
                        failed = Some(error);
                        break;
                    }
                }
            }
            assert_eq!(failed, fault);
            if let Some(error) = fault {
                assert!(!cursor.is_complete());
                assert_eq!(cursor.poll(Tick(1)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            assert!(complete);
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            if trial == 1 {
                let error = Error::Admission(nfc::Error::Work(Stop::Deadline));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert!(!cursor.is_complete());
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                continue;
            }
            let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
            black_box(retained.content_id);
            black_box(retained.content_id_end);
            black_box(retained.content_language);
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
            let mut next = mime_language::Cursor::new(b"fr", work, budget);
            let mut complete = false;
            for _ in 0..1000 {
                if next.poll(Tick(1)).unwrap() == mime_language::Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            let (work, budget) = next.finish(Tick(1)).unwrap();
            assert_eq!(
                (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                identity
            );
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "retained label JSON allocated");
}

fn content_id_values() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_content_id::{Cursor, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("({0})<{0}@b>", "🐈".repeat(4096));
    let nested = format!("<a@b>{}", "(".repeat(33));
    let before = COUNTERS.snapshot();
    for (source, expected, records, output) in [
        (
            b"(x)<a@b> (tail)".as_slice(),
            None,
            100_000_000,
            100_000_000,
        ),
        (long.as_bytes(), None, 100_000_000, 100_000_000),
        ("<\u{fdd0}@b>".as_bytes(), None, 100_000_000, 100_000_000),
        (
            b"<a@b><c@d>",
            Some(Error::Malformed),
            100_000_000,
            100_000_000,
        ),
        (
            nested.as_bytes(),
            Some(Error::NestingLimit),
            100_000_000,
            100_000_000,
        ),
        (b"<a@b>", Some(Error::Work(Stop::Records)), 0, 100_000_000),
        (
            b"<a@b>",
            Some(Error::Work(Stop::OutputBytes)),
            100_000_000,
            0,
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records,
                output_bytes: output,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let mut terminal = false;
        for _ in 0..1_000_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield | Status::Begin | Status::End) => {}
                Ok(Status::Scalar(value)) => {
                    std::hint::black_box(value);
                }
                Ok(Status::Complete) => {
                    assert_eq!(expected, None);
                    assert!(cursor.is_complete());
                    assert!(cursor.is_encoding_problem().is_some());
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    cursor.check_deadline(Tick(1)).unwrap();
                    terminal = true;
                    break;
                }
                Err(error) => {
                    assert_eq!(expected, Some(error));
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.is_encoding_problem(), None);
                    terminal = true;
                    break;
                }
            }
        }
        assert!(terminal);
        if expected.is_none() {
            cursor.finish(Tick(1)).unwrap();
        } else {
            assert_eq!(cursor.finish(Tick(1)).err(), expected);
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Content-ID conversion allocated");
}

fn content_language_values() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_language::{Cursor, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("({})en{}", "🐈".repeat(1024), "-abcdefgh".repeat(1024));
    let nested = format!("en {}x{}", "(".repeat(33), ")".repeat(33));
    let before = COUNTERS.snapshot();
    for (source, expected, records) in [
        (b"en-US, fr (tail)".as_slice(), None, 100_000_000),
        (long.as_bytes(), None, 100_000_000),
        (b"en,", Some(Error::Malformed), 100_000_000),
        (nested.as_bytes(), Some(Error::NestingLimit), 100_000_000),
        (b"en", Some(Error::Work(Stop::Records)), 0),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(source, &mut work, &mut budget);
        let mut done = false;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => assert!(!cursor.is_complete()),
                Ok(Status::Tag(extent)) => {
                    std::hint::black_box(extent);
                }
                Ok(Status::Complete) => {
                    assert_eq!(expected, None);
                    assert!(cursor.is_complete());
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    cursor.check_deadline(Tick(1)).unwrap();
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert!(!cursor.is_complete());
                    done = true;
                    break;
                }
                Err(error) => {
                    assert_eq!(Some(error), expected);
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(!cursor.is_complete());
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
    }
    assert_eq!(COUNTERS.snapshot(), before);
}

fn mime_body_list_selection() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        limits::Limits,
        mime_body_lists::{Backing, Class, Cursor, Disposition, Error, Media, Node, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    fn node(parent: u16, depth: u8, media: Media) -> Node {
        Node {
            parent,
            depth,
            class: Class {
                media,
                ..Class::default()
            },
        }
    }
    let simple = [node(0, 1, Media::Plain)];
    let mut nested = [
        node(0, 1, Media::Alternative),
        node(1, 2, Media::Multipart),
        node(2, 3, Media::Plain),
        node(2, 3, Media::InlineMedia),
        node(1, 2, Media::Related),
        node(5, 3, Media::Html),
        node(5, 3, Media::InlineMedia),
    ];
    nested.get_mut(3).unwrap().class.disposition = Disposition::Inline;
    let invalid = [node(0, 1, Media::Plain), node(0, 0, Media::Plain)];
    let limits = Limits {
        mime_depth: 64,
        mime_parts: 4096,
        ..Limits::default()
    };
    let mut deep = [Node::default(); 64];
    for (index, slot) in deep.iter_mut().enumerate() {
        *slot = node(
            index as u16,
            (index + 1) as u8,
            if index == 63 {
                Media::Plain
            } else {
                Media::Alternative
            },
        );
    }
    let mut t = [0; 64];
    let mut h = [0; 64];
    let mut a = [0; 64];
    let mut f = [0; 64];
    let before = COUNTERS.snapshot();
    for (nodes, failure) in [
        (simple.as_slice(), None),
        (nested.as_slice(), None),
        (deep.as_slice(), None),
        (invalid.as_slice(), Some(Error::InvalidTree)),
        (simple.as_slice(), Some(Error::Work(Stop::Records))),
        (simple.as_slice(), Some(Error::OutputCapacity)),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: if failure == Some(Error::Work(Stop::Records)) {
                    0
                } else {
                    100_000
                },
                output_bytes: 100_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            nodes,
            &limits,
            Backing {
                text: if failure == Some(Error::OutputCapacity) {
                    &mut []
                } else {
                    &mut t
                },
                html: &mut h,
                attachments: &mut a,
                membership: &mut f,
            },
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut done = false;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => assert!(cursor.value().is_none()),
                Ok(Status::Complete) => {
                    assert!(failure.is_none());
                    done = true;
                    break;
                }
                Err(error) => {
                    assert_eq!(Some(error), failure);
                    assert!(cursor.value().is_none());
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        if failure.is_none() {
            assert!(cursor.value().is_some());
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert!(cursor.value().is_none());
        }
    }
    assert_eq!(
        before,
        COUNTERS.snapshot(),
        "MIME body-list selection allocated"
    );
}

fn resident_part_headers() {
    use td_mta::{
        admission::work::{Charge, Meter},
        mime_metadata::Context,
        mime_part_headers::{Backing, Cursor, Entity, Status},
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let source = concat!(
        "Content-Type: TEXT/PLAIN;charset=utf-8;name=type\n",
        "Content-Disposition: INLINE;filename*=utf-8''e%CC%81\n",
        "Content-ID: <bad>\nContent-ID: (🐈) <A@B>\n",
        "Content-Language: en,\nContent-Language: en-GB, FR\n\nbody"
    )
    .as_bytes();
    let deep = format!(
        "Content-Type: text/plain;name=x\nContent-Language: {}en{}\n\n",
        "(".repeat(33),
        ")".repeat(33)
    );
    let before = COUNTERS.snapshot();
    for (source, failed, late) in [
        (source, false, false),
        (b"\n".as_slice(), false, false),
        (source, true, false),
        (deep.as_bytes(), true, true),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut h = [0; 64];
        let mut c = [0; 32];
        let mut n = [0; 64];
        let mut cursor = Cursor::new(
            Entity {
                source,
                base: 0,
                source_end: td_mta::header_select::SourceEnd::Eof,
                header_limit: 1024,
                context: Context::Normal,
            },
            Backing {
                heads: if failed && !late { &mut [] } else { &mut h },
                charset: &mut c,
                filename: &mut n,
            },
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        let mut done = false;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => assert!(cursor.value().is_none()),
                Ok(Status::Complete) => {
                    assert!(!failed);
                    done = true;
                    break;
                }
                Err(error) => {
                    assert!(failed);
                    if late {
                        assert_eq!(
                            error,
                            td_mta::mime_part_headers::Error::Labels(
                                td_mta::mime_label_fields::Error::NestingLimit
                            )
                        );
                    }
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    assert!(cursor.value().is_none());
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        if !failed {
            assert!(cursor.value().is_some());
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (value, work, budget, _) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(value.content_id_field.is_some(), source != b"\n");
            assert_eq!(
                value.content_language_field.is_some(),
                value.content_id_field.is_some()
            );
            let class =
                td_mta::mime_body_lists::Class::from_headers(value, Tick(1), work, budget).unwrap();
            black_box((value, class));
        }
    }
    assert_eq!(
        before,
        COUNTERS.snapshot(),
        "resident part headers allocated"
    );
}

fn resident_mime_traversal() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        limits::Limits,
        mime_traversal::{Cursor, Error, Part, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 10_000_000,
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<(), Error> {
        for _ in 0..100_000 {
            if cursor.poll(Tick(1))? == Status::Complete {
                return Ok(());
            }
        }
        panic!("traversal allocation fixture did not finish")
    }
    let long_qp = format!(
        "Content-Transfer-Encoding: quoted-printable\n\na{}b",
        " ".repeat(5000)
    );
    let nested = concat!(
        "Content-Type: multipart/mixed;boundary=a\n\n--a\n",
        "Content-Type: multipart/digest;boundary=b\n\n--b\n",
        "Content-Transfer-Encoding: strange\n\nFrom: inside\n\nbody\n--b--\n",
        "--a\nContent-Transfer-Encoding: base64\n\nYWJj\n--a--"
    );
    let bad_nesting = format!(
        "Content-Type: {}text/plain{}\n\n",
        "(".repeat(33),
        ")".repeat(33)
    );
    let mut backing = [Part::default(); 64];
    let before = COUNTERS.snapshot();
    for source in [
        b"".as_slice(),
        b"\nbody",
        nested.as_bytes(),
        long_qp.as_bytes(),
        b"Content-Type: multipart/mixed;boundary=x\n\n--x\n--x--",
        b"Content-Type: multipart/mixed;boundary=x\n\n--x\n\nbody",
        b"Content-Transfer-Encoding: base64\n\nYQ!",
        b"Content-Transfer-Encoding: quoted-printable\n\nx=",
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let identity = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
        let mut cursor = Cursor::new(
            black_box(source),
            17,
            SourceEnd::Eof,
            &Limits::default(),
            &mut backing,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert!(cursor.parts().unwrap().is_none());
        drain(&mut cursor).unwrap();
        assert!(!cursor.parts().unwrap().unwrap().is_empty());
        let (parts, work, budget) = cursor.finish(Tick(1)).unwrap();
        if source == nested.as_bytes() {
            assert_eq!(parts.len(), 4);
            assert_eq!(
                parts.get(2).unwrap().context(),
                td_mta::mime_metadata::Context::DigestChild
            );
            assert_eq!(
                parts.get(2).unwrap().diagnostics,
                td_mta::mime_traversal::DIGEST_CHILD_CONTEXT
                    | td_mta::mime_traversal::UNKNOWN_ENCODING
            );
            for index in [0, 1, 3] {
                assert_eq!(
                    parts.get(index).unwrap().context(),
                    td_mta::mime_metadata::Context::Normal
                );
            }
        }
        black_box(parts);
        assert_eq!(
            identity,
            (std::ptr::from_ref(work), std::ptr::from_ref(budget))
        );
    }
    for source in [
        b"Content-Type: multipart/mixed;boundary=x\n\n--x--".as_slice(),
        b"Content-Type: multipart/mixed;boundary=x\nContent-Transfer-Encoding: base64\n\n",
        bad_nesting.as_bytes(),
    ] {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            black_box(source),
            0,
            SourceEnd::Eof,
            &Limits::default(),
            &mut backing,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let error = drain(&mut cursor).unwrap_err();
        assert_eq!(cursor.parts(), Err(error));
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert!(cursor.finish(Tick(1)).is_err());
    }
    for kind in 0..4 {
        let mut caps = Charge {
            io_bytes: 10000,
            records: 10000,
            output_bytes: 10000,
            ..Charge::default()
        };
        let stop = match kind {
            0 => {
                caps.io_bytes = 0;
                Stop::IoBytes
            }
            1 => {
                caps.records = 0;
                Stop::Records
            }
            2 => {
                caps.output_bytes = 0;
                Stop::OutputBytes
            }
            _ => Stop::Deadline,
        };
        let mut work = Meter::new(
            Deadline::after(Tick(0), if kind == 3 { 1 } else { 100 }).unwrap(),
            caps,
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            black_box(nested.as_bytes()),
            0,
            SourceEnd::Eof,
            &Limits::default(),
            &mut backing,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::Work(stop)));
        assert_eq!(cursor.parts(), Err(Error::Work(stop)));
        assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(stop)));
        assert!(cursor.finish(Tick(1)).is_err());
    }
    let mut work = meter();
    let mut budget = HeaderBudget::new();
    let mut cursor = Cursor::new(
        black_box(nested.as_bytes()),
        0,
        SourceEnd::Eof,
        &Limits::default(),
        &mut backing,
        &mut work,
        &mut budget,
    )
    .unwrap();
    drain(&mut cursor).unwrap();
    assert!(matches!(
        cursor.finish(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    ));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "resident MIME traversal allocated");
}

fn bound_mime_part_metadata() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        limits::Limits,
        mime_part_headers::{self, label_json},
        mime_traversal::{
            bound::{Cursor, Error},
            Part, Status,
        },
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) {
        for _ in 0..1_000_000 {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                return;
            }
        }
        panic!("bound allocation traversal did not finish")
    }
    struct Storage {
        heads: Vec<u8>,
        charset: Vec<u8>,
        name: Vec<u8>,
        id: Vec<u8>,
        language: Vec<u8>,
        location: Vec<u8>,
    }
    impl Storage {
        fn new(cap: usize) -> Self {
            Self {
                heads: vec![0; cap],
                charset: vec![0; cap],
                name: vec![0; cap],
                id: vec![0; cap],
                language: vec![0; cap],
                location: vec![0; cap],
            }
        }
        fn backing(&mut self, cap: usize) -> label_json::Backing<'_> {
            label_json::Backing {
                headers: mime_part_headers::Backing {
                    heads: &mut self.heads,
                    charset: &mut self.charset,
                    filename: &mut self.name,
                },
                labels: td_mta::mime_label_fields::json::Backing {
                    content_id: &mut self.id,
                    content_language: &mut self.language,
                },
                content_location: self.location.get_mut(..cap).unwrap(),
            }
        }
    }
    let long = format!(
        concat!(
            "Content-ID: <id@a>\r\nContent-Language: fr\r\n",
            "Content-Location: /{}\r\n\r\nbody"
        ),
        "a".repeat(40963)
    );
    let folded = format!(
        concat!(
            "Content-ID: <id@a>\r\nContent-Language: fr\r\n",
            "Content-Location: {}\r\n\r\nbody"
        ),
        "=?ascii?Q?a?=\r\n ".repeat(1024)
    );
    let malformed = format!(
        concat!(
            "{}Content-ID: <id@a>\r\nContent-Language: fr\r\n",
            "Content-Location: ../ok\r\n\r\nbody"
        ),
        "Content-Location: a%\r\n".repeat(1024)
    );
    let nested = concat!(
        "Content-Type: multipart/digest;boundary=a\r\n",
        "Content-Location: ../root\r\n\r\n--a\r\n",
        "Content-ID: <id@a>\r\nContent-Language: fr\r\n",
        "Content-Location: \r\n\r\nbody\r\n--a\r\n",
        "Content-Type: text/plain\r\nContent-Location: ../leaf\r\n",
        "\r\nbody\r\n--a--\r\n"
    );
    let reserve = long.len().max(folded.len()).max(malformed.len()) * 6 + 32;
    let mut storage = Storage::new(reserve);
    let mut next_storage = Storage::new(256);
    let mut parts = [Part::default(); 64];
    let mut next_parts = [Part::default(); 8];
    let mut scratch = Scratch::new();
    let cases = [
        (long.as_bytes(), reserve),
        (folded.as_bytes(), reserve),
        (malformed.as_bytes(), reserve),
        (nested.as_bytes(), reserve),
        (
            b"Content-Location: =?utf-8?Q?=FF?=\r\n\r\n".as_slice(),
            reserve,
        ),
        (
            b"Content-Location: a%\r\n\r\nContent-Location: ../body".as_slice(),
            0,
        ),
    ];
    let before = COUNTERS.snapshot();
    for (source, cap) in cases {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let pointers = (
            std::ptr::from_mut(&mut work),
            std::ptr::from_mut(&mut budget),
            std::ptr::from_mut(&mut scratch),
        );
        let mut cursor = Cursor::new(
            black_box(source),
            17,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor);
        let mut bound = cursor.finish(Tick(1)).unwrap();
        let count = bound.parts().unwrap().len();
        for index in 0..count {
            let ordinal = u16::try_from(index + 1).unwrap();
            let mut cursor = bound
                .metadata(ordinal, storage.backing(cap), &mut scratch)
                .unwrap();
            let mut complete = false;
            for _ in 0..1_000_000 {
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
                assert!(cursor.value().is_none());
            }
            assert!(complete, "bound source {} part {ordinal}", source.len());
            let (view, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (
                    std::ptr::from_mut(work),
                    std::ptr::from_mut(budget),
                    std::ptr::from_mut(scratch)
                ),
                pointers
            );
            assert_eq!(view.part.ordinal, ordinal);
            assert_eq!(view.metadata.headers.body_start, view.part.body_start);
            if source == nested.as_bytes() && index == 1 {
                assert_eq!(view.metadata.headers.content_type, b"message/rfc822");
                assert_eq!(
                    view.metadata.labels.content_id,
                    Some(b"\"id@a\"".as_slice())
                );
                assert_eq!(view.metadata.location.value, Some(b"\"\"".as_slice()));
            }
            if cap == 0 {
                assert!(view.metadata.location.value.is_none());
            }
            black_box(view);
        }
        let (_, work, budget) = bound.finish(Tick(1)).unwrap();
        assert_eq!(
            (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
            (pointers.0, pointers.1)
        );
        let mut cursor = Cursor::new(
            b"\r\n",
            0,
            SourceEnd::Eof,
            &Limits::default(),
            &mut next_parts,
            work,
            budget,
        )
        .unwrap();
        drain(&mut cursor);
        let mut next = cursor.finish(Tick(1)).unwrap();
        let mut cursor = next
            .metadata(1, next_storage.backing(256), &mut scratch)
            .unwrap();
        let mut complete = false;
        for _ in 0..1_000_000 {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        let (view, _, _, _) = cursor.finish(Tick(1)).unwrap();
        assert!(view.metadata.location.value.is_none());
        next.finish(Tick(1)).unwrap();
    }
    for kind in 0..3 {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut cursor = Cursor::new(
            nested.as_bytes(),
            17,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor);
        let mut bound = cursor.finish(Tick(1)).unwrap();
        let mut cursor = bound
            .metadata(
                1,
                storage.backing(if kind == 0 { 0 } else { reserve }),
                &mut scratch,
            )
            .unwrap();
        if kind == 0 {
            let mut error = None;
            for _ in 0..1_000_000 {
                match cursor.poll(Tick(1)) {
                    Err(value) => {
                        error = Some(value);
                        break;
                    }
                    Ok(_) => assert!(cursor.value().is_none()),
                }
            }
            let error = error.unwrap();
            assert!(matches!(error, Error::Metadata(_)));
            assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
            assert_eq!(bound.finish(Tick(1)).err(), Some(error));
            assert_eq!(work.stopped(), None);
        } else if kind == 1 {
            assert_eq!(bound.parts(), Err(Error::Abandoned));
            assert_eq!(bound.finish(Tick(1)).err(), Some(Error::Abandoned));
        } else {
            let mut complete = false;
            for _ in 0..1_000_000 {
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
            }
            assert!(complete);
            cursor.finish(Tick(1)).unwrap();
            let error = bound.check_deadline(Tick(100)).unwrap_err();
            assert!(matches!(error, Error::Admission(_)));
            assert_eq!(bound.parts(), Err(error));
            assert_eq!(bound.finish(Tick(1)).err(), Some(error));
            assert_eq!(work.stopped(), Some(Stop::Deadline));
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "bound MIME metadata allocated");
}

fn bound_mime_classification() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        limits::Limits,
        mime_body_lists::{self, Node},
        mime_part_headers::{self, label_json},
        mime_traversal::{
            bound::{Cursor, Error},
            Part, Status,
        },
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    const SOURCE: &[u8] = concat!(
        "Content-Type: multipart/mixed;boundary=a\r\n\r\n--a\r\n",
        "Content-Type: text/plain\r\n\r\nplain\r\n--a\r\n",
        "Content-Type: multipart/alternative;boundary=b\r\n\r\n--b\r\n",
        "Content-Type: text/plain\r\n\r\nplain2\r\n--b\r\n",
        "Content-Type: text/html\r\n\r\nhtml\r\n--b--\r\n--a\r\n",
        "Content-Type: image/png\r\nContent-Disposition: inline;filename=x.png\r\n\r\nimage\r\n--a\r\n",
        "Content-Type: application/pdf\r\nContent-Disposition: attachment;filename*=utf-8''caf%C3%A9.pdf\r\n\r\npdf\r\n--a--\r\n"
    ).as_bytes();
    let mut heads = [0; 256];
    let mut charset = [0; 256];
    let mut filename = [0; 256];
    let mut id = [0; 256];
    let mut language = [0; 256];
    let mut location = [0; 256];
    let mut parts = [Part::default(); 8];
    let mut nodes = [Node::default(); 7];
    let mut text = [0; 8];
    let mut html = [0; 8];
    let mut attachments = [0; 8];
    let mut membership = [0; 8];
    let mut scratch = Scratch::new();
    let before = COUNTERS.snapshot();
    for late in [false, true] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let pointers = (
            std::ptr::from_mut(&mut work),
            std::ptr::from_mut(&mut budget),
            std::ptr::from_mut(&mut scratch),
        );
        let mut cursor = Cursor::new(
            black_box(SOURCE),
            17,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut complete = false;
        for _ in 0..100000 {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        let mut bound = cursor.finish(Tick(1)).unwrap();
        assert_eq!(bound.parts().unwrap().len(), nodes.len());
        let mut failure = None;
        for (index, node) in nodes.iter_mut().enumerate() {
            let mut cursor = bound
                .metadata(
                    u16::try_from(index + 1).unwrap(),
                    label_json::Backing {
                        headers: mime_part_headers::Backing {
                            heads: &mut heads,
                            charset: &mut charset,
                            filename: &mut filename,
                        },
                        labels: td_mta::mime_label_fields::json::Backing {
                            content_id: &mut id,
                            content_language: &mut language,
                        },
                        content_location: &mut location,
                    },
                    &mut scratch,
                )
                .unwrap();
            let mut complete = false;
            for _ in 0..100000 {
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    complete = true;
                    break;
                }
                assert!(cursor.value().is_none());
            }
            assert!(complete);
            let result = cursor.finish_classified(if late && index == 6 {
                Tick(100)
            } else {
                Tick(1)
            });
            if let Err(error) = result {
                assert!(matches!(error, Error::Metadata(_)));
                assert_eq!(bound.parts(), Err(error));
                failure = Some(error);
                break;
            }
            let (value, work, budget, scratch) = result.unwrap();
            assert_eq!(
                (
                    std::ptr::from_mut(work),
                    std::ptr::from_mut(budget),
                    std::ptr::from_mut(scratch)
                ),
                pointers
            );
            assert_eq!(value.part.ordinal, u16::try_from(index + 1).unwrap());
            assert_eq!(value.metadata.headers.body_start, value.part.body_start);
            if index == 6 {
                assert_eq!(
                    value.metadata.headers.filename,
                    Some(b"caf\xc3\xa9.pdf".as_slice())
                );
            }
            *node = value.node;
        }
        if let Some(error) = failure {
            assert_eq!(bound.finish(Tick(1)).err(), Some(error));
            assert_eq!(work.stopped(), Some(Stop::Deadline));
            continue;
        }
        let (_, work, budget) = bound.finish(Tick(1)).unwrap();
        let mut cursor = mime_body_lists::Cursor::new(
            &nodes,
            &Limits::default(),
            mime_body_lists::Backing {
                text: &mut text,
                html: &mut html,
                attachments: &mut attachments,
                membership: &mut membership,
            },
            work,
            budget,
        )
        .unwrap();
        let mut complete = false;
        for _ in 0..100000 {
            if cursor.poll(Tick(1)).unwrap() == mime_body_lists::Status::Complete {
                complete = true;
                break;
            }
            assert!(cursor.value().is_none());
        }
        assert!(complete);
        let (view, work, budget) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(view.text, &[2, 4, 6]);
        assert_eq!(view.html, &[2, 5, 6]);
        assert_eq!(view.attachments, &[7]);
        assert!(view.has_attachment);
        assert_eq!(
            (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
            (pointers.0, pointers.1)
        );
        black_box(view);
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "bound MIME classification allocated");
}

fn ordered_mime_classification() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        limits::Limits,
        mime_body_lists::{self, Node},
        mime_part_headers::{self, label_json},
        mime_traversal::{
            bound::{
                ordered::{
                    body_lists::{
                        response::{json, Projecting},
                        Selecting,
                    },
                    Classifying,
                },
                Cursor, Error,
            },
            Part, Status,
        },
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    const SOURCE: &[u8] = concat!(
        "Content-Type: multipart/digest;boundary=a\r\n",
        "Content-Location: ../root\r\n\r\n--a\r\n",
        "Content-ID: <id@a>\r\nContent-Language: fr\r\n\r\nbody\r\n--a\r\n",
        "Content-Type: text/plain\r\nContent-Location: ../leaf\r\n\r\nbody\r\n--a--\r\n"
    )
    .as_bytes();
    fn forget<T>(value: T) {
        std::mem::forget(value);
    }
    let mut heads = [0; 256];
    let mut charset = [0; 256];
    let mut filename = [0; 256];
    let mut id = [0; 256];
    let mut language = [0; 256];
    let mut location = [0; 256];
    let mut parts = [Part::default(); 8];
    let mut nodes = [Node::default(); 8];
    let mut scratch = Scratch::new();
    let mut text = [0; 8];
    let mut html = [0; 8];
    let mut attachments = [0; 8];
    let mut membership = [0; 8];
    let mut json_output = [0; 8];
    let mut retained_output = [0; 256];
    let mut collection_output = [[0; 256]; 3];
    let mut composed_output = [0; 2048];
    use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::Candidate;
    let mut locator_output = [Candidate::default(); 3];
    let before = COUNTERS.snapshot();
    'trial_loop: for trial in 0..65 {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 10_000_000,
                output_bytes: 10_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let pointers = (
            std::ptr::from_mut(&mut work),
            std::ptr::from_mut(&mut budget),
            std::ptr::from_mut(&mut scratch),
        );
        let mut cursor = Cursor::new(
            black_box(SOURCE),
            17,
            SourceEnd::Eof,
            &Limits::default(),
            &mut parts,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut complete = false;
        for _ in 0..100000 {
            if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        let source = cursor.finish(Tick(1)).unwrap();
        if trial == 6 {
            assert_eq!(
                Classifying::new(source, nodes.get_mut(..2).unwrap()).err(),
                Some(Error::NodeCapacity)
            );
            assert_eq!(work.stopped(), None);
            continue;
        }
        let mut owner = Classifying::new(source, &mut nodes).unwrap();
        let total = owner.total().unwrap();
        assert_eq!(total, 3);
        let mut failure = None;
        let count = if trial < 4 { trial } else { total };
        for index in 0..count {
            let mut part = owner
                .next(
                    label_json::Backing {
                        headers: mime_part_headers::Backing {
                            heads: &mut heads,
                            charset: &mut charset,
                            filename: &mut filename,
                        },
                        labels: td_mta::mime_label_fields::json::Backing {
                            content_id: &mut id,
                            content_language: &mut language,
                        },
                        content_location: location
                            .get_mut(..if trial == 5 && index == 2 { 0 } else { 256 })
                            .unwrap(),
                    },
                    &mut scratch,
                )
                .unwrap();
            let mut complete = false;
            let mut error = None;
            for _ in 0..100000 {
                match part.poll(Tick(1)) {
                    Ok(Status::Complete) => {
                        complete = true;
                        break;
                    }
                    Ok(Status::Yield) => {}
                    Err(refusal) => {
                        error = Some(refusal);
                        break;
                    }
                }
            }
            if let Some(error) = error {
                assert_eq!(part.finish(Tick(1)).err(), Some(error));
                assert_eq!(owner.completed(), Err(error));
                failure = Some(error);
                break;
            }
            assert!(complete);
            if trial == 4 && index == 1 {
                forget(part);
                assert_eq!(owner.completed(), Err(Error::Abandoned));
                failure = Some(Error::Abandoned);
                break;
            }
            let (view, work, budget, scratch) = part.finish(Tick(1)).unwrap();
            assert_eq!(view.part.ordinal, u16::try_from(index + 1).unwrap());
            assert_eq!(
                (
                    std::ptr::from_mut(work),
                    std::ptr::from_mut(budget),
                    std::ptr::from_mut(scratch)
                ),
                pointers
            );
            assert_eq!(owner.completed(), Ok(index + 1));
        }
        if let Some(error) = failure {
            assert_eq!(owner.finish(Tick(1)).err(), Some(error));
            assert_eq!(work.stopped(), None);
            continue;
        }
        if count < total {
            assert_eq!(
                owner.finish(Tick(100)).err(),
                Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
            );
            assert_eq!(work.stopped(), Some(Stop::Deadline));
            continue;
        }
        let mut classified = owner.finish(Tick(1)).unwrap();
        assert_eq!(classified.parts().unwrap().len(), 3);
        assert_eq!(classified.nodes().unwrap().len(), 3);
        assert_eq!(
            classified.nodes().unwrap().get(1).unwrap().class.media,
            td_mta::mime_body_lists::Media::Other
        );
        assert_eq!(
            classified.nodes().unwrap().get(2).unwrap().class.media,
            td_mta::mime_body_lists::Media::Plain
        );
        if trial == 7 {
            let (view, work, budget) = classified.finish(Tick(1)).unwrap();
            assert_eq!(view.parts.len(), 3);
            assert_eq!(view.nodes.len(), 3);
            assert_eq!(
                (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                (pointers.0, pointers.1)
            );
            continue;
        }
        if trial >= 8 {
            let mut selection = Selecting::new(
                classified,
                &Limits::default(),
                mime_body_lists::Backing {
                    text: text.get_mut(..if trial == 12 { 0 } else { 8 }).unwrap(),
                    html: &mut html,
                    attachments: &mut attachments,
                    membership: membership
                        .get_mut(..if trial == 11 { 0 } else { 8 })
                        .unwrap(),
                },
                Tick(1),
            )
            .unwrap();
            let mut done = false;
            let mut refused = None;
            for turn in 0..100000 {
                match selection.poll(Tick(1)) {
                    Ok(mime_body_lists::Status::Complete) => {
                        done = true;
                        break;
                    }
                    Ok(mime_body_lists::Status::Yield) => {}
                    Err(error) => {
                        refused = Some(error);
                        break;
                    }
                }
                if trial == 9 && turn == 0 {
                    break;
                }
            }
            if trial == 9 {
                assert_eq!(
                    selection.finish(Tick(100)).err(),
                    Some(Error::BodyLists(mime_body_lists::Error::Work(
                        Stop::Deadline
                    )))
                );
                continue;
            }
            if let Some(error) = refused {
                assert_eq!(
                    error,
                    Error::BodyLists(mime_body_lists::Error::OutputCapacity)
                );
                assert!(selection.value().is_none());
                assert_eq!(selection.finish(Tick(1)).err(), Some(error));
                continue;
            }
            assert!(done);
            let mut selected = selection.finish(Tick(1)).unwrap();
            if trial >= 34 {
                use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::{
                    Cell, Collecting,
                };
                let mut cells = collection_output.each_mut().map(|output| {
                    Cell::new(output.get_mut(..if trial == 37 { 0 } else { 256 }).unwrap())
                });
                if trial == 40 {
                    assert_eq!(
                        Collecting::new(selected, &mut cells, Tick(100)).err(),
                        Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                    );
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                    continue;
                }
                let mut response = Collecting::new(selected, &mut cells, Tick(1)).unwrap();
                let mut stopped = false;
                for _ in 0..3 {
                    let mut child = response
                        .next(
                            label_json::Backing {
                                headers: mime_part_headers::Backing {
                                    heads: &mut heads,
                                    charset: &mut charset,
                                    filename: &mut filename,
                                },
                                labels: td_mta::mime_label_fields::json::Backing {
                                    content_id: &mut id,
                                    content_language: &mut language,
                                },
                                content_location: &mut location,
                            },
                            &mut scratch,
                            Tick(1),
                        )
                        .unwrap();
                    if trial == 35 || trial == 38 {
                        child.poll(Tick(1)).unwrap();
                        if trial == 35 {
                            assert_eq!(
                                child.finish(Tick(100)).err(),
                                Some(Error::Metadata(label_json::Error::Headers(
                                    mime_part_headers::Error::Admission(td_mta::nfc::Error::Work(
                                        Stop::Deadline
                                    ))
                                )))
                            );
                        } else {
                            forget(child);
                        }
                        stopped = true;
                        break;
                    }
                    let mut done = false;
                    let mut refused = None;
                    for _ in 0..100000 {
                        match child.poll(Tick(1)) {
                            Ok(Status::Complete) => {
                                done = true;
                                break;
                            }
                            Ok(Status::Yield) => {}
                            Err(error) => {
                                refused = Some(error);
                                break;
                            }
                        }
                    }
                    if let Some(error) = refused {
                        assert_eq!(trial, 37);
                        assert_eq!(error, Error::ResponseCapacity);
                        assert!(child.value().is_none());
                        assert_eq!(child.finish(Tick(1)).err(), Some(error));
                        stopped = true;
                        break;
                    }
                    assert!(done);
                    if trial == 39 {
                        forget(child);
                        stopped = true;
                        break;
                    }
                    child.finish(Tick(1)).unwrap();
                }
                if stopped {
                    assert!(response.completed().is_err());
                    assert!(response.finish(Tick(1)).is_err());
                    continue;
                }
                let mut serialized = response.finish(Tick(1)).unwrap();
                assert_eq!(serialized.value().unwrap().fragments.len(), 3);
                if trial >= 49 {
                    use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::{Mode, retained::Cursor as WholeRetention};
                    let mode = if trial == 50 || trial == 64 {
                        Mode::Lists
                    } else {
                        Mode::Structure
                    };
                    let deadline = Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline));
                    if trial == 56 {
                        assert_eq!(
                            WholeRetention::new(serialized, mode, &mut composed_output, Tick(100))
                                .err(),
                            Some(deadline)
                        );
                        continue;
                    }
                    let window = if trial == 53 {
                        &mut composed_output[..1]
                    } else {
                        &mut composed_output[..]
                    };
                    let mut cursor =
                        WholeRetention::new(serialized, mode, window, Tick(1)).unwrap();
                    if trial == 51 || trial == 54 {
                        cursor.poll(Tick(1)).unwrap();
                        if trial == 51 {
                            assert_eq!(cursor.finish(Tick(100)).err(), Some(deadline));
                        } else {
                            forget(cursor);
                        }
                        continue;
                    }
                    let mut done = false;
                    for _ in 0..100000 {
                        match cursor.poll(Tick(1)) {
                            Ok(Status::Complete) => {
                                done = true;
                                break;
                            }
                            Err(error) => {
                                assert_eq!(trial, 53);
                                assert_eq!(error, Error::ResponseCapacity);
                                assert!(cursor.value().is_none());
                                assert_eq!(cursor.poll(Tick(100)), Err(error));
                                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                                continue 'trial_loop;
                            }
                            Ok(Status::Yield) => {}
                        }
                    }
                    assert!(done);
                    assert_ne!(trial, 53, "short retention window unexpectedly completed");
                    if trial == 55 {
                        forget(cursor);
                        continue;
                    }
                    let mut retained = cursor.finish(Tick(1)).unwrap();
                    if trial >= 57 {
                        use td_mta::{ids::BlobId, mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::Cursor as Mapping};
                        let parent = BlobId::from_bytes([0x44; 16]);
                        let deadline = Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline));
                        if trial == 63 {
                            assert_eq!(
                                Mapping::new(retained, parent, &mut locator_output, Tick(100))
                                    .err(),
                                Some(deadline)
                            );
                            continue;
                        }
                        if trial == 62 {
                            assert_eq!(
                                Mapping::new(
                                    retained,
                                    parent,
                                    locator_output.get_mut(..2).unwrap(),
                                    Tick(1)
                                )
                                .err(),
                                Some(Error::ResponseCapacity)
                            );
                            continue;
                        }
                        let mut mapping =
                            Mapping::new(retained, parent, &mut locator_output, Tick(1)).unwrap();
                        if trial == 58 || trial == 60 {
                            assert_eq!(mapping.poll(Tick(1)), Ok(Status::Yield));
                            if trial == 58 {
                                assert_eq!(mapping.finish(Tick(100)).err(), Some(deadline));
                            } else {
                                forget(mapping);
                            }
                            continue;
                        }
                        assert_eq!(mapping.poll(Tick(1)), Ok(Status::Yield));
                        assert_eq!(mapping.poll(Tick(1)), Ok(Status::Yield));
                        assert_eq!(mapping.poll(Tick(1)), Ok(Status::Complete));
                        if trial == 61 {
                            forget(mapping);
                            continue;
                        }
                        let mut mapped = mapping.finish(Tick(1)).unwrap();
                        if trial == 59 {
                            assert_eq!(mapped.check_deadline(Tick(100)), Err(deadline));
                            assert!(mapped.value().is_none());
                            assert_eq!(mapped.finish(Tick(1)).err(), Some(deadline));
                            continue;
                        }
                        let ((actual, candidates), ((actual_mode, bytes, view), work, budget)) =
                            mapped.finish(Tick(1)).unwrap();
                        assert_eq!(actual, parent);
                        assert_eq!(actual_mode, mode);
                        assert!(!bytes.is_empty());
                        assert_eq!(view.fragments.len(), 3);
                        assert_eq!(candidates.len(), 3);
                        assert!(candidates.first().unwrap().wire().is_none());
                        assert!(candidates
                            .get(1..)
                            .unwrap()
                            .iter()
                            .all(|c| c.wire().is_some_and(|s| s.len() == 69)));
                        assert_eq!(
                            (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                            (pointers.0, pointers.1)
                        );
                        continue;
                    }
                    if trial == 52 {
                        assert_eq!(retained.check_deadline(Tick(100)), Err(deadline));
                        assert!(retained.value().is_none());
                        assert_eq!(retained.finish(Tick(1)).err(), Some(deadline));
                        continue;
                    }
                    let ((actual, bytes, view), work, budget) = retained.finish(Tick(1)).unwrap();
                    assert_eq!(actual, mode);
                    assert!(!bytes.is_empty());
                    assert_eq!(view.fragments.len(), 3);
                    assert_eq!(
                        (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                        (pointers.0, pointers.1)
                    );
                    continue;
                }
                if trial >= 41 {
                    use td_mta::mime_traversal::bound::ordered::body_lists::response::collected::composed::{Cursor as Composition, Mode, Status as CompositionStatus};
                    let mode = if trial == 42 {
                        Mode::Lists
                    } else {
                        Mode::Structure
                    };
                    if trial == 48 {
                        assert_eq!(
                            Composition::new(serialized, mode, Tick(100)).err(),
                            Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                        );
                        continue;
                    }
                    let mut cursor = Composition::new(serialized, mode, Tick(1)).unwrap();
                    if trial == 45 {
                        assert_eq!(
                            cursor.poll(Tick(100), &mut []).err(),
                            Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                        );
                        assert!(cursor.value().is_none());
                        assert_eq!(
                            cursor.finish(Tick(1)).err(),
                            Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                        );
                        continue;
                    }
                    if trial == 43 || trial == 46 {
                        cursor.poll(Tick(1), &mut json_output).unwrap();
                        if trial == 43 {
                            assert_eq!(
                                cursor.finish(Tick(100)).err(),
                                Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                            );
                        } else {
                            forget(cursor);
                        }
                        continue;
                    }
                    let mut done = false;
                    for _ in 0..100000 {
                        if cursor.poll(Tick(1), &mut json_output).unwrap().status
                            == CompositionStatus::Complete
                        {
                            done = true;
                            break;
                        }
                    }
                    assert!(done);
                    if trial == 47 {
                        forget(cursor);
                        continue;
                    }
                    let mut composed = cursor.finish(Tick(1)).unwrap();
                    if trial == 44 {
                        let error = composed.check_deadline(Tick(100)).unwrap_err();
                        assert!(composed.value().is_none());
                        assert_eq!(composed.finish(Tick(1)).err(), Some(error));
                        continue;
                    }
                    let ((actual, view), work, budget) = composed.finish(Tick(1)).unwrap();
                    assert_eq!(actual, mode);
                    assert_eq!(view.fragments.len(), 3);
                    assert_eq!(
                        (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                        (pointers.0, pointers.1)
                    );
                    continue;
                }
                if trial == 36 {
                    let error = serialized.check_deadline(Tick(100)).unwrap_err();
                    assert!(serialized.value().is_none());
                    assert_eq!(serialized.finish(Tick(1)).err(), Some(error));
                    continue;
                }
                let (view, work, budget) = serialized.finish(Tick(1)).unwrap();
                assert!(view.fragments.iter().all(|cell| cell.value().is_some()));
                assert_eq!(
                    (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                    (pointers.0, pointers.1)
                );
                continue;
            }
            if trial >= 14 {
                if trial == 20 {
                    assert_eq!(
                        Projecting::new(selected, Tick(100)).err(),
                        Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                    );
                    continue;
                }
                let mut response = Projecting::new(selected, Tick(1)).unwrap();
                if trial == 15 {
                    assert_eq!(
                        response.finish(Tick(100)).err(),
                        Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                    );
                    continue;
                }
                let mut stopped = false;
                for ordinal in 1..=3 {
                    let mut part = response
                        .next(
                            label_json::Backing {
                                headers: mime_part_headers::Backing {
                                    heads: &mut heads,
                                    charset: &mut charset,
                                    filename: &mut filename,
                                },
                                labels: td_mta::mime_label_fields::json::Backing {
                                    content_id: &mut id,
                                    content_language: &mut language,
                                },
                                content_location: location
                                    .get_mut(..if trial == 18 { 0 } else { 256 })
                                    .unwrap(),
                            },
                            &mut scratch,
                            Tick(1),
                        )
                        .unwrap();
                    if trial == 19 {
                        forget(part);
                        stopped = true;
                        break;
                    }
                    if trial == 16 {
                        part.poll(Tick(1)).unwrap();
                        assert!(part.finish(Tick(100)).is_err());
                        stopped = true;
                        break;
                    }
                    let mut complete = false;
                    let mut refusal = None;
                    for _ in 0..100000 {
                        match part.poll(Tick(1)) {
                            Ok(Status::Complete) => {
                                complete = true;
                                break;
                            }
                            Ok(Status::Yield) => {}
                            Err(error) => {
                                refusal = Some(error);
                                break;
                            }
                        }
                    }
                    if let Some(error) = refusal {
                        assert_eq!(trial, 18);
                        assert!(part.value().is_none());
                        assert_eq!(part.finish(Tick(1)).err(), Some(error));
                        stopped = true;
                        break;
                    }
                    assert!(complete);
                    assert!(part.value().is_some());
                    if trial >= 27 {
                        if trial == 33 {
                            assert!(json::retained::Cursor::new(
                                part,
                                &mut retained_output,
                                Tick(100)
                            )
                            .is_err());
                            stopped = true;
                            break;
                        }
                        let mut retained = json::retained::Cursor::new(
                            part,
                            retained_output
                                .get_mut(..if trial == 30 { 0 } else { 256 })
                                .unwrap(),
                            Tick(1),
                        )
                        .unwrap();
                        if trial == 28 {
                            retained.poll(Tick(1)).unwrap();
                            assert!(retained.finish(Tick(100)).is_err());
                            stopped = true;
                            break;
                        }
                        if trial == 31 {
                            retained.poll(Tick(1)).unwrap();
                            forget(retained);
                            stopped = true;
                            break;
                        }
                        if trial == 30 {
                            assert_eq!(retained.poll(Tick(1)), Err(Error::ResponseCapacity));
                            assert!(retained.value().is_none());
                            assert_eq!(
                                retained.finish(Tick(1)).err(),
                                Some(Error::ResponseCapacity)
                            );
                            stopped = true;
                            break;
                        }
                        let mut done = false;
                        for _ in 0..100000 {
                            if retained.poll(Tick(1)).unwrap() == Status::Complete {
                                done = true;
                                break;
                            }
                        }
                        assert!(done);
                        let view = retained.value().unwrap();
                        assert_eq!(view.end.part.ordinal, ordinal);
                        assert!(!view.fragment.is_empty());
                        if trial == 29 {
                            let error = retained.check_deadline(Tick(100)).unwrap_err();
                            assert!(retained.value().is_none());
                            assert_eq!(retained.finish(Tick(1)).err(), Some(error));
                            stopped = true;
                            break;
                        }
                        if trial == 32 {
                            forget(retained);
                            stopped = true;
                            break;
                        }
                        let (view, work, budget, scratch) = retained.finish(Tick(1)).unwrap();
                        assert_eq!(view.end.part.ordinal, ordinal);
                        assert_eq!(
                            (
                                std::ptr::from_mut(work),
                                std::ptr::from_mut(budget),
                                std::ptr::from_mut(scratch)
                            ),
                            pointers
                        );
                        continue;
                    }
                    if trial >= 21 {
                        if trial == 26 {
                            assert!(json::Cursor::new(part, Tick(100)).is_err());
                            stopped = true;
                            break;
                        }
                        let mut json = json::Cursor::new(part, Tick(1)).unwrap();
                        if trial == 22 {
                            json.poll(Tick(1), &mut json_output).unwrap();
                            assert!(json.finish(Tick(100)).is_err());
                            stopped = true;
                            break;
                        }
                        if trial == 24 {
                            json.poll(Tick(1), &mut json_output).unwrap();
                            forget(json);
                            stopped = true;
                            break;
                        }
                        let mut done = false;
                        for _ in 0..100000 {
                            if json.poll(Tick(1), &mut json_output).unwrap().status
                                == json::Status::Complete
                            {
                                done = true;
                                break;
                            }
                        }
                        assert!(done);
                        assert_eq!(json.value().unwrap().part.ordinal, ordinal);
                        if trial == 23 {
                            let error = json.check_deadline(Tick(100)).unwrap_err();
                            assert!(json.value().is_none());
                            assert_eq!(json.finish(Tick(1)).err(), Some(error));
                            stopped = true;
                            break;
                        }
                        if trial == 25 {
                            forget(json);
                            stopped = true;
                            break;
                        }
                        let (end, work, budget, scratch) = json.finish(Tick(1)).unwrap();
                        assert_eq!(end.part.ordinal, ordinal);
                        assert_eq!(
                            (
                                std::ptr::from_mut(work),
                                std::ptr::from_mut(budget),
                                std::ptr::from_mut(scratch)
                            ),
                            pointers
                        );
                        continue;
                    }
                    let (view, work, budget, scratch) = part.finish(Tick(1)).unwrap();
                    assert_eq!(view.part.ordinal, ordinal);
                    assert_eq!(
                        (
                            std::ptr::from_mut(work),
                            std::ptr::from_mut(budget),
                            std::ptr::from_mut(scratch)
                        ),
                        pointers
                    );
                }
                if stopped {
                    assert!(response.completed().is_err());
                    assert!(response.finish(Tick(1)).is_err());
                    continue;
                }
                let projected = response.finish(Tick(1)).unwrap();
                if trial == 17 {
                    assert_eq!(
                        projected.finish(Tick(100)).err(),
                        Some(Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline)))
                    );
                    continue;
                }
                let (view, work, budget) = projected.finish(Tick(1)).unwrap();
                assert_eq!(view.lists.text, &[3]);
                assert_eq!(view.lists.html, &[3]);
                assert_eq!(view.lists.attachments, &[2]);
                assert!(view.lists.has_attachment);
                assert_eq!(
                    (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                    (pointers.0, pointers.1)
                );
                continue;
            }
            if trial == 10 {
                let error = selected.check_deadline(Tick(100)).unwrap_err();
                assert!(selected.value().is_none());
                assert_eq!(selected.finish(Tick(1)).err(), Some(error));
                continue;
            }
            if trial == 13 {
                forget(selected);
                continue;
            }
            let (view, work, budget) = selected.finish(Tick(1)).unwrap();
            assert_eq!(view.structure.parts.len(), 3);
            assert_eq!(view.structure.nodes.len(), 3);
            assert_eq!(view.lists.text, &[3]);
            assert_eq!(view.lists.html, &[3]);
            assert_eq!(view.lists.attachments, &[2]);
            assert!(view.lists.has_attachment);
            assert_eq!(
                (std::ptr::from_mut(work), std::ptr::from_mut(budget)),
                (pointers.0, pointers.1)
            );
            continue;
        }
        let error = classified.check_deadline(Tick(100)).unwrap_err();
        assert!(matches!(
            error,
            Error::Admission(td_mta::nfc::Error::Work(Stop::Deadline))
        ));
        assert_eq!(classified.nodes(), Err(error));
        assert_eq!(classified.parts(), Err(error));
        assert_eq!(classified.finish(Tick(1)).err(), Some(error));
        assert_eq!(work.stopped(), Some(Stop::Deadline));
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "ordered MIME classification allocated");
}

fn resident_mime_delimiters() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_delimiter::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    let boundary = "x".repeat(70);
    let source = format!(
        "{}\r\n--{}\r\nHeader: value\r\n\r\nbody\r\n--{}-- \t\r\nepilogue",
        "a".repeat(127),
        boundary,
        boundary
    );
    let before = COUNTERS.snapshot();
    for (input, selected, expected_count) in [
        (source.as_bytes(), boundary.as_bytes(), 2),
        (b"--b\n--b\r\n--b--".as_slice(), b"b".as_slice(), 3),
        (b"bare\r--b\n--b---other\r", b"b", 1),
        (b"preamble only", b"b", 0),
        (b"", b"b", 0),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                ..Charge::default()
            },
        );
        let pointer = std::ptr::from_ref(&work);
        let mut cursor = Cursor::new(black_box(input), 7, black_box(selected), &mut work).unwrap();
        let mut count = 0;
        let mut done = false;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Delimiter(d) => {
                    count += 1;
                    assert!(d.preceding_end <= d.line_start);
                    assert!(d.line_start <= d.after_line);
                }
                Status::Complete => {
                    done = true;
                    break;
                }
            }
        }
        assert!(done);
        assert_eq!(count, expected_count);
        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
        let work = cursor.finish(Tick(1)).unwrap();
        assert_eq!(std::ptr::from_ref(work), pointer);
    }
    for (io, records, now, selected, error) in [
        (
            0,
            100_000,
            Tick(1),
            b"b".as_slice(),
            Error::Work(Stop::IoBytes),
        ),
        (100_000, 0, Tick(1), b"b", Error::Work(Stop::Records)),
        (
            100_000,
            100_000,
            Tick(100),
            b"b",
            Error::Work(Stop::Deadline),
        ),
        (100_000, 100_000, Tick(1), b"bad ", Error::InvalidBoundary),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                records,
                ..Charge::default()
            },
        );
        let mut cursor = Cursor::new(
            black_box(source.as_bytes()),
            0,
            black_box(selected),
            &mut work,
        )
        .unwrap();
        let mut refused = false;
        for _ in 0..100_000 {
            match cursor.poll(now) {
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(e, error);
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
        assert!(cursor.finish(Tick(1)).is_err());
    }
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000,
            records: 100_000,
            ..Charge::default()
        },
    );
    let mut cursor = Cursor::new(
        black_box(source.as_bytes()),
        0,
        black_box(boundary.as_bytes()),
        &mut work,
    )
    .unwrap();
    let mut done = false;
    for _ in 0..100_000 {
        if matches!(cursor.poll(Tick(1)).unwrap(), Status::Complete) {
            done = true;
            break;
        }
    }
    assert!(done);
    assert!(cursor.finish(Tick(100)).is_err());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "resident MIME delimiter scanning allocated");
}

fn mime_protocol_parameters() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_parameter::protocol::{Cursor, Error, Purpose, Status, Value},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let token = format!("text/plain;charset={}", "x".repeat(4096));
    let too_long = format!("multipart/mixed;boundary={}", "x".repeat(71));
    let nested = format!(
        "{}{}text/plain;charset=utf-8",
        "(".repeat(33),
        ")".repeat(33)
    );
    let before = COUNTERS.snapshot();
    for (source, purpose, expected, value) in [
        (
            b"multipart/mixed;boundary=ordinary;boundary*=utf-8''a%20b".as_slice(),
            Purpose::Boundary,
            Some(b"a b".as_slice()),
            Value::Present,
        ),
        (
            b"multipart/mixed;boundary=ordinary;boundary*=utf-8''a%20",
            Purpose::Boundary,
            None,
            Value::Invalid,
        ),
        (
            b"multipart/mixed;boundary*1*=b;boundary*0*=utf-8'en'a",
            Purpose::Boundary,
            Some(b"ab"),
            Value::Present,
        ),
        (
            b"multipart/mixed;boundary=ordinary;boundary*=utf-8''%xx",
            Purpose::Boundary,
            Some(b"ordinary"),
            Value::Present,
        ),
        (
            b"multipart/mixed;boundary=\"=?utf-8?Q?x?=\"",
            Purpose::Boundary,
            Some(b"=?utf-8?Q?x?="),
            Value::Present,
        ),
        (
            b"multipart/mixed;boundary*=unknown''ABC",
            Purpose::Boundary,
            Some(b"ABC"),
            Value::Present,
        ),
        (
            b"text/plain;charset=UtF-8",
            Purpose::Charset,
            Some(b"UtF-8"),
            Value::Present,
        ),
        (
            b"text/plain;charset=unknown",
            Purpose::Charset,
            Some(b"unknown"),
            Value::Present,
        ),
        (
            b"text/plain;charset=ascii;charset*=utf-8''%FF",
            Purpose::Charset,
            None,
            Value::Invalid,
        ),
        (
            b"text/plain;charset=\"\"",
            Purpose::Charset,
            None,
            Value::Invalid,
        ),
        (b"text/plain", Purpose::Charset, None, Value::Absent),
        (too_long.as_bytes(), Purpose::Boundary, None, Value::Invalid),
        (
            token.as_bytes(),
            Purpose::Charset,
            Some(token.as_bytes().get(19..).unwrap()),
            Value::Present,
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 100_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut output = [0; 4096];
        let pointers = (&work as *const _, &budget as *const _);
        let mut cursor = Cursor::new(
            black_box(source),
            purpose,
            &mut output,
            &mut work,
            &mut budget,
        );
        assert!(cursor.value().is_none());
        let mut end = None;
        for _ in 0..100_000 {
            if let Status::Complete(result) = cursor.poll(Tick(1)).unwrap() {
                end = Some(result);
                break;
            }
        }
        let end = end.unwrap();
        assert_eq!(end.value, value);
        assert_eq!(cursor.value(), expected);
        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(end)));
        let (retained, work, budget) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(retained.value(), expected);
        assert_eq!(pointers, (work as *const _, budget as *const _));
    }
    for (source, limit, expired, capacity, wanted) in [
        (
            b"text/plain;charset=utf8".as_slice(),
            100_000,
            false,
            3,
            Error::OutputCapacity,
        ),
        (
            b"text/plain;charset=utf8",
            0,
            false,
            70,
            Error::Parameter(td_mta::mime_parameter::Error::Work(Stop::OutputBytes)),
        ),
        (
            b"text/plain;charset=utf8",
            100_000,
            true,
            70,
            Error::Parameter(td_mta::mime_parameter::Error::Work(Stop::Deadline)),
        ),
        (
            b"text/plain;charset=utf8;broken",
            100_000,
            false,
            70,
            Error::Parameter(td_mta::mime_parameter::Error::Malformed),
        ),
        (
            nested.as_bytes(),
            100_000,
            false,
            70,
            Error::Parameter(td_mta::mime_parameter::Error::NestingLimit),
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: limit,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut output = [0; 70];
        let mut cursor = Cursor::new(
            black_box(source),
            Purpose::Charset,
            output.get_mut(..capacity).unwrap(),
            &mut work,
            &mut budget,
        );
        let mut error = None;
        for _ in 0..100_000 {
            match cursor.poll(if expired { Tick(100) } else { Tick(1) }) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete(_)) => panic!("refused protocol value completed"),
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        assert_eq!(error, Some(wanted));
        assert!(cursor.value().is_none());
        assert_eq!(cursor.check_deadline(Tick(1)), Err(wanted));
        assert_eq!(cursor.poll(Tick(1)), Err(wanted));
        assert!(cursor.finish(Tick(1)).is_err());
    }
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000,
            records: 100_000,
            output_bytes: 100_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut output = [0; 70];
    let mut cursor = Cursor::new(
        black_box(b"text/plain;charset=utf8"),
        Purpose::Charset,
        &mut output,
        &mut work,
        &mut budget,
    );
    let mut done = false;
    for _ in 0..100_000 {
        if matches!(cursor.poll(Tick(1)).unwrap(), Status::Complete(_)) {
            done = true;
            break;
        }
    }
    assert!(done);
    assert!(cursor.finish(Tick(100)).is_err());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "protocol parameter admission allocated");
}

fn mime_parameter_nfc() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_fields::Kind,
        mime_parameter::{
            display::normalized::{Cursor, Error, Status},
            Attribute,
        },
        nfc::{self, HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let long = format!(
        "attachment;filename=\"a{}{}x\"",
        "\u{301}".repeat(257),
        "\u{327}".repeat(257)
    );
    assert!(std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 4608);
    let before = COUNTERS.snapshot();
    for (source, bytes, problem, rejected) in [
        (long.as_bytes(), 1029, false, false),
        (
            b"attachment;filename*=utf-8''e%CC%81".as_slice(),
            2,
            false,
            false,
        ),
        (
            b"attachment;filename*1*=%81;filename*0*=utf-8''e%CC",
            2,
            false,
            false,
        ),
        (
            b"attachment;filename=\"=?utf-8?Q?e?= =?utf-8?Q?=CC=81?=\"",
            2,
            false,
            false,
        ),
        (
            b"attachment;filename=saved;filename*=utf-8''%xx",
            5,
            false,
            true,
        ),
        (b"attachment;filename*=utf-8''%FF", 3, true, false),
        (b"attachment;x=missing", 0, false, false),
    ] {
        for healthy in [true, false] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records: 100_000_000,
                    output_bytes: 100_000_000,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut cursor = Cursor::new(
                black_box(source),
                Kind::ContentDisposition,
                Attribute::Filename,
                &mut scratch,
                &mut work,
                &mut budget,
            )
            .unwrap();
            let mut produced = 0;
            loop {
                match cursor.poll(Tick(1)).unwrap() {
                    Status::Yield => {}
                    Status::Scalar(value) => {
                        cursor
                            .charge_output(Tick(1), value.len_utf8() as u64)
                            .unwrap();
                        produced += value.len_utf8();
                        black_box(value);
                    }
                    Status::Complete(decoded) => {
                        assert_eq!(produced, bytes);
                        assert_eq!(decoded.is_encoding_problem, problem);
                        assert_eq!(decoded.selection.invalid_extended, rejected);
                        if healthy {
                            cursor.check_deadline(Tick(1)).unwrap();
                            let (original_work, original_budget, original_scratch) =
                                cursor.finish(Tick(1)).unwrap();
                            assert!(original_work.remaining().io_bytes > 0);
                            assert!(original_budget.steps_remaining() > 0);
                            black_box(original_scratch);
                            break;
                        }
                        assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(decoded)));
                        let error = Error::Normalization(nfc::Error::Work(Stop::Deadline));
                        assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                        assert_eq!(cursor.poll(Tick(1)), Err(error));
                        assert_eq!(cursor.charge_output(Tick(1), 0), Err(error));
                        assert!(matches!(cursor.finish(Tick(1)), Err(e) if e == error));
                        break;
                    }
                }
            }
        }
    }
    for (charge, expected) in [
        (
            Charge {
                io_bytes: 0,
                records: 100_000,
                output_bytes: 100_000,
                ..Charge::default()
            },
            Stop::IoBytes,
        ),
        (
            Charge {
                io_bytes: 100_000,
                records: 0,
                output_bytes: 100_000,
                ..Charge::default()
            },
            Stop::Records,
        ),
        (
            Charge {
                io_bytes: 100_000,
                records: 100_000,
                output_bytes: 0,
                ..Charge::default()
            },
            Stop::OutputBytes,
        ),
    ] {
        let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), charge);
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut cursor = Cursor::new(
            b"attachment;filename*=utf-8''e%CC%81",
            Kind::ContentDisposition,
            Attribute::Filename,
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let error = loop {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Scalar(c)) => {
                    if let Err(error) = cursor.charge_output(Tick(1), c.len_utf8() as u64) {
                        break error;
                    }
                }
                Ok(Status::Complete(_)) => panic!("allocation probe cut completed"),
                Err(error) => break error,
            }
        };
        let stop = match error {
            Error::Parameter(td_mta::mime_parameter::Error::Work(stop))
            | Error::Normalization(nfc::Error::Work(stop)) => stop,
            other => panic!("unexpected parameter NFC refusal: {other}"),
        };
        assert_eq!(stop, expected);
        assert_eq!(cursor.poll(Tick(1)), Err(error));
        assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
        assert!(matches!(cursor.finish(Tick(1)), Err(e) if e == error));
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME parameter normalization allocated");
}

fn mime_parameter_display() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_fields::Kind,
        mime_parameter::{
            display::{Budgeted, Status},
            Attribute, Error,
        },
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("attachment;filename=\"{}\"", "🐈".repeat(1024));
    let maximal = format!("attachment;filename=\"=?utf-8?Q?{}?=\"", "a".repeat(63));
    let label = format!("attachment;filename*={}''value", "a".repeat(4096));
    let before = COUNTERS.snapshot();
    for (source, kind, attribute, expected, problem, rejected) in [
        (long.as_bytes(), 4096, false, false),
        (maximal.as_bytes(), 63, false, false),
        (label.as_bytes(), 5, true, false),
        (
            b"attachment;filename=\"=?utf-8?Q?a?=\r\n \t=?utf-8?Q?b?= \t\"".as_slice(),
            4,
            false,
            false,
        ),
        (b"attachment;filename=\"\\=?utf-8?Q?x?=\"", 13, false, false),
        (
            b"attachment;filename*=utf-8''%3D%3Futf-8%3FQ%3Fx%3F%3D",
            13,
            false,
            false,
        ),
        (
            b"attachment;filename*0=\"=?utf-8?Q?\";filename*1=\"x?=\"",
            13,
            false,
            false,
        ),
        (b"attachment;filename=\"=?unknown?Q?a?=\"", 15, false, false),
        (b"attachment;filename=\"=?utf-8?Q?=E2?=\"", 3, true, false),
        (
            b"attachment;filename*=utf-8''%00%01%7F%C2%80%EF%B7%90",
            7,
            true,
            false,
        ),
        (b"attachment;filename*=utf-8''%E1%00%80", 6, true, false),
        (
            b"attachment;filename=\"=?utf-8?Q?a?=\";filename*=utf-8''%xx",
            1,
            false,
            true,
        ),
        (b"attachment;filename=\"\"", 0, false, false),
        (b"attachment;filename*=utf-8''", 0, false, false),
        (b"attachment;filename=\"=?utf-8?Q?=00?=\"", 0, false, false),
        (b"attachment;filename=\"a\\\0b\"", 2, false, false),
        (
            b"attachment;filename=\"\\\xc3\xa9 =?utf-8?Q?a?=\"",
            4,
            false,
            false,
        ),
        (b"attachment;x=missing", 0, false, false),
    ]
    .into_iter()
    .map(|(source, expected, problem, rejected)| {
        (
            source,
            Kind::ContentDisposition,
            Attribute::Filename,
            expected,
            problem,
            rejected,
        )
    })
    .chain(std::iter::once((
        b"text/plain;name=\"=?latin1?Q?caf=E9?=\"".as_slice(),
        Kind::ContentType,
        Attribute::Name,
        5,
        false,
        false,
    ))) {
        let case_before = COUNTERS.snapshot();
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(black_box(source), kind, attribute, &mut work, &mut budget);
        assert!(std::mem::size_of_val(&cursor) <= 1536);
        let mut bytes = 0;
        loop {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Scalar(value) => {
                    black_box(value);
                    bytes += value.len_utf8();
                }
                Status::Complete(decoded) => {
                    assert_eq!(bytes, expected);
                    assert_eq!(decoded.is_encoding_problem, problem);
                    assert_eq!(decoded.selection.invalid_extended, rejected);
                    black_box(decoded);
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(decoded)));
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                    break;
                }
            }
        }
        let case_after = COUNTERS.snapshot();
        assert!(!case_before.invalid && !case_after.invalid);
        assert_eq!(
            case_before, case_after,
            "MIME display case allocated: {source:?}"
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME display projection allocated");
}

fn mime_parameter_scalars() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_fields::Kind,
        mime_parameter::{
            scalars::{Budgeted, Status},
            Attribute, Error,
        },
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!(
        "attachment;filename*0=\"{}\";filename*1=tail",
        "🐈".repeat(1024)
    );
    let label = format!("attachment;filename*= {}''value", "a".repeat(4096));
    let before = COUNTERS.snapshot();
    for (source, kind, attribute, expected, problem, rejected) in [
        (long.as_bytes(), 4100, false, false),
        (label.as_bytes(), 5, true, false),
        (
            b"attachment;filename*=unknown''%E2%82%AC%00".as_slice(),
            4,
            true,
            false,
        ),
        (
            b"attachment;filename*1*=%82%AC;filename*0*=utf-8''%E2",
            3,
            false,
            false,
        ),
        (b"attachment;filename*=cp1252''%80%81", 6, true, false),
        (b"attachment;filename*=''", 0, true, false),
        (b"attachment;filename*=utf-8''%E2", 3, true, false),
        (
            b"attachment;filename=saved;filename*=utf-8''%xx",
            5,
            false,
            true,
        ),
        (b"attachment;x=missing", 0, false, false),
    ]
    .into_iter()
    .map(|(source, expected, problem, rejected)| {
        (
            source,
            Kind::ContentDisposition,
            Attribute::Filename,
            expected,
            problem,
            rejected,
        )
    })
    .chain(std::iter::once((
        b"text/plain;name*=iso_8859-1''%E9".as_slice(),
        Kind::ContentType,
        Attribute::Name,
        2,
        false,
        false,
    ))) {
        let case_before = COUNTERS.snapshot();
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 10_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(black_box(source), kind, attribute, &mut work, &mut budget);
        assert!(std::mem::size_of_val(&cursor) <= 1312);
        let mut bytes = 0;
        loop {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Scalar(value) => {
                    black_box(value);
                    bytes += value.len_utf8();
                }
                Status::Complete(decoded) => {
                    assert_eq!(bytes, expected);
                    assert_eq!(decoded.is_encoding_problem, problem);
                    assert_eq!(decoded.selection.invalid_extended, rejected);
                    black_box(decoded);
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(decoded)));
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                    break;
                }
            }
        }
        let case_after = COUNTERS.snapshot();
        assert!(!case_before.invalid && !case_after.invalid);
        assert_eq!(
            case_before, case_after,
            "MIME scalar case allocated: {source:?}"
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME scalar conversion allocated");
}

fn mime_parameter_octets() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_fields::Kind,
        mime_parameter::{Attribute, BudgetedOctets, Error, OctetStatus},
        mime_value::Role,
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let mut long = format!("attachment;x{}=ignored", "x".repeat(4096));
    for index in (0..16).rev() {
        long.push_str(&format!(";filename*{index}=value"));
    }
    let before = COUNTERS.snapshot();
    for (source, expected, rejected) in [
        (long.as_bytes(), [0, 0, 80], false),
        (
            b"attachment;filename*=utf-8'en'%E2%82%AC%00".as_slice(),
            [5, 2, 4],
            false,
        ),
        (
            b"attachment;filename*1*=%82%AC;filename*0*=utf-8''%E2",
            [5, 0, 3],
            false,
        ),
        (
            b"attachment;filename*0*=utf-8''secret;filename*1*=%xx;filename=saved",
            [0, 0, 5],
            true,
        ),
        (b"attachment;filename*=utf-8''%xx", [0, 0, 0], true),
        (b"attachment;x=missing", [0, 0, 0], false),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 10_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = BudgetedOctets::new(
            black_box(source),
            Kind::ContentDisposition,
            Attribute::Filename,
            &mut work,
            &mut budget,
        );
        assert!(std::mem::size_of_val(&cursor) <= 1120);
        let mut counts = [0usize; 3];
        loop {
            match cursor.poll(Tick(1)).unwrap() {
                OctetStatus::Yield => {}
                OctetStatus::Octet { role, value } => {
                    black_box(value);
                    *counts
                        .get_mut(match role {
                            Role::Charset => 0,
                            Role::Language => 1,
                            Role::Data => 2,
                        })
                        .unwrap() += 1;
                }
                OctetStatus::Complete(selection) => {
                    assert_eq!(counts, expected);
                    assert_eq!(selection.invalid_extended, rejected);
                    black_box(selection);
                    assert_eq!(cursor.poll(Tick(100)), Ok(OctetStatus::Complete(selection)));
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                    break;
                }
            }
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME parameter octet replay allocated");
}

fn mime_parameter() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_fields::Kind,
        mime_parameter::{Attribute, Budgeted, Error, Plan, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let mut long = format!("attachment;filename=saved;{}=ignored", "x".repeat(4096));
    for index in (0..24).rev() {
        long.push_str(&format!(";filename*{index}=value"));
    }
    let before = COUNTERS.snapshot();
    for (source, expected, rejected) in [
        (long.as_bytes(), 2, false),
        (b"attachment;filename=one".as_slice(), 0, false),
        (b"attachment;filename*=utf-8'en'%E2%82%AC%00", 1, false),
        (
            b"attachment;filename*0=a;filename*0=b;filename*2=c;filename=saved",
            0,
            true,
        ),
        (b"attachment;filename*=utf-8''%x0;filename=saved", 0, true),
        (b"attachment;filename*01=bad", 3, true),
        (b"attachment;x=one", 3, false),
        (b"attachment;filename=saved;", 4, false),
    ] {
        let case_before = COUNTERS.snapshot();
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 10_000_000,
                records: 10_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(
            black_box(source),
            Kind::ContentDisposition,
            Attribute::Filename,
            &mut work,
            &mut budget,
        );
        assert!(std::mem::size_of_val(&cursor) <= 1056);
        loop {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete(selection)) => {
                    let kind = match selection.plan {
                        Some(Plan::Ordinary(_)) => 0,
                        Some(Plan::Extended(_)) => 1,
                        Some(Plan::Sections { .. }) => 2,
                        None => 3,
                    };
                    assert_eq!(kind, expected);
                    assert_eq!(selection.invalid_extended, rejected);
                    black_box(selection);
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(selection)));
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                    break;
                }
                Err(error) => {
                    assert_eq!(expected, 4);
                    assert_eq!(error, Error::Malformed);
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    break;
                }
            }
        }
        let case_after = COUNTERS.snapshot();
        assert!(!case_before.invalid && !case_after.invalid);
        assert_eq!(
            case_before, case_after,
            "MIME parameter candidate case allocated: {source:?}"
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME parameter candidate replay allocated");
}

fn mime_value() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_value::{Budgeted, Error, Mode, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("\"{}\"", "a".repeat(65_536));
    let unicode = format!("\"{}\"", "🐈".repeat(4096));
    let before = COUNTERS.snapshot();
    for (source, quoted, mode, expected) in [
        (long.as_bytes(), true, Mode::Ordinary, Ok(())),
        (unicode.as_bytes(), true, Mode::Ordinary, Ok(())),
        (
            b"UTF-8'en'%E2%82%AC%00".as_slice(),
            false,
            Mode::ExtendedInitial,
            Ok(()),
        ),
        (
            b"\"''%3D%3Futf-8%3FQ%3Fx%3F%3D\"",
            true,
            Mode::ExtendedInitial,
            Ok(()),
        ),
        (
            b"abc%",
            false,
            Mode::ExtendedContinuation,
            Err(Error::Malformed),
        ),
        (b"\"a\r\n\tb\"", true, Mode::Ordinary, Ok(())),
        (b"\"a\\\r\\\n\\ b\"", true, Mode::Ordinary, Ok(())),
        (b"\"a\\\r\\\nb\"", true, Mode::Ordinary, Ok(())),
        (
            b"utf-8'en-'x",
            false,
            Mode::ExtendedInitial,
            Err(Error::Malformed),
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1_000_000,
                records: 1_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(black_box(source), quoted, mode, &mut work, &mut budget);
        assert!(std::mem::size_of_val(&cursor) <= 192);
        loop {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Octet { role, value }) => {
                    black_box((role, value));
                }
                Ok(Status::Complete) => {
                    assert_eq!(expected, Ok(()));
                    assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                    assert_eq!(
                        cursor.check_deadline(Tick(100)),
                        Err(Error::Work(Stop::Deadline))
                    );
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                    break;
                }
                Err(error) => {
                    assert_eq!(expected, Err(error));
                    assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                    assert_eq!(cursor.poll(Tick(1)), Err(error));
                    break;
                }
            }
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME parameter octet projection allocated");
}

fn mime_attribute() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_attribute::{Budgeted, Error, Form, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("{}*123*", "a".repeat(65_536));
    let before = COUNTERS.snapshot();
    for (source, form) in [
        (
            long.as_bytes(),
            Form::Section {
                index: 123,
                encoded: true,
            },
        ),
        (b"FiLeNaMe*".as_slice(), Form::Extended),
        (b"filename*01*".as_slice(), Form::Malformed),
        (b"filename*18446744073709551616".as_slice(), Form::Malformed),
        (b"filename".as_slice(), Form::Ordinary),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1_000_000,
                records: 1_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(black_box(source), &mut work, &mut budget);
        assert!(std::mem::size_of_val(&cursor) <= 160);
        loop {
            if let Status::Complete(name) = cursor.poll(Tick(1)).unwrap() {
                assert_eq!(name.form, form);
                black_box(name);
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete(name)));
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
                assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
                break;
            }
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME attribute classification allocated");
}

fn mime_metadata() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_select::SourceEnd,
        mime_headers,
        mime_metadata::{Budgeted, ContentType, Context, DefaultType, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!(
        concat!(
            "Content-Type: ({0})text/plain; name=\"{0}\"; x={1}\r\n",
            "Content-Disposition: attachment; filename*1*=b; ",
            "filename*0*=utf-8''a\r\n",
            "Content-Transfer-Encoding: BASE64\r\n\r\nbody"
        ),
        "🐈".repeat(8192),
        "a".repeat(65_536)
    );
    let nested = format!(
        "Content-Type: text/plain\r\nContent-Disposition: attachment {}x{}\r\n\r\n",
        "(".repeat(33),
        ")".repeat(33)
    );
    let before = COUNTERS.snapshot();
    for (source, context, limit, field, error) in [
        (
            long.as_bytes(),
            Context::Normal,
            long.len() as u64,
            true,
            None,
        ),
        (
            b"Content-Type: text/html;\r\nContent-Type: text/plain\r\n\r\n",
            Context::Normal,
            1000,
            true,
            None,
        ),
        (
            b"Content-Type: text/html;\r\n\r\n",
            Context::Normal,
            1000,
            false,
            None,
        ),
        (b"\r\n", Context::DigestChild, 1000, false, None),
        (
            nested.as_bytes(),
            Context::Normal,
            1000,
            false,
            Some(Error::NestingLimit),
        ),
        (
            b"Content-Type: text/plain\r\nX-Long: aaaaa\r\n\r\n",
            Context::Normal,
            30,
            false,
            Some(Error::Headers(mime_headers::Error::HeaderLimit)),
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 16 * 1024 * 1024,
                records: 2_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(
            black_box(source),
            0,
            limit,
            context,
            SourceEnd::Eof,
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert!(std::mem::size_of_val(&cursor) <= 1024);
        assert_eq!(cursor.selection(), Ok(None));
        let result = loop {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield) => {}
                Ok(Status::Complete) => break None,
                Err(error) => break Some(error),
            }
        };
        assert_eq!(result, error);
        if let Some(error) = result {
            assert_eq!(cursor.selection(), Err(error));
            assert_eq!(cursor.poll(Tick(1)), Err(error));
        } else {
            let selection = cursor.selection().unwrap().unwrap();
            assert_eq!(
                matches!(selection.content_type, ContentType::Field(_)),
                field
            );
            if !field {
                let expected = if context == Context::DigestChild {
                    DefaultType::MessageRfc822
                } else {
                    DefaultType::TextPlain
                };
                assert_eq!(selection.content_type, ContentType::Default(expected));
            }
            black_box(selection);
            cursor.check_deadline(Tick(1)).unwrap();
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.selection(), Err(Error::Work(Stop::Deadline)));
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME metadata selection allocated");
}

fn mime_fields() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        mime_fields::{Budgeted, Error, Kind, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!(
        "({0})text/plain; name=\"{0}\"; x={1}",
        "🐈".repeat(8192),
        "a".repeat(65_536)
    );
    let nested = format!("text/plain {}x{}", "(".repeat(33), ")".repeat(33));
    let before = COUNTERS.snapshot();
    for (source, kind, parameters, error) in [
        (long.as_bytes(), Kind::ContentType, 2, None),
        (
            b"attachment; filename*1*=b; filename*0*=utf-8''a",
            Kind::ContentDisposition,
            2,
            None,
        ),
        (b"(note) BASE64", Kind::TransferEncoding, 0, None),
        (
            b"text/plain; ok=y; bad=",
            Kind::ContentType,
            1,
            Some(Error::Malformed),
        ),
        (
            nested.as_bytes(),
            Kind::ContentType,
            0,
            Some(Error::NestingLimit),
        ),
    ] {
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 16 * 1024 * 1024,
                records: 2_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut cursor = Budgeted::new(black_box(source), kind, &mut work, &mut budget);
        let mut count = 0;
        let result = loop {
            match cursor.poll(Tick(1)) {
                Ok(Status::Yield | Status::Head(_)) => {}
                Ok(Status::Parameter(value)) => {
                    black_box(value);
                    count += 1;
                }
                Ok(Status::Complete) => break None,
                Err(error) => break Some(error),
            }
        };
        assert_eq!(result, error);
        assert_eq!(count, parameters);
        if let Some(error) = result {
            assert_eq!(cursor.poll(Tick(1)), Err(error));
        } else {
            cursor.check_deadline(Tick(1)).unwrap();
            assert_eq!(
                cursor.check_deadline(Tick(100)),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MIME field syntax allocated");
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
        mime_unfold::{Decoder, Error, Status},
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
        Err(Error::Work(Stop::IoBytes))
    );
    let mut fresh = budget(100);
    let remaining = fresh.remaining();
    assert_eq!(
        decoder.poll(b"a", &mut [0; 1], true, Tick(1), &mut fresh),
        Err(Error::Work(Stop::IoBytes))
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

fn header_address_text() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_address_text::{Cursor, Error, Mode, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<usize, Error> {
        let mut bytes = 0;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete => return Ok(bytes),
                Status::Yield => {}
                Status::Scalar(value) => bytes += black_box(value).len_utf8(),
            }
        }
        panic!("address text allocation probe did not finish");
    }
    let parsed = format!("{}@b", "🐈".repeat(1000));
    let fallback = format!(" \t{}\r\n ", "🐈".repeat(1000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, mode, expected, problem) in [
        (parsed.as_bytes(), Mode::Parsed, Ok(4002), false),
        (fallback.as_bytes(), Mode::Fallback, Ok(4000), false),
        (
            b"a@b bad".as_slice(),
            Mode::Parsed,
            Err(Error::Malformed),
            false,
        ),
        (b"\xffx\xe2\x82", Mode::Fallback, Ok(7), true),
        ("\u{fdd0}@b".as_bytes(), Mode::Parsed, Ok(5), true),
        (b" \t\r\n", Mode::Fallback, Ok(0), false),
        (b"\0a\r\n b", Mode::Fallback, Ok(4), false),
    ] {
        let mut cursor = Cursor::new(black_box(source), mode);
        assert_eq!(drive(&mut cursor, &mut work), expected);
        assert_eq!(cursor.is_encoding_problem(), problem);
    }
    for mode in [Mode::Parsed, Mode::Fallback] {
        let mut cursor = Cursor::new(b"a@b", mode);
        let mut limited = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1000,
                records: 1000,
                output_bytes: 1,
                ..Charge::default()
            },
        );
        assert_eq!(
            drive(&mut cursor, &mut limited),
            Err(Error::Work(Stop::OutputBytes))
        );
        assert_eq!(
            drive(&mut cursor, &mut work),
            Err(Error::Work(Stop::OutputBytes))
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "address text projection allocated");
}

fn header_address_groups() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_addresses::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<usize, Error> {
        let mut events = 0;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete => return Ok(events),
                Status::Yield => {}
                event => {
                    black_box(event);
                    events += 1;
                }
            }
        }
        panic!("address group allocation probe did not finish");
    }
    let source = format!("{}: {}@b;", "🐈".repeat(1000), "é".repeat(1000));
    let nested = format!("a@b,{}", "(".repeat(33));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, expected) in [
        (source.as_bytes(), Ok(3)),
        (b"a@b, G: <@route:c@d>;bad".as_slice(), Ok(9)),
        (b"G:; H:(comment);;", Ok(4)),
        (b"G: bad,Nested:c@d", Ok(4)),
        (b"\"unclosed,tail", Ok(3)),
        (b"a@b, (unclosed, tail", Ok(4)),
        (b"(\xff) a@b", Ok(3)),
        (b" ;,\r\n", Ok(0)),
        (nested.as_bytes(), Err(Error::NestingLimit)),
    ] {
        assert_eq!(
            drive(&mut Cursor::new(black_box(source)), &mut work),
            expected
        );
    }
    let mut cursor = Cursor::new(b"G: a@b;");
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 20,
            records: 1000,
            ..Charge::default()
        },
    );
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::IoBytes))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::IoBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "address group parsing allocated");
}

fn header_single_mailbox() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_mailbox::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<(), Error> {
        for _ in 0..100_000 {
            if let Status::Complete(mailbox) = cursor.poll(Tick(1), work)? {
                black_box(mailbox);
                return Ok(());
            }
        }
        panic!("mailbox allocation probe did not finish");
    }
    let source = format!(
        "{} <(c),@route,,:{}@b(Name)>",
        "🐈".repeat(1000),
        "é".repeat(1000)
    );
    let nested = "(".repeat(33);
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, expected) in [
        (source.as_bytes(), Ok(())),
        (b"a@b (name)".as_slice(), Ok(())),
        (b"\"a\r\n b\" <\"x\r\n y\"@[x,;]>", Ok(())),
        (b"Name <@a,b:c@d>", Err(Error::Malformed)),
        (b"Name <a@b> junk", Err(Error::Malformed)),
        (b"a@b\r\n", Err(Error::Malformed)),
        (nested.as_bytes(), Err(Error::NestingLimit)),
    ] {
        let mut cursor = Cursor::new(black_box(source));
        assert_eq!(drive(&mut cursor, &mut work), expected);
    }
    let mut cursor = Cursor::new(b"Name <@route:a@b>");
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 20,
            records: 1000,
            ..Charge::default()
        },
    );
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::IoBytes))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::IoBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "mailbox parsing allocated");
}

fn shared_character_projection() {
    use td_header::projection::{character, CharacterError, Error};
    let before = COUNTERS.snapshot();
    for source in [
        b"a".as_slice(),
        "é".as_bytes(),
        "例".as_bytes(),
        "🐈".as_bytes(),
        b"\\\xc3\\\xa9",
        b"\\\r\\\n\\\t",
        b"\\\0",
        "\u{10ffff}".as_bytes(),
        b"\xed\xa0\x80",
        b"\\",
        b"",
    ] {
        let mut count = 0;
        let _ = black_box(character(
            0,
            true,
            &mut count,
            |count, at| {
                *count += 1;
                Ok::<_, u8>(black_box(source).get(at).copied())
            },
            |count, width| {
                *count += 1;
                black_box(width);
                Ok(())
            },
        ));
        for cut in 0..count {
            let mut attempted = 0;
            let result = character(
                0,
                true,
                &mut attempted,
                |n, at| {
                    *n += 1;
                    if *n > cut {
                        Err(7)
                    } else {
                        Ok(source.get(at).copied())
                    }
                },
                |n, _| {
                    *n += 1;
                    if *n > cut {
                        Err(7)
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(matches!(
                result,
                Err(CharacterError::Projection(Error::Read(7)) | CharacterError::Verify(7))
            ));
            assert_eq!(attempted, cut + 1);
        }
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "shared character projection allocated");
}

fn header_phrase_display() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_phrase::{
            self,
            decode::{Cursor, Error, Status},
            Extent,
        },
        ports::{Deadline, Tick},
    };
    let long = format!("\" {} \"", "🐈".repeat(10_000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 10_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, expected) in [
        (long.as_bytes(), 10_000),
        (b"=?utf-8?q?one?= \r\n\t=?utf-8?q?two?=".as_slice(), 6),
        (b"=?utf-8?q?one?= (x) =?utf-8?q?two?=", 7),
        (b"=?utf-8?q?one?=\r\n =?utf-8?q?two?=", 6),
        (b"=?utf-8?q?one?=\n =?utf-8?q?two?=", 6),
        (b"\"\\\0 a\"", 1),
        (b"\"a \\\0\"", 1),
        (b"\" \t\"", 0),
        (b"\"a\\\"b\"", 3),
        (b"=?utf-8?q?=FF?=", 1),
        ("例\u{fdd0}".as_bytes(), 2),
    ] {
        let mut parser = header_phrase::Cursor::new(black_box(source));
        while !matches!(
            parser.poll(Tick(1), &mut work).unwrap(),
            header_phrase::Status::Complete(_)
        ) {}
        let mut cursor = Cursor::new(
            parser.into_validated().unwrap(),
            source,
            Extent {
                start: 0,
                end: source.len(),
            },
        )
        .unwrap();
        let initial = cursor;
        let mut count = 0;
        loop {
            let mut copy = cursor;
            let status = cursor.poll(Tick(1), &mut work).unwrap();
            assert_eq!(copy.poll(Tick(1), &mut work).unwrap(), status);
            match status {
                Status::Yield => {}
                Status::Scalar(value) => {
                    black_box(value);
                    count += 1;
                }
                Status::Complete => break,
            }
        }
        assert_eq!(count, expected);
        let mut refused = initial;
        let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        assert_eq!(
            refused.poll(Tick(1), &mut limited),
            Err(Error::Work(Stop::Records))
        );
        let mut copy = refused;
        assert_eq!(
            copy.poll(Tick(1), &mut work),
            Err(Error::Work(Stop::Records))
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "phrase display decoding allocated");
}

fn header_phrase_replay() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_phrase::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    let source = format!("{} (x)\"name\\\"tail\". (last)", "🐈".repeat(10_000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for source in [source.as_bytes(), b"a\r\n \"b\\\"c\" (x(y))".as_slice()] {
        let mut parser = Cursor::new(black_box(source));
        while !matches!(
            parser.poll(Tick(1), &mut work).unwrap(),
            Status::Complete(_)
        ) {}
        let proof = parser.into_validated().unwrap();
        let mut replay = proof.replay();
        loop {
            let checkpoint = replay;
            let status = replay.poll(Tick(1), &mut work).unwrap();
            let mut copy = black_box(checkpoint);
            assert_eq!(copy.poll(Tick(1), &mut work).unwrap(), status);
            if matches!(status, Status::Complete(_)) {
                break;
            }
        }
        let mut replay = proof.replay();
        let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        assert_eq!(
            replay.poll(Tick(1), &mut limited),
            Err(Error::Work(Stop::IoBytes))
        );
        let mut copy = replay;
        assert_eq!(
            copy.poll(Tick(1), &mut work),
            Err(Error::Work(Stop::IoBytes))
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "phrase replay allocated");
}

fn header_phrase_tokens() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_phrase::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<usize, Error> {
        let mut count = 0;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete(_) => return Ok(count),
                Status::Token(extent) => {
                    black_box(extent);
                    count += 1;
                }
                Status::Yield => {}
            }
        }
        panic!("phrase allocation probe did not finish");
    }
    let source = format!("{} (x)\"name\".", "🐈".repeat(10_000));
    let nested = "(".repeat(33);
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, expected) in [
        (source.as_bytes(), Ok(3)),
        (b"\"a\r\n b\" Name.".as_slice(), Ok(3)),
        (b"\"a\r\n b\".@bad", Err(Error::Malformed)),
        (b"a. (unclosed", Err(Error::Malformed)),
        (nested.as_bytes(), Err(Error::NestingLimit)),
    ] {
        let mut cursor = Cursor::new(black_box(source));
        let result = drive(&mut cursor, &mut work);
        assert_eq!(result, expected);
    }
    let mut cursor = Cursor::new(b"a");
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 3,
            records: 100,
            ..Charge::default()
        },
    );
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::IoBytes))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::IoBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "phrase parsing allocated");
}

fn header_single_addr_spec() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_addr_spec::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<usize, Error> {
        let mut count = 0;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete => return Ok(count),
                Status::Part(extent) => {
                    black_box(extent);
                    count += 1;
                }
                Status::Yield => {}
            }
        }
        panic!("addr-spec allocation probe did not finish");
    }
    let source = format!("{}@(x)example", "🐈".repeat(10_000));
    let nested = "(".repeat(33);
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, expected) in [
        (source.as_bytes(), Ok(3)),
        (b"\"a\r\n b\"@[c\n\td]".as_slice(), Ok(3)),
        (b"a@b, c@d", Err(Error::Malformed)),
        (b"a@b>", Err(Error::Malformed)),
        (nested.as_bytes(), Err(Error::NestingLimit)),
    ] {
        let mut cursor = Cursor::new(black_box(source));
        let result = drive(&mut cursor, &mut work);
        assert_eq!(result, expected);
    }
    let mut cursor = Cursor::new(b"a@b");
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 3,
            records: 100,
            ..Charge::default()
        },
    );
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::IoBytes))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::IoBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "addr-spec parsing allocated");
}

fn header_address_boundaries() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_address_items::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<usize, Error> {
        let mut count = 0;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete => return Ok(count),
                Status::Item(item) => {
                    black_box(item);
                    count += 1;
                }
                Status::Yield => {}
            }
        }
        panic!("address boundary allocation probe did not finish");
    }
    let long = format!("\"{}\" <a@b>,c@d", "é,;".repeat(100_000));
    let comments = "(".repeat(33);
    let angles = "<".repeat(33);
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 10_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, expected) in [
        (long.as_bytes(), Ok(2)),
        (b"a@[x,y];b".as_slice(), Ok(2)),
        (b"ok,\"a,b;c", Ok(2)),
        (b";,,", Ok(4)),
        (comments.as_bytes(), Err(Error::NestingLimit)),
        (angles.as_bytes(), Err(Error::NestingLimit)),
    ] {
        assert_eq!(
            drive(&mut Cursor::new(black_box(source)), &mut work),
            expected
        );
    }
    let mut cursor = Cursor::new(b"a,b");
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 2,
            records: 100,
            ..Charge::default()
        },
    );
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::IoBytes))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::IoBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "address boundary scan allocated");
}

fn header_url_text() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_urls::{Cursor, Error, Mode, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<(usize, usize), Error> {
        let mut ends = 0;
        let mut bytes = 0;
        for _ in 0..1_000_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete => return Ok((ends, bytes)),
                Status::End => ends += 1,
                Status::Byte(value) => {
                    black_box(value);
                    bytes += 1;
                }
                _ => {}
            }
        }
        panic!("URL allocation probe did not finish");
    }
    let source = format!("<x:/{}>", "a".repeat(100_000));
    let future = format!("<x://[v1.{}]>", "a".repeat(100_000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 10_000_000,
            output_bytes: 10_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, mode, expected) in [
        (source.as_bytes(), Mode::URLs, Ok((1, 100003))),
        (future.as_bytes(), Mode::URLs, Ok((1, 100009))),
        (b"NO".as_slice(), Mode::ListPost, Ok((0, 0))),
        (b"<mailto:l@x>, <https://x/>", Mode::ListPost, Ok((2, 20))),
        (b"<x://[::1]>", Mode::URLs, Ok((1, 9))),
        (b"<x: \r\n a>", Mode::URLs, Ok((1, 3))),
        (b"<x:>, bad", Mode::URLs, Err(Error::Malformed)),
        (b"<x://[:::]>", Mode::URLs, Err(Error::Malformed)),
    ] {
        assert_eq!(
            drive(&mut Cursor::new(black_box(source), mode), &mut work),
            expected
        );
    }
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1000,
            records: 64,
            output_bytes: 1000,
            ..Charge::default()
        },
    );
    let mut cursor = Cursor::new(b"<x://[::1]>", Mode::URLs);
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::Records))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::Records))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "URL parsing allocated");
}

fn header_message_id_text() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_message_ids::{
            project::{Cursor, Status},
            Error, Mode,
        },
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<(usize, usize), Error> {
        let mut ends = 0;
        let mut scalars = 0;
        for _ in 0..1_000_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete => return Ok((ends, scalars)),
                Status::End => ends += 1,
                Status::Scalar(value) => {
                    black_box(value);
                    scalars += 1;
                }
                _ => {}
            }
        }
        panic!("message-id text allocation probe did not finish");
    }
    let source = format!("<{}@b>", "🐈".repeat(10_000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 10_000_000,
            output_bytes: 10_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, mode, expected, problem) in [
        (source.as_bytes(), Mode::Strict, Ok((1, 10002)), false),
        (b"".as_slice(), Mode::ObsoletePhrases, Ok((0, 0)), false),
        (b"<\"a\r\n b\"@[c\n\td]>", Mode::Strict, Ok((1, 11)), false),
        ("<\u{fdd0}@b>".as_bytes(), Mode::Strict, Ok((1, 3)), true),
        (b"<a@b><bad>", Mode::Strict, Err(Error::Malformed), false),
    ] {
        let mut cursor = Cursor::new(black_box(source), mode);
        assert_eq!(drive(&mut cursor, &mut work), expected);
        assert_eq!(cursor.is_encoding_problem(), problem);
    }
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1000,
            records: 1000,
            output_bytes: 1,
            ..Charge::default()
        },
    );
    let mut cursor = Cursor::new(b"<a@b>", Mode::Strict);
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::OutputBytes))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::OutputBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "message-id projection allocated");
}

fn header_message_id_lists() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_message_ids::{Cursor, Error, Mode, Status},
        ports::{Deadline, Tick},
    };
    fn drive(cursor: &mut Cursor<'_>, work: &mut Meter) -> Result<usize, Error> {
        let mut ends = 0;
        for _ in 0..100_000 {
            match cursor.poll(Tick(1), work)? {
                Status::Complete => return Ok(ends),
                Status::End => ends += 1,
                Status::Part(extent) => {
                    black_box(extent);
                }
                _ => {}
            }
        }
        panic!("message-id allocation probe did not finish");
    }
    let source = format!("<{}@(x)example> <\"a b\"@[c d]>", "🐈".repeat(10_000));
    let nesting = format!("{}<a@b>", "(".repeat(33));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 10_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, mode, expected) in [
        (source.as_bytes(), Mode::Strict, Ok(2)),
        (b"".as_slice(), Mode::ObsoletePhrases, Ok(0)),
        (
            b"old phrase <a@b> trailing...",
            Mode::ObsoletePhrases,
            Ok(1),
        ),
        (b"<a@b><bad>", Mode::Strict, Err(Error::Malformed)),
        (b"<a@[broken>", Mode::Strict, Err(Error::Malformed)),
        (nesting.as_bytes(), Mode::Strict, Err(Error::NestingLimit)),
    ] {
        let mut cursor = Cursor::new(black_box(source), mode);
        assert_eq!(drive(&mut cursor, &mut work), expected);
    }
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 5,
            records: 100,
            ..Charge::default()
        },
    );
    let mut cursor = Cursor::new(b"<a@b>", Mode::Strict);
    assert_eq!(
        drive(&mut cursor, &mut limited),
        Err(Error::Work(Stop::IoBytes))
    );
    assert_eq!(
        drive(&mut cursor, &mut work),
        Err(Error::Work(Stop::IoBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "message-id parsing allocated");
}

fn header_delimited_tokens() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_delimited::{Cursor, Error, Extent, Kind, Status},
        ports::{Deadline, Tick},
    };
    let quoted = format!("\"{}\"", "🐈".repeat(10_000));
    let literal = format!("[{}]", "🐈".repeat(10_000));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 100_000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (source, kind) in [
        (quoted.as_bytes(), Kind::QuotedString),
        (literal.as_bytes(), Kind::DomainLiteral),
        (b"\"a\\\"b\"".as_slice(), Kind::QuotedString),
        (b"[a\\]b]".as_slice(), Kind::DomainLiteral),
    ] {
        let mut cursor = Cursor::new(black_box(source), 0, kind);
        let mut complete = false;
        for _ in 0..10_000 {
            if let Status::Complete(extent) = cursor.poll(Tick(1), &mut work).unwrap() {
                assert_eq!(
                    extent,
                    Extent {
                        start: 0,
                        end: source.len()
                    }
                );
                complete = true;
                break;
            }
        }
        assert!(complete);
    }
    for (source, kind) in [
        (b"\"\\".as_slice(), Kind::QuotedString),
        (b"[\\".as_slice(), Kind::DomainLiteral),
    ] {
        let mut cursor = Cursor::new(source, 0, kind);
        assert_eq!(cursor.poll(Tick(1), &mut work), Err(Error::Malformed));
    }
    for (source, kind) in [
        (quoted.as_bytes(), Kind::QuotedString),
        (literal.as_bytes(), Kind::DomainLiteral),
    ] {
        let mut limited = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                records: 100,
                ..Charge::default()
            },
        );
        let mut cursor = Cursor::new(source, 0, kind);
        assert_eq!(
            cursor.poll(Tick(1), &mut limited),
            Err(Error::Work(Stop::IoBytes))
        );
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "delimited header token scanning allocated");
}

fn budgeted_date_projection() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_date::{
            project::{render_with_budget, Error, Outcome},
            Date, Offset,
        },
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let ordinary = Date {
        year: 2000,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        offset: Offset::Known(5999),
    };
    let leap = Date {
        year: 2016,
        month: 12,
        day: 31,
        hour: 23,
        minute: 59,
        second: 60,
        offset: Offset::Known(0),
    };
    let mut budget = HeaderBudget::new();
    let mut output = [0xa5; 25];
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records: 1000,
            output_bytes: 1000,
            ..Charge::default()
        },
    );
    let before = COUNTERS.snapshot();
    for (date, expected) in [
        (ordinary, "1999-12-27T20:01:00Z"),
        (
            Date {
                offset: Offset::Unknown,
                ..ordinary
            },
            "2000-01-01T00:00:00-00:00",
        ),
        (leap, "2016-12-31T23:59:60Z"),
    ] {
        assert_eq!(
            render_with_budget(
                black_box(date),
                &mut output,
                Tick(1),
                &mut work,
                &mut budget
            ),
            Ok(Outcome::Date(expected))
        );
    }
    assert_eq!(
        render_with_budget(
            Date { year: 2020, ..leap },
            &mut output,
            Tick(1),
            &mut work,
            &mut budget
        ),
        Ok(Outcome::LeapSecondUnverified)
    );
    assert_eq!(
        render_with_budget(
            Date {
                year: u16::MAX,
                ..ordinary
            },
            &mut output,
            Tick(1),
            &mut work,
            &mut budget
        ),
        Ok(Outcome::OutOfRange)
    );
    output.fill(0xa5);
    let mut short = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records: 1000,
            output_bytes: 19,
            ..Charge::default()
        },
    );
    assert_eq!(
        render_with_budget(ordinary, &mut output, Tick(1), &mut short, &mut budget),
        Err(Error::Work(Stop::OutputBytes))
    );
    assert_eq!(output, [0xa5; 25]);
    assert_eq!(
        render_with_budget(ordinary, &mut output, Tick(100), &mut work, &mut budget),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(output, [0xa5; 25]);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted date formatting allocated");
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
    let leap = Date {
        year: 2016,
        month: 12,
        day: 31,
        hour: 23,
        minute: 59,
        second: 60,
        offset: Offset::Known(0),
    };
    for (offset, expected) in [
        (Offset::Known(0), "2016-12-31T23:59:60Z"),
        (Offset::Unknown, "2016-12-31T23:59:60-00:00"),
    ] {
        assert_eq!(
            render(
                black_box(Date { offset, ..leap }),
                &mut output,
                Tick(1),
                &mut work
            ),
            Ok(Outcome::Date(expected))
        );
    }
    let mut short = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records: 13,
            output_bytes: 100,
            ..Charge::default()
        },
    );
    assert_eq!(
        render(leap, &mut output, Tick(1), &mut short),
        Err(Error::Work(Stop::Records))
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

fn url_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property::{self, Context},
        header_select::SourceEnd,
        header_value::{Input, Status, URLs},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let url = format!("https://EXAMPLE/{}", "path/".repeat(4096));
    let source = format!("List-Post:<{url}>\nList-Post:<bad>\nList-Post:NO\n\n");
    let expected = format!("[[\"{url}\"],null,[]]");
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut property_cursor =
        header_property::Cursor::new("header:List-Post:asURLs:all", Context::Email);
    let property = loop {
        if let header_property::Status::Complete(value) =
            property_cursor.poll(Tick(1), &mut work).unwrap()
        {
            break value.unwrap();
        }
    };
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    let mut cursor = URLs::new(
        Input {
            bytes: black_box(source.as_bytes()),
            base: 0,
            header_limit: source.len() as u64,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut complete = false;
    for _ in 0..1_000_000 {
        let mut output = [0; 1];
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        if progress.written == 1 {
            assert_eq!(output.first(), expected.as_bytes().get(offset));
        }
        offset += progress.written;
        if matches!(progress.status, Status::Complete(_)) {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(offset, expected.len());
    cursor.check_deadline(Tick(1)).unwrap();
    let source = b"List-Post:<x:a>\nList-Post:<x:b>\n\n";
    let mut cursor = URLs::new(
        Input {
            bytes: source,
            base: 0,
            header_limit: b"List-Post:<x:a>\n".len() as u64 + 1,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut refused = false;
    let expected = b"[[\"x:a\"]";
    for _ in 0..1000 {
        let mut output = [0; 1];
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                if progress.written == 1 {
                    assert_eq!(output.first(), expected.get(offset));
                }
                offset += progress.written;
                assert!(!matches!(progress.status, Status::Complete(_)));
            }
            Err(error) => {
                assert!(matches!(error, td_mta::header_value::Error::Selection(_)));
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(offset, expected.len());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "URLs property assembly allocated");
}

fn address_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property::{self, Context},
        header_select::SourceEnd,
        header_value::{Addresses, Input, Status},
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let name = "e\u{301}".repeat(4096);
    let address = format!("{name}@EXAMPLE");
    let source = format!("To:G:{name} <{address}>,bad; Empty:;\nTo:a@b(Name)\n\n");
    let expected = format!(
        "[[{{\"name\":\"{}\",\"email\":\"{address}\"}},{{\"name\":null,\"email\":\"bad\"}}],[{{\"name\":\"Name\",\"email\":\"a@b\"}}]]",
        "é".repeat(4096),
    );
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut property_cursor =
        header_property::Cursor::new("header:To:asAddresses:all", Context::Email);
    let property = loop {
        if let header_property::Status::Complete(value) =
            property_cursor.poll(Tick(1), &mut work).unwrap()
        {
            break value.unwrap();
        }
    };
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    let before = COUNTERS.snapshot();
    let mut cursor = Addresses::new(
        Input {
            bytes: black_box(source.as_bytes()),
            base: 0,
            header_limit: source.len() as u64,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut complete = false;
    for _ in 0..1_000_000 {
        let mut output = [0; 1];
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        if progress.written == 1 {
            assert_eq!(output.first(), expected.as_bytes().get(offset));
        }
        offset += progress.written;
        if matches!(progress.status, Status::Complete(_)) {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(offset, expected.len());
    cursor.check_deadline(Tick(1)).unwrap();
    let source = b"To:a@b\nTo:c@d\n\n";
    let mut cursor = Addresses::new(
        Input {
            bytes: source,
            base: 0,
            header_limit: b"To:a@b\n".len() as u64 + 1,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut refused = false;
    let expected = br#"[[{"name":null,"email":"a@b"}]"#;
    for _ in 0..1000 {
        let mut output = [0; 1];
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                if progress.written == 1 {
                    assert_eq!(output.first(), expected.get(offset));
                }
                offset += progress.written;
                assert!(!matches!(progress.status, Status::Complete(_)));
            }
            Err(error) => {
                assert!(matches!(error, td_mta::header_value::Error::Selection(_)));
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(offset, expected.len());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Addresses property assembly allocated");
}

fn grouped_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property::{self, Context},
        header_select::SourceEnd,
        header_value::{GroupedAddresses, Input, Status},
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let name = "e\u{301}".repeat(4096);
    let address = format!("{name}@EXAMPLE");
    let source = format!("To:{name}:{name} <{address}>,bad; Empty:;\nTo:a@b(Name)\n\n");
    let expected = format!(
        "[[{{\"name\":\"{}\",\"addresses\":[{{\"name\":\"{}\",\"email\":\"{address}\"}},{{\"name\":null,\"email\":\"bad\"}}]}},{{\"name\":\"Empty\",\"addresses\":[]}}],[{{\"name\":null,\"addresses\":[{{\"name\":\"Name\",\"email\":\"a@b\"}}]}}]]",
        "é".repeat(4096), "é".repeat(4096),
    );
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut property_cursor =
        header_property::Cursor::new("header:To:asGroupedAddresses:all", Context::Email);
    let property = loop {
        if let header_property::Status::Complete(value) =
            property_cursor.poll(Tick(1), &mut work).unwrap()
        {
            break value.unwrap();
        }
    };
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    let before = COUNTERS.snapshot();
    let mut cursor = GroupedAddresses::new(
        Input {
            bytes: black_box(source.as_bytes()),
            base: 0,
            header_limit: source.len() as u64,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut complete = false;
    for _ in 0..1_000_000 {
        let mut output = [0; 1];
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        if progress.written == 1 {
            assert_eq!(output.first(), expected.as_bytes().get(offset));
        }
        offset += progress.written;
        if matches!(progress.status, Status::Complete(_)) {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(offset, expected.len());
    cursor.check_deadline(Tick(1)).unwrap();
    let source = b"To:a@b\nTo:c@d\n\n";
    let mut cursor = GroupedAddresses::new(
        Input {
            bytes: source,
            base: 0,
            header_limit: b"To:a@b\n".len() as u64 + 1,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut scratch,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut refused = false;
    let expected = br#"[[{"name":null,"addresses":[{"name":null,"email":"a@b"}]}]"#;
    for _ in 0..1000 {
        let mut output = [0; 1];
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                if progress.written == 1 {
                    assert_eq!(output.first(), expected.get(offset));
                }
                offset += progress.written;
                assert!(!matches!(progress.status, Status::Complete(_)));
            }
            Err(error) => {
                assert!(matches!(error, td_mta::header_value::Error::Selection(_)));
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(offset, expected.len());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(
        before, after,
        "GroupedAddresses property assembly allocated"
    );
}

fn budgeted_address_name_json() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_address_text,
        header_name::{self, Extent, Kind},
        json_string::{Cursor, Error, Status},
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    fn stream(cursor: &mut Cursor<'_, '_, '_>, expected: &[u8]) {
        let mut offset = 0;
        let mut complete = false;
        for _ in 0..1_000_000 {
            let mut output = [0; 1];
            let progress = cursor.poll(Tick(1), &mut output).unwrap();
            if progress.written == 1 {
                assert_eq!(output.first(), expected.get(offset));
            }
            offset += progress.written;
            if progress.status == Status::Complete {
                complete = true;
                break;
            }
        }
        assert!(complete);
        assert_eq!(offset, expected.len());
        cursor.check_deadline(Tick(1)).unwrap();
    }
    let name = "e\u{301}".repeat(4096);
    let address = format!("{name}@EXAMPLE");
    let expected_address = format!("\"{address}\"");
    let expected_name = format!("\"{}\"", "é".repeat(4096));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    let before = COUNTERS.snapshot();
    let mut source = header_address_text::Budgeted::new(
        black_box(address.as_bytes()),
        header_address_text::Mode::Parsed,
        &mut work,
        &mut budget,
    );
    stream(
        &mut Cursor::from_budgeted_address(&mut source),
        expected_address.as_bytes(),
    );
    let mut source = header_name::Cursor::new(
        black_box(name.as_bytes()),
        Extent {
            start: 0,
            end: name.len(),
        },
        Kind::Phrase,
        &mut work,
        &mut budget,
        &mut scratch,
    )
    .unwrap();
    stream(
        &mut Cursor::from_name(&mut source),
        expected_name.as_bytes(),
    );
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1000,
            records: 1000,
            output_bytes: 1,
            ..Charge::default()
        },
    );
    let mut source = header_address_text::Budgeted::new(
        b"a@b",
        header_address_text::Mode::Parsed,
        &mut limited,
        &mut budget,
    );
    let mut cursor = Cursor::from_budgeted_address(&mut source);
    let expected = Error::Address(header_address_text::Error::Work(Stop::OutputBytes));
    let mut refused = false;
    let mut written = 0;
    for _ in 0..1000 {
        match cursor.poll(Tick(1), &mut [0; 1]) {
            Ok(progress) => {
                written += progress.written;
                assert_ne!(progress.status, Status::Complete);
            }
            Err(error) => {
                assert_eq!(error, expected);
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(written, 1);
    assert_eq!(cursor.poll(Tick(1), &mut [0; 1]), Err(expected));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted address/name JSON allocated");
}

fn selected_header_names() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_name::{Cursor, Error, Extent, Kind, Status},
        nfc::{self, HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let phrase = "e\u{301}".repeat(4096);
    let comment = format!("({phrase})");
    let overflow = format!("e{}", "\u{315}\u{301}".repeat(257));
    let expected = format!("é{}{}", "\u{301}".repeat(256), "\u{315}".repeat(257));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    let before = COUNTERS.snapshot();
    for (source, kind, count) in [
        (phrase.as_bytes(), Kind::Phrase, 4096),
        (comment.as_bytes(), Kind::Comment, 4096),
    ] {
        let mut cursor = Cursor::new(
            black_box(source),
            Extent {
                start: 0,
                end: source.len(),
            },
            kind,
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        let mut scalars = 0;
        let mut complete = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Scalar(value) => {
                    assert_eq!(value, 'é');
                    scalars += 1;
                }
                Status::Yield => {}
                Status::Complete => {
                    complete = true;
                    break;
                }
            }
        }
        assert!(complete);
        assert_eq!(scalars, count);
        assert!(!cursor.is_encoding_problem());
        cursor.check_deadline(Tick(1)).unwrap();
    }
    let mut cursor = Cursor::new(
        overflow.as_bytes(),
        Extent {
            start: 0,
            end: overflow.len(),
        },
        Kind::Phrase,
        &mut work,
        &mut budget,
        &mut scratch,
    )
    .unwrap();
    let mut characters = expected.chars();
    let mut complete = false;
    for _ in 0..1_000_000 {
        match cursor.poll(Tick(1)).unwrap() {
            Status::Scalar(value) => assert_eq!(Some(value), characters.next()),
            Status::Yield => {}
            Status::Complete => {
                complete = true;
                break;
            }
        }
    }
    assert!(complete);
    assert_eq!(characters.next(), None);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Normalize(nfc::Error::Work(Stop::Deadline)))
    );
    assert_eq!(
        cursor.poll(Tick(1)),
        Err(Error::Normalize(nfc::Error::Work(Stop::Deadline)))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(
        before, after,
        "selected name validation/normalization allocated"
    );
}

fn budgeted_address_text() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_address_text::{Budgeted, Error, Mode, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("{}@b", "🐈".repeat(4096));
    let fallback = format!(" \t{}\r\n ", "e\u{301}".repeat(4096));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    for (source, mode, count, problem, malformed) in [
        (long.as_bytes(), Mode::Parsed, 4098, false, false),
        (fallback.as_bytes(), Mode::Fallback, 8192, false, false),
        (b"\xffx\xe2\x82".as_slice(), Mode::Fallback, 3, true, false),
        ("\u{fdd0}@b".as_bytes(), Mode::Parsed, 3, true, false),
        (b"a@b bad", Mode::Parsed, 0, false, true),
    ] {
        let mut cursor = Budgeted::new(black_box(source), mode, &mut work, &mut budget);
        let mut scalars = 0;
        let mut finished = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Scalar(_)) => scalars += 1,
                Ok(Status::Complete) => {
                    assert!(!malformed);
                    cursor.check_deadline(Tick(1)).unwrap();
                    finished = true;
                    break;
                }
                Err(Error::Malformed) => {
                    assert!(malformed);
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Malformed));
                    finished = true;
                    break;
                }
                Ok(Status::Yield) => {}
                Err(error) => panic!("unexpected budgeted address text failure: {error}"),
            }
        }
        assert!(finished);
        assert_eq!(scalars, count);
        assert_eq!(cursor.is_encoding_problem(), problem);
    }
    let mut cursor = Budgeted::new(b"a@b", Mode::Parsed, &mut work, &mut budget);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted address text allocated");
}

fn budgeted_header_addresses() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_addresses::{Address, Budgeted, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!(
        "{}: {} <@route:é@例.test>,bad;",
        "🐈".repeat(4096),
        "é".repeat(4096)
    );
    let nested = format!("a@b,{}", "(".repeat(33));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut limited_budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    for (source, parsed, raw, nesting) in [
        (long.as_bytes(), 1, 1, false),
        (b"a@b (Name), \"Jo\" <q@[x y]>".as_slice(), 2, 0, false),
        (b"bad, \"unclosed,tail", 0, 2, false),
        (b"G:; H:a@b;", 1, 0, false),
        (nested.as_bytes(), 1, 0, true),
    ] {
        let mut cursor = Budgeted::new(black_box(source), &mut work, &mut budget);
        let mut parsed_count = 0;
        let mut raw_count = 0;
        let mut finished = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Mailbox(Address::Parsed(_))) => parsed_count += 1,
                Ok(Status::Mailbox(Address::Raw(_))) => raw_count += 1,
                Ok(Status::Complete) => {
                    assert!(!nesting);
                    cursor.check_deadline(Tick(1)).unwrap();
                    finished = true;
                    break;
                }
                Err(Error::NestingLimit) => {
                    assert!(nesting);
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::NestingLimit));
                    finished = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected budgeted address failure: {error}"),
            }
        }
        assert!(finished);
        assert_eq!((parsed_count, raw_count), (parsed, raw));
    }
    let mut exhausted = false;
    for _ in 0..1000 {
        let mut cursor = Budgeted::new(black_box(long.as_bytes()), &mut work, &mut limited_budget);
        let mut complete = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Complete) => {
                    complete = true;
                    break;
                }
                Ok(_) => {}
                Err(Error::InterpretationLimit) => {
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
                    exhausted = true;
                    break;
                }
                Err(error) => panic!("unexpected aggregate address failure: {error}"),
            }
        }
        assert!(complete || exhausted);
        if exhausted {
            break;
        }
    }
    assert!(exhausted);
    let mut cursor = Budgeted::new(b"a@b", &mut work, &mut limited_budget);
    assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
    assert_eq!(cursor.poll(Tick(1)), Err(Error::InterpretationLimit));
    let mut no_io = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            records: 100,
            ..Charge::default()
        },
    );
    let mut cursor = Budgeted::new(b"a@b", &mut no_io, &mut budget);
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::IoBytes)));
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::IoBytes)));
    let mut cursor = Budgeted::new(b"a@b", &mut work, &mut budget);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted address parsing allocated");
}

fn budgeted_header_urls() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_urls::{Budgeted, Error, Mode, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!(
        "({})<https://example.test/{}>",
        "🐈".repeat(4096),
        "path/".repeat(4096)
    );
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    for (source, count, malformed) in [
        (
            long.as_bytes(),
            b"https://example.test/".len() + 4096 * 5,
            false,
        ),
        (b"<x://[::1]>".as_slice(), 9, false),
        (b"<x://[v1.a:b]>", 12, false),
        (b"NO", 0, false),
        (b"<x:a> (bad", 0, true),
    ] {
        let mut cursor = Budgeted::new(black_box(source), Mode::ListPost, &mut work, &mut budget);
        let mut bytes = 0;
        let mut finished = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Byte(_)) => bytes += 1,
                Ok(Status::Complete) => {
                    assert!(!malformed);
                    cursor.check_deadline(Tick(1)).unwrap();
                    finished = true;
                    break;
                }
                Err(Error::Malformed) => {
                    assert!(malformed);
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Malformed));
                    finished = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected budgeted URL failure: {error}"),
            }
        }
        assert!(finished);
        assert_eq!(bytes, count);
    }
    let mut cursor = Budgeted::new(b"NO", Mode::ListPost, &mut work, &mut budget);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted URLs allocated");
}

fn message_id_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property::{self, Context},
        header_select::SourceEnd,
        header_value::{Input, MessageIds, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = "🐈".repeat(4096);
    let source = format!(
        "References:old <{long}@b> tail\nReferences:<a@b> (bad\nReferences:<\u{fdd0}@b>\n\n"
    );
    let expected = format!("[[\"{long}@b\"],null,[\"�@b\"]]");
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut property_cursor =
        header_property::Cursor::new("header:References:asMessageIds:all", Context::Email);
    let property = loop {
        if let header_property::Status::Complete(value) =
            property_cursor.poll(Tick(1), &mut work).unwrap()
        {
            break value.unwrap();
        }
    };
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    let mut cursor = MessageIds::new(
        Input {
            bytes: black_box(source.as_bytes()),
            base: 0,
            header_limit: source.len() as u64,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut complete = false;
    for _ in 0..1_000_000 {
        let mut output = [0; 1];
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        if progress.written == 1 {
            assert_eq!(output.first(), expected.as_bytes().get(offset));
        }
        offset += progress.written;
        if matches!(progress.status, Status::Complete(_)) {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(offset, expected.len());
    assert!(cursor.is_encoding_problem());
    cursor.check_deadline(Tick(1)).unwrap();
    let source = b"References:<a@b>\nReferences:<c@d>\n\n";
    let mut cursor = MessageIds::new(
        Input {
            bytes: source,
            base: 0,
            header_limit: b"References:<a@b>\n".len() as u64 + 1,
            property,
            source_end: SourceEnd::Eof,
        },
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut offset = 0;
    let mut refused = false;
    let expected = b"[[\"a@b\"]";
    for _ in 0..1000 {
        let mut output = [0; 1];
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                if progress.written == 1 {
                    assert_eq!(output.first(), expected.get(offset));
                }
                offset += progress.written;
                assert!(!matches!(progress.status, Status::Complete(_)));
            }
            Err(error) => {
                assert!(matches!(error, td_mta::header_value::Error::Selection(_)));
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(offset, expected.len());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "MessageIds property assembly allocated");
}

fn budgeted_message_id_text() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_message_ids::{
            project::{Budgeted, Status},
            Error, Mode,
        },
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("<{}@b>", "🐈".repeat(4096));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    for (source, scalars, problem, malformed) in [
        (long.as_bytes(), 4098, false, false),
        (b"<\"a\r\n b\"@[c\n\td]>".as_slice(), 11, false, false),
        ("<\u{fdd0}@b>".as_bytes(), 3, true, false),
        (b"<a@b> (bad", 0, false, true),
    ] {
        let mut cursor = Budgeted::new(black_box(source), Mode::Strict, &mut work, &mut budget);
        let mut count = 0;
        let mut finished = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Scalar(_)) => count += 1,
                Ok(Status::Complete) => {
                    assert!(!malformed);
                    assert_eq!(cursor.is_encoding_problem(), problem);
                    cursor.check_deadline(Tick(1)).unwrap();
                    finished = true;
                    break;
                }
                Err(Error::Malformed) => {
                    assert!(malformed);
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Malformed));
                    finished = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected MessageIds conversion failure: {error}"),
            }
        }
        assert!(finished);
        assert_eq!(count, scalars);
    }
    let mut cursor = Budgeted::new(b"", Mode::ObsoletePhrases, &mut work, &mut budget);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted MessageIds conversion allocated");
}

fn budgeted_message_ids() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_message_ids::{Budgeted, Error, Mode, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("({0})<{0}@b><\"{0}\"@[{0}]>", "🐈".repeat(4096));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    for (source, expected_ids, malformed) in [
        (long.as_bytes(), 2, false),
        (b"old words <a@b> tail".as_slice(), 1, false),
        (b"<a@b> (bad", 1, true),
    ] {
        let mut cursor = Budgeted::new(
            black_box(source),
            Mode::ObsoletePhrases,
            &mut work,
            &mut budget,
        );
        let mut ids = 0;
        let mut finished = false;
        for _ in 0..10000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::End) => ids += 1,
                Ok(Status::Complete) => {
                    assert!(!malformed);
                    finished = true;
                    cursor.check_deadline(Tick(1)).unwrap();
                    break;
                }
                Err(Error::Malformed) => {
                    assert!(malformed);
                    assert_eq!(cursor.poll(Tick(1)), Err(Error::Malformed));
                    finished = true;
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("unexpected MessageIds failure: {error}"),
            }
        }
        assert!(finished);
        assert_eq!(ids, expected_ids);
    }
    let mut cursor = Budgeted::new(b"", Mode::ObsoletePhrases, &mut work, &mut budget);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted MessageIds parsing allocated");
}

fn budgeted_header_dates() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_date::{Budgeted, Error, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let long = format!("({})21 Nov 1997 09:55:06 CST", "🐈".repeat(4096));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    for source in [
        long.as_bytes(),
        b"1 Jan 2000 00:00 -0000",
        b"1 Jan 2000 00:00 +0000 (bad",
    ] {
        let mut cursor = Budgeted::new(black_box(source), &mut work, &mut budget);
        let mut complete = false;
        for _ in 0..10000 {
            if let Status::Complete(value) = cursor.poll(Tick(1)).unwrap() {
                assert_eq!(value.is_none(), source.ends_with(b"(bad"));
                complete = true;
                break;
            }
        }
        assert!(complete);
        cursor.check_deadline(Tick(1)).unwrap();
    }
    let mut cursor = Budgeted::new(b"", &mut work, &mut budget);
    assert_eq!(
        cursor.check_deadline(Tick(100)),
        Err(Error::Work(Stop::Deadline))
    );
    assert_eq!(cursor.poll(Tick(1)), Err(Error::Work(Stop::Deadline)));
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted header date parsing allocated");
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

fn header_selection_budget() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property,
        header_select::{Cursor, Error, SourceEnd, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let name = "x".repeat(4096);
    let key = format!("header:{name}:all");
    let source = format!("{name}: one\n{name}: two\n\n");
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 4_000_000,
            ..Charge::default()
        },
    );
    let mut property = header_property::Cursor::new(&key, header_property::Context::Email);
    let mut selected = None;
    for _ in 0..1000 {
        if let header_property::Status::Complete(value) = property.poll(Tick(1), &mut work).unwrap()
        {
            selected = value;
            break;
        }
    }
    let selected = selected.unwrap();
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    let mut refused = false;
    let mut completions = 0;
    for _ in 0..2000 {
        let mut cursor = Cursor::new(
            black_box(source.as_bytes()),
            0,
            source.len() as u64,
            selected,
            SourceEnd::Prefix,
        );
        let mut count = 0;
        let mut complete = false;
        for _ in 0..10_000 {
            match cursor.poll_with_budget(Tick(1), &mut work, &mut budget) {
                Ok(Status::Match(_)) => count += 1,
                Ok(Status::Yield) => {}
                Ok(Status::Complete(_)) => {
                    assert_eq!(count, 2);
                    complete = true;
                    completions += 1;
                    break;
                }
                Err(Error::InterpretationLimit) => {
                    let remaining = work.remaining();
                    assert_eq!(
                        cursor.poll_with_budget(Tick(1), &mut work, &mut budget),
                        Err(Error::InterpretationLimit)
                    );
                    assert_eq!(work.remaining(), remaining);
                    refused = true;
                    break;
                }
                Err(error) => panic!("unexpected selection refusal: {error}"),
            }
        }
        assert!(complete || refused);
        if refused {
            break;
        }
    }
    assert!(refused && completions > 0);
    let mut next = Cursor::new(
        source.as_bytes(),
        0,
        source.len() as u64,
        selected,
        SourceEnd::Prefix,
    );
    assert_eq!(
        next.poll_with_budget(Tick(1), &mut work, &mut budget),
        Err(Error::InterpretationLimit)
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "aggregate header traversal allocated");
}

fn budgeted_raw_output() {
    use td_mta::{
        admission::work::{Charge, Meter, Stop},
        header_raw, json_string,
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let input = "e\u{301}\r\n\t\u{1}\0�".repeat(4096);
    let expected_len = 4096 * 18 + 2;
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut limited = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1000,
            records: 1000,
            output_bytes: 1,
            ..Charge::default()
        },
    );
    let mut limited_budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    let mut source = header_raw::Budgeted::new(black_box(input.as_bytes()), &mut work, &mut budget);
    let mut cursor = json_string::Cursor::from_budgeted_raw(&mut source);
    let mut output = [0; 1];
    let mut written = 0;
    let mut complete = false;
    for _ in 0..1_000_000 {
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        written += progress.written;
        if progress.status == json_string::Status::Complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(written, expected_len);
    cursor.check_deadline(Tick(1)).unwrap();
    let mut source = header_raw::Budgeted::new(b"a", &mut limited, &mut limited_budget);
    let mut cursor = json_string::Cursor::from_budgeted_raw(&mut source);
    let error = json_string::Error::Raw(header_raw::Error::Work(Stop::OutputBytes));
    let mut refused = false;
    for _ in 0..100 {
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => assert_ne!(progress.status, json_string::Status::Complete),
            Err(actual) => {
                assert_eq!(actual, error);
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
    assert_eq!(
        source.poll(Tick(1)),
        Err(header_raw::Error::Work(Stop::OutputBytes))
    );
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "budgeted Raw JSON allocated");
}

fn raw_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property,
        header_select::SourceEnd,
        header_value::{Raw, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let source = format!("X:{}\r\nX:last\r\n\r\nbody", "a".repeat(16384));
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 1_000_000,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut key = header_property::Cursor::new("header:X:all", header_property::Context::Email);
    let mut selected = None;
    for _ in 0..100 {
        if let header_property::Status::Complete(value) = key.poll(Tick(1), &mut work).unwrap() {
            selected = value;
            break;
        }
    }
    let selected = selected.unwrap();
    let before = COUNTERS.snapshot();
    let mut cursor = Raw::new(
        black_box(source.as_bytes()),
        0,
        source.len() as u64,
        selected,
        SourceEnd::Prefix,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut output = [0; 1];
    let mut written = 0;
    let mut complete = false;
    for _ in 0..200_000 {
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        written += progress.written;
        if let Status::Complete(end) = progress.status {
            assert_eq!(end.body_start, source.len() as u64 - 4);
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(written, 16384 + 11);
    cursor.check_deadline(Tick(1)).unwrap();
    let mut cursor = Raw::new(
        b"X:a\nOther: long\n\n",
        0,
        5,
        selected,
        SourceEnd::Eof,
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut refused = false;
    let mut written = 0;
    for _ in 0..1000 {
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                assert!(!matches!(progress.status, Status::Complete(_)));
                written += progress.written;
            }
            Err(error) => {
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(written, 4);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Raw property assembly allocated");
}

fn dispatched_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property,
        header_select::SourceEnd,
        header_value::{Cursor, Input, Status},
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let source = format!(
        "Subject:a{}\nTo:a{} <a@b>\nDate:1 Jan 2000 00:00 +0000\nMessage-ID:<a@b>\nList-Help:<https://x.test/>\n\n",
        "\u{301}".repeat(512), "\u{301}".repeat(512)
    );
    let mut scratch = Scratch::new();
    for key in [
        "header:Subject:asRaw:all",
        "header:Subject:asText:all",
        "header:To:asAddresses:all",
        "header:To:asGroupedAddresses:all",
        "header:Message-ID:asMessageIds:all",
        "header:Date:asDate:all",
        "header:List-Help:asURLs:all",
    ] {
        for output_bytes in [1_000_000, 1] {
            let mut work = Meter::new(
                Deadline::after(Tick(0), 100).unwrap(),
                Charge {
                    io_bytes: 100_000_000,
                    records: 2_000_000,
                    output_bytes,
                    ..Charge::default()
                },
            );
            let mut budget = HeaderBudget::new();
            let mut key = header_property::Cursor::new(key, header_property::Context::Email);
            let mut selected = None;
            for _ in 0..1000 {
                if let header_property::Status::Complete(value) =
                    key.poll(Tick(1), &mut work).unwrap()
                {
                    selected = value;
                    break;
                }
            }
            let property = selected.unwrap();
            let before = COUNTERS.snapshot();
            let mut cursor = Cursor::new(
                Input {
                    bytes: black_box(source.as_bytes()),
                    base: 0,
                    header_limit: source.len() as u64,
                    property,
                    source_end: SourceEnd::Eof,
                },
                &mut scratch,
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert!(std::mem::size_of_val(&cursor) <= 2560);
            let mut written = 0;
            let mut complete = false;
            let mut refused = false;
            for _ in 0..1_000_000 {
                let mut output = [0xa5; 1];
                match cursor.poll(Tick(1), &mut output) {
                    Ok(progress) => {
                        written += progress.written;
                        if let Status::Complete(end) = progress.status {
                            assert_eq!(end.body_start, source.len() as u64);
                            complete = true;
                            break;
                        }
                    }
                    Err(error) => {
                        assert_eq!(output, [0xa5]);
                        assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                        assert_eq!(cursor.check_deadline(Tick(1)), Err(error));
                        refused = true;
                        break;
                    }
                }
            }
            if output_bytes == 1 {
                assert!(refused && !complete);
                assert_eq!(written, 1);
            } else {
                assert!(complete && !refused);
                assert!(!cursor.is_encoding_problem());
                assert!(!cursor.has_unverified_leap());
                cursor.check_deadline(Tick(1)).unwrap();
            }
            let after = COUNTERS.snapshot();
            assert!(!before.invalid && !after.invalid);
            assert_eq!(before, after, "header dispatch allocated");
        }
    }
}

fn date_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property::{self, Context},
        header_select::SourceEnd,
        header_value::{Date, Input, Status},
        nfc::HeaderBudget,
        ports::{Deadline, Tick},
    };
    let source = format!("Date: ({})21 Nov 1997 09:55:06 CST\nDate: 31 Dec 2020 23:59:60 +0000\nDate: 31 Dec 2016 23:59:60 +0000\n\n", "🐈".repeat(4096));
    let expected = b"[\"1997-11-21T15:55:06Z\",null,\"2016-12-31T23:59:60Z\"]";
    let mut work = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 100_000_000,
            records: 2_000_000,
            output_bytes: 10000,
            ..Charge::default()
        },
    );
    let mut selector = header_property::Cursor::new("header:Date:asDate:all", Context::Email);
    let selected = loop {
        if let header_property::Status::Complete(value) = selector.poll(Tick(1), &mut work).unwrap()
        {
            break value.unwrap();
        }
    };
    let mut budget = HeaderBudget::new();
    let before = COUNTERS.snapshot();
    let mut cursor = Date::new(
        Input {
            bytes: source.as_bytes(),
            base: 0,
            header_limit: source.len() as u64,
            property: selected,
            source_end: SourceEnd::Eof,
        },
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut output = [0; 1];
    let mut count = 0;
    let mut complete = false;
    for _ in 0..100_000 {
        let progress = cursor.poll(Tick(1), &mut output).unwrap();
        if progress.written != 0 {
            assert_eq!(expected.get(count), output.first());
            count += 1;
        }
        if matches!(progress.status, Status::Complete(_)) {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert_eq!(count, expected.len());
    assert!(cursor.has_unverified_leap());
    cursor.check_deadline(Tick(1)).unwrap();
    let late = b"Date: 1 Jan 2000 00:00 +0000\nOther: long\n\n";
    let mut cursor = Date::new(
        Input {
            bytes: late,
            base: 0,
            header_limit: b"Date: 1 Jan 2000 00:00 +0000\nO".len() as u64,
            property: selected,
            source_end: SourceEnd::Eof,
        },
        &mut work,
        &mut budget,
    )
    .unwrap();
    let mut refused = false;
    let mut count = 0;
    for _ in 0..1000 {
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                assert!(!matches!(progress.status, Status::Complete(_)));
                count += progress.written;
            }
            Err(error) => {
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                refused = true;
                break;
            }
        }
    }
    assert!(refused);
    assert_eq!(count, 23);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Date property assembly allocated");
}

fn text_header_values() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_property,
        header_select::SourceEnd,
        header_value::{Input, Status, Text},
        nfc::{HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    for field in [
        "Subject",
        "Keywords",
        "List-Id",
        "Content-Description",
        "Content-Type",
        "Content-Disposition",
        "X-Long-Header",
        "X-Custom",
    ] {
        let refused_source = format!("{field}: a\nOther: long\n\n");
        let refused_limit = field.len() as u64 + 5;
        let property_name = format!("header:{field}:asText:all");
        let mime = matches!(field, "Content-Type" | "Content-Disposition");
        let word = if mime {
            "(=?utf-8?Q?cafe=CC=81?=)"
        } else {
            "=?utf-8?Q?cafe=CC=81?="
        };
        let structured = mime || matches!(field, "Keywords" | "List-Id");
        let extra = if structured {
            format!("{field}: \" =?utf-8?Q?literal?= \" (=?utf-8?Q?cafe=CC=81?=) < =?utf-8?Q?literal?= >\r\n")
        } else {
            String::new()
        };
        let extra_size = if structured {
            let expected = if field == "List-Id" || mime {
                "\" =?utf-8?Q?literal?= \" (café) < =?utf-8?Q?literal?= >"
            } else {
                "\" =?utf-8?Q?literal?= \" (café) < literal >"
            };
            td_json::Json::from(expected).to_string().len() + 1
        } else {
            0
        };
        let source = format!(
            "{field}: a{}\u{323}\r\n{field}: {word}\r\n{extra}\r\n",
            "\u{301}".repeat(300)
        );
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 2_000_000,
                output_bytes: 1_000_000,
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut key = header_property::Cursor::new(&property_name, header_property::Context::Email);
        let mut selected = None;
        for _ in 0..100 {
            if let header_property::Status::Complete(value) = key.poll(Tick(1), &mut work).unwrap()
            {
                selected = value;
                break;
            }
        }
        let selected = selected.unwrap();
        let before = COUNTERS.snapshot();
        let mut cursor = Text::new(
            Input {
                bytes: black_box(source.as_bytes()),
                base: 0,
                header_limit: source.len() as u64,
                property: selected,
                source_end: SourceEnd::Eof,
            },
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut output = [0; 1];
        let mut written = 0;
        let mut complete = false;
        for _ in 0..100_000 {
            let progress = cursor.poll(Tick(1), &mut output).unwrap();
            written += progress.written;
            if matches!(progress.status, Status::Complete(_)) {
                complete = true;
                break;
            }
        }
        assert!(complete);
        // U+1EA1, 300 acute marks, café, punctuation and optional MIME parentheses.
        assert_eq!(
            written,
            3 + 600 + 5 + 7 + extra_size + if mime { 2 } else { 0 }
        );
        cursor.check_deadline(Tick(1)).unwrap();
        let mut cursor = Text::new(
            Input {
                bytes: refused_source.as_bytes(),
                base: 0,
                header_limit: refused_limit,
                property: selected,
                source_end: SourceEnd::Eof,
            },
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut refused = false;
        let mut written = 0;
        for _ in 0..1000 {
            match cursor.poll(Tick(1), &mut output) {
                Ok(progress) => {
                    assert!(!matches!(progress.status, Status::Complete(_)));
                    written += progress.written;
                }
                Err(error) => {
                    assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert_eq!(written, 4);
        let after = COUNTERS.snapshot();
        assert!(!before.invalid && !after.invalid);
        assert_eq!(before, after, "Text property assembly allocated");
    }
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

fn json_identity_output() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_address_text::{self, Mode},
        header_raw,
        json_string::{Cursor, Error, Status},
        ports::{Deadline, Tick},
    };
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 1024 * 1024,
                records: 100_000,
                output_bytes: 1024 * 1024,
                ..Charge::default()
            },
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_, '_>) -> usize {
        let mut output = [0; 1];
        let mut written = 0;
        for _ in 0..10_000 {
            let progress = cursor.poll(Tick(1), &mut output).unwrap();
            written += progress.written;
            black_box(output);
            if progress.status == Status::Complete {
                return written;
            }
        }
        panic!("identity JSON allocation fixture did not finish");
    }
    let before = COUNTERS.snapshot();
    let mut work = meter();
    let mut raw = header_raw::Cursor::new(black_box(b"e\xcc\x81\r\n"));
    let mut cursor = Cursor::from_raw(&mut raw, &mut work);
    assert_eq!(drain(&mut cursor), 9);
    let mut work = meter();
    let mut address = header_address_text::Cursor::new(black_box(b"e\xcc\x81@b"), Mode::Parsed);
    let mut cursor = Cursor::from_address(&mut address, &mut work);
    assert_eq!(drain(&mut cursor), 7);
    let mut work = meter();
    let mut address = header_address_text::Cursor::new(black_box(b" \xff\0 "), Mode::Fallback);
    let mut cursor = Cursor::from_address(&mut address, &mut work);
    assert_eq!(drain(&mut cursor), 11);
    assert!(cursor.is_encoding_problem());
    let mut work = meter();
    let mut invalid = header_address_text::Cursor::new(b"a@", Mode::Parsed);
    let mut cursor = Cursor::from_address(&mut invalid, &mut work);
    let mut output = [0; 1];
    let mut failed = false;
    let mut written = 0;
    for _ in 0..10_000 {
        match cursor.poll(Tick(1), &mut output) {
            Ok(progress) => {
                assert_ne!(progress.status, Status::Complete);
                written += progress.written;
            }
            Err(error) => {
                assert_eq!(error, Error::Address(header_address_text::Error::Malformed));
                assert_eq!(cursor.poll(Tick(1), &mut output), Err(error));
                failed = true;
                break;
            }
        }
    }
    assert!(failed);
    assert_eq!(written, 1);
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "Raw/address JSON serialization allocated");
}

fn json_string_output() {
    use td_mta::{
        admission::work::{Charge, Meter},
        json_string::{Cursor, Status},
        nfc::{self, HeaderBudget, Scratch},
        ports::{Deadline, Tick},
    };
    let long = format!("a{}", "\u{315}\u{300}".repeat(257));
    let mut scratch = Scratch::new();
    let mut meter = Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 16 * 1024 * 1024,
            records: 2_000_000,
            output_bytes: 1024 * 1024,
            ..Charge::default()
        },
    );
    let mut budget = HeaderBudget::new();
    let mut output = [0; 1];
    let before = COUNTERS.snapshot();
    for input in ["", "\"\\\n\0é🐈", long.as_str()] {
        let mut source = nfc::Cursor::new(black_box(input), &mut scratch, &mut meter, &mut budget);
        let mut cursor = Cursor::new(&mut source);
        loop {
            let progress = cursor.poll(Tick(1), &mut output).unwrap();
            black_box(output);
            if progress.status == Status::Complete {
                break;
            }
        }
        cursor.check_deadline(Tick(1)).unwrap();
    }
    let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    let mut source = nfc::Cursor::new("", &mut scratch, &mut limited, &mut budget);
    let mut refused = Cursor::new(&mut source);
    assert!(refused.poll(Tick(100), &mut output).is_err());
    assert!(refused.poll(Tick(1), &mut output).is_err());
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "JSON string serialization allocated");
}

fn phrase_nfc() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_phrase,
        nfc::{Cursor, HeaderBudget, Scratch, Status},
        ports::{Deadline, Tick},
    };
    let long = format!("=?utf-8?q?a?={}", " =?utf-8?q?=CC=95=CC=80?=".repeat(150));
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
    for (input, count) in [
        (b"\" e\xcc\x81 \"".as_slice(), 1),
        (b"=?utf-8?q?e?= =?utf-8?q?=CC=81?=", 1),
        (b"=?utf-8?q?e=00=CC=81?=", 1),
        (long.as_bytes(), 300),
    ] {
        let mut parser = header_phrase::Cursor::new(black_box(input));
        while !matches!(
            parser.poll(Tick(1), &mut work).unwrap(),
            header_phrase::Status::Complete(_)
        ) {}
        let proof = parser.into_validated().unwrap();
        let mut cursor = Cursor::from_phrase(
            proof,
            input,
            header_phrase::Extent {
                start: 0,
                end: input.len(),
            },
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .unwrap();
        let mut scalars = 0;
        loop {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Scalar(value) => {
                    cursor
                        .charge_output(Tick(1), value.len_utf8() as u64)
                        .unwrap();
                    black_box(value);
                    scalars += 1;
                }
                Status::Complete => break,
            }
        }
        assert_eq!(scalars, count);
        let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        let mut refused = Cursor::from_phrase(
            proof,
            input,
            header_phrase::Extent {
                start: 0,
                end: input.len(),
            },
            &mut scratch,
            &mut limited,
            &mut budget,
        )
        .unwrap();
        assert!(refused.poll(Tick(100)).is_err());
        assert!(refused.poll(Tick(1)).is_err());
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "phrase NFC allocated");
}

fn comment_nfc() {
    use td_mta::{
        admission::work::{Charge, Meter},
        header_comment,
        nfc::{Cursor, HeaderBudget, Scratch, Status},
        ports::{Deadline, Tick},
    };
    let long = format!("(=?utf-8?q?a?={})", " =?utf-8?q?=CC=95=CC=80?=".repeat(150));
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
    for (input, count) in [
        (b"( e\xcc\x81 )".as_slice(), 1),
        (b"(=?utf-8?q?e?= =?utf-8?q?=CC=81?=)", 1),
        (b"(=?utf-8?q?e=00=CC=81?=)", 1),
        (b"(=?utf-8?q?x?=(y))", 4),
        (b"(\\ =?utf-8?q?x?=)", 2),
        (long.as_bytes(), 300),
    ] {
        let mut parser = header_comment::Cursor::new(black_box(input));
        while !matches!(
            parser.poll(Tick(1), &mut work).unwrap(),
            header_comment::Status::Complete
        ) {}
        let proof = parser.into_validated().unwrap();
        let mut cursor = Cursor::from_comment(proof, &mut scratch, &mut work, &mut budget);
        let mut scalars = 0;
        loop {
            match cursor.poll(Tick(1)).unwrap() {
                Status::Yield => {}
                Status::Scalar(value) => {
                    cursor
                        .charge_output(Tick(1), value.len_utf8() as u64)
                        .unwrap();
                    black_box(value);
                    scalars += 1;
                }
                Status::Complete => break,
            }
        }
        assert_eq!(scalars, count);
        let mut limited = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
        let mut refused = Cursor::from_comment(proof, &mut scratch, &mut limited, &mut budget);
        assert!(refused.poll(Tick(100)).is_err());
        assert!(refused.poll(Tick(1)).is_err());
    }
    let after = COUNTERS.snapshot();
    assert!(!before.invalid && !after.invalid);
    assert_eq!(before, after, "comment NFC allocated");
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

fn pinned_source_binding() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    pinned::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "pinned source binding allocated");
    }
}

fn source_bound_members() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    members::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "source-bound part members allocated");
    }
}

fn selected_source_bound_part_fields() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    selected::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "selected source-bound part fields allocated");
    }
}

fn whole_selected_source_bound_part_fields() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    retained::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "whole selected source-bound part fields allocated"
        );
    }
}

fn collected_selected_source_bound_part_fields() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    collected::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "collected selected source-bound part fields allocated"
        );
    }
}

fn composed_selected_source_bound_part_fields() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    composed::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "composed selected source-bound part fields allocated"
        );
    }
}

fn body_property_names() {
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    body_property::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "body property names allocated");
    }
}

fn requested_composed_selected_source_bound_part_fields() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    requested::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "requested composed selected source-bound part fields allocated"
        );
    }
}

fn retained_requested_composed_selected_source_bound_part_fields() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::retained;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    retained::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "retained requested composed selected source-bound part fields allocated"
        );
    }
}

fn body_property_selections() {
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    body_properties::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "body property selections allocated");
    }
}

fn body_property_json_arguments() {
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    body_properties::json::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "body property JSON arguments allocated");
    }
}

fn body_property_requests() {
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    body_properties::request::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "body property requests allocated");
    }
}

fn selected_subparts_composition() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    subparts::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "selected subParts composition allocated");
    }
}

fn selected_subparts_retention() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::selected::retained::collected::composed::requested::subparts::retained;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    retained::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "selected subParts retention allocated");
    }
}

fn resident_header_arrays() {
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    mime_headers::json::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "resident header array allocated");
    }
}

fn whole_source_bound_members() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    retained::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "whole source-bound part members allocated");
    }
}
fn collected_source_bound_members() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    collected::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "collected source-bound part members allocated"
        );
    }
}
fn composed_source_bound_members() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    composed::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "composed source-bound part members allocated"
        );
    }
}
fn retained_source_bound_composition() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::retained;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    retained::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "retained source-bound composition allocated");
    }
}
fn selected_source_bound_lists() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::selected;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    selected::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "selected source-bound lists allocated");
    }
}

fn requested_source_bound_properties() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    requested::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(before, after, "requested source-bound properties allocated");
    }
}

fn retained_selected_source_bound_lists() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::selected::retained;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    retained::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "retained selected source-bound lists allocated"
        );
    }
}

fn retained_requested_source_bound_properties() {
    use mime_traversal::bound::ordered::body_lists::response::collected::composed::retained::locators::pinned::members::retained::collected::composed::requested::retained;
    let mut samples = [COUNTERS.snapshot(); 16];
    let mut slots = samples.iter_mut();
    retained::probe_allocations(|| *slots.next().unwrap() = COUNTERS.snapshot());
    assert!(slots.next().is_none());
    assert!(samples.iter().all(|sample| !sample.invalid));
    for [before, after] in samples.as_chunks::<2>().0 {
        assert_eq!(
            before, after,
            "retained requested source-bound properties allocated"
        );
    }
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
        header_delimited_tokens();
        header_message_id_lists();
        header_message_id_text();
        header_url_text();
        header_address_boundaries();
        header_address_text();
        header_address_groups();
        header_single_mailbox();
        header_phrase_tokens();
        header_phrase_replay();
        shared_character_projection();
        header_phrase_display();
        header_single_addr_spec();
        header_date_projection();
        budgeted_date_projection();
        header_dates();
        url_header_values();
        address_header_values();
        grouped_header_values();
        budgeted_address_name_json();
        selected_header_names();
        budgeted_address_text();
        budgeted_header_addresses();
        budgeted_header_urls();
        message_id_header_values();
        budgeted_message_id_text();
        budgeted_message_ids();
        budgeted_header_dates();
        header_comments();
        header_selection();
        header_selection_budget();
        budgeted_raw_output();
        raw_header_values();
        text_header_values();
        date_header_values();
        dispatched_header_values();
        header_properties();
        header_nfc();
        phrase_nfc();
        comment_nfc();
        json_string_output();
        json_identity_output();
        encoded_word_candidates();
        encoded_word_decoding();
        header_text();
        mime_input();
        mime_headers();
        mime_fields();
        mime_metadata();
        mime_attribute();
        mime_value();
        mime_parameter();
        mime_parameter_octets();
        mime_parameter_scalars();
        mime_parameter_display();
        mime_parameter_nfc();
        mime_filename_retention();
        mime_protocol_parameters();
        resident_mime_delimiters();
        resident_mime_traversal();
        bound_mime_part_metadata();
        bound_mime_classification();
        ordered_mime_classification();
        resident_part_headers();
        mime_body_list_selection();
        mime_label_fields();
        uri_literal_field_values();
        uri_selection_values();
        uri_spelling_values();
        uri_literal_values();
        uri_word_values();
        uri_word_runs();
        uri_location_fields();
        uri_location_json();
        uri_location_discovery();
        uri_location_source_bound();
        part_header_location_json();
        uri_location_retention();
        uri_unfold_values();
        uri_reference_values();
        content_id_values();
        content_id_json();
        content_language_json();
        retained_label_json();
        composed_part_label_json();
        content_language_values();
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
    pinned_source_binding();
    source_bound_members();
    selected_source_bound_part_fields();
    whole_selected_source_bound_part_fields();
    collected_selected_source_bound_part_fields();
    composed_selected_source_bound_part_fields();
    requested_composed_selected_source_bound_part_fields();
    retained_requested_composed_selected_source_bound_part_fields();
    body_property_names();
    body_property_selections();
    body_property_json_arguments();
    body_property_requests();
    selected_subparts_composition();
    selected_subparts_retention();
    resident_header_arrays();
    whole_source_bound_members();
    collected_source_bound_members();
    composed_source_bound_members();
    retained_source_bound_composition();
    selected_source_bound_lists();
    retained_selected_source_bound_lists();
    requested_source_bound_properties();
    retained_requested_source_bound_properties();
    mime_base64();
    mime_qp();
    mime_qp_input();
    unicode_lookups();
    unicode_nfc();
    header_delimited_tokens();
    header_message_id_lists();
    header_message_id_text();
    header_url_text();
    header_address_boundaries();
    header_address_text();
    header_address_groups();
    header_single_mailbox();
    header_phrase_tokens();
    header_phrase_replay();
    shared_character_projection();
    header_phrase_display();
    header_single_addr_spec();
    header_date_projection();
    budgeted_date_projection();
    header_dates();
    url_header_values();
    address_header_values();
    grouped_header_values();
    budgeted_address_name_json();
    selected_header_names();
    budgeted_address_text();
    budgeted_header_addresses();
    budgeted_header_urls();
    message_id_header_values();
    budgeted_message_id_text();
    budgeted_message_ids();
    budgeted_header_dates();
    header_comments();
    header_selection();
    header_selection_budget();
    budgeted_raw_output();
    raw_header_values();
    text_header_values();
    date_header_values();
    dispatched_header_values();
    header_properties();
    header_nfc();
    phrase_nfc();
    comment_nfc();
    json_string_output();
    json_identity_output();
    encoded_word_candidates();
    encoded_word_decoding();
    header_text();
    mime_input();
    mime_headers();
    mime_fields();
    mime_metadata();
    mime_attribute();
    mime_value();
    mime_parameter();
    mime_parameter_octets();
    mime_parameter_scalars();
    mime_parameter_display();
    mime_parameter_nfc();
    mime_filename_retention();
    mime_protocol_parameters();
    resident_mime_delimiters();
    resident_mime_traversal();
    bound_mime_part_metadata();
    bound_mime_classification();
    ordered_mime_classification();
    resident_part_headers();
    mime_body_list_selection();
    mime_label_fields();
    uri_literal_field_values();
    uri_selection_values();
    uri_spelling_values();
    uri_literal_values();
    uri_word_values();
    uri_word_runs();
    uri_location_fields();
    uri_location_json();
    uri_location_discovery();
    uri_location_source_bound();
    part_header_location_json();
    uri_location_retention();
    uri_unfold_values();
    uri_reference_values();
    content_id_values();
    content_id_json();
    content_language_json();
    retained_label_json();
    composed_part_label_json();
    content_language_values();
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
