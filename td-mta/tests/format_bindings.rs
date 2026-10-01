#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use td_crypto::{Crypto, Digest, Provider};
use td_mta::{
    format::{
        bindings::Selection,
        container::{Current, Error, JournalHeader, StoreIdentity},
        manifest::{self, Manifest, TableDescriptor},
        table::{record_extent, TableHeader},
        table_stream::{Summary, Verifier},
        Error as FormatError, Sequence, Table,
    },
    ids::{AccountId, StoreEpoch},
};
fn hex(input: &str) -> Vec<u8> {
    let digits: String = input.split_whitespace().collect();
    let (pairs, rest) = digits.as_bytes().as_chunks::<2>();
    assert!(rest.is_empty());
    pairs
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}
fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut d = Provider.sha256().unwrap();
    d.update(bytes).unwrap();
    d.finish().unwrap()
}
fn summary(bytes: &[u8]) -> Summary {
    let mut verifier = Verifier::new(&Provider, &bytes[..112]).unwrap();
    let mut rest = &bytes[112..];
    while !rest.is_empty() {
        let n = record_extent(&rest[..16]).unwrap();
        verifier.push(&rest[..n]).unwrap();
        rest = &rest[n..];
    }
    verifier.finish().unwrap()
}
fn format() -> Vec<u8> {
    hex(include_str!("fixtures/format-v1/format.hex"))
}
fn account() -> AccountId {
    AccountId::from_bytes([0x33; 16])
}
fn selected_current(manifest_bytes: &[u8]) -> [u8; 120] {
    let header = Manifest::decode(&Provider, manifest_bytes)
        .unwrap()
        .header();
    let mut bytes = [0; 120];
    Current {
        account: header.account,
        epoch: header.epoch,
        generation: header.generation,
        manifest_digest: hash(manifest_bytes),
    }
    .encode(&Provider, &mut bytes)
    .unwrap();
    bytes
}

#[test]
fn literal_selections_bind_all_table_summaries_and_journal_headers() {
    let empty = [
        include_str!("fixtures/format-v1/empty-table-1.hex"),
        include_str!("fixtures/format-v1/empty-table-2.hex"),
        include_str!("fixtures/format-v1/empty-table-3.hex"),
        include_str!("fixtures/format-v1/empty-table-4.hex"),
        include_str!("fixtures/format-v1/empty-table-5.hex"),
        include_str!("fixtures/format-v1/empty-table-6.hex"),
        include_str!("fixtures/format-v1/empty-table-7.hex"),
        include_str!("fixtures/format-v1/empty-table-8.hex"),
        include_str!("fixtures/format-v1/empty-table-9.hex"),
        include_str!("fixtures/format-v1/empty-table-10.hex"),
        include_str!("fixtures/format-v1/empty-table-11.hex"),
    ];
    let checkpoint = [
        include_str!("fixtures/format-v1/populated-blob-table.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-2.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-3.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-4.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-5.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-6.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-7.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-8.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-9.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-10.hex"),
        include_str!("fixtures/format-v1/checkpoint-table-11.hex"),
    ];
    let format = format();
    for (manifest, current, journal, tables) in [
        (
            include_str!("fixtures/format-v1/manifest.hex"),
            include_str!("fixtures/format-v1/current.hex"),
            include_str!("fixtures/format-v1/empty-journal.hex"),
            empty,
        ),
        (
            include_str!("fixtures/format-v1/manifest-history.hex"),
            include_str!("fixtures/format-v1/current-history.hex"),
            include_str!("fixtures/format-v1/active-journal-two.hex"),
            checkpoint,
        ),
    ] {
        let manifest = hex(manifest);
        let current = hex(current);
        let journal = hex(journal);
        let binding =
            Selection::decode(&Provider, account(), &format, &current, &manifest).unwrap();
        assert_eq!(
            binding.current(),
            Current::decode(&Provider, &current).unwrap()
        );
        assert_eq!(
            binding.store(),
            StoreIdentity::decode(&Provider, &format).unwrap()
        );
        for (i, table) in tables.into_iter().enumerate() {
            let bytes = hex(table);
            assert_eq!(
                binding.check_table(
                    &Provider,
                    Table::from_tag(i as u16 + 1).unwrap(),
                    summary(&bytes)
                ),
                Ok(())
            );
        }
        assert_eq!(
            binding.check_active_header(&Provider, &journal),
            JournalHeader::decode(&Provider, &journal)
        );
        if binding.manifest().history_count() != 0 {
            let history = hex(include_str!("fixtures/format-v1/journal-with-frame.hex"));
            assert_eq!(
                binding.check_history_header(&Provider, 0, &history[..96]),
                JournalHeader::decode(&Provider, &history[..96])
            );
            assert_eq!(
                binding.check_history_header(&Provider, 0, &history),
                Err(Error::Format(FormatError::TrailingBytes))
            );
        }
        assert_eq!(
            binding.check_history_header(&Provider, usize::MAX, &journal),
            Err(Error::Format(FormatError::InvalidValue))
        );
    }
}

