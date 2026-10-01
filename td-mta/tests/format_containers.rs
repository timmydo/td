#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use td_crypto::{Crypto, Digest, Error as CryptoError, Provider, Sha256};
use td_mta::{
    format::{
        container::{Current, Error, JournalHeader, StoreIdentity},
        table::{Record, TableHeader},
        Error as FormatError, Sequence, Table,
    },
    ids::{AccountId, InstanceId, StoreEpoch},
};

fn hex(input: &str) -> Vec<u8> {
    let digits: String = input.split_whitespace().collect();
    let (pairs, rest) = digits.as_bytes().as_chunks::<2>();
    assert!(rest.is_empty());
    pairs
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn rehash(bytes: &mut [u8]) {
    let end = bytes.len() - 32;
    let mut digest = Provider.sha256().unwrap();
    digest.update(&bytes[..end]).unwrap();
    bytes[end..].copy_from_slice(&digest.finish().unwrap());
}

trait Codec: Copy + std::fmt::Debug + PartialEq {
    fn decode<C: Crypto>(crypto: &C, bytes: &[u8]) -> Result<Self, Error>;
    fn encode<C: Crypto>(self, crypto: &C, bytes: &mut [u8]) -> Result<usize, Error>;
}
macro_rules! codec {
    ($ty:ty) => {
        impl Codec for $ty {
            fn decode<C: Crypto>(crypto: &C, bytes: &[u8]) -> Result<Self, Error> {
                <$ty>::decode(crypto, bytes)
            }
            fn encode<C: Crypto>(self, crypto: &C, bytes: &mut [u8]) -> Result<usize, Error> {
                self.encode(crypto, bytes)
            }
        }
    };
}
codec!(StoreIdentity);
codec!(Current);
codec!(JournalHeader);
codec!(TableHeader);

fn check<T: Codec>(literal: &str, expected: T) {
    let bytes = hex(literal);
    let length = bytes.len();
    assert_eq!(T::decode(&Provider, &bytes), Ok(expected));
    let mut output = vec![0xa5; length + 7];
    assert_eq!(expected.encode(&Provider, &mut output), Ok(length));
    assert_eq!(&output[..length], bytes);
    assert_eq!(&output[length..], &[0xa5; 7]);
    for end in 0..length {
        assert_eq!(
            T::decode(&Provider, &bytes[..end]),
            Err(Error::Format(FormatError::Truncated))
        );
        let mut short = vec![0xa5; end];
        assert_eq!(
            expected.encode(&Provider, &mut short),
            Err(Error::Format(FormatError::OutputFull))
        );
        assert!(short.iter().all(|&b| b == 0xa5));
    }
    let mut appended = bytes.clone();
    appended.push(0);
    assert_eq!(
        T::decode(&Provider, &appended),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    for offset in 0..length {
        let mut damaged = bytes.clone();
        damaged[offset] ^= 1;
        assert_eq!(T::decode(&Provider, &damaged), Err(Error::Checksum));
    }
    // Recomputed checksums cannot legalize wrong magic, versions or flags.
    for offset in 0..16 {
        let mut invalid = bytes.clone();
        invalid[offset] ^= 0x80;
        rehash(&mut invalid);
        assert_eq!(
            T::decode(&Provider, &invalid),
            Err(Error::Format(FormatError::InvalidTag))
        );
    }
    for fault in [Fault::Start, Fault::Update(1), Fault::Finish] {
        let crypto = FaultCrypto(fault);
        assert_eq!(
            T::decode(&crypto, &bytes),
            Err(Error::Crypto(CryptoError::Crypto))
        );
        let mut output = vec![0xa5; length + 7];
        assert_eq!(
            expected.encode(&crypto, &mut output),
            Err(Error::Crypto(CryptoError::Crypto))
        );
        assert!(output.iter().all(|&b| b == 0xa5));
        // Capacity/extent checks run before requesting a digest.
        assert_eq!(
            T::decode(&crypto, &bytes[..length - 1]),
            Err(Error::Format(FormatError::Truncated))
        );
        assert_eq!(
            expected.encode(&crypto, &mut output[..length - 1]),
            Err(Error::Format(FormatError::OutputFull))
        );
    }
    assert_eq!(
        T::decode(&FaultCrypto(Fault::Compare), &bytes),
        Err(Error::Checksum)
    );
}

#[test]
fn exact_fixed_containers_use_independent_literal_bytes() {
    check(
        include_str!("fixtures/format-v1/format.hex"),
        StoreIdentity {
            instance: InstanceId::from_bytes([0x11; 16]),
            epoch: StoreEpoch::from_bytes([0x22; 16]),
        },
    );
    for (literal, generation, digest) in [
        (
            include_str!("fixtures/format-v1/current.hex"),
            1,
            "a1b649d90246ebc6cd13e92376a5208401676b621da83dadb34ce1aaafe877ba",
        ),
        (
            include_str!("fixtures/format-v1/current-history.hex"),
            2,
            "39dc0d63a690809856a8e33f4e9d976539037e3d790b321c345ca3c3fa8064dd",
        ),
    ] {
        check(
            literal,
            Current {
                account: AccountId::from_bytes([0x33; 16]),
                epoch: StoreEpoch::from_bytes([0x22; 16]),
                generation,
                manifest_digest: hex(digest).try_into().unwrap(),
            },
        );
    }
    for (literal, segment, base) in [
        (include_str!("fixtures/format-v1/empty-journal.hex"), 1, 0),
        (
            include_str!("fixtures/format-v1/active-journal-two.hex"),
            2,
            1,
        ),
    ] {
        check(
            literal,
            JournalHeader {
                account: AccountId::from_bytes([0x33; 16]),
                epoch: StoreEpoch::from_bytes([0x22; 16]),
                segment,
                base: Sequence::from_u64(base),
            },
        );
    }
}

#[test]
fn zero_names_refuse_even_with_valid_checksums() {
    let mut current = hex(include_str!("fixtures/format-v1/current.hex"));
    let mut journal = hex(include_str!("fixtures/format-v1/empty-journal.hex"));
    let invalid_current = Current {
        generation: 0,
        ..Current::decode(&Provider, &current).unwrap()
    };
    let invalid_journal = JournalHeader {
        segment: 0,
        ..JournalHeader::decode(&Provider, &journal).unwrap()
    };
    for bytes in [&mut current, &mut journal] {
        bytes[48..56].fill(0);
        rehash(bytes);
    }
    assert_eq!(
        Current::decode(&Provider, &current),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(
        JournalHeader::decode(&Provider, &journal),
        Err(Error::Format(FormatError::InvalidValue))
    );
    let mut output = [0xa5; 128];
    assert_eq!(
        invalid_current.encode(&Provider, &mut output),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(output, [0xa5; 128]);
    assert_eq!(
        invalid_journal.encode(&Provider, &mut output),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(output, [0xa5; 128]);
}

#[test]
fn opaque_ids_and_exhausted_sequence_remain_readable() {
    let mut bytes = [0; 120];
    let identity = StoreIdentity {
        instance: InstanceId::from_bytes([0; 16]),
        epoch: StoreEpoch::from_bytes([0xff; 16]),
    };
    let n = identity.encode(&Provider, &mut bytes).unwrap();
    assert_eq!(StoreIdentity::decode(&Provider, &bytes[..n]), Ok(identity));
    let current = Current {
        account: AccountId::from_bytes([0; 16]),
        epoch: identity.epoch,
        generation: u64::MAX,
        manifest_digest: [0; 32],
    };
    let n = current.encode(&Provider, &mut bytes).unwrap();
    assert_eq!(Current::decode(&Provider, &bytes[..n]), Ok(current));
    let journal = JournalHeader {
        account: current.account,
        epoch: current.epoch,
        segment: u64::MAX,
        base: Sequence::from_u64(u64::MAX),
    };
    let n = journal.encode(&Provider, &mut bytes).unwrap();
    assert_eq!(JournalHeader::decode(&Provider, &bytes[..n]), Ok(journal));
    assert_eq!(journal.base.successor(), Err(FormatError::Exhausted));
    let whole = hex(include_str!("fixtures/format-v1/journal-with-frame.hex"));
    assert_eq!(
        JournalHeader::decode(&Provider, &whole),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    assert_eq!(
        JournalHeader::decode(&Provider, &whole[..96])
            .unwrap()
            .base
            .number(),
        0
    );
}

fn table_header(table: Table) -> TableHeader {
    TableHeader {
        table,
        account: AccountId::from_bytes([0x33; 16]),
        epoch: StoreEpoch::from_bytes([0x22; 16]),
        generation: 1,
        through: Sequence::default(),
        record_count: 0,
        payload_bytes: 0,
    }
}

#[test]
fn table_headers_match_all_initial_tags_and_populated_checkpoint() {
    let literals = [
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
    for (tag, literal) in (1..=11).zip(literals) {
        check(literal, table_header(Table::from_tag(tag).unwrap()));
    }
    check(
        include_str!("fixtures/format-v1/checkpoint-table-2.hex"),
        TableHeader {
            generation: 2,
            through: Sequence::from_u64(1),
            ..table_header(Table::Mailboxes)
        },
    );
    let full = hex(include_str!("fixtures/format-v1/populated-blob-table.hex"));
    let header = TableHeader {
        generation: 2,
        through: Sequence::from_u64(1),
        record_count: 1,
        payload_bytes: 113,
        ..table_header(Table::Blobs)
    };
    assert_eq!(TableHeader::decode(&Provider, &full[..112]), Ok(header));
    let mut encoded = [0; 112];
    assert_eq!(header.encode(&Provider, &mut encoded), Ok(112));
    assert_eq!(encoded, &full[..112]);
    assert_eq!(header.file_bytes(), Ok(full.len() as u64));
    assert_eq!(
        TableHeader::decode(&Provider, &full),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    assert_eq!(
        &full[112..],
        hex(include_str!("fixtures/format-v1/record-blob.hex"))
    );
}

#[test]
fn table_header_sanity_and_extent_arithmetic_refuse_invalid_metadata() {
    let valid = TableHeader {
        through: Sequence::from_u64(1),
        record_count: 1,
        payload_bytes: 113,
        ..table_header(Table::Blobs)
    };
    let cases = [
        (
            TableHeader {
                generation: 0,
                ..valid
            },
            FormatError::InvalidValue,
        ),
        (
            TableHeader {
                through: Sequence::default(),
                ..valid
            },
            FormatError::InvalidValue,
        ),
        (
            TableHeader {
                record_count: 0,
                ..valid
            },
            FormatError::InvalidValue,
        ),
        (
            TableHeader {
                payload_bytes: 0,
                ..valid
            },
            FormatError::InvalidValue,
        ),
        (
            TableHeader {
                payload_bytes: 63,
                ..valid
            },
            FormatError::InvalidValue,
        ),
        (
            TableHeader {
                record_count: 2,
                payload_bytes: 127,
                ..valid
            },
            FormatError::InvalidValue,
        ),
        (
            TableHeader {
                payload_bytes: 66609,
                ..valid
            },
            FormatError::InvalidValue,
        ),
        (
            TableHeader {
                record_count: u64::MAX,
                ..valid
            },
            FormatError::Overflow,
        ),
        (
            TableHeader {
                record_count: 1_000_000_000_000_000,
                payload_bytes: u64::MAX,
                ..valid
            },
            FormatError::Overflow,
        ),
    ];
    for (header, error) in cases {
        let mut output = [0xa5; 120];
        assert_eq!(
            header.encode(&Provider, &mut output),
            Err(Error::Format(error))
        );
        assert_eq!(output, [0xa5; 120]);
        // Build a checksum-valid header independently of the encoder's validation.
        let mut bytes = hex(include_str!("fixtures/format-v1/empty-table-1.hex"));
        bytes[48..56].copy_from_slice(&header.generation.to_le_bytes());
        bytes[56..64].copy_from_slice(&header.through.number().to_le_bytes());
        bytes[64..72].copy_from_slice(&header.record_count.to_le_bytes());
        bytes[72..80].copy_from_slice(&header.payload_bytes.to_le_bytes());
        rehash(&mut bytes);
        assert_eq!(
            TableHeader::decode(&Provider, &bytes),
            Err(Error::Format(error))
        );
    }
    let upper = TableHeader {
        payload_bytes: 66608,
        ..valid
    };
    let mut upper_bytes = [0; 112];
    upper.encode(&Provider, &mut upper_bytes).unwrap();
    assert_eq!(TableHeader::decode(&Provider, &upper_bytes), Ok(upper));
    let count = u64::MAX / 64 - 2;
    let header = TableHeader {
        table: Table::Threads,
        generation: u64::MAX,
        through: Sequence::from_u64(u64::MAX),
        record_count: count,
        payload_bytes: count * 64,
        ..valid
    };
    let mut bytes = [0; 112];
    header.encode(&Provider, &mut bytes).unwrap();
    assert_eq!(TableHeader::decode(&Provider, &bytes), Ok(header));
    assert_eq!(header.file_bytes(), Ok(count * 64 + 112));
}

#[test]
fn table_record_uses_literal_envelope_and_validates_every_extent() {
    let bytes = hex(include_str!("fixtures/format-v1/record-blob.hex"));
    let value = hex(include_str!("fixtures/format-v1/row-blob.hex"));
    let key = [0x44; 16];
    let through = Sequence::from_u64(1);
    let record = Record::new(Table::Blobs, through, &key, &value).unwrap();
    let decoded = Record::decode(&Provider, Table::Blobs, through, &bytes).unwrap();
    assert_eq!(decoded, record);
    assert_eq!(decoded.key_bytes(), key);
    assert_eq!(decoded.value_bytes(), value);
    assert_eq!(decoded.row().last_change, through);
    assert_eq!(decoded.row().key.table(), Table::Blobs);
    assert_eq!(decoded.encoded_len(), Ok(113));
    let mut output = [0xa5; 120];
    assert_eq!(record.encode(&Provider, through, &mut output), Ok(113));
    assert_eq!(&output[..113], bytes);
    assert_eq!(&output[113..], &[0xa5; 7]);
    for later in [Sequence::from_u64(2), Sequence::from_u64(u64::MAX)] {
        assert_eq!(
            Record::decode(&Provider, Table::Blobs, later, &bytes),
            Ok(record)
        );
        output.fill(0xa5);
        assert_eq!(record.encode(&Provider, later, &mut output), Ok(113));
        assert_eq!(&output[..113], bytes);
        assert_eq!(&output[113..], &[0xa5; 7]);
    }
    for end in 0..bytes.len() {
        assert_eq!(
            Record::decode(&Provider, Table::Blobs, through, &bytes[..end]),
            Err(Error::Format(FormatError::Truncated))
        );
        let mut short = vec![0xa5; end];
        assert_eq!(
            record.encode(&Provider, through, &mut short),
            Err(Error::Format(FormatError::OutputFull))
        );
        assert!(short.iter().all(|&b| b == 0xa5));
    }
    let mut appended = bytes.clone();
    appended.push(0);
    assert_eq!(
        Record::decode(&Provider, Table::Blobs, through, &appended),
        Err(Error::Format(FormatError::TrailingBytes))
    );
    for offset in 0..bytes.len() {
        let mut changed = bytes.clone();
        changed[offset] ^= 1;
        let result = Record::decode(&Provider, Table::Blobs, through, &changed);
        if offset >= 8 {
            assert_eq!(result, Err(Error::Checksum));
        } else {
            assert!(result.is_err());
        }
    }
    for fault in [
        Fault::Start,
        Fault::Update(1),
        Fault::Update(2),
        Fault::Update(3),
        Fault::Finish,
    ] {
        let mut output = [0xa5; 120];
        assert_eq!(
            record.encode(&FaultCrypto(fault), through, &mut output),
            Err(Error::Crypto(CryptoError::Crypto))
        );
        assert_eq!(output, [0xa5; 120]);
        if !matches!(fault, Fault::Update(2 | 3)) {
            assert_eq!(
                Record::decode(&FaultCrypto(fault), Table::Blobs, through, &bytes),
                Err(Error::Crypto(CryptoError::Crypto))
            );
        }
    }
    assert_eq!(
        Record::decode(&FaultCrypto(Fault::Compare), Table::Blobs, through, &bytes),
        Err(Error::Checksum)
    );
}

#[test]
fn record_limits_sequence_and_row_grammar_are_separate_from_checksum() {
    let literal = hex(include_str!("fixtures/format-v1/record-blob.hex"));
    let through = Sequence::from_u64(1);
    for (key, value) in [
        (0u32, 0u32),
        (15, 0),
        (1025, 0),
        (16, 65537),
        (u32::MAX, u32::MAX),
    ] {
        let mut prefix = [0; 16];
        prefix[..4].copy_from_slice(&key.to_le_bytes());
        prefix[4..8].copy_from_slice(&value.to_le_bytes());
        assert_eq!(
            Record::decode(&FaultCrypto(Fault::Start), Table::Blobs, through, &prefix),
            Err(Error::Format(FormatError::Limit))
        );
    }
    for (offset, change, error) in [
        (8, 0, FormatError::InvalidValue),
        (8, 2, FormatError::InvalidValue),
        (32, 0xff, FormatError::InvalidTag),
    ] {
        let mut bytes = literal.clone();
        bytes[offset] = change;
        rehash(&mut bytes);
        assert_eq!(
            Record::decode(&Provider, Table::Blobs, through, &bytes),
            Err(Error::Format(error))
        );
    }
    assert_eq!(
        Record::decode(&Provider, Table::Memberships, through, &literal),
        Err(Error::Format(FormatError::Truncated))
    );
    let record = Record::decode(&Provider, Table::Blobs, through, &literal).unwrap();
    assert_eq!(
        Record::new(
            Table::Blobs,
            Sequence::default(),
            record.key_bytes(),
            record.value_bytes()
        ),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(
        Record::new(Table::Blobs, through, &vec![0; 1025], record.value_bytes()),
        Err(Error::Format(FormatError::Limit))
    );
    assert_eq!(
        Record::new(Table::Blobs, through, record.key_bytes(), &vec![0; 65537]),
        Err(Error::Format(FormatError::Limit))
    );
    let mut output = [0xa5; 120];
    assert_eq!(
        record.encode(&Provider, Sequence::default(), &mut output),
        Err(Error::Format(FormatError::InvalidValue))
    );
    assert_eq!(output, [0xa5; 120]);
    let last = Record::new(
        Table::Blobs,
        Sequence::from_u64(u64::MAX),
        record.key_bytes(),
        record.value_bytes(),
    )
    .unwrap();
    last.encode(&Provider, Sequence::from_u64(u64::MAX), &mut output)
        .unwrap();
    assert_eq!(
        Record::decode(
            &Provider,
            Table::Blobs,
            Sequence::from_u64(u64::MAX),
            &output[..113]
        ),
        Ok(last)
    );
    let mut key = vec![b'a'; 1024];
    key[..4].copy_from_slice(&1004u32.to_le_bytes());
    key[1008..].fill(0x77);
    let largest_key = Record::new(Table::ThreadAnchors, through, &key, &[]).unwrap();
    let mut output = vec![0; 1072];
    assert_eq!(
        largest_key.encode(&Provider, through, &mut output),
        Ok(1072)
    );
    assert_eq!(
        Record::decode(&Provider, Table::ThreadAnchors, through, &output),
        Ok(largest_key)
    );
}

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    Start,
    Update(usize),
    Finish,
    Compare,
}
struct FaultCrypto(Fault);
struct FaultDigest {
    fault: Fault,
    inner: Sha256,
    failed: bool,
    updates: usize,
}
impl Digest for FaultDigest {
    fn update(&mut self, bytes: &[u8]) -> Result<(), CryptoError> {
        self.updates += 1;
        if self.failed || self.fault == Fault::Update(self.updates) {
            self.failed = true;
            return Err(CryptoError::Crypto);
        }
        let result = self.inner.update(bytes);
        self.failed |= result.is_err();
        result
    }
    fn finish(self) -> Result<[u8; 32], CryptoError> {
        if self.failed || self.fault == Fault::Finish {
            return Err(CryptoError::Crypto);
        }
        self.inner.finish()
    }
}
impl Crypto for FaultCrypto {
    type Sha256 = FaultDigest;
    type SigningKey = ();
    fn sha256(&self) -> Result<FaultDigest, CryptoError> {
        if self.0 == Fault::Start {
            return Err(CryptoError::Crypto);
        }
        Ok(FaultDigest {
            fault: self.0,
            inner: Sha256::try_new()?,
            failed: false,
            updates: 0,
        })
    }
    fn equal_digest(&self, left: &[u8; 32], right: &[u8; 32]) -> bool {
        self.0 != Fault::Compare && Provider.equal_digest(left, right)
    }
    fn generate_p256(&self, _: &mut [u8]) -> Result<usize, CryptoError> {
        Err(CryptoError::Invalid)
    }
    fn load_p256(&self, _: &[u8]) -> Result<(), CryptoError> {
        Err(CryptoError::Invalid)
    }
    fn p256_public(&self, _: &(), _: &mut [u8; 65]) -> Result<(), CryptoError> {
        Err(CryptoError::Invalid)
    }
    fn sign_es256(&self, _: &(), _: &[u8], _: &mut [u8; 64]) -> Result<(), CryptoError> {
        Err(CryptoError::Invalid)
    }
}