#[test]
fn selection_checks_expected_account_epoch_generation_and_entire_manifest_hash() {
    let format = format();
    let manifest = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let current = selected_current(&manifest);
    let invalid = Err(Error::Format(FormatError::InvalidValue));
    assert_eq!(
        Selection::decode(
            &Provider,
            AccountId::from_bytes([0; 16]),
            &format,
            &current,
            &manifest
        ),
        invalid
    );
    let mut store = StoreIdentity::decode(&Provider, &format).unwrap();
    store.epoch = StoreEpoch::from_bytes([0; 16]);
    let mut wrong_format = [0; 80];
    store.encode(&Provider, &mut wrong_format).unwrap();
    assert_eq!(
        Selection::decode(&Provider, account(), &wrong_format, &current, &manifest),
        invalid
    );
    let selected = Current::decode(&Provider, &current).unwrap();
    for offset in [16, 32, 48] {
        let mut changed = manifest.clone();
        changed[offset] ^= 1;
        let end = changed.len() - 32;
        let digest = hash(&changed[..end]);
        changed[end..].copy_from_slice(&digest);
        let mut matched = selected;
        matched.manifest_digest = hash(&changed);
        let mut bytes = [0; 120];
        matched.encode(&Provider, &mut bytes).unwrap();
        assert_eq!(
            Selection::decode(&Provider, account(), &format, &bytes, &changed),
            invalid
        );
    }
    let mut wrong_hash = selected;
    wrong_hash.manifest_digest[0] ^= 1;
    let mut bytes = [0; 120];
    wrong_hash.encode(&Provider, &mut bytes).unwrap();
    assert_eq!(
        Selection::decode(&Provider, account(), &format, &bytes, &manifest),
        Err(Error::Checksum)
    );
    let mut excludes_footer = selected;
    excludes_footer.manifest_digest = hash(&manifest[..manifest.len() - 32]);
    excludes_footer.encode(&Provider, &mut bytes).unwrap();
    assert_eq!(
        Selection::decode(&Provider, account(), &format, &bytes, &manifest),
        Err(Error::Checksum)
    );
    for kind in 0..3 {
        let mut current = selected;
        match kind {
            0 => current.account = AccountId::from_bytes([0; 16]),
            1 => current.epoch = StoreEpoch::from_bytes([0; 16]),
            _ => current.generation = 3,
        }
        current.encode(&Provider, &mut bytes).unwrap();
        assert_eq!(
            Selection::decode(&Provider, account(), &format, &bytes, &manifest),
            invalid
        );
    }
}

#[test]
fn matching_hashes_cannot_hide_wrong_table_identity_count_or_extent() {
    let format = format();
    let literal = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let manifest = Manifest::decode(&Provider, &literal).unwrap();
    let tables: [TableDescriptor; 11] = std::array::from_fn(|i| {
        manifest
            .table(Table::from_tag(i as u16 + 1).unwrap())
            .unwrap()
    });
    let history = [manifest.history(0).unwrap()];
    let original = hex(include_str!("fixtures/format-v1/populated-blob-table.hex"));
    for change in 0..7 {
        let mut file = original.clone();
        let mut header = TableHeader::decode(&Provider, &file[..112]).unwrap();
        match change {
            0 => header.account = AccountId::from_bytes([0; 16]),
            1 => header.epoch = StoreEpoch::from_bytes([0; 16]),
            2 => header.generation = 3,
            3 => header.through = Sequence::from_u64(2),
            4 => {
                header.table = Table::Mailboxes;
                header.record_count = 0;
                header.payload_bytes = 0;
                file.truncate(112);
            }
            _ => {}
        }
        header.encode(&Provider, &mut file[..112]).unwrap();
        let actual = summary(&file);
        let mut descriptors = tables;
        descriptors[0].record_count = header.record_count;
        descriptors[0].file_bytes = header.file_bytes().unwrap();
        descriptors[0].digest = actual.digest();
        if change == 5 {
            descriptors[0].record_count = 0;
            descriptors[0].file_bytes = 112;
        }
        if change == 6 {
            descriptors[0].file_bytes += 1;
        }
        let mut bytes = [0; 800];
        manifest::encode(
            &Provider,
            manifest.header(),
            &descriptors,
            &history,
            &mut bytes,
        )
        .unwrap();
        let current = selected_current(&bytes);
        let selection = Selection::decode(&Provider, account(), &format, &current, &bytes).unwrap();
        assert_eq!(
            selection.check_table(&Provider, Table::Blobs, actual),
            Err(Error::Format(FormatError::InvalidValue)),
            "case {change}"
        );
    }
    // Two valid records fit a descriptor declaring one at the same full size.
    let mut file = original.clone();
    let mut second = original[112..].to_vec();
    second[31] = second[31].checked_add(1).unwrap();
    let end = second.len() - 32;
    let checksum = hash(&second[..end]);
    second[end..].copy_from_slice(&checksum);
    file.extend_from_slice(&second);
    let mut header = TableHeader::decode(&Provider, &file[..112]).unwrap();
    header.record_count = 2;
    header.payload_bytes *= 2;
    header.encode(&Provider, &mut file[..112]).unwrap();
    let actual = summary(&file);
    let mut descriptors = tables;
    descriptors[0].record_count = 1;
    descriptors[0].file_bytes = header.file_bytes().unwrap();
    descriptors[0].digest = actual.digest();
    let mut bytes = [0; 800];
    manifest::encode(
        &Provider,
        manifest.header(),
        &descriptors,
        &history,
        &mut bytes,
    )
    .unwrap();
    let current = selected_current(&bytes);
    let selection = Selection::decode(&Provider, account(), &format, &current, &bytes).unwrap();
    assert_eq!(
        selection.check_table(&Provider, Table::Blobs, actual),
        Err(Error::Format(FormatError::InvalidValue))
    );

    let current = selected_current(&literal);
    let selection = Selection::decode(&Provider, account(), &format, &current, &literal).unwrap();
    let mut different = original.clone();
    // Change a valid raw blob digest, then repair the record checksum and compute the file summary.
    different[112 + 16 + 16 + 9] ^= 1;
    let end = different.len() - 32;
    let digest = hash(&different[112..end]);
    different[end..].copy_from_slice(&digest);
    assert_eq!(
        selection.check_table(&Provider, Table::Blobs, summary(&different)),
        Err(Error::Checksum)
    );
}

#[test]
fn journal_headers_bind_account_epoch_segment_and_base_but_not_frames() {
    let format = format();
    let manifest = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let current = selected_current(&manifest);
    let selection = Selection::decode(&Provider, account(), &format, &current, &manifest).unwrap();
    let active = hex(include_str!("fixtures/format-v1/active-journal-two.hex"));
    let history = hex(include_str!("fixtures/format-v1/empty-journal.hex"));
    for (bytes, is_history) in [(&active, true), (&history, false)] {
        let result = if is_history {
            selection.check_history_header(&Provider, 0, bytes)
        } else {
            selection.check_active_header(&Provider, bytes)
        };
        assert_eq!(result, Err(Error::Format(FormatError::InvalidValue)));
    }
    for (bytes, is_history) in [(&active, false), (&history, true)] {
        let original = JournalHeader::decode(&Provider, bytes).unwrap();
        for field in 0..4 {
            let mut header = original;
            match field {
                0 => header.account = AccountId::from_bytes([0; 16]),
                1 => header.epoch = StoreEpoch::from_bytes([0; 16]),
                2 => header.segment = 3,
                _ => header.base = Sequence::from_u64(4),
            }
            let mut changed = [0; 96];
            header.encode(&Provider, &mut changed).unwrap();
            let result = if is_history {
                selection.check_history_header(&Provider, 0, &changed)
            } else {
                selection.check_active_header(&Provider, &changed)
            };
            assert_eq!(result, Err(Error::Format(FormatError::InvalidValue)));
        }
    }
}

struct FailFactory {
    comparisons: std::sync::atomic::AtomicUsize,
    reject_comparison: bool,
    count: std::sync::atomic::AtomicUsize,
    at: usize,
}
impl Crypto for FailFactory {
    type Sha256 = td_crypto::Sha256;
    type SigningKey = ();
    fn sha256(&self) -> Result<Self::Sha256, td_crypto::Error> {
        let call = self
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if call == self.at {
            return Err(td_crypto::Error::Crypto);
        }
        Provider.sha256()
    }
    fn equal_digest(&self, a: &[u8; 32], b: &[u8; 32]) -> bool {
        self.comparisons
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        !self.reject_comparison && Provider.equal_digest(a, b)
    }
    fn generate_p256(&self, _: &mut [u8]) -> Result<usize, td_crypto::Error> {
        Err(td_crypto::Error::Invalid)
    }
    fn load_p256(&self, _: &[u8]) -> Result<(), td_crypto::Error> {
        Err(td_crypto::Error::Invalid)
    }
    fn p256_public(&self, _: &(), _: &mut [u8; 65]) -> Result<(), td_crypto::Error> {
        Err(td_crypto::Error::Invalid)
    }
    fn sign_es256(&self, _: &(), _: &[u8], _: &mut [u8; 64]) -> Result<(), td_crypto::Error> {
        Err(td_crypto::Error::Invalid)
    }
}
#[test]
fn selection_propagates_failure_at_each_digest_including_whole_manifest() {
    let format = format();
    let manifest = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let current = selected_current(&manifest);
    for at in 1..=4 {
        let crypto = FailFactory {
            count: std::sync::atomic::AtomicUsize::new(0),
            comparisons: std::sync::atomic::AtomicUsize::new(0),
            reject_comparison: false,
            at,
        };
        assert_eq!(
            Selection::decode(&crypto, account(), &format, &current, &manifest),
            Err(Error::Crypto(td_crypto::Error::Crypto))
        );
    }
    let selection = Selection::decode(&Provider, account(), &format, &current, &manifest).unwrap();
    let crypto = FailFactory {
        count: std::sync::atomic::AtomicUsize::new(0),
        comparisons: std::sync::atomic::AtomicUsize::new(0),
        reject_comparison: false,
        at: 1,
    };
    assert_eq!(
        selection.check_history_header(&crypto, selection.manifest().history_count(), &[0; 96]),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(crypto.count.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(
        selection.check_active_header(&crypto, &[0; 96]),
        Err(Error::Crypto(td_crypto::Error::Crypto))
    );
}

fn journal_summary(bytes: &[u8]) -> td_mta::format::journal_stream::Summary {
    let mut verifier =
        td_mta::format::journal_stream::Verifier::new(&Provider, &bytes[..96]).unwrap();
    let mut rest = &bytes[96..];
    while !rest.is_empty() {
        let header = td_mta::format::frame_header::Header::decode(&Provider, &rest[..64]).unwrap();
        verifier.push(&rest[..header.frame_bytes]).unwrap();
        rest = &rest[header.frame_bytes..];
    }
    verifier.finish().unwrap()
}
fn selected_history_manifest(descriptor: manifest::HistoryDescriptor) -> Vec<u8> {
    let literal = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let view = Manifest::decode(&Provider, &literal).unwrap();
    let tables =
        std::array::from_fn(|i| view.table(Table::from_tag(i as u16 + 1).unwrap()).unwrap());
    let mut header = view.header();
    header.through = descriptor.through;
    let mut bytes = vec![0; 800];
    manifest::encode(&Provider, header, &tables, &[descriptor], &mut bytes).unwrap();
    bytes
}
fn journal_deletes(count: usize) -> Vec<u8> {
    let mut bytes = vec![0; 96 + 104 + 28 * count];
    bytes[..96].copy_from_slice(&hex(include_str!("fixtures/format-v1/empty-journal.hex")));
    let key = [0x44; 16];
    let deletion = td_mta::format::operation::Operation::delete(Table::Blobs, &key).unwrap();
    for slot in bytes[160..160 + 28 * count].as_chunks_mut::<28>().0 {
        deletion.encode(slot).unwrap();
    }
    td_mta::format::frame::seal(&Provider, Sequence::from_u64(1), count, &mut bytes[96..]).unwrap();
    bytes
}
#[test]
fn completed_history_and_active_prefixes_bind_to_selected_ranges() {
    let format = format();
    let manifest = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let current = selected_current(&manifest);
    let selection = Selection::decode(&Provider, account(), &format, &current, &manifest).unwrap();
    let history = hex(include_str!("fixtures/format-v1/journal-with-frame.hex"));
    selection
        .check_history_journal(&Provider, 0, journal_summary(&history))
        .unwrap();
    let active = hex(include_str!("fixtures/format-v1/active-journal-two.hex"));
    selection
        .check_active_prefix(Sequence::from_u64(1), 96, journal_summary(&active))
        .unwrap();
    let frame = hex(include_str!("fixtures/format-v1/frame-delete-change.hex"));
    let active = [active.as_slice(), frame.as_slice()].concat();
    let summary = journal_summary(&active);
    selection
        .check_active_prefix(Sequence::from_u64(2), active.len() as u64, summary)
        .unwrap();
    let invalid = Err(Error::Format(FormatError::InvalidValue));
    assert_eq!(
        selection.check_active_prefix(Sequence::from_u64(1), active.len() as u64, summary),
        invalid
    );
    assert_eq!(
        selection.check_active_prefix(Sequence::from_u64(2), active.len() as u64 + 1, summary),
        invalid
    );
    assert_eq!(
        selection.check_active_prefix(
            Sequence::from_u64(1),
            history.len() as u64,
            journal_summary(&history)
        ),
        invalid
    );
    assert_eq!(
        selection.check_history_journal(&Provider, 0, summary),
        invalid
    );
    for field in 0..4 {
        let mut changed = active.clone();
        let mut header = JournalHeader::decode(&Provider, &changed[..96]).unwrap();
        match field {
            0 => header.account = AccountId::from_bytes([0; 16]),
            1 => header.epoch = StoreEpoch::from_bytes([0; 16]),
            2 => header.segment = 3,
            _ => header.base = Sequence::from_u64(2),
        }
        header.encode(&Provider, &mut changed[..96]).unwrap();
        td_mta::format::frame::seal(
            &Provider,
            header.base.successor().unwrap(),
            2,
            &mut changed[96..],
        )
        .unwrap();
        let summary = journal_summary(&changed);
        assert_eq!(
            selection.check_active_prefix(summary.through(), changed.len() as u64, summary),
            invalid
        );
    }
}
#[test]
fn history_summary_identity_range_and_size_are_checked_even_with_matching_hashes() {
    let format = format();
    let original = hex(include_str!("fixtures/format-v1/journal-with-frame.hex"));
    let manifest = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let descriptor = Manifest::decode(&Provider, &manifest)
        .unwrap()
        .history(0)
        .unwrap();
    for field in 0..6 {
        let mut bytes = if field == 4 {
            journal_deletes(6)
        } else {
            original.clone()
        };
        let mut header = JournalHeader::decode(&Provider, &bytes[..96]).unwrap();
        match field {
            0 => header.account = AccountId::from_bytes([0; 16]),
            1 => header.epoch = StoreEpoch::from_bytes([0; 16]),
            2 => header.segment = 3,
            3 => header.base = Sequence::from_u64(1),
            _ => {}
        }
        header.encode(&Provider, &mut bytes[..96]).unwrap();
        let frame =
            td_mta::format::frame_header::Header::decode(&Provider, &bytes[96..160]).unwrap();
        td_mta::format::frame::seal(
            &Provider,
            header.base.successor().unwrap(),
            frame.operations,
            &mut bytes[96..],
        )
        .unwrap();
        let summary = journal_summary(&bytes);
        let mut declared = descriptor;
        declared.digest = summary.digest();
        declared.file_bytes = bytes.len() as u64;
        if field == 4 {
            declared.through = Sequence::from_u64(2);
        }
        if field == 5 {
            declared.file_bytes += 1;
        }
        let selected = selected_history_manifest(declared);
        let current = selected_current(&selected);
        let selection =
            Selection::decode(&Provider, account(), &format, &current, &selected).unwrap();
        assert_eq!(
            selection.check_history_journal(&Provider, 0, summary),
            Err(Error::Format(FormatError::InvalidValue)),
            "field {field}"
        );
    }
}
#[test]
fn history_requires_whole_file_digest_and_rejects_missing_descriptors() {
    let format = format();
    let manifest = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let current = selected_current(&manifest);
    let selection = Selection::decode(&Provider, account(), &format, &current, &manifest).unwrap();
    let mut bytes = hex(include_str!("fixtures/format-v1/journal-with-frame.hex"));
    bytes[96 + 64 + 12] ^= 1;
    td_mta::format::frame::seal(&Provider, Sequence::from_u64(1), 1, &mut bytes[96..]).unwrap();
    let actual = journal_summary(&bytes);
    assert_eq!(
        selection.check_history_journal(&Provider, 0, actual),
        Err(Error::Checksum)
    );
    assert_eq!(
        selection.check_history_journal(&Provider, 1, actual),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(
        selection.check_history_journal(&Provider, usize::MAX, actual),
        Err(Error::Format(FormatError::InvalidValue))
    );
    let initial = hex(include_str!("fixtures/format-v1/manifest.hex"));
    let current = selected_current(&initial);
    let empty = Selection::decode(&Provider, account(), &format, &current, &initial).unwrap();
    let header = hex(include_str!("fixtures/format-v1/empty-journal.hex"));
    empty
        .check_active_prefix(Sequence::default(), 96, journal_summary(&header))
        .unwrap();
    assert_eq!(
        empty.check_history_journal(&Provider, 0, actual),
        Err(Error::Format(FormatError::InvalidValue))
    );
    let mut descriptor = selection.manifest().history(0).unwrap();
    descriptor.digest = hash(&bytes[96..]);
    let manifest = selected_history_manifest(descriptor);
    let current = selected_current(&manifest);
    let selection = Selection::decode(&Provider, account(), &format, &current, &manifest).unwrap();
    assert_eq!(
        selection.check_history_journal(&Provider, 0, actual),
        Err(Error::Checksum)
    );
}

#[test]
fn history_binding_uses_injected_comparison_after_descriptor_checks_without_rehashing() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let format = format();
    let manifest = hex(include_str!("fixtures/format-v1/manifest-history.hex"));
    let current = selected_current(&manifest);
    let selection = Selection::decode(&Provider, account(), &format, &current, &manifest).unwrap();
    let bytes = hex(include_str!("fixtures/format-v1/journal-with-frame.hex"));
    let actual = journal_summary(&bytes);
    let crypto = FailFactory {
        count: AtomicUsize::new(0),
        comparisons: AtomicUsize::new(0),
        reject_comparison: true,
        at: 1,
    };
    assert_eq!(
        selection.check_history_journal(&crypto, usize::MAX, actual),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(crypto.comparisons.load(Ordering::Relaxed), 0);
    let wrong_size = journal_summary(&journal_deletes(1));
    assert_eq!(
        selection.check_history_journal(&crypto, 0, wrong_size),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(crypto.comparisons.load(Ordering::Relaxed), 0);
    assert_eq!(
        selection.check_history_journal(&crypto, 0, actual),
        Err(Error::Checksum)
    );
    assert_eq!(crypto.comparisons.load(Ordering::Relaxed), 1);
    assert_eq!(crypto.count.load(Ordering::Relaxed), 0);
}
